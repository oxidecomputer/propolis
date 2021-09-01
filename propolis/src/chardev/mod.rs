mod sock;

pub use sock::UDSock;

pub type SinkNotifier = Box<dyn Fn(&dyn Sink) + Send + Sync + 'static>;
pub type SourceNotifier = Box<dyn Fn(&dyn Source) + Send + Sync + 'static>;

pub trait Sink: Send + Sync + 'static {
    // XXX: make this slice based
    fn write(&self, data: u8) -> bool;

    /// Set notifier callback for when sink becomes writable.  If that callback acquires any
    /// exclusion resources (locks, etc), they must not be held setting the notifier.
    fn set_notifier(&self, f: SinkNotifier);
}

pub trait Source: Send + Sync + 'static {
    // XXX: make this slice based
    fn read(&self) -> Option<u8>;

    fn discard(&self, count: usize) -> usize;
    fn set_autodiscard(&self, active: bool);
    /// Set notifier callback for when source becomes readable.  If that callback acquires any
    /// exclusion resources (locks, etc), they must not be held setting the notifier.
    fn set_notifier(&self, f: SourceNotifier);
}
