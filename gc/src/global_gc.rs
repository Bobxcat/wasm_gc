use std::{
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    sync::{Mutex, Once},
};

use crate::{GcAble, GcAlloc, GcBox};

static GC: GcAllock = GcAllock::new();

struct GcAllock {
    gc: Mutex<MaybeUninit<GcAlloc>>,
    once: Once,
}

impl GcAllock {
    const fn new() -> Self {
        Self {
            gc: Mutex::new(MaybeUninit::uninit()),
            once: Once::new(),
        }
    }
}

/// A locked initialized Gc instance
struct GcAllocked<M>
where
    M: DerefMut<Target = MaybeUninit<GcAlloc>>,
{
    lock: M,
}

impl<M> GcAllocked<M>
where
    M: DerefMut<Target = MaybeUninit<GcAlloc>>,
{
    pub unsafe fn assume_init(lock: M) -> Self {
        Self { lock }
    }
}

impl<M> Deref for GcAllocked<M>
where
    M: DerefMut<Target = MaybeUninit<GcAlloc>>,
{
    type Target = GcAlloc;

    fn deref(&self) -> &Self::Target {
        unsafe { self.lock.assume_init_ref() }
    }
}

impl<M> DerefMut for GcAllocked<M>
where
    M: DerefMut<Target = MaybeUninit<GcAlloc>>,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.lock.assume_init_mut() }
    }
}

/// Returns `true` iff the global Gc has been initialized
pub fn is_init() -> bool {
    GC.once.is_completed()
}

/// Locks the global Gc and makes sure it's init
#[inline(always)]
pub fn lock() -> impl DerefMut<Target = GcAlloc> {
    GC.once.call_once(|| {
        let mut gc = GC.gc.lock().unwrap();
        gc.write(GcAlloc::new());
    });
    unsafe { lock_assume_init() }
}

/// Locks the global Gc without making sure it's init
#[inline(always)]
pub unsafe fn lock_assume_init() -> impl DerefMut<Target = GcAlloc> {
    unsafe { GcAllocked::assume_init(GC.gc.lock().unwrap()) }
}
