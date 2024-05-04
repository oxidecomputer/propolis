// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use async_trait::async_trait;
use propolis::common::GuestAddr;
use propolis::hw::ps2::ctrl::PS2Ctrl;
use propolis::hw::ps2::mouse::MouseEventRep;
use propolis::hw::qemu::ramfb::{Config, FramebufferSpec};
use rfb::encodings::RawEncoding;
use rfb::pixel_formats::fourcc;
use rfb::rfb::{
    FramebufferUpdate, KeyEvent, MouseButtons, PointerEvent, ProtoVersion,
    Rectangle, SecurityType, SecurityTypes,
};
use rfb::server::{Server, VncServer, VncServerConfig, VncServerData};
use slog::{debug, error, info, o, trace, Logger};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::vm::VmController;

const INITIAL_WIDTH: u16 = 1024;
const INITIAL_HEIGHT: u16 = 768;

#[derive(Debug, Clone, Copy)]
pub struct RamFb {
    addr: u64,
    width: u32,
    height: u32,
    fourcc: u32,
}

impl RamFb {
    pub fn new(fb_spec: FramebufferSpec) -> Self {
        Self {
            addr: fb_spec.addr,
            width: fb_spec.width,
            height: fb_spec.height,
            fourcc: fb_spec.fourcc,
        }
    }
}

struct DefaultFb {
    width: u16,
    height: u16,
}

enum Framebuffer {
    Uninitialized(DefaultFb),
    Initialized(RamFb),
}

struct PropolisVncServerInner {
    framebuffer: Framebuffer,
    last_screen_sent: Vec<u8>,
    relative_mouse_hack: (i32, i32),
    ps2ctrl: Option<Arc<PS2Ctrl>>,
    vm: Option<Arc<VmController>>,
}

#[derive(Clone)]
pub struct PropolisVncServer {
    inner: Arc<Mutex<PropolisVncServerInner>>,
    log: Logger,
}

impl PropolisVncServer {
    pub fn new(initial_width: u16, initial_height: u16, log: Logger) -> Self {
        PropolisVncServer {
            inner: Arc::new(Mutex::new(PropolisVncServerInner {
                framebuffer: Framebuffer::Uninitialized(DefaultFb {
                    width: initial_width,
                    height: initial_height,
                }),
                last_screen_sent: vec![],
                ps2ctrl: None,
                vm: None,
                relative_mouse_hack: (0, 0),
            })),
            log,
        }
    }

    pub async fn initialize(
        &self,
        fb: RamFb,
        ps2ctrl: Arc<PS2Ctrl>,
        vm: Arc<VmController>,
    ) {
        let mut inner = self.inner.lock().await;
        inner.framebuffer = Framebuffer::Initialized(fb);
        inner.ps2ctrl = Some(ps2ctrl);
        inner.vm = Some(vm);
    }

    pub async fn update(
        &self,
        config: &Config,
        is_valid: bool,
        rfb_server: &VncServer<Self>,
    ) {
        if is_valid {
            debug!(self.log, "updating framebuffer");

            let fb_spec = config.get_framebuffer_spec();
            let fb = RamFb::new(fb_spec);

            let mut inner = self.inner.lock().await;

            if fb.addr != 0 {
                inner.framebuffer = Framebuffer::Initialized(fb);
            }

            match fourcc::fourcc_to_pixel_format(fb.fourcc) {
                Ok(pf) => {
                    rfb_server.set_pixel_format(pf).await;
                    info!(
                        self.log,
                        "pixel format set to fourcc={:#04x}", fb.fourcc
                    );
                }
                Err(e) => {
                    error!(self.log, "could not set pixel format: {:?}", e);
                }
            }
        } else {
            error!(self.log, "invalid config update");
        }
    }
}

