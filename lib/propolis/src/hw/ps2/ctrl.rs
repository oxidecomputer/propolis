// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::convert::TryFrom;
use std::mem::replace;
use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::hw::ibmpc;
use crate::intr_pins::IntrPin;
use crate::migrate::*;
use crate::pio::{PioBus, PioFn};

use rfb::rfb::KeyEvent;

use super::keyboard::KeyEventRep;

/// PS/2 Controller (Intel 8042) Emulation
///
/// Here we emulate both the PS/2 controller and a virtual keyboard device.
///
/// CONTROLLER OVERVIEW
///
///     I/O PORTS
///
///     There are two I/O ports: a control port (0x64) and a data port (0x60).
///     These ports may be used by are the OS to send commands to the controller
///     and PS/2 devices; the controller can respond to commands; and the
///     controller can send device data such as keyboard data to the OS.
///
///     Control Port:
///     - Reads will read from the Controller Status Register ([CtrlStatus]).
///     - Writes will be interpreted as commands to the controller. See
///     `PS2C_CMD_*` for example commands. Some commands require an additional
///     byte, which should be written to the data port.
///
///     Data Port:
///     - Reads may return responses to commands, or device data, depending on
///     the state of the controller.
///     - Writes are interpreted as command data for outstanding commands, or as
///     commands to devices. Commands to specific devices are issued by the OS
///     writing a `PS2_CMD_WRITE_{PRI_AUX}_OUT` command to the Control port,
///     then writing the device-specific command on the Data port.
///
///     REGISTERS
///
///     There are 3 8-bit registers involved in communicating with the CPU:
///     - input buffer: can be written by the CPU by writing to either port
///     - output buffer: can be read by the CPU by reading the data port
///     - controller status register ([CtrlStatus])
///
///     The input buffer is not directly represented, as our hardware is virtual
///     as well. See [CtrlOutPort] for the output buffer representation.
///
///     CONFIGURATION
///
///     The OS may read and write from the Controller Configuration Byte, which
///     lives at byte 0 in the PS/2 internal RAM. See [CtrlCfg].
///
/// INTERRUPTS
///
/// Interrupts are edge-triggered, so we pulse the interrupt to notify the guest
/// of outgoing data.
///
/// INTERACTION WITH VNC
///
/// Instead of reacting to an input buffer, the controller is notified of input
/// keyboard data via the VNC server. VNC uses keysyms to represent keys;
/// internally, we must convert these to scan codes from a keyboard.
///
/// The flow looks like this. An end user interacting with a guest over VNC
/// presses a key. The local VNC client sends its associated keysym and whether
/// the key is pressed in a VNC key event message. The propolis VNC server for
/// the guest receives the message, and will pass the key event to the
/// controller, which translates the keysym into a scan code representation,
/// then places the scan code in the output buffer. The controller also notifies
/// the guest via a keyboard interrupt.

#[usdt::provider(provider = "propolis")]
mod probes {
    // Controller Configuration updates from OS
    fn ps2ctrl_ctrlcfg_update(ctrl_cfg: u8) {}

    // reads/writes on control/data ports
    fn ps2ctrl_data_read(val: u8) {}
    fn ps2ctrl_data_read_empty() {}
    fn ps2ctrl_data_write(v: u8) {}
    fn ps2ctrl_cmd_write(v: u8) {}
    fn ps2ctrl_unknown_cmd(v: u8) {}

    // reads of Controller Status register
    fn ps2ctrl_status_read(status: u8) {}

    // interrupts: fire when the controller issues pri/aux interrupts
    fn ps2ctrl_pulse_pri() {}
    fn ps2ctrl_pulse_aux() {}

    // keyboard event probes
    fn ps2ctrl_keyevent(
        keysym_raw: u32,
        scan_code_set: u8,
        s0: u8,
        s1: u8,
        s2: u8,
        s3: u8,
    ) {
    }
    fn ps2ctrl_keyevent_dropped(
        keysym_raw: u32,
        is_pressed: u8,
        scan_code_set: u8,
    ) {
    }

    // internal device buffer writes
    fn ps2ctrl_keyboard_data(v: u8) {}
    fn ps2ctrl_mouse_data(v: u8) {}
    fn ps2ctrl_keyboard_overflow(v: u8) {}
    fn ps2ctrl_mouse_overflow(v: u8) {}

    // internal device buffer reads
    fn ps2ctrl_keyboard_data_read(v: u8) {}
    fn ps2ctrl_mouse_data_read(v: u8) {}

