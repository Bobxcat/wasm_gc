#![warn(unsafe_op_in_unsafe_fn)]

use std::{
    alloc::Layout,
    cell::{OnceCell, RefCell},
    collections::{BTreeSet, HashMap, HashSet},
    fmt::Debug,
    marker::PhantomData,
    mem::MaybeUninit,
    num::NonZeroUsize,
    ops::{Deref, DerefMut},
    ptr::{addr_of, NonNull},
    sync::{atomic::AtomicBool, Mutex, Once, OnceLock},
    thread::JoinHandle,
    time::Duration,
};

mod alloc_store;
mod global_gc;

/// Makes sure the global garbage collector is initialized, and initializes it if is isn't
pub fn init_gc() {
    let _ = global_gc::lock();
}

/// Makes sure all memory that can be freed at the moment is freed
pub fn force_collect() {
    global_gc::lock().mark_sweep()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct AllocAddr(NonZeroUsize);

impl<T: ?Sized + GcAble> From<*const GcBox<T>> for AllocAddr {
    fn from(value: *const GcBox<T>) -> Self {
        Self((value as *const () as usize).try_into().unwrap())
    }
}

impl<T: ?Sized + GcAble> From<*mut GcBox<T>> for AllocAddr {
    fn from(value: *mut GcBox<T>) -> Self {
        Self((value as *mut () as usize).try_into().unwrap())
    }
}

struct AllocInfo {
    marked: bool,
}

/// Stores all the information about the GC
struct GcAlloc {
    allocs: HashMap<AllocAddr, NonNull<GcBox<dyn GcAble>>>,
    collection_handle: JoinHandle<()>,
}

unsafe impl Send for GcAlloc {}

impl GcAlloc {
    fn collection_loop() -> ! {
        loop {
            std::thread::sleep(Duration::from_millis(1));
            global_gc::lock().mark_sweep()
        }
    }
    pub fn new() -> Self {
        GcAlloc {
            allocs: HashMap::new(),
            collection_handle: std::thread::spawn(|| Self::collection_loop()),
        }
    }

    /// Gives this `GcAlloc` control over the given `GcBox`, which is needed for it to be collected
    pub fn register_gcbox<T: Sized + GcAble>(&mut self, gcb: &mut GcBox<T>) {
        let addr = AllocAddr::from(gcb as *mut _);
    }

    /// Mark then sweep
    pub fn mark_sweep(&mut self) {
        // Unmark all
        for (_, nn) in &self.allocs {
            let gcb = unsafe { nn.as_ref() };
            gcb.header.unmark()
        }

        // Mark from stack
        for (_, nn) in &self.allocs {
            let gcb = unsafe { nn.as_ref() };
            if gcb.header.is_rooted() {
                gcb.header.mark();
                unsafe { gcb.val.mark() }
            }
        }

        // Deallocate & Drop unmarked
        self.allocs.retain(|_, nn| {
            let ptr = unsafe { nn.as_mut() };
            let to_drop = !ptr.header.marked();
            if to_drop {
                // Drop & deallocate
                Box::leak(unsafe { Box::from_raw(ptr) });
                *nn = NonNull::<GcBox<()>>::dangling();
            }

            !to_drop
        });
    }
}

pub(crate) struct GcBoxHeader {
    /// `true` -> This is referenced (indirectly or not) by a stack `Gc<_>`
    root_count: Mutex<u32>,
    marked: Mutex<bool>,
}

impl GcBoxHeader {
    /// Returns true if this has a root count of more than 0
    pub fn is_rooted(&self) -> bool {
        *self.root_count.lock().unwrap() > 0
    }
    pub fn marked(&self) -> bool {
        *self.marked.lock().unwrap()
    }
    pub fn unmark(&self) {
        *self.marked.lock().unwrap() = false;
    }
    pub fn mark(&self) {
        *self.marked.lock().unwrap() = true;
    }
}

#[repr(C)]
pub(crate) struct GcBox<T: ?Sized + GcAble> {
    header: GcBoxHeader,
    val: T,
}

impl<T: ?Sized + GcAble> GcBox<T> {
    pub fn header(this: *const Self) -> *const GcBoxHeader {
        unsafe { addr_of!((*this).header) }
    }
    pub fn val(this: *const Self) -> *const T {
        unsafe { addr_of!((*this).val) }
    }
}

trait IncOrDec: Copy + 'static {
    fn get() -> i32;
}

#[derive(Debug, Clone, Copy)]
struct PosOne;

impl IncOrDec for PosOne {
    #[inline(always)]
    fn get() -> i32 {
        1
    }
}

#[derive(Debug, Clone, Copy)]
struct NegOne;

