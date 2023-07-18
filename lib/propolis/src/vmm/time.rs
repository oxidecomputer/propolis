// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

use super::VmmHdl;

pub const NS_PER_SEC: u64 = 1_000_000_000;
pub const SEC_PER_DAY: u64 = 24 * 60 * 60;

/// Representation of guest time data state
///
/// This is serialized/deserialized as part of the migration protocol
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct VmTimeData {
    /// guest TSC frequency (hz)
    pub guest_freq: u64,

    /// current guest TSC
    pub guest_tsc: u64,

    /// monotonic host clock (ns)
    pub hrtime: i64,

    /// wall clock host clock (sec)
    pub hres_sec: u64,

    /// wall clock host clock (ns)
    pub hres_ns: u64,

    /// guest boot_hrtime (can be negative)
    pub boot_hrtime: i64,
}

/// A collection of data about adjustments made to the time data to enable
/// richer log messages
#[derive(Debug)]
pub struct VmTimeDataAdjustments {
    pub guest_uptime_ns: u64,
    pub migrate_delta: Duration,
    pub migrate_delta_negative: bool,
    pub guest_tsc_delta: u64,
    pub boot_hrtime_delta: u64,
}

impl VmTimeData {
    pub fn wall_clock(&self) -> Duration {
        Duration::new(self.hres_sec, self.hres_ns as u32)
    }
}

impl From<bhyve_api::vdi_time_info_v1> for VmTimeData {
    fn from(raw: bhyve_api::vdi_time_info_v1) -> Self {
        Self {
            guest_freq: raw.vt_guest_freq,
            guest_tsc: raw.vt_guest_tsc,
            hrtime: raw.vt_hrtime,
            hres_sec: raw.vt_hres_sec,
            hres_ns: raw.vt_hres_ns,
            boot_hrtime: raw.vt_boot_hrtime,
        }
    }
}
impl From<VmTimeData> for bhyve_api::vdi_time_info_v1 {
    fn from(info: VmTimeData) -> Self {
        bhyve_api::vdi_time_info_v1 {
            vt_guest_freq: info.guest_freq,
            vt_guest_tsc: info.guest_tsc,
            vt_hrtime: info.hrtime,
            vt_hres_sec: info.hres_sec,
            vt_hres_ns: info.hres_ns,
            vt_boot_hrtime: info.boot_hrtime,
        }
    }
}

pub fn import_time_data(
    hdl: &VmmHdl,
    time_info: VmTimeData,
) -> std::io::Result<()> {
    let raw = bhyve_api::vdi_time_info_v1::from(time_info);
    hdl.data_op(bhyve_api::VDC_VMM_TIME, 1).write(&raw)?;

    Ok(())
}

pub fn export_time_data(hdl: &VmmHdl) -> std::io::Result<VmTimeData> {
    let raw = hdl
        .data_op(bhyve_api::VDC_VMM_TIME, 1)
        .read::<bhyve_api::vdi_time_info_v1>()?;

    Ok(VmTimeData::from(raw))
}

/// Returns the current host hrtime and wall clock time
//
// The current host hrtime and wall clock time are exposed via the VMM time data
// interface.
//
// The kernel side of the interface disables interrupts while it takes the clock
// readings; in the absence of a function to translate between the two clock
// values, this is a best effort way to read the hrtime and wall clock times as
// close to as possible at the same point in time. Thus fishing this data out of
// the VMM time data read payload is strictly better than calling
// clock_gettime(3c) twice from userspace.
pub fn host_time_snapshot(hdl: &VmmHdl) -> std::io::Result<(i64, Duration)> {
    let ti = export_time_data(hdl)?;
    let wc = Duration::new(ti.hres_sec, ti.hres_ns as u32);
    let hrt = ti.hrtime;

    Ok((hrt, wc))
}

