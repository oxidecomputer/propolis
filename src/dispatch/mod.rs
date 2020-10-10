use std::fmt::format;
use std::io::Result;
use std::sync::{Arc, Mutex, Weak};
use std::thread::{Builder, JoinHandle, Thread};

use crate::machine::MachineCtx;
use crate::pio::PioBus;
use crate::vcpu::VcpuHdl;

pub mod event_disp;
mod event_ports;

use event_disp::{EventCtx, EventDispatch};

pub struct Dispatcher {
    mctx: MachineCtx,
    event_dispatch: Arc<EventDispatch>,
    event_thread: Option<JoinHandle<()>>,
    tasks: Mutex<Vec<(String, JoinHandle<()>)>>,
}

impl Dispatcher {
    pub fn new(mctx: MachineCtx) -> Self {
        Self {
            mctx,
            event_dispatch: Arc::new(EventDispatch::new()),
            event_thread: None,
            tasks: Mutex::new(Vec::new()),
        }
    }

    pub fn spawn_events(&mut self) -> Result<()> {
        if self.event_thread.is_some() {
            // XXX: better error handling
            panic!();
        }
        let ctx = DispCtx::new(self.mctx.clone(), self.event_dispatch.clone());
        let edisp = self.event_dispatch.clone();
        let hdl = Builder::new().name("event-dispatch".to_string()).spawn(
            move || {
                event_disp::event_loop(ctx, edisp);
            },
        )?;
        self.event_thread = Some(hdl);
        Ok(())
    }

    pub fn spawn<D>(
        &self,
        name: String,
        data: D,
        func: fn(DispCtx, D),
    ) -> Result<()>
    where
        D: Send + 'static,
    {
        let ctx = DispCtx::new(self.mctx.clone(), self.event_dispatch.clone());
        let hdl = Builder::new().name(name.clone()).spawn(move || {
            func(ctx, data);
        })?;
        self.tasks.lock().unwrap().push((name, hdl));
        Ok(())
    }
    pub fn spawn_vcpu(
        &self,
        vcpu: VcpuHdl,
        func: fn(DispCtx, VcpuHdl),
    ) -> Result<()> {
        let ctx = DispCtx::new(self.mctx.clone(), self.event_dispatch.clone());
        let name = format!("vcpu-{}", vcpu.cpuid());
        let hdl = Builder::new().name(name.clone()).spawn(move || {
            func(ctx, vcpu);
        })?;
        self.tasks.lock().unwrap().push((name, hdl));
        Ok(())
    }
    pub fn join(&self) {
        let mut tasks = self.tasks.lock().unwrap();
        for (_name, joinhdl) in tasks.drain(..) {
            joinhdl.join().unwrap()
        }
    }
    pub fn with_ctx<F>(&self, f: F)
        where F: FnOnce(&DispCtx)
    {
        let ctx = DispCtx::new(self.mctx.clone(), self.event_dispatch.clone());
        f(&ctx)
    }
}

pub struct DispCtx {
    pub mctx: MachineCtx,
    pub vcpu: Option<VcpuHdl>,
    pub event: EventCtx,
}

impl DispCtx {
    fn new(mctx: MachineCtx, edisp: Arc<EventDispatch>) -> DispCtx {
        DispCtx { mctx, vcpu: None, event: EventCtx::new(edisp) }
    }

    fn for_vcpu(
        mctx: MachineCtx,
        edisp: Arc<EventDispatch>,
        cpu: VcpuHdl,
    ) -> DispCtx {
        Self { mctx, vcpu: Some(cpu), event: EventCtx::new(edisp) }
    }
}