    // device commands
    fn ps2ctrl_keyboard_cmd(v: u8) {}
    fn ps2ctrl_mouse_cmd(v: u8) {}
    fn ps2ctrl_unknown_keyboard_cmd(v: u8) {}
    fn ps2ctrl_unknown_mouse_cmd(v: u8) {}
}

bitflags! {
    /// Controller Status Register
    ///
    /// An 8-bit register indicating the status of the controller, accessed by
    /// reading from the Control Port.
    #[derive(Default)]
    pub struct CtrlStatus: u8 {
        /// Output Buffer Status
        /// This bit must be set to 1 (indicating the buffer is full) before the
        /// OS attempts to read data from the data port.
        const OUT_FULL = 1 << 0;

        /// Input Buffer Status
        /// 0 if the input buffer is empty; 1 if the input buffer is full and
        /// shouldn't be written to by the OS.
        /// XXX(JPH): Not sure if this should be used somewhere.
        const IN_FULL = 1 << 1;

        /// System Flag
        /// This bit should be cleared to 0 by the controller on reset, and set
        /// to 1 if the system passes self tests.
        const SYS_FLAG = 1 << 2;

        /// Command/Data
        /// 1 if the last write to the input buffer (data port) was a command; 0
        ///   if the last write to the input buffer was data.
        const CMD_DATA = 1 << 3;

        /// Keyboard Locked
        const UNLOCKED = 1 << 4;

        /// Auxiliary Output Buffer contains data
        const AUX_FULL = 1 << 5;

        /// Timeout Error (0 = no error, 1 = timeout error)
        const TMO = 1 << 6;

        /// Parity Error with last byte (0 = no error, 1 = timeout error)
        const PARITY = 1 << 7;
    }
}

bitflags! {
    /// Controller Configuration Byte
    /// The OS can read and write this byte to configure the controller.
    #[derive(Default)]
    pub struct CtrlCfg: u8 {
        /// Primary Port Interrupt (1 = enabled, 0 = disabled)
        const PRI_INTR_EN = 1 << 0;

        /// Auxiliary Port Interrupt (1 = enabled, 0 = disabled)
        const AUX_INTR_EN = 1 << 1;

        /// System Flag (1 = system tests passed)
        const SYS_FLAG = 1 << 2;

        // bit 3: must be 0

        /// Primary Port Clock (1 = disabled, 0 = enabled)
        const PRI_CLOCK_DIS = 1 << 4;

        /// Auxiliary Port Clock (1 = disabled, 0 = enabled)
        const AUX_CLOCK_DIS = 1 << 5;

        /// Primary Port Translation (1 = enabled, 0 = disabled)
        /// If enabled, the controller should translate keyboard data to scan
        /// code set 1.
        const PRI_XLATE_EN = 1 << 6;

        // bit 7: must be 0
    }
}

bitflags! {
    /// Controller Output Port
    #[derive(Default, Copy, Clone)]
    pub struct CtrlOutPort: u8 {
        // bit 0: system reset
        // should always be set to 1
        // XXX(JPH): why does this work

        /// A20 Gate
        const A20 = 1 << 1;

        /// Auxiliary Port Clock
        const AUX_CLOCK = 1 << 2;

        /// Auxiliary Port Data
        const AUX_DATA = 1 << 3;

        /// Primary Port Output Buffer Full
        const PRI_FULL = 1 << 4;

        /// Auxiliary Port Output Buffer Full
        const AUX_FULL = 1 << 5;

        /// Primary Port Clock
        const PRI_CLOCK = 1 << 6;

        /// Primary Port Data
        const PRI_DATA = 1 << 7;

        // PRI_FULL and AUX_FULL are dynamic
        const DYN_FLAGS = (1 << 4) | (1 << 5);
    }
}

// Controller Commands

// Read/write Controller Configuration Byte (byte 0 of controller internal RAM)
const PS2C_CMD_READ_CTRL_CFG: u8 = 0x20;
const PS2C_CMD_WRITE_CTRL_CFG: u8 = 0x60;

// Read byte N from controller internal RAM, where N is in the range: 0x21-0x3f
const PS2C_CMD_READ_RAM_START: u8 = 0x21;
const PS2C_CMD_READ_RAM_END: u8 = 0x3f;

// Write byte N from controller internal RAM, where N is in the range: 0x21-0x3f
const PS2C_CMD_WRITE_RAM_START: u8 = 0x61;
const PS2C_CMD_WRITE_RAM_END: u8 = 0x7f;

// Disable/enable auxiliary port
const PS2C_CMD_AUX_PORT_DIS: u8 = 0xa7;
const PS2C_CMD_AUX_PORT_ENA: u8 = 0xa8;

// Test auxiliary port
const PS2C_CMD_AUX_PORT_TEST: u8 = 0xa9;