/// Given an input representation of guest time data on a source host, and a
/// current host hrtime and wallclock time on the target host, output an
/// "adjusted" view of the guest time data. This data can be imported to bhyve
/// to allow guest time (namely, the guest TSC and its device timers) to allow
/// the guest's sense of time to function properly on the target.
//  See comments inline for more details about how we calculate a new guest TSC
//  and boot_hrtime.
pub fn adjust_time_data(
    src: VmTimeData,
    dst_hrt: i64,
    dst_wc: Duration,
) -> Result<(VmTimeData, VmTimeDataAdjustments), TimeAdjustError> {
    // Basic validation: there is no reason system hrtime should be negative,
    // and other calculations assume that, so validate that first
    if dst_hrt < 0 {
        return Err(TimeAdjustError::Hrtime { hrtime: dst_hrt });
    }

    // Find delta between export on source and import on target using wall clock
    // time. This delta is used for adjusting the TSC and boot_hrtime.
    //
    // We expect to be operating on machines with well-synchronized wall
    // clocks, so using wall clock time is a useful shorthand for observing how
    // much time has passed. If for some reason we see a negative delta (see
    // also: #357), clamp the delta to 0.
    //
    // migrate_delta = target wall clock - source wall clock
    let (migrate_delta, migrate_delta_negative) =
        match dst_wc.checked_sub(src.wall_clock()) {
            Some(d) => (d, false),
            None => (Duration::from_secs(0), true),
        };

    if migrate_delta.as_nanos() > (i64::MAX as u128) {
        // migrate delta won't fit in hrtime calculations
        return Err(TimeAdjustError::InvalidMigrateDelta {
            src_wc: src.wall_clock(),
            dst_wc,
        });
    }
    let migrate_delta_ns = migrate_delta.as_nanos() as i64;
    assert!(migrate_delta_ns >= 0, "migrate delta cannot be negative");

    // Find a new boot_hrtime for the guest
    //
    // Device timers are scheduled based on hrtime of the host: For example, a
    // timer that should fire after 1 second is scheduled as: host hrtime + 1s.
    //
    // When devices are exported for migration, time values are normalized
    // against the guest's boot_hrtime on export, and de-normalized against
    // boot_hrtime on import. The boot_hrtime of the guest is set to the hrtime
    // of when the guest booted. Because this value is used on import to fix up
    // timer values, it is critical to set this value prior to importing device
    // state such that existing timers are normalized correctly. As with booting
    // a guest, on the target, it should be set to the hrtime of the host when
    // the guest would have booted, had it booted on that target.
    //
    // An example may be helpful. Consider a guest that has 5 days of uptime,
    // booted on a host with 30 days of uptime. Suppose that guest is migrated
    // with a device timer that should fire 1 second in the future.
    //
    // +=================================================================+
    // | hrtime (source) | guest hrtime | boot_hrtime  | timer value     |
    // +-----------------------------------------------------------------+
    // | 30 days         | 5 days       |(30 - 5) days | src hrtime + 1s |
    // |                 |              | 25 days      | 30 days + 1s    |
    // +=================================================================+
    //
    // Suppose the guest is then migrated to a host with 100 days of uptime.
    // On migration, the existing timer is normalized before export by
    // subtracting out boot_hrtime:
    //       normalized = timer - boot_hrtime
    //                  = (30 days + 1 sec) - 25 days
    //                  = 5 days + 1 sec
    //
    // When the timer is imported, it is denormalized by adding back in
    // the new boot_hrtime. The timer should still fire 1 second from the
    // current hrtime of the host. The target hrtime is 100 days, so the timer
    // should fire at 100 days + 1 sec.
    //
    // Working backwards to get the new boot_hrtime, we have:
    //
    //       denormalized = normalized + boot_hrtime
    //       boot_hrtime  = denormalized - normalized
    //       boot_hrtime  = (100 days + 1 sec) - (5 days + 1 sec)
    //       boot_hrtime  = 95 days
    //
    // And on the target, the timer should still fire 1 second into the future
    // as expected:
    //
    // +=====================================================================+
    // | hrtime (target) | guest hrtime | boot_hrtime   | timer value        |
    // +---------------------------------------------------------------------+
    // | 100 days        | 5 days       |(100 - 5) days |     5 days + 1 sec |
    // |                 |              | 95 days       |   + 95 days        |
    // |                 |              |               | = 100 days + 1 sec |
    // +=====================================================================+
    //
    // NB: It is possible for boot_hrtime to be negative; this occurs if a
    // guest has a longer uptime than its host (an expected common case for
    // migration). This is okay: hrtime is a signed value, and the normalization
    // maths still work with negative values.
    //

    // guest_uptime_ns  = source hrtime - source boot_hrtime
    let guest_uptime_ns: i64 = src
        .hrtime
        .checked_sub(src.boot_hrtime)
        .ok_or_else(|| TimeAdjustError::GuestUptimeOverflow {
            desc: "src_hrt - boot_hrtime",
            src_hrt: src.hrtime,
            boot_hrtime: src.boot_hrtime,
        })?;
    if guest_uptime_ns < 0 {
        // This can only happen if somehow boot_hrtime was in the future on the
        // source, which is an invalid state
        return Err(TimeAdjustError::GuestUptimeOverflow {
            desc: "src_hrt < boot_hrtime",
            src_hrt: src.hrtime,
            boot_hrtime: src.boot_hrtime,
        });
    }

    // boot_hrtime_delta = guest_uptime_ns + migrate_delta_ns
    let boot_hrtime_delta: i64 = guest_uptime_ns
        .checked_add(migrate_delta_ns)
        .ok_or_else(|| TimeAdjustError::TimeDeltaOverflow {
            uptime_ns: guest_uptime_ns,
            migrate_delta,
        })?;

    // boot_hrtime = target hrtime - boot_hrtime_delta
    let new_boot_hrtime: i64 = dst_hrt
        .checked_sub(boot_hrtime_delta)
        .ok_or_else(|| TimeAdjustError::BootHrtimeOverflow {
            total_delta: boot_hrtime_delta as u64,
            dst_hrtime: dst_hrt,
        })?;

    // Get the guest TSC adjustment and add it to the old guest TSC
    //
    // We move the guest TSC forward based on the migrate delta, such that the
    // guest TSC reflects the time passed in migration.
    //
    // NB: It is okay to overflow the TSC here: It is possible for the guest to
    // write to the TSC, and if it did so it might expect it to overflow.
    let guest_tsc_delta = calc_tsc_delta(migrate_delta, src.guest_freq)?;
    let new_guest_tsc = src.guest_tsc.wrapping_add(guest_tsc_delta);

    Ok((
        VmTimeData {
            guest_freq: src.guest_freq,
            guest_tsc: new_guest_tsc,
            hrtime: dst_hrt,
            hres_sec: dst_wc.as_secs(),
            hres_ns: dst_wc.subsec_nanos() as u64,
            boot_hrtime: new_boot_hrtime,
        },
        VmTimeDataAdjustments {
            guest_uptime_ns: guest_uptime_ns as u64,
            migrate_delta,
            migrate_delta_negative,
            guest_tsc_delta,
            boot_hrtime_delta: boot_hrtime_delta as u64,
        },
    ))
}

