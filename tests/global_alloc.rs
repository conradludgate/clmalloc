//! Integration tests exercising clmalloc as `#[global_allocator]`.

use clmalloc::ClMalloc;

#[global_allocator]
static ALLOC: ClMalloc = ClMalloc::new();

// r[verify alloc.layout] r[verify alloc.size-class-dispatch] r[verify alloc.thread-local]
#[test]
fn vec_alloc_dealloc() {
    let mut v: Vec<u64> = Vec::new();
    for i in 0..1024 {
        v.push(i);
    }
    assert_eq!(v.len(), 1024);
    assert_eq!(v[512], 512);
}

// r[verify alloc.zst]
#[test]
fn zst_alloc() {
    let v: Vec<()> = vec![(); 100];
    assert_eq!(v.len(), 100);

    let b = Box::new(());
    drop(b);
}

// r[verify alloc.layout]
#[test]
fn large_allocation() {
    let layout = std::alloc::Layout::from_size_align(1 << 20, 4096).unwrap();
    let ptr = unsafe { std::alloc::alloc(layout) };
    assert!(!ptr.is_null());
    assert_eq!(ptr as usize % 4096, 0);
    unsafe { std::alloc::dealloc(ptr, layout) };
}

// r[verify dealloc.remote-path] r[verify alloc.thread-local]
#[test]
fn cross_thread_dealloc() {
    let v: Vec<u8> = vec![42; 256];
    let ptr = v.as_ptr() as usize;

    std::thread::spawn(move || {
        drop(v);
    })
    .join()
    .unwrap();

    let _ = ptr;
}

// r[verify alloc.tls-pthread-cleanup] r[verify alloc.tls-no-destructor]
#[test]
fn thread_spawn_exit_cleanup() {
    let handles: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                let mut vecs: Vec<Vec<u8>> = Vec::new();
                for size in [8, 64, 256, 1024, 4096] {
                    vecs.push(vec![0xAB; size]);
                }
                drop(vecs);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

// r[verify alloc.null-on-failure]
#[test]
fn null_on_impossible_layout() {
    let layout = std::alloc::Layout::from_size_align(usize::MAX / 2, 1).unwrap();
    let ptr = unsafe { std::alloc::alloc(layout) };
    assert!(ptr.is_null());
}

// r[verify dealloc.layout-trusted] r[verify dealloc.local-fast-path]
#[test]
fn alloc_dealloc_many_sizes() {
    for size in [
        8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
    ] {
        let layout = std::alloc::Layout::from_size_align(size, 1).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null(), "alloc failed for size {size}");
        assert_eq!(ptr as usize % layout.align(), 0);
        unsafe { std::alloc::dealloc(ptr, layout) };
    }
}

// r[verify alloc.thread-local]
#[test]
fn concurrent_alloc_dealloc() {
    let handles: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                let mut ptrs = Vec::new();
                for _ in 0..1000 {
                    let layout = std::alloc::Layout::from_size_align(64, 1).unwrap();
                    let ptr = unsafe { std::alloc::alloc(layout) };
                    assert!(!ptr.is_null());
                    ptrs.push((ptr, layout));
                }
                for (ptr, layout) in ptrs {
                    unsafe { std::alloc::dealloc(ptr, layout) };
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}
