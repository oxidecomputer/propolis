// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use async_trait::async_trait;
use propolis::common::GuestAddr;
use propolis::hw::ps2::ctrl::PS2Ctrl;
use propolis::hw::qemu::ramfb::{Config, FramebufferSpec};
use rfb::encodings::RawEncoding;
use rfb::pixel_formats::fourcc;
use rfb::rfb::{
    FramebufferUpdate, KeyEvent, ProtoVersion, Rectangle, SecurityType,
    SecurityTypes,
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
                ps2ctrl: None,
                vm: None,
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
        let inner = self.inner.lock().await;

        match &inner.framebuffer {
            Framebuffer::Uninitialized(fb) => {
                debug!(self.log, "framebuffer: uninitialized");

                // Display a white screen if the guest isn't ready yet.
                let len: usize = fb.width as usize * fb.height as usize * 4;
                let pixels = vec![0xffu8; len];

                let r = Rectangle::new(
                    0,
                    0,
                    fb.width,
                    fb.height,
                    Box::new(RawEncoding::new(pixels)),
                );
                FramebufferUpdate::new(vec![r])
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

                let r = Rectangle::new(
                    0,
                    0,
                    fb.width as u16,
                    fb.height as u16,
                    Box::new(RawEncoding::new(buf)),
                );
                FramebufferUpdate::new(vec![r])
            }
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