// Test controller (and response, if test passes)
const PS2C_CMD_CTRL_TEST: u8 = 0xaa;
const PS2C_R_CTRL_TEST_PASS: u8 = 0x55;

// Test primary port (and response, if test passes)
const PS2C_CMD_PRI_PORT_TEST: u8 = 0xab;
const PS2C_R_PORT_TEST_PASS: u8 = 0x00;

// Disable/enable primary port
const PS2C_CMD_PRI_PORT_DIS: u8 = 0xad;
const PS2C_CMD_PRI_PORT_ENA: u8 = 0xae;

// Read/write next byte to the Controller Output Port
const PS2C_CMD_READ_CTLR_OUT: u8 = 0xd0;
const PS2C_CMD_WRITE_CTLR_OUT: u8 = 0xd1;

// Write next byte to the primary port output
const PS2C_CMD_WRITE_PRI_OUT: u8 = 0xd2;

// Write next byte to the auxiliary port output
const PS2C_CMD_WRITE_AUX_OUT: u8 = 0xd3;

// Write next byte to the auxiliary port input
const PS2C_CMD_WRITE_AUX_IN: u8 = 0xd4;

// Pulse output line low
const PS2C_CMD_PULSE_START: u8 = 0xf0;
const PS2C_CMD_PULSE_END: u8 = 0xff;

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

    pri_pin: Option<Box<dyn IntrPin>>,
    aux_pin: Option<Box<dyn IntrPin>>,
    reset_pin: Option<Arc<dyn IntrPin>>,
}

pub struct PS2Ctrl {
    state: Mutex<PS2State>,
}

