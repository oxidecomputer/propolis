use crate::dispatch::DispCtx;

mod sock;

pub use sock::UDSock;

pub type Notifier = Box<dyn Fn(&DispCtx) -> () + Send + Sync + 'static>;

pub trait Sink: Send + Sync + 'static {
    // XXX: make this slice based
    fn sink_write(&self, data: u8) -> bool;

    /// Set notifier callback for when sink becomes writable.  If that callback acquires any
    /// exclusion resources (locks, etc), they must not be held setting the notifier.
    fn sink_set_notifier(&self, f: Notifier);
}

pub trait Source: Send + Sync + 'static {
    // XXX: make this slice based
    fn source_read(&self) -> Option<u8>;

    fn source_discard(&self, count: usize) -> usize;
    fn source_set_autodiscard(&self, active: bool);
    /// Set notifier callback for when source becomes readable.  If that callback acquires any
    /// exclusion resources (locks, etc), they must not be held setting the notifier.
    fn source_set_notifier(&self, f: Notifier);
}
