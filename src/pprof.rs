//! Heap profiling via statistical sampling, producing pprof-compatible profiles.
//!
//! Sampling uses the Poisson per-byte model (same as jemalloc): a geometric
//! countdown is decremented by each allocation's size. When it crosses zero,
//! a stack trace is captured and associated with the allocated pointer in a
//! side table. On dealloc, sampled pointers are looked up and live counters
//! decremented.
//!
//! The profile can be dumped as a gzip'd pprof protobuf at any time via
//! `dump()`.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

use rand::rngs::SmallRng;
use rand::{Rng, RngExt, SeedableRng};

// -- Configuration -----------------------------------------------------------

/// Default sample interval in bytes (512 KiB, matching jemalloc's `lg_prof_sample=19`).
const DEFAULT_SAMPLE_INTERVAL: u64 = 512 * 1024;

/// Profiling configuration. Pass `Some(PprofConfig)` to activate profiling,
/// `None` to deactivate.
#[derive(Clone, Debug)]
pub struct PprofConfig {
    /// Mean bytes between samples (Poisson per-byte model).
    /// Must be > 0. Defaults to 512 KiB.
    pub sample_interval: u64,
}

impl Default for PprofConfig {
    fn default() -> Self {
        Self {
            sample_interval: DEFAULT_SAMPLE_INTERVAL,
        }
    }
}

// r[impl pprof.activate]
static PROF_ACTIVE: AtomicBool = AtomicBool::new(false);
// r[impl pprof.sample-interval]
static SAMPLE_INTERVAL: AtomicU64 = AtomicU64::new(DEFAULT_SAMPLE_INTERVAL);

/// Activate (`Some`) or deactivate (`None`) heap profiling.
///
/// # Panics
/// Panics if `config.sample_interval` is zero.
pub fn set_pprof_config(config: Option<PprofConfig>) {
    match config {
        Some(c) => {
            assert!(c.sample_interval > 0, "sample interval must be > 0");
            SAMPLE_INTERVAL.store(c.sample_interval, Ordering::Release);
            PROF_ACTIVE.store(true, Ordering::Release);
        }
        None => {
            PROF_ACTIVE.store(false, Ordering::Release);
        }
    }
}

/// Returns the current profiling configuration, or `None` if inactive.
pub fn pprof_config() -> Option<PprofConfig> {
    if PROF_ACTIVE.load(Ordering::Acquire) {
        Some(PprofConfig {
            sample_interval: SAMPLE_INTERVAL.load(Ordering::Acquire),
        })
    } else {
        None
    }
}

/// Geometric(1/R) sample: ceil(-R * ln(U)) where U is uniform (0,1).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn geometric(rng: &mut impl Rng, mean: u64) -> u64 {
    let u: f64 = rng.random();
    let u = if u <= 0.0 { f64::MIN_POSITIVE } else { u };
    let result = -(mean as f64) * u.ln();
    (result.ceil() as u64).max(1)
}

// -- Sampler (per-heap) ------------------------------------------------------

// r[impl pprof.geometric-sampling] r[impl pprof.fast-path-cost]
pub(crate) struct Sampler {
    countdown: i64,
    rng: SmallRng,
    interval: u64,
}

impl Sampler {
    pub fn new(seed: u64) -> Self {
        let mut rng = SmallRng::seed_from_u64(seed);
        let interval = SAMPLE_INTERVAL.load(Ordering::Relaxed);
        let countdown = geometric(&mut rng, interval).cast_signed();
        Self {
            countdown,
            rng,
            interval,
        }
    }

    /// Subtract `size` from the countdown. Returns `true` if a sample should
    /// be taken (countdown crossed zero).
    #[inline]
    #[allow(clippy::cast_possible_wrap)]
    pub fn check(&mut self, size: usize) -> bool {
        self.countdown -= size as i64;
        if self.countdown <= 0 {
            self.reset();
            true
        } else {
            false
        }
    }

