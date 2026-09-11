// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use crate::accessors::MemAccessor;
use crate::common::*;
use crate::migrate::*;
use crate::util::regmap::RegMap;
use crate::vmm::mem::SubMapping;
use crate::vmm::MemCtx;

use lazy_static::lazy_static;
use pin_project_lite::pin_project;
use rgb_frame::{FourCC, Frame, Spec};
use tokio::sync::{futures::Notified, Notify};

#[derive(Copy, Clone, Eq, PartialEq)]
enum Reg {
    Addr,
    FourCC,
    Flags,
    Width,
    Height,
    Stride,
}

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
        RegMap::create_packed(RamFb::FWCFG_ENTRY_SIZE, &layout, None)
    };
}

#[derive(Default, Debug)]
#[repr(C, packed)]
struct Config {
    addr: u64,
    fourcc: u32,
    flags: u32,
    width: u32,
    height: u32,
    stride: u32,
}
impl Config {
    /// Attempt to get a readable mapping to the guest memory backing this ramfb
    fn mapping<'a>(&self, mem: &'a MemCtx) -> Option<SubMapping<'a>> {
        if self.height == 0 || self.width == 0 {
            return None;
        }
        let bytepp = FourCC::from_repr(self.fourcc)?.bytes_per_pixel().get();

        let stride = self.stride(bytepp);
        let linesize = (self.width as usize).checked_mul(bytepp)?;
        let len =
            usize::checked_mul((self.height - 1) as usize, stride)? + linesize;
        mem.readable_region(&GuestRegion(GuestAddr(self.addr), len))
    }

    fn stride(&self, bytepp: usize) -> usize {
        if self.stride == 0 {
            (self.width as usize).checked_mul(bytepp).unwrap_or(0)
        } else {
            self.stride as usize
        }
    }
}

pub enum ConfigError {
    ZeroWidth,
    ZeroHeight,
    UnknownFourCC(u32),
    SizeOverflow,
}
impl TryFrom<&Config> for Spec {
    type Error = ConfigError;

    fn try_from(value: &Config) -> Result<Self, Self::Error> {
        let width = NonZeroUsize::new(value.width as usize)
            .ok_or(ConfigError::ZeroWidth)?;
        let height = NonZeroUsize::new(value.height as usize)
            .ok_or(ConfigError::ZeroHeight)?;
        let fourcc = FourCC::from_repr(value.fourcc)
            .ok_or(ConfigError::UnknownFourCC(value.fourcc))?;
        // The Frame initializer will choose an appropriate stride when it
        // allocates a buffer for the pixel data.  Until then, just emit one
        // consistent with a contiguous buffer.
        let stride = fourcc
            .bytes_per_pixel()
            .checked_mul(width)
            .ok_or(ConfigError::SizeOverflow)?;

        Ok(Spec { width, height, stride, fourcc })
    }
}

#[derive(Clone, Copy)]
pub struct FramebufferSpec {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub fourcc: u32,
}

pub struct FrameSnap {
    pub frame: Frame,
    pub when: Instant,
}
impl FrameSnap {
    fn read_from(config: &Config, mem: &MemCtx) -> Option<Self> {
        let mapping = config.mapping(mem)?;

        // With a valid Spec for the frame, we know the bytes-per-pixel
        let spec: Spec = config.try_into().ok()?;
        let bytepp = spec.fourcc.bytes_per_pixel().get();

        let fb_linesize = (config.width as usize).checked_mul(bytepp)?;
        let fb_stride = config.stride(bytepp);
        if fb_stride <= fb_linesize {
            // Pixel data is contiguous, so its a single big copy
            let len = mapping.len();
            let frame = Frame::new_uninit(spec, |buf, buf_stride| {
                // Expect that the Frame allocation layout matches that of the
                // framebuffer, since it is contiguous.
                assert_eq!(fb_stride, buf_stride.get());
                assert_eq!(len, buf.len());

                // Use raw pointer instead of MaybeUninit::write
                let buf_ptr = buf.as_mut_ptr() as *mut u8;
                unsafe {
                    mapping
                        .raw_readable()
                        .unwrap()
                        .copy_to_nonoverlapping(buf_ptr, len);
                }
            });
            Some(Self { frame, when: Instant::now() })
        } else {
            // Pixel data has "empty" space in stride to skip over
            let frame = Frame::new_uninit(spec, |buf, buf_stride| {
                // While the framebuffer is non-contiguous, we still expect the
                // Frame allocation to be (at this time)
                assert_eq!(fb_linesize, buf_stride.get());

                unsafe {
                    // Use raw pointer instead of MaybeUninit::write
                    let write_ptr = buf.as_mut_ptr() as *mut u8;
                    let read_ptr = mapping.raw_readable().unwrap();
                    for n in 0..(config.height as usize) {
                        read_ptr.add(n * fb_stride).copy_to_nonoverlapping(
                            write_ptr.add(n * fb_linesize),
                            fb_linesize,
                        );
                    }
                };
            });

            Some(Self { frame, when: Instant::now() })
        }
    }
}