impl PS2Ctrl {
    pub fn create() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(PS2State::default()) })
    }
    pub fn attach(
        self: &Arc<Self>,
        bus: &PioBus,
        pri_pin: Box<dyn IntrPin>,
        aux_pin: Box<dyn IntrPin>,
        reset_pin: Arc<dyn IntrPin>,
    ) {
        let this = Arc::clone(self);
        let piofn = Arc::new(move |port: u16, rwo: RWOp| this.pio_rw(port, rwo))
            as Arc<PioFn>;

        bus.register(ibmpc::PORT_PS2_DATA, 1, Arc::clone(&piofn)).unwrap();
        bus.register(ibmpc::PORT_PS2_CMD_STATUS, 1, piofn).unwrap();

        let mut state = self.state.lock().unwrap();
        state.pri_pin = Some(pri_pin);
        state.aux_pin = Some(aux_pin);
        state.reset_pin = Some(reset_pin);
    }

    pub fn key_event(&self, ke: KeyEvent) {
        let mut state = self.state.lock().unwrap();
        let translate = state.ctrl_cfg.contains(CtrlCfg::PRI_XLATE_EN);
        let key_rep;

        match KeyEventRep::try_from(ke) {
            Ok(kr) => {
                key_rep = kr;
            }
            Err(_) => {
                // ignore any unrecognized keys
                probes::ps2ctrl_keyevent_dropped!(|| {
                    let set = if translate { 1 } else { 2 };
                    let is_pressed = if ke.is_pressed() { 1 } else { 0 };
                    (ke.keysym_raw(), is_pressed, set)
                });
                return;
            }
        };

        // If the translation bit is set, the guest expects Scan Code Set 1; otherwise, the general
        // default is set 2.
        let scan_code = if translate {
            key_rep.to_scan_code(PS2ScanCodeSet::Set1)
        } else {
            key_rep.to_scan_code(PS2ScanCodeSet::Set2)
        };

        // In the event that we need to debug why a specific key isn't behaving as expected, it
        // might be nice to have access to the underlying keysym value that was sent and what scan
        // code was produced.
        probes::ps2ctrl_keyevent!(|| {
            let set = if translate { 1 } else { 2 };
            let sc_len = scan_code.len();

            let (mut s0, mut s1, mut s2, mut s3) = (0, 0, 0, 0);

            if sc_len > 0 {
                s0 = scan_code[0];
            }

            if sc_len > 1 {
                s1 = scan_code[1];
            }

            if sc_len > 2 {
                s2 = scan_code[2];
            }

            if sc_len > 3 {
                s3 = scan_code[3];
            }

            (key_rep.keysym_raw, set, s0, s1, s2, s3)
        });

        state.pri_port.recv_scancode(scan_code);
        self.update_intr(&mut state);
    }

    fn pio_rw(&self, port: u16, rwo: RWOp) {
        assert_eq!(rwo.len(), 1);
        match port {
            ibmpc::PORT_PS2_DATA => match rwo {
                RWOp::Read(ro) => ro.write_u8(self.data_read()),
                RWOp::Write(wo) => self.data_write(wo.read_u8()),
            },

            ibmpc::PORT_PS2_CMD_STATUS => match rwo {
                RWOp::Read(ro) => ro.write_u8(self.status_read()),
                RWOp::Write(wo) => self.cmd_write(wo.read_u8()),
            },
            _ => {
                panic!("unexpected pio in {:x}", port);
            }
        }
    }

    fn data_write(&self, v: u8) {
        let mut state = self.state.lock().unwrap();
        let cmd_prefix = replace(&mut state.cmd_prefix, None);

        probes::ps2ctrl_data_write!(|| v);

        // If there's an outstanding command (written to the Control Port), then the remaining part
        // of the command will be on the data port.
        //
        // Otherwise, assume the value is a command for the keyboard.
        if let Some(prefix) = cmd_prefix {
            match prefix {
                PS2C_CMD_WRITE_CTRL_CFG => {
                    let cfg = CtrlCfg::from_bits_truncate(v);
                    probes::ps2ctrl_ctrlcfg_update!(|| cfg.bits());
                    state.ctrl_cfg = cfg;
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
            probes::ps2ctrl_data_read!(|| rval);
            rval
        } else if state.pri_port.has_output() {
            let rval = state.pri_port.read_output().unwrap();
            probes::ps2ctrl_keyboard_data_read!(|| rval);
            self.update_intr(&mut state);
            rval
        } else if state.aux_port.has_output() {
            let rval = state.aux_port.read_output().unwrap();
            probes::ps2ctrl_mouse_data_read!(|| rval);
            self.update_intr(&mut state);
            rval
        } else {
            probes::ps2ctrl_data_read_empty!(|| {});
            0
        }
    }
    fn cmd_write(&self, v: u8) {
        let mut state = self.state.lock().unwrap();
        probes::ps2ctrl_cmd_write!(|| v);
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
                    state.reset_pin.as_ref().unwrap().pulse();
                }
            }

            _ => {
                // ignore all other unrecognized commands
                probes::ps2ctrl_unknown_cmd!(|| v);
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

        probes::ps2ctrl_status_read!(|| val.bits());

        val.bits()
    }
    fn update_intr(&self, state: &mut PS2State) {
        // We currently choose to mimic qemu, which gates the keyboard interrupt
        // with the keyboard-clock-disable in addition to the interrupt enable.
        let pri_pin = state.pri_pin.as_ref().unwrap();
        if state.ctrl_cfg.contains(CtrlCfg::PRI_INTR_EN)
            && !state.ctrl_cfg.contains(CtrlCfg::PRI_CLOCK_DIS)
            && state.pri_port.has_output()
        {
            probes::ps2ctrl_pulse_pri!(|| {});
            pri_pin.pulse();
        }

        let aux_pin = state.aux_pin.as_ref().unwrap();
        if state.ctrl_cfg.contains(CtrlCfg::AUX_INTR_EN)
            && state.aux_port.has_output()
        {
            probes::ps2ctrl_pulse_aux!(|| {});
            aux_pin.pulse();
        }
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
impl Lifecycle for PS2Ctrl {
    fn type_name(&self) -> &'static str {
        "lpc-ps2ctrl"
    }
    fn reset(&self) {
        PS2Ctrl::reset(self);
    }
    fn migrate(&self) -> Migrator {
        Migrator::Single(self)
    }
}
impl MigrateSingle for PS2Ctrl {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError> {
        let state = self.state.lock().unwrap();
        let kbd = &state.pri_port;
        let mouse = &state.aux_port;

        Ok(migrate::PS2CtrlV1 {
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
        }
        .into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let migrate::PS2CtrlV1 {
            ctrl: saved_ctrl,
            kbd: saved_kbd,
            mouse: saved_mouse,
        } = offer.parse()?;

        let mut inner = self.state.lock().unwrap();

        inner.resp = saved_ctrl.response;
        inner.cmd_prefix = saved_ctrl.cmd_prefix;
        inner.ctrl_cfg =
            CtrlCfg::from_bits(saved_ctrl.ctrl_cfg).ok_or_else(|| {
                MigrateStateError::ImportFailed(format!(
                    "PS2 ctrl_cfg: failed to import value {:#x}",
                    saved_ctrl.ctrl_cfg
                ))
            })?;
        inner.ctrl_out_port = CtrlOutPort::from_bits(saved_ctrl.ctrl_out_port)
            .ok_or_else(|| {
                MigrateStateError::ImportFailed(format!(
                    "PS2 ctrl_out_port: failed to import value {:#x}",
                    saved_ctrl.ctrl_cfg
                ))
            })?;
        inner.ram = saved_ctrl.ram;

        let kbd = &mut inner.pri_port;
        kbd.cur_cmd = saved_kbd.current_cmd;
        kbd.enabled = saved_kbd.enabled;
        kbd.led_status = saved_kbd.led_status;
        kbd.typematic = saved_kbd.typematic;
        kbd.scan_code_set = PS2ScanCodeSet::from_byte(saved_kbd.scan_code_set)
            .ok_or_else(|| {
                MigrateStateError::ImportFailed(format!(
                    "PS2 kbd scan code: failed to import value {}",
                    saved_kbd.scan_code_set
                ))
            })?;
        kbd.buf = VecDeque::from(saved_kbd.buf);

        let mouse = &mut inner.aux_port;
        mouse.cur_cmd = saved_mouse.current_cmd;
        mouse.status =
            PS2MStatus::from_bits(saved_mouse.status).ok_or_else(|| {
                MigrateStateError::ImportFailed(format!(
                    "PS2 mouse status: failed to import value {:#x}",
                    saved_mouse.status
                ))
            })?;
        mouse.resolution = saved_mouse.resolution;
        mouse.sample_rate = saved_mouse.sample_rate;
        mouse.buf = VecDeque::from(saved_mouse.buf);

        Ok(())
    }
}

// Keyboard-specific commands

// Set LEDs: bit 0 is ScrollLock, bit 1 is NumberLock, bit 2 is CapsLock
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

pub(crate) enum PS2ScanCodeSet {
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

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(PS2ScanCodeSet::Set1),
            2 => Some(PS2ScanCodeSet::Set2),
            _ => None,
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
        probes::ps2ctrl_keyboard_cmd!(|| v);
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
                    probes::ps2ctrl_unknown_keyboard_cmd!(|| v);
                }
            }
        }
    }
    fn resp(&mut self, v: u8) {
        let remain = PS2_KBD_BUFSZ - self.buf.len();
        match remain {
            0 => {
                // overrun already in progress, do nothing
                probes::ps2ctrl_keyboard_overflow!(|| v);
            }
            1 => {
                // indicate overflow instead
                probes::ps2ctrl_keyboard_overflow!(|| v);
                self.buf.push_back(0xff)
            }
            _ => {
                probes::ps2ctrl_keyboard_data!(|| v);
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

    fn recv_scancode(&mut self, scan_code: Vec<u8>) {
        for s in scan_code.into_iter() {
            self.resp(s);
        }
    }
}
impl Default for PS2Kbd {
    fn default() -> Self {
        Self::new()
    }
}

// Mouse-specific commands
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
        probes::ps2ctrl_mouse_cmd!(|| v);
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
                    probes::ps2ctrl_unknown_mouse_cmd!(|| v);
                }
            }
        }
    }
    fn resp(&mut self, v: u8) {
        let remain = PS2_KBD_BUFSZ - self.buf.len();
        match remain {
            0 => {
                probes::ps2ctrl_mouse_overflow!(|| v);
                // overrun already in progress, do nothing
            }
            1 => {
                // indicate overflow instead
                probes::ps2ctrl_mouse_overflow!(|| v);
                self.buf.push_back(0xff)
            }
            _ => {
                probes::ps2ctrl_mouse_data!(|| v);
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
    use crate::migrate::*;

    use super::PS2C_RAM_LEN;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct PS2CtrlV1 {
        pub ctrl: PS2CtrlStateV1,
        pub kbd: PS2KbdV1,
        pub mouse: PS2MouseV1,
    }
    impl Schema<'_> for PS2CtrlV1 {
        fn id() -> SchemaId {
            ("ps2-ctrl", 1)
        }
    }

    #[derive(Deserialize, Serialize)]
    pub struct PS2CtrlStateV1 {
        pub response: Option<u8>,
        pub cmd_prefix: Option<u8>,
        pub ctrl_cfg: u8,
        pub ctrl_out_port: u8,
        pub ram: [u8; PS2C_RAM_LEN],
    }
    #[derive(Deserialize, Serialize)]
    pub struct PS2KbdV1 {
        pub buf: Vec<u8>,
        pub current_cmd: Option<u8>,
        pub enabled: bool,
        pub led_status: u8,
        pub typematic: u8,
        pub scan_code_set: u8,
    }
    #[derive(Deserialize, Serialize)]
    pub struct PS2MouseV1 {
        pub buf: Vec<u8>,
        pub current_cmd: Option<u8>,
        pub status: u8,
        pub resolution: u8,
        pub sample_rate: u8,
    }
}
