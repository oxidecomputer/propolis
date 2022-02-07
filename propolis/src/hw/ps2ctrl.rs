use std::collections::VecDeque;
use std::mem::replace;
use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::chipset::Chipset;
use crate::hw::ibmpc;
use crate::instance;
use crate::intr_pins::LegacyPin;
use crate::migrate::Migrate;
use crate::pio::{PioBus, PioFn};

use erased_serde::Serialize;

bitflags! {
    #[derive(Default)]
    pub struct CtrlStatus: u8 {
        const OUT_FULL = 1 << 0;
        const IN_FULL = 1 << 1;
        const SYS_FLAG = 1 << 2;
        const CMD_DATA = 1 << 3;
        const UNLOCKED = 1 << 4;
        const AUX_FULL = 1 << 5;
        const TMO = 1 << 6;
        const PARITY = 1 << 7;
    }
}

bitflags! {
    #[derive(Default)]
    pub struct CtrlCfg: u8 {
        const PRI_INTR_EN = 1 << 0;
        const AUX_INTR_EN = 1 << 1;
        const SYS_FLAG = 1 << 2;
        const PRI_CLOCK_DIS = 1 << 4;
        const AUX_CLOCK_DIS = 1 << 5;
        const PRI_XLATE_EN = 1 << 6;
    }
}

bitflags! {
    #[derive(Default)]
    pub struct CtrlOutPort: u8 {
        const A20 = 1 << 1;
        const AUX_CLOCK = 1 << 2;
        const AUX_DATA = 1 << 3;
        const PRI_FULL = 1 << 4;
        const AUX_FULL = 1 << 5;
        const PRI_CLOCK = 1 << 6;
        const PRI_DATA = 1 << 7;

        // PRI_FULL and AUX_FULL are dynamic
        const DYN_FLAGS = (1 << 4) | (1 << 5);
    }
}

const PS2C_CMD_READ_CTRL_CFG: u8 = 0x20;
const PS2C_CMD_WRITE_CTRL_CFG: u8 = 0x60;
const PS2C_CMD_READ_RAM_START: u8 = 0x21;
const PS2C_CMD_READ_RAM_END: u8 = 0x3f;
const PS2C_CMD_WRITE_RAM_START: u8 = 0x61;
const PS2C_CMD_WRITE_RAM_END: u8 = 0x7f;
const PS2C_CMD_AUX_PORT_DIS: u8 = 0xa7;
const PS2C_CMD_AUX_PORT_ENA: u8 = 0xa8;
const PS2C_CMD_AUX_PORT_TEST: u8 = 0xa9;
const PS2C_CMD_CTRL_TEST: u8 = 0xaa;
const PS2C_CMD_PRI_PORT_TEST: u8 = 0xab;
const PS2C_CMD_PRI_PORT_DIS: u8 = 0xad;
const PS2C_CMD_PRI_PORT_ENA: u8 = 0xae;
const PS2C_CMD_READ_CTLR_OUT: u8 = 0xd0;
const PS2C_CMD_WRITE_CTLR_OUT: u8 = 0xd1;
const PS2C_CMD_WRITE_PRI_OUT: u8 = 0xd2;
const PS2C_CMD_WRITE_AUX_OUT: u8 = 0xd3;
const PS2C_CMD_WRITE_AUX_IN: u8 = 0xd4;
const PS2C_CMD_PULSE_START: u8 = 0xf0;
const PS2C_CMD_PULSE_END: u8 = 0xff;

const PS2C_R_CTRL_TEST_PASS: u8 = 0x55;
const PS2C_R_PORT_TEST_PASS: u8 = 0x00;

const PS2C_RAM_LEN: usize =
    (PS2C_CMD_WRITE_RAM_END - PS2C_CMD_WRITE_RAM_START) as usize;

#[derive(Default)]
struct PS2State {
    resp: Option<u8>,
    cmd_prefix: Option<u8>,
    ctrl_cfg: CtrlCfg,
    ctrl_out_port: CtrlOutPort,
    ram: [u8; PS2C_RAM_LEN],

    pri_port: PS2Kbd,
    aux_port: PS2Mouse,

    pri_pin: Option<LegacyPin>,
    aux_pin: Option<LegacyPin>,
}

