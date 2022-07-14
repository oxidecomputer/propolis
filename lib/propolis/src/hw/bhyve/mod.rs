mod atpic;
mod atpit;
mod hpet;
mod ioapic;
mod pmtimer;
mod rtc;

pub use atpic::*;
pub use atpit::*;
pub use hpet::*;
pub use ioapic::*;
pub use pmtimer::*;
pub use rtc::*;

use std::sync::Arc;

pub type KernelDefaults = (
    Arc<BhyveAtPic>,
    Arc<BhyveAtPit>,
    Arc<BhyveHpet>,
    Arc<BhyveIoApic>,
    Arc<BhyveRtc>,
);

/// Create cohort of devices emulated by bhyve kernel component
pub fn defaults() -> KernelDefaults {
    (
        BhyveAtPic::create(),
        BhyveAtPit::create(),
        BhyveHpet::create(),
        BhyveIoApic::create(),
        BhyveRtc::create(),
    )
}