    fn reset(&mut self) {
        self.countdown = geometric(&mut self.rng, self.interval).cast_signed();
    }
}

// -- Stack table (global, dedup) ---------------------------------------------

type StackId = u32;

struct StackCounters {
    alloc_count: u64,
    alloc_bytes: u64,
    live_count: i64,
    live_bytes: i64,
}

// r[impl pprof.backtrace-dedup]
struct StackTable {
    map: HashMap<Vec<usize>, StackId>,
    counters: Vec<StackCounters>,
}

impl StackTable {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            counters: Vec::new(),
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn intern(&mut self, frames: Vec<usize>) -> StackId {
        let next_id = self.counters.len() as StackId;
        *self.map.entry(frames).or_insert_with(|| {
            self.counters.push(StackCounters {
                alloc_count: 0,
                alloc_bytes: 0,
                live_count: 0,
                live_bytes: 0,
            });
            next_id
        })
    }
}

// -- Side table (sampled pointer tracking) -----------------------------------

// r[impl pprof.sample-record]
struct SampleRecord {
    stack_id: StackId,
    size: usize,
    class_idx: u8,
}

// r[impl pprof.live-tracking]
struct SideTable {
    map: HashMap<usize, SampleRecord>,
}

impl SideTable {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

// -- Global state ------------------------------------------------------------

struct PprofState {
    stacks: StackTable,
    side: SideTable,
}

impl PprofState {
    fn new() -> Self {
        Self {
            stacks: StackTable::new(),
            side: SideTable::new(),
        }
    }
}

static PPROF: Mutex<Option<PprofState>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut PprofState) -> R) -> R {
    let mut guard = PPROF
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = guard.get_or_insert_with(PprofState::new);
    f(state)
}

// -- Reentrancy guard --------------------------------------------------------

// r[impl pprof.no-reentrant-sample]
thread_local! {
    static IN_PPROF: Cell<bool> = const { Cell::new(false) };
}

struct ReentrancyGuard;

impl ReentrancyGuard {
    fn acquire() -> Option<Self> {
        if IN_PPROF.replace(true) {
            None
        } else {
            Some(Self)
        }
    }
}

impl Drop for ReentrancyGuard {
    fn drop(&mut self) {
        IN_PPROF.set(false);
    }
}

// -- Public instrumentation API ----------------------------------------------

// r[impl pprof.backtrace]
#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn record_sample(ptr: *mut u8, size: usize, class_idx: u8) {
    let Some(_guard) = ReentrancyGuard::acquire() else {
        return;
    };

    let mut frames = Vec::with_capacity(32);
    backtrace::trace(|frame| {
        frames.push(frame.ip() as usize);
        frames.len() < 128
    });

    let interval = SAMPLE_INTERVAL.load(Ordering::Relaxed);

    with_state(|state| {
        let stack_id = state.stacks.intern(frames);
        let counters = &mut state.stacks.counters[stack_id as usize];

        // r[impl pprof.unbiased-weight]
        let weight = unbiased_weight(size, interval);
        counters.alloc_count += weight;
        counters.alloc_bytes += weight * size as u64;
        counters.live_count += weight.cast_signed();
        counters.live_bytes += (weight * size as u64).cast_signed();

        state.side.map.insert(
            ptr as usize,
            SampleRecord {
                stack_id,
                size,
                class_idx,
            },
        );
    });
}

// r[impl pprof.free-decrement]
pub(crate) fn maybe_remove_sample(ptr: *mut u8) {
    let Some(_guard) = ReentrancyGuard::acquire() else {
        return;
    };

    with_state(|state| {
        if let Some(record) = state.side.map.remove(&(ptr as usize)) {
            let interval = SAMPLE_INTERVAL.load(Ordering::Relaxed);
            let weight = unbiased_weight(record.size, interval);
            let counters = &mut state.stacks.counters[record.stack_id as usize];
            counters.live_count -= weight.cast_signed();
            counters.live_bytes -= (weight * record.size as u64).cast_signed();
        }
    });
}

