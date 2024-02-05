// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use crate::accessors::MemAccessor;
use crate::common::*;
use crate::hw::qemu::fwcfg::{self, FwCfgBuilder, Item};
use crate::migrate::*;
use crate::util::regmap::RegMap;
use crate::vmm::MemCtx;

use lazy_static::lazy_static;

#[derive(Copy, Clone, Eq, PartialEq)]
enum Reg {
    Addr,
    FourCC,
    Flags,
    Width,
    Height,
    Stride,
}
const CFG_REGS_LEN: usize = 28;

lazy_static! {
    static ref CFG_REGS: RegMap<Reg> = {
        let layout = [
            (Reg::Addr, 8),
            (Reg::FourCC, 4),
            (Reg::Flags, 4),
            (Reg::Width, 4),
            (Reg::Height, 4),
            (Reg::Stride, 4),
        ];
        RegMap::create_packed(CFG_REGS_LEN, &layout, None)
    };
}

fn fourcc_bytepp(fourcc: u32) -> Option<u32> {
    match fourcc {
        // The edk2 default: XR24, little-endian xRGB with 4 bytes per pixel.
        rfb::pixel_formats::fourcc::FOURCC_XR24 => Some(4),
        _ => None,
    }
}

#[derive(Default, Debug)]
pub struct Config {
    addr: u64,
    fourcc: u32,
    flags: u32,
    width: u32,
    height: u32,
    stride: u32,
}
impl Config {
    fn verify(&self, mem: &MemCtx) -> Option<()> {
        if self.height == 0 || self.width == 0 {
            return None;
        }

        let bypp = fourcc_bytepp(self.fourcc)?;

        let line_sz = if self.stride == 0 { self.width } else { self.stride };
        let total_sz = u32::checked_mul(self.height - 1, line_sz)?
            .checked_add(self.width)?
            .checked_mul(bypp)?;
        let _ = mem.readable_region(&GuestRegion(
            GuestAddr(self.addr),
            total_sz as usize,
        ))?;

        Some(())
    }

    pub fn get_framebuffer_spec(&self) -> FramebufferSpec {
        FramebufferSpec {
            addr: self.addr,
            width: self.width,
            height: self.height,
            fourcc: self.fourcc,
        }
    }
}

#[derive(Clone, Copy)]
pub struct FramebufferSpec {
    pub addr: u64,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
}

type NotifyFn = Box<dyn Fn(&Config, bool) + Send + Sync + 'static>;

pub struct RamFb {
    config: Mutex<Config>,
    notify: Mutex<Option<NotifyFn>>,
    acc_mem: MemAccessor,
    log: slog::Logger,
}
impl RamFb {
    pub fn create(log: slog::Logger) -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(Config::default()),
            notify: Mutex::new(None),
            acc_mem: MemAccessor::new_orphan(),
            log,
        })
    }
    pub fn attach(
        self: &Arc<Self>,
        builder: &mut FwCfgBuilder,
        acc_mem: &MemAccessor,
    ) {
        acc_mem.adopt(&self.acc_mem, Some("ramfb".to_string()));
        builder
            .add_named("etc/ramfb", Arc::clone(self) as Arc<dyn Item>)
            .unwrap();
    }
    pub fn get_framebuffer_spec(&self) -> FramebufferSpec {
        self.config.lock().unwrap().get_framebuffer_spec()
    }
    pub fn set_notifier(&self, n: NotifyFn) {
        let mut locked = self.notify.lock().unwrap();
        *locked = Some(n);
    }
}
impl Item for RamFb {
    fn size(&self) -> u32 {
        CFG_REGS_LEN as u32
    }
    fn fwcfg_rw(&self, mut rwo: RWOp) -> fwcfg::Result {
        let mem = self.acc_mem.access().expect("usable mem accessor");
        let mut config = self.config.lock().unwrap();
        let valid_before =
            if rwo.is_write() { config.verify(&mem).is_some() } else { false };

        CFG_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => match id {
                Reg::Addr => ro.write_u64(config.addr.to_be()),
                Reg::FourCC => ro.write_u32(config.fourcc.to_be()),
                Reg::Flags => ro.write_u32(config.flags.to_be()),
                Reg::Width => ro.write_u32(config.width.to_be()),
                Reg::Height => ro.write_u32(config.height.to_be()),
                Reg::Stride => ro.write_u32(config.stride.to_be()),
            },
            RWOp::Write(wo) => match id {
                Reg::Addr => config.addr = u64::from_be(wo.read_u64()),
                Reg::FourCC => config.fourcc = u32::from_be(wo.read_u32()),
                Reg::Flags => config.flags = u32::from_be(wo.read_u32()),
                Reg::Width => config.width = u32::from_be(wo.read_u32()),
                Reg::Height => config.height = u32::from_be(wo.read_u32()),
                Reg::Stride => config.stride = u32::from_be(wo.read_u32()),
            },
        });
        if rwo.is_write() {
            let valid_after = config.verify(&mem).is_some();

            if valid_after {
                slog::info!(self.log, "ramfb change"; "state" => "valid", "config" => ?config);
            } else if valid_before {
                slog::info!(self.log, "ramfb change"; "state" => "invalid");
            }
            match (valid_before, valid_after) {
                (true, _) | (false, true) => {
                    let notify = self.notify.lock().unwrap();
                    if let Some(func) = notify.as_ref() {
                        slog::info!(self.log, "notifying");
                        func(&config, valid_after);
                    } else {
                        slog::info!(self.log, "no notify fn set");
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
impl Lifecycle for RamFb {
    fn type_name(&self) -> &'static str {
        "qemu-ramfb"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Single(self)
    }
}
impl MigrateSingle for RamFb {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError> {
        let state = self.config.lock().unwrap();
        Ok(migrate::RamFbV1 {
            addr: state.addr,
            fourcc: state.fourcc,
            flags: state.flags,
            width: state.width,
            height: state.height,
            stride: state.stride,
        }
        .into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let data: migrate::RamFbV1 = offer.parse()?;

        let mut state = self.config.lock().unwrap();
        state.addr = data.addr;
        state.fourcc = data.fourcc;
        state.flags = data.flags;
        state.width = data.width;
        state.height = data.height;
        state.stride = data.stride;

        Ok(())
    }
}

pub mod migrate {
    use crate::migrate::*;

    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct RamFbV1 {
        pub addr: u64,
        pub fourcc: u32,
        pub flags: u32,
        pub width: u32,
        pub height: u32,
        pub stride: u32,
    }
    impl Schema<'_> for RamFbV1 {
        fn id() -> SchemaId {
            ("qemu-ramfb", 1)
        }
    }
}
