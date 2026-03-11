#![warn(clippy::pedantic)]
#![allow(
    clippy::large_stack_arrays,
    reason = "Core allocator data structures are intentionally large fixed-size arrays."
)]

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
