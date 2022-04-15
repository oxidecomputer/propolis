use async_trait::async_trait;
use propolis::common::GuestAddr;
use propolis::dispatch::AsyncCtx;
use propolis::hw::qemu::ramfb::Config;
use rfb::encodings::RawEncoding;
use rfb::rfb::{
    FramebufferUpdate, ProtoVersion, Rectangle, SecurityType, SecurityTypes,
};
use rfb::server::{Server, VncServer, VncServerConfig, VncServerData};
use slog::{debug, error, o, Logger};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug)]
pub struct RamFb {
    addr: u64,
    width: u32,
    height: u32,
}

impl RamFb {
    pub fn new(addr: u64, width: u32, height: u32) -> Self {
        Self { addr, width, height }
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

#[derive(Clone)]
pub struct PropolisVncServer {
    framebuffer: Arc<Mutex<Framebuffer>>,
    actx: Arc<Mutex<Option<AsyncCtx>>>,
    log: Logger,
}

impl PropolisVncServer {
    pub fn new(initial_width: u16, initial_height: u16, log: Logger) -> Self {
        PropolisVncServer {
            framebuffer: Arc::new(Mutex::new(Framebuffer::Uninitialized(
                DefaultFb { width: initial_width, height: initial_height },
            ))),
            actx: Arc::new(Mutex::new(None)),
            log,
        }
    }

    pub async fn set_async_ctx(&self, actx: AsyncCtx) {
        let mut locked = self.actx.lock().await;
        *locked = Some(actx);
    }

    pub async fn initialize_framebuffer(&self, fb: RamFb) {
        if fb.addr != 0 {
            let mut locked = self.framebuffer.lock().await;
            *locked = Framebuffer::Initialized(fb);
        }
    }

    pub async fn update(&self, config: &Config, is_valid: bool) {
        if is_valid {
            debug!(self.log, "updating framebuffer");
            let (addr, w, h) = config.get_framebuffer_info();
            let fb = RamFb::new(addr, w, h);
            self.initialize_framebuffer(fb).await;
        } else {
            error!(self.log, "invalid config update");
        }
    }
}

#[async_trait]
impl Server for PropolisVncServer {
    async fn get_framebuffer_update(&self) -> FramebufferUpdate {
        let locked = self.framebuffer.lock().await;
        let fb = match &*locked {
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
                debug!(self.log, "framebuffer initialized={:?}", fb);

                let len = fb.height as usize * fb.width as usize * 4;
                let mut buf = vec![0u8; len];

                let locked = self.actx.lock().await;
                let actx = locked.as_ref().unwrap();
                let memctx = actx.dispctx().await.unwrap().mctx.memctx();
                let read = memctx.read_into(GuestAddr(fb.addr), &mut buf, len);
                drop(memctx);

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
        };

        fb
    }
}

// Default VNC server configuration.
// XXX: Do we want to specify this information in the config file?
pub fn setup_vnc(log: &Logger) -> VncServer<PropolisVncServer> {
    let initial_width = 1024;
    let initial_height = 768;

    let config = VncServerConfig {
        addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 5900),
        version: ProtoVersion::Rfb38,
        // vncviewer won't work without offering VncAuth, even though it doesn't ask to use
        // it.
        sec_types: SecurityTypes(vec![
            SecurityType::None,
            SecurityType::VncAuthentication,
        ]),
        name: "propolis-vnc".to_string(),
    };
    let data = VncServerData { width: initial_width, height: initial_height };
    let pvnc = PropolisVncServer::new(
        initial_width,
        initial_height,
        log.new(o!("component" => "vnc-server")),
    );

    VncServer::new(pvnc, config, data)
}