pub struct PS2Ctrl {
    state: Mutex<PS2State>,
}
impl PS2Ctrl {
    pub fn create() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(PS2State::default()) })
    }
    pub fn attach(self: &Arc<Self>, bus: &PioBus, chipset: &dyn Chipset) {
        let this = Arc::clone(self);
        let piofn = Arc::new(move |port: u16, rwo: RWOp, ctx: &DispCtx| {
            this.pio_rw(port, rwo, ctx)
        }) as Arc<PioFn>;

        bus.register(ibmpc::PORT_PS2_DATA, 1, Arc::clone(&piofn)).unwrap();
        bus.register(ibmpc::PORT_PS2_CMD_STATUS, 1, piofn).unwrap();

        let mut state = self.state.lock().unwrap();
        state.pri_pin = Some(chipset.irq_pin(ibmpc::IRQ_PS2_PRI).unwrap());
        state.aux_pin = Some(chipset.irq_pin(ibmpc::IRQ_PS2_AUX).unwrap());
    }

    fn pio_rw(&self, port: u16, rwo: RWOp, ctx: &DispCtx) {
        assert_eq!(rwo.len(), 1);
        match port {
            ibmpc::PORT_PS2_DATA => match rwo {
                RWOp::Read(ro) => ro.write_u8(self.data_read()),
                RWOp::Write(wo) => self.data_write(wo.read_u8()),
            },
            ibmpc::PORT_PS2_CMD_STATUS => match rwo {
                RWOp::Read(ro) => ro.write_u8(self.status_read()),
                RWOp::Write(wo) => self.cmd_write(wo.read_u8(), ctx),
            },
            _ => {
                panic!("unexpected pio in {:x}", port);
            }
        }
    }

    fn data_write(&self, v: u8) {
        let mut state = self.state.lock().unwrap();
        let cmd_prefix = replace(&mut state.cmd_prefix, None);
        if let Some(prefix) = cmd_prefix {
            match prefix {
                PS2C_CMD_WRITE_CTRL_CFG => {
                    state.ctrl_cfg = CtrlCfg::from_bits_truncate(v);
                }
                PS2C_CMD_WRITE_RAM_START..=PS2C_CMD_WRITE_RAM_END => {
                    let off = v - PS2C_CMD_WRITE_RAM_START;
                    state.ram[off as usize] = v;
                }
                PS2C_CMD_WRITE_CTLR_OUT => {
                    state.ctrl_out_port = CtrlOutPort::from_bits_truncate(v);
                    state.ctrl_out_port.remove(CtrlOutPort::DYN_FLAGS);
                }
                PS2C_CMD_WRITE_PRI_OUT => {
                    state.pri_port.loopback(v);
                }
                PS2C_CMD_WRITE_AUX_OUT => {
                    state.aux_port.loopback(v);
                }
                PS2C_CMD_WRITE_AUX_IN => {
                    state.aux_port.cmd_input(v);
                }
                _ => {
                    panic!("unexpected chain cmd {:x}", prefix);
                }
            }
        } else {
            state.pri_port.cmd_input(v);
        }
        self.update_intr(&mut state);
    }
    fn data_read(&self) -> u8 {
        let mut state = self.state.lock().unwrap();
        if let Some(rval) = state.resp {
            state.resp = None;
            rval
        } else if state.pri_port.has_output() {
            let rval = state.pri_port.read_output().unwrap();
            self.update_intr(&mut state);
            rval
        } else if state.aux_port.has_output() {
            let rval = state.aux_port.read_output().unwrap();
            self.update_intr(&mut state);
            rval
        } else {
            0
        }
    }
    fn cmd_write(&self, v: u8, ctx: &DispCtx) {
        let mut state = self.state.lock().unwrap();
        match v {
            PS2C_CMD_READ_CTRL_CFG => {
                state.resp = Some(state.ctrl_cfg.bits());
            }
            PS2C_CMD_READ_RAM_START..=PS2C_CMD_READ_RAM_END => {
                let off = v - PS2C_CMD_READ_RAM_START;
                state.resp = Some(state.ram[off as usize])
            }
            PS2C_CMD_CTRL_TEST => {
                state.resp = Some(PS2C_R_CTRL_TEST_PASS);
            }

            PS2C_CMD_PRI_PORT_TEST => {
                state.resp = Some(PS2C_R_PORT_TEST_PASS);
            }
            PS2C_CMD_AUX_PORT_TEST => {
                state.resp = Some(PS2C_R_PORT_TEST_PASS);
            }
            PS2C_CMD_PRI_PORT_ENA | PS2C_CMD_PRI_PORT_DIS => {
                state
                    .ctrl_cfg
                    .set(CtrlCfg::PRI_CLOCK_DIS, v == PS2C_CMD_PRI_PORT_DIS);
            }
            PS2C_CMD_AUX_PORT_ENA | PS2C_CMD_AUX_PORT_DIS => {
                state
                    .ctrl_cfg
                    .set(CtrlCfg::AUX_CLOCK_DIS, v == PS2C_CMD_AUX_PORT_DIS);
            }

            PS2C_CMD_READ_CTLR_OUT => {
                let mut val = state.ctrl_out_port;
                val.set(CtrlOutPort::PRI_FULL, state.pri_port.has_output());
                val.set(CtrlOutPort::AUX_FULL, state.aux_port.has_output());
                state.resp = Some(val.bits());
            }

            // commands with a following byte to complete
            PS2C_CMD_WRITE_CTRL_CFG
            | PS2C_CMD_WRITE_CTLR_OUT
            | PS2C_CMD_WRITE_PRI_OUT
            | PS2C_CMD_WRITE_AUX_OUT
            | PS2C_CMD_WRITE_AUX_IN
            | PS2C_CMD_WRITE_RAM_START..=PS2C_CMD_WRITE_RAM_END => {
                state.cmd_prefix = Some(v)
            }

            PS2C_CMD_PULSE_START..=PS2C_CMD_PULSE_END => {
                let to_pulse = v - PS2C_CMD_PULSE_START;
                if to_pulse == 0xe {
                    ctx.trigger_suspend(
                        instance::SuspendKind::Reset,
                        instance::SuspendSource::Device("PS/2 Controller"),
                    );
                }
            }

            _ => {
                // ignore all other unrecognized commands
            }
        }
    }
    fn status_read(&self) -> u8 {
        let state = self.state.lock().unwrap();
        // Always report unlocked
        let mut val = CtrlStatus::UNLOCKED;

        if state.resp.is_some()
            || state.pri_port.has_output()
            || state.aux_port.has_output()
        {
            val.insert(CtrlStatus::OUT_FULL);
        }
        val.set(CtrlStatus::AUX_FULL, state.aux_port.has_output());
        val.set(CtrlStatus::CMD_DATA, state.cmd_prefix.is_some());
        val.set(
            CtrlStatus::SYS_FLAG,
            state.ctrl_cfg.contains(CtrlCfg::SYS_FLAG),
        );

        val.bits()
    }
    fn update_intr(&self, state: &mut PS2State) {
        // We currently choose to mimic qemu, which gates the keyboard interrupt
        // with the keyboard-clock-disable in addition to the interrupt enable.
        let pri_pin = state.pri_pin.as_ref().unwrap();
        pri_pin.set_state(
            state.ctrl_cfg.contains(CtrlCfg::PRI_INTR_EN)
                && !state.ctrl_cfg.contains(CtrlCfg::PRI_CLOCK_DIS)
                && state.pri_port.has_output(),
        );
        let aux_pin = state.aux_pin.as_ref().unwrap();
        aux_pin.set_state(
            state.ctrl_cfg.contains(CtrlCfg::AUX_INTR_EN)
                && state.aux_port.has_output(),
        );
    }
    fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        state.pri_port.reset();
        state.aux_port.reset();
        state.resp = None;
        state.cmd_prefix = None;
        state.ctrl_cfg = CtrlCfg::default();
        state.ctrl_out_port = CtrlOutPort::default();
        for b in state.ram.iter_mut() {
            *b = 0;
        }
        self.update_intr(&mut state);
    }
}
impl Entity for PS2Ctrl {
    fn type_name(&self) -> &'static str {
        "lpc-ps2ctrl"
    }
    fn reset(&self, _ctx: &DispCtx) {
        PS2Ctrl::reset(self);
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for PS2Ctrl {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        let state = self.state.lock().unwrap();
        let kbd = &state.pri_port;
        let mouse = &state.aux_port;
        Box::new(migrate::PS2CtrlV1 {
            ctrl: migrate::PS2CtrlStateV1 {
                response: state.resp,
                cmd_prefix: state.cmd_prefix,
                ctrl_cfg: state.ctrl_cfg.bits(),
                ctrl_out_port: state.ctrl_out_port.bits(),
                ram: state.ram,
            },
            kbd: migrate::PS2KbdV1 {
                buf: kbd.buf.clone().into(),
                current_cmd: kbd.cur_cmd,
                enabled: kbd.enabled,
                led_status: kbd.led_status,
                typematic: kbd.typematic,
                scan_code_set: kbd.scan_code_set.as_byte(),
            },
            mouse: migrate::PS2MouseV1 {
                buf: mouse.buf.clone().into(),
                current_cmd: mouse.cur_cmd,
                status: mouse.status.bits(),
                resolution: mouse.resolution,
                sample_rate: mouse.sample_rate,
            },
        })
    }
}

