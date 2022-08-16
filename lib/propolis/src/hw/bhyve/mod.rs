mod atpic;
mod atpit;
mod hpet;
mod ioapic;
mod pmtimer;
mod rtc;

pub use atpic::BhyveAtPic;
pub use atpit::BhyveAtPit;
pub use hpet::BhyveHpet;
pub use ioapic::BhyveIoApic;
pub use pmtimer::BhyvePmTimer;
pub use rtc::BhyveRtc;

use std::sync::Arc;

pub type KernelDefaults = (
    Arc<BhyveAtPic>,
    Arc<BhyveAtPit>,
    Arc<BhyveHpet>,
    Arc<BhyveIoApic>,
    Arc<BhyveRtc>,
);
