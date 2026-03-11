#![warn(clippy::pedantic)]
// Hot-path allocator code benefits from forced inlining.
#![allow(clippy::inline_always)]
// Core allocator data structures are intentionally large fixed-size arrays.
#![allow(clippy::large_stack_arrays)]
// We cast mmap'd pointers to aligned types; alignment is guaranteed by the page allocator.
#![allow(clippy::cast_ptr_alignment)]

mod global;
mod heap;
#[cfg(feature = "metrics")]
pub mod metrics;
mod pool;
#[cfg(feature = "pprof")]
pub mod pprof;
mod size_class;
mod slab;
mod sync;
mod sys;

pub use global::ClMalloc;
#[cfg(feature = "metrics")]
pub use metrics::MetricsSnapshot;
