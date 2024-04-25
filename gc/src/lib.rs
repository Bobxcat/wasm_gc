#![warn(unsafe_op_in_unsafe_fn)]

use std::{
    alloc::Layout,
    cell::OnceCell,
    collections::{BTreeSet, HashMap},
    fmt::Debug,
    marker::PhantomData,
    mem::MaybeUninit,
    num::NonZeroUsize,
    ops::{Deref, DerefMut},
    ptr::{addr_of, NonNull},
    sync::{Mutex, Once, OnceLock},
    thread::JoinHandle,
    time::Duration,
};

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
struct AllocAddr(pub NonZeroUsize);

struct AllocInfo {
    start: AllocAddr,
    layout: Layout,
}

/// Stores all the information about the GC
struct GcAlloc {
    first_alloc: Option<NonNull<GcBox<dyn GcAble>>>,
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
            first_alloc: None,
            collection_handle: std::thread::spawn(|| Self::collection_loop()),
        }
    }

    fn for_each_alloc(&self, f: impl Fn(*const GcBox<dyn GcAble>)) {
        let mut this = self.first_alloc.as_ref();
        loop {
            match this {
                Some(this_gcb) => {
                    let this_gcb = this_gcb.as_ptr().cast_const();
                    f(this_gcb);
                    let next_value_of_this = GcBoxHeader::next_allocation(GcBox::header(this_gcb));
                    this = unsafe { (*next_value_of_this).as_ref() };
                }
                None => return,
            }
        }
    }

    /// Gives this `GcAlloc` control over the given `GcBox`, which is needed for it to be collected
    pub fn register_gcbox<T: Sized + GcAble>(&mut self, gcb: &mut GcBox<T>) {
        if gcb.header.next_allocation.is_some() {
            panic!(
                "Called `register_gcbox` on a `GcBox<{}>` with a filled `next_allocation` field",
                std::any::type_name::<T>()
            )
        }
        gcb.header.next_allocation = self.first_alloc;
        let gcb = NonNull::new(gcb as &mut GcBox<dyn GcAble>).unwrap();
        self.first_alloc = Some(gcb);
    }

    /// Mark then sweep
    pub fn mark_sweep(&mut self) {
        // Unmark all
        // There's no need to recurse here
        self.for_each_alloc(|alloc| {
            let header = GcBox::header(alloc);
            unsafe { &*header }.unmark()
        });
        // Mark all from stack
        // Recurse to mark all downstream values
        self.for_each_alloc(|alloc| {
            let alloc = unsafe { alloc.as_ref() }.unwrap();
            if alloc.header.is_rooted() {
                alloc.header.mark();
                unsafe { alloc.val.mark() };
            }
        });

        /// Removes the allocation `dropping` and updates the linked list
        ///
        /// SAFETY:
        /// `dropping` must be pointed to by `pointer_to_dropping`
        unsafe fn remove_allocation(
            pointer_to_dropping: &mut Option<NonNull<GcBox<dyn GcAble>>>,
            dropping: &mut GcBox<dyn GcAble>,
        ) {
            // Update linked list
            let pointer_to_after_dropping = dropping.header.next_allocation.clone();
            *pointer_to_dropping = pointer_to_after_dropping;
            // Drop and Deallocate
            let b = unsafe { Box::from_raw(dropping) };
            std::mem::drop(b);
        }

        // Cleanup unmarked boxes
        let mut ptr_to_next = &mut self.first_alloc;
        loop {
            loop {
                let Some(next) = ptr_to_next.as_mut() else {
                    break;
                };

                let next = unsafe { next.as_mut() };

                if !next.header.marked() {
                    // drop...
                    // deallocate...
                    // update the linked list...
                    unsafe { remove_allocation(ptr_to_next, next) };
                }
            }
            match ptr_to_next {
                Some(next) => ptr_to_next = &mut unsafe { next.as_mut() }.header.next_allocation,
                None => break,
            }
        }
    }
}

pub(crate) struct GcBoxHeader {
    /// `true` -> This is referenced (indirectly or not) by a stack `Gc<_>`
    marked: Mutex<bool>,
    root_count: Mutex<u32>,
    next_allocation: Option<NonNull<GcBox<dyn GcAble>>>,
}

impl GcBoxHeader {
    fn next_allocation(this: *const Self) -> *const Option<NonNull<GcBox<dyn GcAble>>> {
        unsafe { addr_of!((*this).next_allocation) }
    }

    /// Returns true if this has a root count of more than 0
    pub fn is_rooted(&self) -> bool {
        *self.root_count.lock().unwrap() > 0
    }
    pub fn marked(&self) -> bool {
        *self.marked.lock().unwrap()
    }
    /// Not recursive
    pub fn mark(&self) {
        *self.marked.lock().unwrap() = true;
    }
    /// Not recursive
    pub fn unmark(&self) {
        *self.marked.lock().unwrap() = false;
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
                next_allocation: None,
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

// unsafe impl GcAble for i8 {}
// unsafe impl GcAble for i16 {}
// unsafe impl GcAble for i32 {}
// unsafe impl GcAble for i64 {}
// unsafe impl GcAble for i128 {}

// unsafe impl GcAble for u8 {}
// unsafe impl GcAble for u16 {}
// unsafe impl GcAble for u32 {}
// unsafe impl GcAble for u64 {}
// unsafe impl GcAble for u128 {}