const PS2K_CMD_SET_LEDS: u8 = 0xed;
const PS2K_CMD_SCAN_CODE: u8 = 0xf0;
const PS2K_CMD_TYPEMATIC: u8 = 0xf3;

const PS2K_CMD_ECHO: u8 = 0xee;
const PS2K_CMD_IDENT: u8 = 0xf2;
const PS2K_CMD_SCAN_EN: u8 = 0xf4;
const PS2K_CMD_SCAN_DIS: u8 = 0xf5;
const PS2K_CMD_SET_DEFAULT: u8 = 0xf6;
const PS2K_CMD_RESEND: u8 = 0xfe;
const PS2K_CMD_RESET: u8 = 0xff;

const PS2K_R_ACK: u8 = 0xfa;
const PS2K_R_ECHO: u8 = 0xee;
const PS2K_R_SELF_TEST_PASS: u8 = 0xaa;

const PS2K_TYPEMATIC_MASK: u8 = 0x7f;

const PS2_KBD_BUFSZ: usize = 16;

enum PS2ScanCodeSet {
    Set1,
    Set2,
    // ignoring fancy set3
}
impl PS2ScanCodeSet {
    fn as_byte(&self) -> u8 {
        match self {
            PS2ScanCodeSet::Set1 => 0x1,
            PS2ScanCodeSet::Set2 => 0x2,
        }
    }
}

