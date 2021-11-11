use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::qemu::fwcfg::{self, FwCfgBuilder, Item};
use crate::migrate::Migrate;
use crate::util::regmap::RegMap;

use erased_serde::Serialize;
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
        // edk2 default - xRGB, 4 bytes per pixels
        0x34325258 => Some(4),
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
    fn verify(&self, ctx: &DispCtx) -> Option<()> {
        if self.height == 0 || self.width == 0 {
            return None;
        }

        let bypp = fourcc_bytepp(self.fourcc)?;

        let mem = ctx.mctx.memctx();
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
}
#[derive(Default)]
pub struct RamFb {
    config: Mutex<Config>,
}
impl RamFb {
    pub fn create() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn attach(self: &Arc<Self>, builder: &mut FwCfgBuilder) {
        builder
            .add_named("etc/ramfb", Arc::clone(self) as Arc<dyn Item>)
            .unwrap();
    }
}
impl Item for RamFb {
    fn size(&self) -> u32 {
        CFG_REGS_LEN as u32
    }
    fn fwcfg_rw(&self, mut rwo: RWOp, ctx: &DispCtx) -> fwcfg::Result {
        let mut config = self.config.lock().unwrap();
        let valid_before =
            if rwo.is_write() { config.verify(ctx).is_some() } else { false };

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
            let valid_after = config.verify(ctx).is_some();

            if valid_after {
                slog::info!(ctx.log, "ramfb change"; "state" => "valid", "config" => ?config);
            } else if valid_before {
                slog::info!(ctx.log, "ramfb change"; "state" => "invalid");
            }
            match (valid_before, valid_after) {
                (true, _) | (false, true) => {
                    //TODO: notify about update
                }
                _ => {}
            }
        }
        Ok(())
    }
}
impl Entity for RamFb {
    fn type_name(&self) -> &'static str {
        "qemu-ramfb"
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for RamFb {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        let state = self.config.lock().unwrap();
        Box::new(migrate::RamFbV1 {
            addr: state.addr,
            fourcc: state.fourcc,
            flags: state.flags,
            width: state.width,
            height: state.height,
            stride: state.stride,
        })
    }
}

pub mod migrate {
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct RamFbV1 {
        pub addr: u64,
        pub fourcc: u32,
        pub flags: u32,
        pub width: u32,
        pub height: u32,
        pub stride: u32,
    }
}
