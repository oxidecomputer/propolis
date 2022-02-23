use crate::dispatch::DispCtx;

use erased_serde::Serialize;
use futures::future::{self, BoxFuture};

pub trait Migrate: Send + Sync + 'static {
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize>;

    /// Called to indicate the device should stop servicing the
    /// guest and attempt to cancel or complete any pending operations.
    ///
    /// The device isn't necessarily expected to complete the pause
    /// operation within this call but should instead return a future
    /// indicating such via the `paused` method.
    fn pause(&self, _ctx: &DispCtx) {}

    /// Return a future indicating when the device has finished pausing.
    fn paused(&self) -> BoxFuture<'static, ()> {
        Box::pin(future::ready(()))
    }
}
