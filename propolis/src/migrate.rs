use erased_serde::Serialize;

pub trait Migrate: Send + Sync + 'static {
    fn export(&self) -> Box<dyn Serialize>;
}