/// Unbiased weight: compensate for size-dependent sampling probability.
/// w = Z / (1 - e^{-Z/R}) where Z = alloc size, R = sample interval.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn unbiased_weight(size: usize, interval: u64) -> u64 {
    let z = size as f64;
    let r = interval as f64;
    let ratio = z / r;
    if ratio < 1e-6 {
        // For tiny allocations relative to interval, weight ≈ R/1 ≈ interval
        return interval;
    }
    let w = z / (1.0 - (-ratio).exp());
    (w / z).round().max(1.0) as u64
}

// -- Dump (pprof protobuf) ---------------------------------------------------

// r[impl pprof.dump-api] r[impl pprof.dump-format] r[impl pprof.sample-types]
/// # Errors
/// Returns an error if writing the gzip-compressed protobuf data fails.
pub fn dump(writer: &mut dyn Write) -> std::io::Result<()> {
    // Suppress sampling for allocations made during profile serialization.
    // Without this, build_profile's internal allocations (Vec, HashMap, String)
    // would try to call record_sample → with_state → deadlock on PPROF mutex.
    let _guard = ReentrancyGuard::acquire();

    let profile_bytes = with_state(|state| build_profile(state));
    let mut encoder = flate2::write::GzEncoder::new(writer, flate2::Compression::default());
    encoder.write_all(&profile_bytes)?;
    encoder.finish()?;
    Ok(())
}

