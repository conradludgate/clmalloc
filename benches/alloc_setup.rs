/// Allocator selection via cargo features.
/// Usage: cargo bench --features clmalloc
///        cargo bench --features jemalloc
///        cargo bench --features mimalloc
///        cargo bench --features snmalloc
///        cargo bench                       (system allocator)

#[cfg(feature = "clmalloc")]
#[global_allocator]
static ALLOC: clmalloc::ClMalloc = clmalloc::ClMalloc::new();

#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "snmalloc")]
#[global_allocator]
static ALLOC: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

pub fn allocator_name() -> &'static str {
    #[cfg(feature = "clmalloc")]
    return "clmalloc";
    #[cfg(feature = "jemalloc")]
    return "jemalloc";
    #[cfg(feature = "mimalloc")]
    return "mimalloc";
    #[cfg(feature = "snmalloc")]
    return "snmalloc";
    #[cfg(not(any(
        feature = "clmalloc",
        feature = "jemalloc",
        feature = "mimalloc",
        feature = "snmalloc"
    )))]
    return "system";
}