/// Errors related to making timing adjustment calcultions
#[derive(Clone, Debug, Error)]
pub enum TimeAdjustError {
    /// Negative system hrtime
    #[error("invalid system hrtime: src={hrtime}")]
    Hrtime {
        /// target host hrtime
        hrtime: i64,
    },

    /// Error calculating migration time delta
    #[error("invalid migration delta: src={src_wc:?},dst={dst_wc:?}")]
    InvalidMigrateDelta {
        /// source host wall clock time
        src_wc: Duration,

        /// destination host wall clock time
        dst_wc: Duration,
    },

    /// Error calculating guest uptime
    #[error(
        "guest uptime cannot be represented: \
        desc={desc}, src_hrtime={src_hrt:?}, boot_hrtime={boot_hrtime}"
    )]
    GuestUptimeOverflow {
        /// error description
        desc: &'static str,

        /// source host hrtime
        src_hrt: i64,

        /// input guest boot_hrtime
        boot_hrtime: i64,
    },

    /// Invalid total delta for boot_hrtime calculations
    #[error(
        "could not calculate time delta: \
            guest uptime {uptime_ns} ns, migrate_delta={migrate_delta:?}"
    )]
    TimeDeltaOverflow {
        /// guest uptime
        uptime_ns: i64,

        /// migration time delta
        migrate_delta: Duration,
    },

    /// Invalid calculated boot_hrtime
    #[error(
        "guest boot_hrtime cannot be represented: \
            total_delta={total_delta:?}, dst_hrtime={dst_hrtime:?}"
    )]
    BootHrtimeOverflow {
        /// calculated total delta (uptime + migration delta)
        total_delta: u64,

        /// destination host hrtime
        dst_hrtime: i64,
    },

    /// Invalid guest TSC adjustment
    #[error(
        "could not calculate TSC adjustment: \
            desc=\"{desc:?}\", migrate_delta={migrate_delta:?},
            guest_hz={guest_hz}, tsc_adjust={tsc_adjust}"
    )]
    TscAdjustOverflow {
        /// error description
        desc: &'static str,

        /// migration time delta
        migrate_delta: Duration,

        /// guest TSC frequency (hz)
        guest_hz: u64,

        /// calculated TSC adjustment
        tsc_adjust: u128,
    },
}