fn build_profile(state: &PprofState) -> Vec<u8> {
    let mut strings = StringTable::new();
    strings.intern(""); // index 0 is always empty

    let st_alloc_objects = strings.intern("alloc_objects");
    let st_alloc_space = strings.intern("alloc_space");
    let st_inuse_objects = strings.intern("inuse_objects");
    let st_inuse_space = strings.intern("inuse_space");
    let st_count = strings.intern("count");
    let st_bytes = strings.intern("bytes");
    let st_size_class = strings.intern("size_class");
    let st_space = strings.intern("space");

    let mut locations: Vec<(u64, usize)> = Vec::new();
    let mut loc_map: HashMap<usize, u64> = HashMap::new();
    let mut functions: Vec<(u64, usize)> = Vec::new();
    let mut func_map: HashMap<usize, u64> = HashMap::new();

    // Build locations and functions from all stacks.
    #[allow(clippy::cast_possible_truncation)]
    for frames in state.stacks.map.keys() {
        for &ip in frames {
            if loc_map.contains_key(&ip) {
                continue;
            }
            let func_id = if let Some(&fid) = func_map.get(&ip) {
                fid
            } else {
                let fid = func_map.len() as u64 + 1;
                let name = resolve_symbol(ip, &mut strings);
                func_map.insert(ip, fid);
                functions.push((fid, name));
                fid
            };
            let loc_id = loc_map.len() as u64 + 1;
            loc_map.insert(ip, loc_id);
            locations.push((loc_id, func_id as usize));
        }
    }

    // Build protobuf.
    let mut profile = Vec::with_capacity(4096);

    // sample_type (field 1): [alloc_objects/count, alloc_space/bytes,
    //                          inuse_objects/count, inuse_space/bytes]
    for &(type_idx, unit_idx) in &[
        (st_alloc_objects, st_count),
        (st_alloc_space, st_bytes),
        (st_inuse_objects, st_count),
        (st_inuse_space, st_bytes),
    ] {
        let vt = encode_value_type(type_idx as u64, unit_idx as u64);
        encode_field(&mut profile, 1, &vt);
    }

    // samples (field 2)
    // r[impl pprof.class-label]
    for (frames, &stack_id) in &state.stacks.map {
        let counters = &state.stacks.counters[stack_id as usize];
        if counters.alloc_count == 0 && counters.live_count == 0 {
            continue;
        }

        let mut sample = Vec::new();
        // location_id (field 1, repeated uint64)
        for &ip in frames {
            let loc_id = loc_map[&ip];
            encode_varint_field(&mut sample, 1, loc_id);
        }
        // value (field 2, repeated int64)
        encode_varint_field(&mut sample, 2, counters.alloc_count);
        encode_varint_field(&mut sample, 2, counters.alloc_bytes);
        encode_varint_field(&mut sample, 2, counters.live_count.cast_unsigned());
        encode_varint_field(&mut sample, 2, counters.live_bytes.cast_unsigned());

        // label: size_class (field 3)
        if let Some(record) = state.side.map.values().find(|r| r.stack_id == stack_id) {
            let class_str = strings.intern(&format!("{}", record.class_idx));
            let label = encode_label(st_size_class as u64, class_str as u64);
            encode_field(&mut sample, 3, &label);
        }

        encode_field(&mut profile, 2, &sample);
    }

    // mapping (field 3) — empty, symbols resolved inline

    // location (field 4)
    for &(loc_id, func_id) in &locations {
        let mut loc = Vec::new();
        encode_varint_field(&mut loc, 1, loc_id); // id
        // line (field 4): function_id (field 1)
        let mut line = Vec::new();
        encode_varint_field(&mut line, 1, func_id as u64); // function_id
        encode_field(&mut loc, 4, &line);
        encode_field(&mut profile, 4, &loc);
    }

    // function (field 5)
    for &(func_id, name_idx) in &functions {
        let mut func = Vec::new();
        encode_varint_field(&mut func, 1, func_id); // id
        encode_varint_field(&mut func, 2, name_idx as u64); // name
        encode_varint_field(&mut func, 3, name_idx as u64); // system_name
        encode_field(&mut profile, 5, &func);
    }

    // string_table (field 6)
    for s in strings.table {
        encode_bytes_field(&mut profile, 6, s.as_bytes());
    }

    // time_nanos (field 9)
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    encode_varint_field(&mut profile, 9, nanos);

    // period_type (field 11): ("space", "bytes")
    let pt = encode_value_type(st_space as u64, st_bytes as u64);
    encode_field(&mut profile, 11, &pt);

    // period (field 12): sample interval in bytes
    let interval = SAMPLE_INTERVAL.load(Ordering::Relaxed);
    encode_varint_field(&mut profile, 12, interval);

    profile
}

fn resolve_symbol(ip: usize, strings: &mut StringTable) -> usize {
    let mut name = format!("0x{ip:x}");
    backtrace::resolve(ip as *mut core::ffi::c_void, |symbol| {
        if let Some(sym_name) = symbol.name() {
            name = sym_name.to_string();
        }
    });
    strings.intern(&name)
}

// -- Minimal string table for protobuf ---------------------------------------

struct StringTable {
    table: Vec<String>,
    map: HashMap<String, usize>,
}

impl StringTable {
    fn new() -> Self {
        Self {
            table: Vec::new(),
            map: HashMap::new(),
        }
    }

    fn intern(&mut self, s: &str) -> usize {
        if let Some(&idx) = self.map.get(s) {
            return idx;
        }
        let idx = self.table.len();
        self.table.push(s.to_string());
        self.map.insert(s.to_string(), idx);
        idx
    }
}

// -- Minimal protobuf encoder ------------------------------------------------

