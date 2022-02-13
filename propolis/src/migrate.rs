use crate::dispatch::DispCtx;

use erased_serde::Serialize;
use futures::future::{self, BoxFuture};

pub trait Migrate: Send + Sync + 'static {
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize>;

    /// Called when the device should stop servicing any guest
    /// requests. The returned future should become ready
    /// once the device has stopped servicing the guest and
    /// any pending operations have been completed or cancelled.
    fn pause(&self, _ctx: &DispCtx) -> BoxFuture<'static, ()> {
        Box::pin(future::ready(()))
    }
}