// TODO: wire up remote console to enabled/led_status/typematic
#[allow(unused)]
struct PS2Kbd {
    buf: VecDeque<u8>,
    cur_cmd: Option<u8>,
    enabled: bool,
    led_status: u8,
    typematic: u8,
    scan_code_set: PS2ScanCodeSet,
}
impl PS2Kbd {
    fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(PS2_KBD_BUFSZ),
            cur_cmd: None,
            enabled: true,
            led_status: 0,
            typematic: 0,
            scan_code_set: PS2ScanCodeSet::Set1,
        }
    }
    fn cmd_input(&mut self, v: u8) {
        if let Some(cmd) = self.cur_cmd {
            self.cur_cmd = None;
            match cmd {
                PS2K_CMD_SET_LEDS => {
                    // low three bits set scroll/num/caps lock
                    self.led_status = v & 0b111;
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_SCAN_CODE => {
                    match v {
                        0 => {
                            // get scan code set
                            self.resp(PS2K_R_ACK);
                            self.resp(self.scan_code_set.as_byte());
                        }
                        1 => {
                            self.resp(PS2K_R_ACK);
                            self.scan_code_set = PS2ScanCodeSet::Set1;
                        }
                        2 => {
                            self.resp(PS2K_R_ACK);
                            self.scan_code_set = PS2ScanCodeSet::Set2;
                        }
                        _ => {}
                    }
                }
                PS2K_CMD_TYPEMATIC => {
                    self.typematic = v & PS2K_TYPEMATIC_MASK;
                    self.resp(PS2K_R_ACK);
                }
                _ => {
                    panic!("bad multi-part ps2 cmd {}", cmd);
                }
            }
        } else {
            match v {
                PS2K_CMD_SET_LEDS | PS2K_CMD_SCAN_CODE | PS2K_CMD_TYPEMATIC => {
                    // multi-part command, wait for next byte
                    self.cur_cmd = Some(v);
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_ECHO => {
                    self.resp(PS2K_R_ECHO);
                }
                PS2K_CMD_IDENT => {
                    self.resp(PS2K_R_ACK);
                    // MF2 keyboard
                    self.resp(0xab);
                    self.resp(0x83);
                }
                PS2K_CMD_SCAN_EN => {
                    self.enabled = true;
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_SCAN_DIS => {
                    self.enabled = false;
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_SET_DEFAULT => {
                    // XXX which things to reset?
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_RESEND => {
                    // XXX we do not track last byte for now
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_RESET => {
                    self.reset();
                    // Even for reset, ack is expected
                    self.resp(PS2K_R_ACK);
                    self.resp(PS2K_R_SELF_TEST_PASS);
                }
                _ => {
                    // ignore unrecognized cmds
                }
            }
        }
    }
    fn resp(&mut self, v: u8) {
        let remain = PS2_KBD_BUFSZ - self.buf.len();
        match remain {
            0 => {
                // overrun already in progress, do nothing
            }
            1 => {
                // indicate overflow instead
                self.buf.push_back(0xff)
            }
            _ => {
                self.buf.push_back(v);
            }
        }
    }
    fn reset(&mut self) {
        // XXX  what should the defaults be?
        self.cur_cmd = None;
        self.enabled = true;
        self.led_status = 0;
        self.typematic = 0;
        self.scan_code_set = PS2ScanCodeSet::Set1;
        self.buf.clear();
    }
    fn has_output(&self) -> bool {
        !self.buf.is_empty()
    }
    fn read_output(&mut self) -> Option<u8> {
        self.buf.pop_front()
    }
    fn loopback(&mut self, v: u8) {
        self.resp(v);
    }
}
impl Default for PS2Kbd {
    fn default() -> Self {
        Self::new()
    }
}

const PS2M_CMD_RESET: u8 = 0xff;
const PS2M_CMD_RESEND: u8 = 0xfe;
const PS2M_CMD_SET_DEFAULTS: u8 = 0xf6;
const PS2M_CMD_DATA_REP_DIS: u8 = 0xf5;
const PS2M_CMD_DATA_REP_ENA: u8 = 0xf4;
const PS2M_CMD_SET_SAMP_RATE: u8 = 0xf3;
const PS2M_CMD_GET_DEVID: u8 = 0xf2;
const PS2M_CMD_REMOTE_MODE_SET: u8 = 0xf0;
const PS2M_CMD_WRAP_MODE_SET: u8 = 0xee;
const PS2M_CMD_WRAP_MODE_RESET: u8 = 0xec;
const PS2M_CMD_READ_DATA: u8 = 0xeb;
const PS2M_CMD_STREAM_MODE_SET: u8 = 0xea;
const PS2M_CMD_STATUS_REQ: u8 = 0xe9;
const PS2M_CMD_RESOLUTION_SET: u8 = 0xe8;
const PS2M_CMD_SCALING1_SET: u8 = 0xe7;
const PS2M_CMD_SCALING2_SET: u8 = 0xe6;

const PS2M_R_ACK: u8 = 0xfa;
const PS2M_R_SELF_TEST_PASS: u8 = 0xaa;
// basic mouse device ID
const PS2M_R_DEVID: u8 = 0x00;

bitflags! {
    #[derive(Default)]
    pub struct PS2MStatus: u8 {
        const B_LEFT = 1 << 0;
        const B_RIGHT = 1 << 1;
        const B_MID = 1 << 2;

        const SCALE2 = 1 << 4;
        const ENABLE = 1 << 5;
        const REMOTE = 1 << 6;
    }
}

struct PS2Mouse {
    buf: VecDeque<u8>,
    cur_cmd: Option<u8>,
    status: PS2MStatus,
    resolution: u8,
    sample_rate: u8,
}
impl PS2Mouse {
    fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(PS2_KBD_BUFSZ),
            cur_cmd: None,
            status: PS2MStatus::empty(),
            resolution: 0,
            sample_rate: 10,
        }
    }
    fn cmd_input(&mut self, v: u8) {
        if let Some(cmd) = self.cur_cmd {
            self.cur_cmd = None;
            match cmd {
                PS2M_CMD_SET_SAMP_RATE => {
                    // XXX: check for valid values?
                    self.sample_rate = v;
                }
                PS2M_CMD_RESOLUTION_SET => {
                    // XXX: check for valid values?
                    self.resolution = v;
                }
                _ => {
                    panic!("bad multi-part ps2 cmd {}", cmd);
                }
            }
        } else {
            match v {
                PS2M_CMD_RESET => {
                    self.reset();
                    self.resp(PS2M_R_ACK);
                    self.resp(PS2M_R_SELF_TEST_PASS);
                    self.resp(PS2M_R_DEVID);
                }
                PS2M_CMD_RESEND => {
                    // XXX: no last byte tracking for now
                    self.resp(PS2M_R_ACK);
                }
                PS2M_CMD_SET_DEFAULTS => {
                    // XXX: set which defaults?
                    self.resp(PS2M_R_ACK);
                }
                PS2M_CMD_DATA_REP_DIS => {
                    self.resp(PS2M_R_ACK);
                    self.status.remove(PS2MStatus::ENABLE);
                }
                PS2M_CMD_DATA_REP_ENA => {
                    self.resp(PS2M_R_ACK);
                    self.status.insert(PS2MStatus::ENABLE);
                }
                PS2M_CMD_GET_DEVID => {
                    self.resp(PS2M_R_ACK);
                    // standard ps2 mouse dev id
                    self.resp(PS2M_R_DEVID);
                }
                PS2M_CMD_REMOTE_MODE_SET => {
                    self.resp(PS2M_R_ACK);
                    self.status.insert(PS2MStatus::REMOTE);
                }
                PS2M_CMD_WRAP_MODE_SET | PS2M_CMD_WRAP_MODE_RESET => {
                    self.resp(PS2M_R_ACK);
                }

                PS2M_CMD_READ_DATA => {
                    self.resp(PS2M_R_ACK);
                    self.movement();
                }
                PS2M_CMD_STREAM_MODE_SET => {
                    // XXX wire to what?
                }
                PS2M_CMD_STATUS_REQ => {
                    // status, resolution, sample rate
                    self.resp(PS2M_R_ACK);
                    self.resp(self.status.bits());
                    self.resp(self.resolution);
                    self.resp(self.sample_rate);
                }

                PS2M_CMD_SET_SAMP_RATE | PS2M_CMD_RESOLUTION_SET => {
                    self.cur_cmd = Some(v);
                    self.resp(PS2M_R_ACK);
                }
                PS2M_CMD_SCALING1_SET | PS2M_CMD_SCALING2_SET => {
                    self.status
                        .set(PS2MStatus::SCALE2, v == PS2M_CMD_SCALING2_SET);
                    self.resp(PS2M_R_ACK);
                }

                _ => {
                    // ignore unrecognized cmds
                }
            }
        }
    }
    fn resp(&mut self, v: u8) {
        let remain = PS2_KBD_BUFSZ - self.buf.len();
        match remain {
            0 => {
                // overrun already in progress, do nothing
            }
            1 => {
                // indicate overflow instead
                self.buf.push_back(0xff)
            }
            _ => {
                self.buf.push_back(v);
            }
        }
    }
    fn reset(&mut self) {
        // XXX  what should the defaults be?
        self.buf.clear();
        self.cur_cmd = None;
        self.status = PS2MStatus::empty();
        self.resolution = 0;
        self.sample_rate = 10;
    }
    fn has_output(&self) -> bool {
        !self.buf.is_empty()
    }
    fn read_output(&mut self) -> Option<u8> {
        self.buf.pop_front()
    }
    fn loopback(&mut self, v: u8) {
        self.resp(v);
    }
    fn movement(&mut self) {
        // no buttons, just the always-one bit
        self.resp(0b00001000);
        // no X movement
        self.resp(0x00);
        // no Y movement
        self.resp(0x00);
    }
}
impl Default for PS2Mouse {
    fn default() -> Self {
        Self::new()
    }
}

pub mod migrate {
    use super::PS2C_RAM_LEN;
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct PS2CtrlV1 {
        pub ctrl: PS2CtrlStateV1,
        pub kbd: PS2KbdV1,
        pub mouse: PS2MouseV1,
    }
    #[derive(Serialize)]
    pub struct PS2CtrlStateV1 {
        pub response: Option<u8>,
        pub cmd_prefix: Option<u8>,
        pub ctrl_cfg: u8,
        pub ctrl_out_port: u8,
        pub ram: [u8; PS2C_RAM_LEN],
    }
    #[derive(Serialize)]
    pub struct PS2KbdV1 {
        pub buf: Vec<u8>,
        pub current_cmd: Option<u8>,
        pub enabled: bool,
        pub led_status: u8,
        pub typematic: u8,
        pub scan_code_set: u8,
    }
    #[derive(Serialize)]
    pub struct PS2MouseV1 {
        pub buf: Vec<u8>,
        pub current_cmd: Option<u8>,
        pub status: u8,
        pub resolution: u8,
        pub sample_rate: u8,
    }
}