fn encode_varint(buf: &mut Vec<u8>, mut val: u64) {
    loop {
        let byte = (val & 0x7F) as u8;
        val >>= 7;
        if val == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn encode_varint_field(buf: &mut Vec<u8>, field: u32, val: u64) {
    encode_varint(buf, u64::from(field) << 3); // wire type 0 = varint
    encode_varint(buf, val);
}

fn encode_field(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    encode_varint(buf, (u64::from(field) << 3) | 2); // wire type 2 = length-delimited
    encode_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn encode_bytes_field(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    encode_field(buf, field, data);
}

fn encode_value_type(type_idx: u64, unit_idx: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_varint_field(&mut buf, 1, type_idx);
    encode_varint_field(&mut buf, 2, unit_idx);
    buf
}

fn encode_label(key: u64, str_val: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_varint_field(&mut buf, 1, key);
    encode_varint_field(&mut buf, 2, str_val);
    buf
}

// -- Inactive fast path ------------------------------------------------------

// r[impl pprof.inactive-cost]
/// Returns false quickly when profiling is off, avoiding any overhead.
#[inline]
pub(crate) fn should_sample() -> bool {
    PROF_ACTIVE.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pprof tests mutate global config via `set_pprof_config`.
    /// Serialize them so parallel test threads don't interfere.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_test() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // r[verify pprof.geometric-sampling] r[verify pprof.sample-interval]
    #[test]
    fn sampler_triggers_near_interval() {
        let _g = lock_test();
        set_pprof_config(Some(PprofConfig {
            sample_interval: 1024,
        }));
        let mut sampler = Sampler::new(42);
        let mut triggers = 0;
        let total_bytes = 1024 * 1000;
        let mut allocated = 0usize;
        while allocated < total_bytes {
            if sampler.check(64) {
                triggers += 1;
            }
            allocated += 64;
        }
        // Expected: ~total_bytes/interval = 1000 samples.
        // With randomness, allow wide tolerance.
        assert!(
            triggers > 500 && triggers < 2000,
            "expected ~1000 triggers, got {triggers}"
        );
    }

    // r[verify pprof.fast-path-cost]
    #[test]
    fn sampler_check_does_not_trigger_every_time() {
        let _g = lock_test();
        set_pprof_config(Some(PprofConfig::default()));
        let mut sampler = Sampler::new(123);
        let mut triggered = false;
        for _ in 0..10 {
            if sampler.check(64) {
                triggered = true;
            }
        }
        assert!(
            !triggered,
            "64-byte allocs should not trigger at 512K interval"
        );
    }

    // r[verify pprof.inactive-cost]
    #[test]
    fn should_sample_reflects_active_flag() {
        let _g = lock_test();
        set_pprof_config(None);
        assert!(
            !should_sample(),
            "should_sample must return false when inactive"
        );
        set_pprof_config(Some(PprofConfig::default()));
        assert!(
            should_sample(),
            "should_sample must return true when active"
        );
        set_pprof_config(None);
    }

    // r[verify pprof.live-tracking] r[verify pprof.activate] r[verify pprof.sample-record]
    // r[verify pprof.free-decrement] r[verify pprof.backtrace]
    #[test]
    fn side_table_tracks_live_samples() {
        let _g = lock_test();
        set_pprof_config(Some(PprofConfig { sample_interval: 1 }));

        let ptr = 0xDEAD_BEEF as *mut u8;
        record_sample(ptr, 128, 5);

        let stack_id = with_state(|state| {
            let record = state
                .side
                .map
                .get(&(ptr as usize))
                .expect("sampled pointer should be in side table");
            let live = state.stacks.counters[record.stack_id as usize].live_count;
            assert!(live > 0, "live count should be positive after sample");
            record.stack_id
        });

        maybe_remove_sample(ptr);

        with_state(|state| {
            assert!(
                !state.side.map.contains_key(&(ptr as usize)),
                "freed pointer should be removed from side table"
            );
            let live = state.stacks.counters[stack_id as usize].live_count;
            assert_eq!(live, 0, "live count should be 0 after free");
        });

        set_pprof_config(None);
    }

    // r[verify pprof.dump-api] r[verify pprof.dump-format]
    // r[verify pprof.sample-types] r[verify pprof.class-label]
    #[test]
    fn dump_produces_valid_pprof_protobuf() {
        use pprof::protos::Message;

        let _g = lock_test();
        set_pprof_config(Some(PprofConfig { sample_interval: 1 }));

        let ptr = 0xCAFE_0000 as *mut u8;
        record_sample(ptr, 256, 2);

        let mut buf = Vec::new();
        dump(&mut buf).unwrap();

        // Decompress.
        let mut decoder = flate2::read::GzDecoder::new(&buf[..]);
        let mut decompressed = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut decompressed).unwrap();

        // Decode as pprof Profile protobuf.
        let profile = pprof::protos::Profile::decode(decompressed.as_slice())
            .expect("dump output must be a valid pprof Profile protobuf");

        // string_table[0] must be "".
        assert_eq!(profile.string_table[0], "");

        // Four sample types: alloc_objects, alloc_space, inuse_objects, inuse_space.
        let type_names: Vec<&str> = profile
            .sample_type
            .iter()
            .map(|st| profile.string_table[st.ty as usize].as_str())
            .collect();
        assert_eq!(
            type_names,
            [
                "alloc_objects",
                "alloc_space",
                "inuse_objects",
                "inuse_space"
            ]
        );

        // At least one sample with 4 values.
        assert!(!profile.sample.is_empty(), "profile should have samples");
        for sample in &profile.sample {
            assert_eq!(sample.value.len(), 4, "each sample must have 4 values");
        }

        // Locations and functions should exist.
        assert!(
            !profile.location.is_empty(),
            "profile should have locations"
        );
        assert!(
            !profile.function.is_empty(),
            "profile should have functions"
        );

        // At least one sample should have a size_class label.
        let size_class_key = profile
            .string_table
            .iter()
            .position(|s| s == "size_class")
            .expect("string table must contain 'size_class'") as i64;
        let has_class_label = profile
            .sample
            .iter()
            .any(|s| s.label.iter().any(|l| l.key == size_class_key));
        assert!(
            has_class_label,
            "at least one sample should have a size_class label"
        );

        // time_nanos should be a recent timestamp.
        assert!(profile.time_nanos > 0, "time_nanos should be set");

        // period_type should be ("space", "bytes").
        let pt = profile
            .period_type
            .as_ref()
            .expect("period_type should be set");
        assert_eq!(profile.string_table[pt.ty as usize], "space");
        assert_eq!(profile.string_table[pt.unit as usize], "bytes");

        // period should match our sample interval.
        assert_eq!(profile.period, 1, "period should match sample interval");

        maybe_remove_sample(ptr);
        set_pprof_config(None);
    }

    // r[verify pprof.backtrace-dedup]
    #[test]
    fn duplicate_stacks_share_id() {
        let _g = lock_test();
        set_pprof_config(Some(PprofConfig { sample_interval: 1 }));

        let addrs = [0xAAAA_0001usize, 0xAAAA_0002];
        for &addr in &addrs {
            record_sample(addr as *mut u8, 64, 0);
        }

        with_state(|state| {
            let r1 = state.side.map.get(&addrs[0]).unwrap();
            let r2 = state.side.map.get(&addrs[1]).unwrap();
            assert_eq!(
                r1.stack_id, r2.stack_id,
                "samples from same call site must share a deduped stack ID"
            );
        });

        for &addr in &addrs {
            maybe_remove_sample(addr as *mut u8);
        }
        set_pprof_config(None);
    }

    // r[verify pprof.unbiased-weight]
    #[test]
    fn unbiased_weight_converges() {
        // For size == interval, weight should be ~1/(1-e^{-1}) ≈ 1.58 → rounds to 2.
        let w = unbiased_weight(512 * 1024, 512 * 1024);
        assert!((1..=3).contains(&w), "expected ~2, got {w}");

        // For very small size relative to interval, weight ≈ interval/size.
        let w_small = unbiased_weight(64, 512 * 1024);
        let expected = 512 * 1024 / 64;
        assert!(
            w_small > expected / 2 && w_small < expected * 2,
            "expected ~{expected}, got {w_small}"
        );
    }
}
