use std::cell::UnsafeCell;
use std::sync::{Arc, Weak};

pub struct SelfArcCell<T> {
    inner: UnsafeCell<Option<Weak<T>>>,
}

impl<T: SelfArc> Default for SelfArcCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: SelfArc> SelfArcCell<T> {
    pub fn new() -> Self {
        Self { inner: UnsafeCell::new(None) }
    }
    fn set(this: &mut Arc<T>) {
        // SAFETY: Our exclusive access to the UnsafeCell is ensured by the Arc::get_mut() call.
        // Any later attempt to call set() again will fail, as the stored Weak reference will
        // prevent it from a successful return.
        //
        // We also check to ensure that the ARefCell is contained within the Arc<T> via pointer
        // math to make certain the memory write is within the exclusive access.
        unsafe {
            let top = Arc::get_mut(this).unwrap() as *mut T as *const u8;
            let bottom = top.add(std::mem::size_of::<T>());
            let acell = this.aref_cell();
            let acell_ptr = acell as *const Self as *const u8;

            assert!(top <= acell_ptr && bottom > acell_ptr);

            let weak = Arc::downgrade(this);
            let inner = acell.inner.get();
            std::ptr::replace(inner, Some(weak));
        }
    }
    fn get(&self) -> Weak<T> {
        let pointer = self.inner.get();
        // SAFETY: The UnsafeCell will either hold the None from initialization, or a valid
        // Some(Weak<T>) as written by set().
        let oref = unsafe { pointer.as_ref().unwrap() };
        oref.as_ref().unwrap().clone()
    }
    fn get_arc(&self) -> Arc<T> {
        let pointer = self.inner.get();
        // SAFETY: The UnsafeCell will either hold the None from initialization, or a valid
        // Some(Weak<T>) as written by set().
        let oref = unsafe { pointer.as_ref().unwrap() };
        Weak::upgrade(oref.as_ref().unwrap()).unwrap()
    }
}

// SAFETY: With the one write access to the UnsafeCell constrained to an context we know is
// exclusive, we are willing to grant Sync.
unsafe impl<T: Sync> Sync for SelfArcCell<T> {}

pub trait SelfArc: Sized {
    fn aref_cell(&self) -> &SelfArcCell<Self>;
    fn aref(&self) -> Weak<Self> {
        self.aref_cell().get()
    }
    fn aref_arc(&self) -> Arc<Self> {
        self.aref_cell().get_arc()
    }
    fn aref_init(this: &mut Arc<Self>) {
        SelfArcCell::set(this)
    }
}