#[async_trait]
impl Server for PropolisVncServer {
    async fn get_framebuffer_update(&self) -> FramebufferUpdate {
        let mut inner = self.inner.lock().await;

        let (width, height, pixels) = match &inner.framebuffer {
            Framebuffer::Uninitialized(fb) => {
                debug!(self.log, "framebuffer: uninitialized");

                // Display a white screen if the guest isn't ready yet.
                let len: usize = fb.width as usize * fb.height as usize * 4;
                let pixels = vec![0xffu8; len];
                (fb.width, fb.height, pixels)
            }
            Framebuffer::Initialized(fb) => {
                debug!(self.log, "framebuffer initialized: fb={:?}", fb);

                let len = fb.height as usize * fb.width as usize * 4;
                let mut buf = vec![0u8; len];

                let read = tokio::task::block_in_place(|| {
                    let machine = inner.vm.as_ref().unwrap().machine();
                    let memctx = machine.acc_mem.access().unwrap();
                    memctx.read_into(GuestAddr(fb.addr), &mut buf, len)
                });

                assert!(read.is_some());
                debug!(self.log, "read {} bytes from guest", read.unwrap());
                (fb.width as u16, fb.height as u16, buf)
            }
        };

        if inner.last_screen_sent.len() != pixels.len() {
            inner.last_screen_sent = pixels.clone();
            let r = Rectangle::new(
                0,
                0,
                width,
                height,
                Box::new(RawEncoding::new(pixels)),
            );
            FramebufferUpdate::new(vec![r])
        } else {
            let last = &inner.last_screen_sent;
            const BYTES_PER_PX: usize = 4;
            let width = width as usize;
            let height = height as usize;
            // simple optimization: divide screen into grid, then find
            // which of those have had changes, and shrink their bounding
            // rectangles to whatever those changes were.
            const SUBDIV_X: usize = 8;
            const SUBDIV_Y: usize = 8;
            let subwidth = width / SUBDIV_X;
            let subheight = height / SUBDIV_Y;
            let stride = width * BYTES_PER_PX;
            let mut dirty_rects = vec![];
            for sub_y in (0..height).step_by(subheight) {
                for sub_x in (0..width).step_by(subwidth) {
                    let base = ((sub_y * width) + sub_x) * BYTES_PER_PX;
                    let mut dirty = false;
                    // would make these non-mut bindings, but break-with-value
                    // only supports `loop {}` and not `for {}`...
                    let mut first_y = 0;
                    for y in 0..subheight {
                        let start = base + (y * stride);
                        let end = start + (subwidth * BYTES_PER_PX);
                        if last[start..end] != pixels[start..end] {
                            dirty = true;
                            first_y = y;
                            break;
                        }
                    }
                    if dirty {
                        let mut first_x = 0;
                        let mut last_x = 0;
                        let mut last_y = 0;
                        // find other bounds
                        for y in (0..subheight).rev() {
                            let start = base + (y * stride);
                            let end = start + (subwidth * BYTES_PER_PX);
                            if last[start..end] != pixels[start..end] {
                                last_y = y;
                                break;
                            }
                        }
                        'fx: for x in 0..subwidth {
                            for y in 0..subheight {
                                let start =
                                    base + (y * stride) + (x * BYTES_PER_PX);
                                let end = start + BYTES_PER_PX;
                                if last[start..end] != pixels[start..end] {
                                    first_x = x;
                                    break 'fx;
                                }
                            }
                        }
                        'lx: for x in (0..subwidth).rev() {
                            for y in 0..subheight {
                                let start =
                                    base + (y * stride) + (x * BYTES_PER_PX);
                                let end = start + BYTES_PER_PX;
                                if last[start..end] != pixels[start..end] {
                                    last_x = x;
                                    break 'lx;
                                }
                            }
                        }
                        let rect_x = first_x + sub_x;
                        let rect_y = first_y + sub_y;
                        let rect_width = last_x - first_x + 1;
                        let rect_height = last_y - first_y + 1;
                        let mut data = Vec::with_capacity(
                            rect_width * rect_height * BYTES_PER_PX,
                        );
                        for y in first_y..=last_y {
                            let start =
                                base + (y * stride) + (first_x * BYTES_PER_PX);
                            let end = start + (rect_width * BYTES_PER_PX);
                            data.extend_from_slice(&pixels[start..end]);
                        }
                        dirty_rects.push(Rectangle::new(
                            rect_x as u16,
                            rect_y as u16,
                            rect_width as u16,
                            rect_height as u16,
                            Box::new(RawEncoding::new(data)),
                        ));
                    }
                }
            }
            inner.last_screen_sent = pixels;
            FramebufferUpdate::new(dirty_rects)
        }
    }

    async fn key_event(&self, ke: KeyEvent) {
        let inner = self.inner.lock().await;
        let ps2 = inner.ps2ctrl.as_ref();

        if let Some(ps2) = ps2 {
            trace!(self.log, "keyevent: {:?}", ke);
            ps2.key_event(ke);
        } else {
            trace!(self.log, "guest not initialized; dropping keyevent");
        }
    }

    async fn pointer_event(&self, pe: PointerEvent) {
        let mut inner = self.inner.lock().await;
        let ps2 = inner.ps2ctrl.as_ref();

        if let Some(ps2) = ps2 {
            trace!(self.log, "pointerevent: {:?}", pe);
            let (old_x, old_y) = inner.relative_mouse_hack;
            let x = pe.position.x as i32;
            let y = pe.position.y as i32;
            let x_movement = (x - old_x).clamp(-256, 255) as i16;
            let y_movement = (y - old_y).clamp(-256, 255) as i16;
            ps2.mouse_event(MouseEventRep {
                y_overflow: false,
                x_overflow: false,
                middle_button: pe.pressed.intersects(MouseButtons::MIDDLE),
                right_button: pe.pressed.intersects(MouseButtons::RIGHT),
                left_button: pe.pressed.intersects(MouseButtons::LEFT),
                x_movement,
                y_movement,
            });
            inner.relative_mouse_hack =
                (x + x_movement as i32, y + y_movement as i32);
        } else {
            trace!(self.log, "guest not initialized; dropping pointerevent");
        }
    }

    async fn stop(&self) {
        info!(self.log, "stopping VNC server");

        let mut inner = self.inner.lock().await;
        inner.framebuffer = Framebuffer::Uninitialized(DefaultFb {
            width: INITIAL_WIDTH,
            height: INITIAL_HEIGHT,
        });
        inner.ps2ctrl = None;
        inner.vm = None;
    }
}

// Default VNC server configuration.
pub fn setup_vnc(
    log: &Logger,
    addr: SocketAddr,
) -> Arc<VncServer<PropolisVncServer>> {
    let config = VncServerConfig {
        addr,
        version: ProtoVersion::Rfb38,
        // vncviewer won't work without offering VncAuth, even though it doesn't ask to use
        // it.
        sec_types: SecurityTypes(vec![
            SecurityType::None,
            SecurityType::VncAuthentication,
        ]),
        name: "propolis-vnc".to_string(),
    };

    let pf = fourcc::fourcc_to_pixel_format(fourcc::FOURCC_XR24).unwrap();
    let data = VncServerData {
        width: INITIAL_WIDTH,
        height: INITIAL_HEIGHT,
        input_pixel_format: pf,
    };
    let pvnc = PropolisVncServer::new(
        INITIAL_WIDTH,
        INITIAL_HEIGHT,
        log.new(o!("component" => "vnc-server")),
    );

    VncServer::new(pvnc, config, data)
}