struct Inner {
    config: Config,
    last_update: Instant,
}

pub struct RamFb {
    state: Mutex<Inner>,
    acc_mem: MemAccessor,
    notify: Notify,
    log: slog::Logger,
}
impl RamFb {
    /// Size of the entry exposed via `fw_cfg` interface
    pub const FWCFG_ENTRY_SIZE: usize = 28;

    /// Expected name of entry exposed via `fw_cfg` interface
    pub const FWCFG_ENTRY_NAME: &'static str = "etc/ramfb";

    pub fn create(log: slog::Logger) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Inner {
                config: Config::default(),
                last_update: Instant::now(),
            }),
            notify: Notify::new(),
            acc_mem: MemAccessor::new_orphan(),
            log,
        })
    }
    pub fn attach(&self, acc_mem: &MemAccessor) {
        acc_mem.adopt(&self.acc_mem, Some("ramfb".to_string()));
    }

    /// Attempt to read contents of framebuffer
    ///
    /// A [Spec] representing the current device configuration will
    /// be passed to the `validate_bpp` callback, which will determine if the
    /// [Frame] contents should be fetched, and if so, what bits-per-pixel
    /// should be used for the configured `fourcc` of the device.
    ///
    /// Returns a [Frame] if `validate_bpp` returned `Some(bpp)`, and the frame
    /// contents could be copied from the region of guest memory specified in
    /// the configuration register.
    pub fn read_framebuffer(
        &self,
        interested: impl FnOnce(&Spec) -> bool,
    ) -> Option<FrameSnap> {
        let state = self.state.lock().unwrap();
        let mem = self.acc_mem.access()?;

        // Is the configuration even remotely valid?
        let spec = (&state.config).try_into().ok()?;

        // Is the consumer interested in the buffer as configured?
        if !interested(&spec) {
            return None;
        }

        FrameSnap::read_from(&state.config, &mem)
    }

    /// Get [Spec] representing the current device configuration, if it happens
    /// to be valid for a [Frame].
    pub fn read_spec(&self) -> Result<Spec, ConfigError> {
        let state = self.state.lock().unwrap();
        Spec::try_from(&state.config)
    }

    pub fn updated_since(&self, when: Instant) -> UpdatedSince<'_> {
        UpdatedSince {
            ramfb: self,
            notified: self.notify.notified(),
            since: when,
        }
    }

    pub(crate) fn fwcfg_rw(&self, mut rwo: RWOp) -> Result<(), ()> {
        let mut state = self.state.lock().unwrap();

        // Writes outside the bounds of the config register are not allowed
        if let RWOp::Write(wo) = &rwo {
            let start = wo.offset();
            let end = start.saturating_add(wo.len());
            if start >= Self::FWCFG_ENTRY_SIZE || end > Self::FWCFG_ENTRY_SIZE {
                return Err(());
            }
        }

        CFG_REGS.process(&mut rwo, |id, rwo| {
            let config = &mut state.config;
            match rwo {
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
            }
        });
        if rwo.is_write() {
            slog::debug!(self.log, "ramfb change"; "config" => ?state.config);
            state.last_update = Instant::now();
            self.notify.notify_waiters();
        }
        Ok(())
    }
}

pin_project! {
    pub struct UpdatedSince<'a> {
        ramfb: &'a RamFb,
        #[pin]
        notified: Notified<'a>,
        since: Instant,
    }
}
impl Future for UpdatedSince<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let since = self.since;
        let mut this = self.project();
        loop {
            if this.ramfb.state.lock().unwrap().last_update > since {
                return Poll::Ready(());
            }
            if let Poll::Ready(_) = Notified::poll(this.notified.as_mut(), cx) {
                // refresh the now-consumed Notified, and take another lap to
                // check the status
                this.notified.set(this.ramfb.notify.notified());
                continue;
            } else {
                return Poll::Pending;
            }
        }
    }
}

impl Lifecycle for RamFb {
    fn type_name(&self) -> &'static str {
        "qemu-ramfb"
    }
    fn migrate(&self) -> Migrator<'_> {
        Migrator::Single(self)
    }
}

impl MigrateSingle for RamFb {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError> {
        let state = self.state.lock().unwrap();
        let config = &state.config;
        Ok(migrate::RamFbV1 {
            addr: config.addr,
            fourcc: config.fourcc,
            flags: config.flags,
            width: config.width,
            height: config.height,
            stride: config.stride,
        }
        .into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let data: migrate::RamFbV1 = offer.parse()?;

        let mut state = self.state.lock().unwrap();
        let config = &mut state.config;
        config.addr = data.addr;
        config.fourcc = data.fourcc;
        config.flags = data.flags;
        config.width = data.width;
        config.height = data.height;
        config.stride = data.stride;
        state.last_update = Instant::now();

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
#[cfg(test)]
mod test {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn config_reg_size() {
        assert_eq!(size_of::<Config>(), RamFb::FWCFG_ENTRY_SIZE);
    }
}
