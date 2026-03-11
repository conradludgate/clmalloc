/// Portable helper to read the process's current resident set size.
///
/// Returns RSS in bytes, or 0 if unavailable.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
pub fn get_rss() -> u64 {
    use std::mem::MaybeUninit;
    // SAFETY: mach_task_self_ is always valid; info struct is fully initialized by the kernel.
    unsafe {
        let mut info = MaybeUninit::<libc::mach_task_basic_info_data_t>::uninit();
        let mut count = (std::mem::size_of::<libc::mach_task_basic_info_data_t>()
            / std::mem::size_of::<libc::natural_t>())
            as libc::mach_msg_type_number_t;
        let kr = libc::task_info(
            libc::mach_task_self_,
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        );
        if kr == libc::KERN_SUCCESS {
            info.assume_init().resident_size
        } else {
            0
        }
    }
}

#[cfg(target_os = "linux")]
pub fn get_rss() -> u64 {
    if let Ok(contents) = std::fs::read_to_string("/proc/self/statm") {
        let resident_pages: u64 = contents
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size > 0 {
            return resident_pages * page_size as u64;
        }
    }
    0
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn get_rss() -> u64 {
    0
}