/// Calculate the adjustment needed for the guest TSC.
///
/// ticks = (migrate_delta ns * guest_hz hz) / `NS_PER_SEC`
fn calc_tsc_delta(
    migrate_delta: Duration,
    guest_hz: u64,
) -> Result<u64, TimeAdjustError> {
    assert_ne!(guest_hz, 0);

    let delta_ns: u128 = migrate_delta.as_nanos();
    let mut tsc_adjust: u128 = 0;

    let upper: u128 =
        delta_ns.checked_mul(guest_hz as u128).ok_or_else(|| {
            TimeAdjustError::TscAdjustOverflow {
                desc: "migrate_delta * guest_hz",
                migrate_delta,
                guest_hz,
                tsc_adjust,
            }
        })?;

    tsc_adjust = upper.checked_div(NS_PER_SEC as u128).ok_or_else(|| {
        TimeAdjustError::TscAdjustOverflow {
            desc: "upper / NS_PER_SEC",
            migrate_delta,
            guest_hz,
            tsc_adjust,
        }
    })?;
    if tsc_adjust > u64::MAX as u128 {
        return Err(TimeAdjustError::TscAdjustOverflow {
            desc: "tsc_adjust > 64-bits",
            migrate_delta,
            guest_hz,
            tsc_adjust,
        });
    }

    Ok(tsc_adjust as u64)
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use crate::vmm::time::SEC_PER_DAY;

    use super::{
        adjust_time_data, calc_tsc_delta, TimeAdjustError, VmTimeData,
        NS_PER_SEC,
    };

    fn base_time_data() -> VmTimeData {
        VmTimeData {
            // non-zero freq, so as not to blow any asserts
            guest_freq: 1,
            guest_tsc: 0,
            hrtime: 0,
            hres_sec: 0,
            hres_ns: 0,
            boot_hrtime: 0,
        }
    }

    #[test]
    fn test_invalid_hrtime() {
        // system hrtime should not be negative
        let res =
            adjust_time_data(base_time_data(), -1, Duration::from_nanos(0));
        assert!(res.is_err());
        assert!(matches!(res, Err(TimeAdjustError::Hrtime { .. })));
    }

    // migrate_delta = target wall clock - source wall clock
    //               = dst_wc - src.wall_clock()
    #[test]
    fn test_calc_migrate_delta() {
        // valid input

        // 1 sec - 0 sec
        let dst_wc = Duration::from_secs(1);
        let res = adjust_time_data(base_time_data(), 0, dst_wc);
        assert!(res.is_ok());
        let adj = res.unwrap().1;
        assert_eq!(adj.migrate_delta, Duration::from_secs(1));
        assert!(!adj.migrate_delta_negative);

        // 1 ns - 0 sec
        let dst_wc = Duration::from_nanos(1);
        let res = adjust_time_data(base_time_data(), 0, dst_wc);
        assert!(res.is_ok());
        let adj = res.unwrap().1;
        assert_eq!(adj.migrate_delta, Duration::from_nanos(1));
        assert!(!adj.migrate_delta_negative);

        // 0 sec - 0 sec
        let dst_wc = Duration::from_nanos(0);
        let res = adjust_time_data(base_time_data(), 0, dst_wc);
        assert!(res.is_ok());
        let adj = res.unwrap().1;
        assert_eq!(adj.migrate_delta, Duration::from_nanos(0));
        assert!(!adj.migrate_delta_negative);

        // negative migrate delta should be clamped to 0
        // 0 sec - 1 sec
        let src_td = VmTimeData { hres_sec: 1, ..base_time_data() };
        let dst_wc = Duration::from_nanos(0);
        let res = adjust_time_data(src_td, 0, dst_wc);
        assert!(res.is_ok());
        let adj = res.unwrap().1;
        assert_eq!(adj.migrate_delta, Duration::from_nanos(0));
        assert!(adj.migrate_delta_negative);

        // 0 sec - 1 ns
        let src_td = VmTimeData { hres_ns: 1, ..base_time_data() };
        let dst_wc = Duration::from_nanos(0);
        let res = adjust_time_data(src_td, 0, dst_wc);
        assert!(res.is_ok());
        let adj = res.unwrap().1;
        assert_eq!(adj.migrate_delta, Duration::from_nanos(0));
        assert!(adj.migrate_delta_negative);

        // error case: migrate delta overflows i64
        // (i64::MAX + 1) sec - 0 sec
        let dst_wc = Duration::from_nanos((i64::MAX as u64) + 1);
        let res = adjust_time_data(base_time_data(), 0, dst_wc);
        assert!(res.is_err());
        assert!(matches!(
            res,
            Err(TimeAdjustError::InvalidMigrateDelta { .. })
        ));
    }

    struct Gutv {
        hrt: i64,
        bhrt: i64,
        res: u64,
    }
    const GUEST_UPTIME_TESTS_VALID: &'static [Gutv] = &[
        // edge case: boot_hrtime == 0
        Gutv { hrt: 1, bhrt: 0, res: 1 },
        // boot_hrtime > 0
        // guest was booted on this host, or was migrated to a host with higher
        // uptime than itself
        Gutv {
            hrt: 300_000_000_000,
            bhrt: 200_000_000_000,
            res: 100_000_000_000,
        },
        Gutv { hrt: i64::MAX, bhrt: i64::MAX - 1, res: 1 },
        // edge case: src_hrt == boot_hrtime
        Gutv { hrt: 0, bhrt: 0, res: 0 },
        Gutv { hrt: 300_000_000_000, bhrt: 300_000_000_000, res: 0 },
        Gutv { hrt: i64::MAX, bhrt: i64::MAX, res: 0 },
        // boot_hrtime < 0
        // guest came from a host with less uptime than itself
        Gutv { hrt: 1000, bhrt: -100, res: 1100 },
    ];
    struct Guti {
        hrt: i64,
        bhrt: i64,
    }
    const GUEST_UPTIME_TESTS_INVALID: &'static [Guti] = &[
        // src_hrt - boot_hrtime underflows i64
        // (0 - i64::MAX)
        Guti { hrt: 0, bhrt: i64::MAX },
        // (src_hrt - boot_hrtime) overflows i64
        // (i64::MAX - -1)
        Guti { hrt: i64::MAX, bhrt: -1 },
        // src_hrt < boot_hrtime
        // (0 < 1)
        Guti { hrt: 0, bhrt: 1 },
    ];

    // guest_uptime_ns  = source hrtime - boot_hrtime
    #[test]
    fn test_calc_guest_uptime() {
        // valid cases
        for i in 0..GUEST_UPTIME_TESTS_VALID.len() {
            let t = &GUEST_UPTIME_TESTS_VALID[i];

            let msg = format!(
                "src_hrtime={}, boot_hrtime={}, expected={:?}",
                t.hrt, t.bhrt, t.res
            );

            let src_td = VmTimeData {
                hrtime: t.hrt,
                boot_hrtime: t.bhrt,
                ..base_time_data()
            };
            let res = adjust_time_data(src_td, 0, Duration::from_nanos(0));
            match res {
                Ok(v) => {
                    assert_eq!(
                        v.1.guest_uptime_ns, t.res,
                        "got incorrect value: {}",
                        msg
                    );
                }
                Err(e) => {
                    assert!(false, "got error {}: {}", e, msg);
                }
            }
        }

        // error cases
        for i in 0..GUEST_UPTIME_TESTS_INVALID.len() {
            let t = &GUEST_UPTIME_TESTS_INVALID[i];
            let msg = format!("src_hrtime={}, boot_hrtime={}", t.hrt, t.bhrt,);

            let src_td = VmTimeData {
                hrtime: t.hrt,
                boot_hrtime: t.bhrt,
                ..base_time_data()
            };
            let res = adjust_time_data(src_td, 0, Duration::from_nanos(0));
            match res {
                Ok(v) => {
                    assert!(
                        false,
                        "expected error but got value {:?}: {}",
                        v, msg
                    );
                }
                Err(TimeAdjustError::GuestUptimeOverflow { .. }) => {
                    // test passes
                }
                Err(e) => {
                    assert!(false, "got incorrect error type {:?}: {}", e, msg);
                }
            }
        }
    }

    // boot_hrtime_delta = guest_uptime_ns + migrate_delta
    #[test]
    fn test_calc_boot_hrtime_delta() {
        // valid input
        //
        // 30 secs guest uptime + 10 sec migrate delta = 40 sec delta
        let src_td = VmTimeData {
            // 30 sec host uptime - 0 boot_hrtime = 30 secs guest uptime
            hrtime: 30 * (NS_PER_SEC as i64),
            ..base_time_data()
        };
        // 10 sec dst_wc - 0 sec src_wc = 10 secs migrate_delta
        let res = adjust_time_data(src_td, 0, Duration::from_secs(10));
        assert!(res.is_ok());
        assert_eq!(res.unwrap().1.boot_hrtime_delta, 40 * NS_PER_SEC);

        // edge case: max guest uptime
        //
        // i64::MAX ns guest uptime + 0 sec migrate delta = i64::MAX ns delta
        let src_td = VmTimeData { hrtime: i64::MAX, ..base_time_data() };
        let res = adjust_time_data(src_td, 0, Duration::from_secs(0));
        assert!(res.is_ok());
        assert_eq!(res.unwrap().1.boot_hrtime_delta, i64::MAX as u64);

        // error case: uptime + migrate_delta overflows u64
        //
        // i64::MAX ns guest uptime + 1 ns delta = overflow i64
        let src_td = VmTimeData { hrtime: i64::MAX, ..base_time_data() };
        let res = adjust_time_data(src_td, 0, Duration::from_nanos(1));
        assert!(res.is_err());
        assert!(matches!(res, Err(TimeAdjustError::TimeDeltaOverflow { .. })));
    }

    // boot_hrtime = target hrtime - boot_hrtime_delta
    //
    // boot_hrtime = target hrtime - ((source_hrtime - boot_hrtime)
    //                                + (dst_wc - src.wall_clock())
    #[test]
    fn test_calc_boot_hrtime() {
        // valid input

        // positive boot_hrtime: target hrtime > boot_hrtime_delta
        //
        // target_hrtime     = 4 days, 1 min, 500 ns
        // boot_hrtime_delta = 3 days, 300 ns
        // boot_hrtime       = 1 day, 1 min, 200 ns
        let dst_hrt =
            Duration::new(4 * SEC_PER_DAY + 60, 500).as_nanos() as i64;
        // decompose boot_hrtime delta into:
        // - 3 days guest uptime
        // - 300 ns migrate delta
        let src_td = VmTimeData {
            // 5 days of host uptime
            hrtime: (5 * SEC_PER_DAY * NS_PER_SEC) as i64,

            // 3 days of guest uptime = 2 days boot_hrtime
            boot_hrtime: (2 * SEC_PER_DAY * NS_PER_SEC) as i64,
            ..base_time_data()
        };
        // migrate_delta: 300 ns dst_wc - 0 ns src_wc
        let dst_wc = Duration::from_nanos(300);

        // expect: 1 day, 1 min, 200 ns
        let expect = ((SEC_PER_DAY + 60) * NS_PER_SEC) as i64 + 200;
        let res = adjust_time_data(src_td, dst_hrt, dst_wc);
        assert!(res.is_ok());
        assert_eq!(res.unwrap().0.boot_hrtime, expect);

        // negative boot_hrtime result: target hrtime < boot_hrtime_delta
        // (guest has longer uptime than target host)
        //
        // target_hrtime     = 3 days, 300 ns
        // boot_hrtime_delta = 4 days, 1 min, 500 ns
        // boot_hrtime       = -(1 day, 1 min, 200 ns)
        let dst_hrt = Duration::new(3 * SEC_PER_DAY, 300).as_nanos() as i64;
        // decompose boot_hrtime_delta: 4 days, 1 min, 500 ns
        // - 4 days guest uptime
        // - 1 min 500 ns migrate delta
        let src_td = VmTimeData {
            // 10 days host uptime
            hrtime: (10 * SEC_PER_DAY * NS_PER_SEC) as i64,

            // 4 days of guest uptime = 6 days boot_hrtime
            boot_hrtime: (6 * SEC_PER_DAY * NS_PER_SEC) as i64,

            // src_wc = 5 sec
            hres_sec: 5,
            hres_ns: 0,
            ..base_time_data()
        };
        // migrate_delta: 1 min 500 ns = dst_wc - src_wc
        //
        //               1 min 500 ns = dst_wc - 5 sec
        // dst_wc        1 min 500 ns + 5 sec = 65 sec 500 ns
        let dst_wc = Duration::new(65, 500);

        // expect: - (1 day, 1 min, 200 ns)
        let expect: i64 = -(((SEC_PER_DAY + 60) * NS_PER_SEC) as i64 + 200);
        let res = adjust_time_data(src_td, dst_hrt, dst_wc);
        assert!(res.is_ok());
        assert_eq!(res.unwrap().0.boot_hrtime, expect);
    }

    #[test]
    fn test_calc_tsc_delta() {
        // valid input

        // 1 GHz, 1 second
        let migrate_delta = Duration::from_nanos(NS_PER_SEC);
        let guest_hz = 1_000_000_000;
        let expect = NS_PER_SEC;
        let res = calc_tsc_delta(migrate_delta, guest_hz);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), expect);

        // 1 GHz, 20 seconds
        let migrate_delta = Duration::from_nanos(NS_PER_SEC * 20);
        let guest_hz = 1_000_000_000;
        let expect = NS_PER_SEC * 20;
        let res = calc_tsc_delta(migrate_delta, guest_hz);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), expect);

        // 2.5 GHz, 1 second
        let migrate_delta = Duration::from_nanos(NS_PER_SEC);
        let guest_hz = 2_500_000_000;
        let expect = 2_500_000_000;
        let res = calc_tsc_delta(migrate_delta, guest_hz);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), expect);

        // 2.5 GHz, 20 seconds
        let migrate_delta = Duration::from_nanos(NS_PER_SEC * 20);
        let guest_hz = 2_500_000_000;
        let expect = 50_000_000_000;
        let res = calc_tsc_delta(migrate_delta, guest_hz);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), expect);

        // error cases

        // delta * guest_hz overflows u128
        let migrate_delta = Duration::from_secs(u64::MAX);
        let guest_hz = u64::MAX;
        let res = calc_tsc_delta(migrate_delta, guest_hz);
        assert!(res.is_err());
        assert!(matches!(res, Err(TimeAdjustError::TscAdjustOverflow { .. })));

        // (delta * guest_hz) / NS_PER_SEC overflows u64
        let migrate_delta = Duration::from_secs(u64::MAX);
        let guest_hz = 1_000_000_000;
        let res = calc_tsc_delta(migrate_delta, guest_hz);
        assert!(res.is_err());
        assert!(matches!(res, Err(TimeAdjustError::TscAdjustOverflow { .. })));
    }

    #[test]
    fn test_calc_guest_tsc() {
        // valid input
        //
        // 1GHz
        // TSC + 3 sec 3 ns
        let src_td = VmTimeData { guest_freq: NS_PER_SEC, ..base_time_data() };
        let res = adjust_time_data(src_td, 0, Duration::new(3, 3));
        let expect = 3 * NS_PER_SEC + 3;
        assert!(res.is_ok());
        assert_eq!(res.unwrap().0.guest_tsc, expect);

        // 2Ghz
        // TSC + 3 sec 3 ns
        let src_td =
            VmTimeData { guest_freq: 2 * NS_PER_SEC, ..base_time_data() };
        let res = adjust_time_data(src_td, 0, Duration::new(3, 3));
        let expect = 2 * (3 * NS_PER_SEC + 3);
        assert!(res.is_ok());
        assert_eq!(res.unwrap().0.guest_tsc, expect);

        // valid input: tsc overflows u64
        let src_td = VmTimeData {
            guest_freq: NS_PER_SEC,
            guest_tsc: u64::MAX,
            ..base_time_data()
        };
        // + 3 sec 3 ns delta: TSC should wrap around to 3 sec 2 ns
        let res = adjust_time_data(src_td, 0, Duration::new(3, 3));
        let expect = 3 * NS_PER_SEC + 2;
        assert!(res.is_ok());
        assert_eq!(res.unwrap().0.guest_tsc, expect);
    }
}