impl IncOrDec for NegOne {
    #[inline(always)]
    fn get() -> i32 {
        1
    }
}

pub struct Gc<T: GcAble> {
    is_root: Mutex<bool>,
    gcbox: NonNull<GcBox<T>>,
}

// SAFETY: All referenced values are managed between multiple threads
unsafe impl<T: GcAble> Send for Gc<T> {}
// SAFETY: All referenced values are managed between multiple threads,
// and any interior mutation is hidden behind syncronization primitives (in `GcBox<T>`)
unsafe impl<T: GcAble> Sync for Gc<T> {}

impl<T: GcAble> Gc<T> {
    pub fn new(val: T) -> Gc<T> {
        Gc::from_box(Box::new(val))
    }
    pub fn from_box(owned_ptr: Box<T>) -> Gc<T> {
        let val = *owned_ptr;
        unsafe { val.set_not_root() };

        let gcbox = Box::leak(Box::new(GcBox {
            header: GcBoxHeader {
                marked: Mutex::new(false),
                root_count: Mutex::new(1), // < `1` since we are creating the first Gc here
            },
            val,
        }));

        global_gc::lock().register_gcbox(gcbox);

        Gc {
            is_root: Mutex::new(true),
            gcbox: NonNull::new(gcbox).unwrap(),
        }
    }

    pub fn as_ptr(&self) -> *const T {
        GcBox::val(self.gcbox.as_ptr())
    }

    /// Recursively marks all pointed to values
    ///
    /// Ends recursion if this was already marked
    pub unsafe fn mark(&self) {
        let g = unsafe { self.gcbox.as_ref() };
        let was_marked = g.header.marked();
        if !was_marked {
            g.header.mark();
            unsafe { g.val.mark() };
        }
    }
    pub unsafe fn set_not_root(&self) {
        let mut is_root = self.is_root.lock().unwrap();
        if *is_root {
            unsafe { self.dec_root_count() };
        }
        *is_root = false;
    }
    pub unsafe fn inc_root_count(&self) {
        unsafe { self.change_root_count::<PosOne>() }
    }
    pub unsafe fn dec_root_count(&self) {
        unsafe { self.change_root_count::<NegOne>() }
    }
    unsafe fn change_root_count<Delta: IncOrDec>(&self) {
        let gcb = unsafe { self.gcbox.as_ref() };
        let mut rc = gcb.header.root_count.lock().unwrap();
        match Delta::get() {
            -1 => {
                // Should never underflow
                *rc -= 1;
            }
            1 => {
                *rc = rc.checked_add(1).unwrap();
            }
            _ => unreachable!(),
        }
    }
}

impl<T: GcAble> Clone for Gc<T> {
    fn clone(&self) -> Self {
        unsafe { self.inc_root_count() };
        Self {
            is_root: Mutex::new(true),
            gcbox: self.gcbox,
        }
    }
}

impl<T: GcAble> Drop for Gc<T> {
    fn drop(&mut self) {
        if *self.is_root.lock().unwrap() {
            unsafe { self.dec_root_count() };
        }
    }
}

impl<T: GcAble> Deref for Gc<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.as_ptr() }
    }
}

impl<T: GcAble> AsRef<T> for Gc<T> {
    fn as_ref(&self) -> &T {
        &*self
    }
}

impl<T: GcAble + Debug> Debug for Gc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_ref())
    }
}

/// An item which can be used and tracked by the Gc
pub unsafe trait GcAble: Send + Sync + 'static {
    /// Call `Gc::mark(..)` on every `Gc<_>` in this struct
    unsafe fn mark(&self);
    /// Call `Gc::inc_root_count` on every `Gc<_>` in this struct
    unsafe fn inc_root_count(&self);
    /// Call `Gc::dec_root_count` on every `Gc<_>` in this struct
    unsafe fn dec_root_count(&self);
    /// Call `Gc::set_not_root` on every `Gc<_>` in this struct
    unsafe fn set_not_root(&self);
}

macro_rules! impl_gc_no_children {
    ($t:ty) => {
        unsafe impl GcAble for $t {
            unsafe fn mark(&self) {}

            unsafe fn inc_root_count(&self) {}

            unsafe fn dec_root_count(&self) {}

            unsafe fn set_not_root(&self) {}
        }
    };
}

impl_gc_no_children!(());

impl_gc_no_children!(i8);
impl_gc_no_children!(i16);
impl_gc_no_children!(i32);
impl_gc_no_children!(i64);
impl_gc_no_children!(i128);

impl_gc_no_children!(u8);
impl_gc_no_children!(u16);
impl_gc_no_children!(u32);
impl_gc_no_children!(u64);
impl_gc_no_children!(u128);
