use crate::dispatch::DispCtx;

use erased_serde::Serialize;

pub trait Migrate: Send + Sync + 'static {
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize>;
}
