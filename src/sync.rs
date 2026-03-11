//! Synchronization primitives with loom compatibility.
//!
//! Under `cfg(loom)`, re-exports loom's tracked types for systematic
//! concurrency testing. Otherwise, provides thin wrappers around std
//! types with a matching API.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicPtr, Ordering};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicPtr, Ordering};

#[cfg(loom)]
pub(crate) use loom::cell::UnsafeCell;

/// Wrapper around `core::cell::UnsafeCell` matching loom's closure-based API.
#[cfg(not(loom))]
pub(crate) struct UnsafeCell<T>(core::cell::UnsafeCell<T>);

#[cfg(not(loom))]
impl<T> UnsafeCell<T> {
    #[inline(always)]
    pub const fn new(data: T) -> Self {
        Self(core::cell::UnsafeCell::new(data))
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
        f(self.0.get())
    }

    #[inline(always)]
    pub fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        f(self.0.get())
    }
}
