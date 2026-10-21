//! Diagnostic-only observers. No domain decisions depend on these counters.
//! Timing is inclusive, sampled elapsed work; it is NOT process critical-path time.
use serde::Serialize;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub const N: usize = 45;
pub const LABELS: [&str; N] = [
    "parse.response.message",
    "parse.response.custom_tool_call",
    "parse.response.custom_tool_call_output",
    "parse.response.function_call",
    "parse.response.function_call_output",
    "parse.event.item_completed",
    "parse.event.token_count",
    "parse.event.other",
    "parse.session_meta",
    "parse.turn_context",
    "parse.other_canonical",
    "parse.noncanonical",
    "reduce.response.message",
    "reduce.response.custom_tool_call",
    "reduce.response.custom_tool_call_output",
    "reduce.response.function_call",
    "reduce.response.function_call_output",
    "reduce.event.item_completed",
    "reduce.event.token_count",
    "reduce.event.other",
    "reduce.session_meta",
    "reduce.turn_context",
    "reduce.other_canonical",
    "reduce.noncanonical",
    "snapshot.read",
    "integration.total",
    "integration.root_clone",
    "integration.child_clone",
    "integration.drop_originals",
    "handoff.root",
    "handoff.subagent",
    "handoff.payload_clone",
    "activity.sort",
    "serialize",
    "temp_io",
    "enrichment",
    "finalization",
    "scan.result_collection",
    "root.ordering",
    "unscoped",
    "attachment.classifier",
    "goal.classifier",
    "javascript.command_data",
    "javascript.code_mask",
    "javascript.exec_normalize",
];
const EVENT_LABELS: [&str; 7] = [
    "prefix_fallback",
    "prefix_drop",
    "direct_success",
    "outer_suffix_fallback",
    "envelope_fallback",
    "retained",
    "parse_error",
];
static MODE: AtomicU8 = AtomicU8::new(0);
static SAMPLE_EVERY: AtomicU64 = AtomicU64::new(16);
static REPORT: OnceLock<PathBuf> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();
static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
const NONE: usize = usize::MAX;

#[derive(Clone, Copy, Default, Serialize)]
pub struct Stats {
    pub calls: u64,
    pub input_bytes: u64,
    pub samples: u64,
    pub sample_wall_ns: u64,
    pub alloc_calls: u64,
    pub alloc_requested_bytes: u64,
    pub realloc_calls: u64,
    pub realloc_new_requested_bytes: u64,
    pub dealloc_calls: u64,
    pub dealloc_requested_bytes: u64,
}
impl Stats {
    fn diff(self, b: Self) -> Self {
        macro_rules! sub {($($f:ident),*) => {Self {$($f: self.$f.saturating_sub(b.$f)),*}}}
        sub!(
            calls,
            input_bytes,
            samples,
            sample_wall_ns,
            alloc_calls,
            alloc_requested_bytes,
            realloc_calls,
            realloc_new_requested_bytes,
            dealloc_calls,
            dealloc_requested_bytes
        )
    }
}
const ZERO: Stats = Stats {
    calls: 0,
    input_bytes: 0,
    samples: 0,
    sample_wall_ns: 0,
    alloc_calls: 0,
    alloc_requested_bytes: 0,
    realloc_calls: 0,
    realloc_new_requested_bytes: 0,
    dealloc_calls: 0,
    dealloc_requested_bytes: 0,
};
thread_local! {
    static CURRENT: Cell<usize> = const {Cell::new(39)};
    static DATA: RefCell<[Stats; N]> = const {RefCell::new([ZERO; N])};
    static EVENTS: RefCell<[u64; 7]> = const {RefCell::new([0; 7])};
}

fn suspended<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(usize);
    impl Drop for Reset {
        fn drop(&mut self) {
            CURRENT.with(|c| c.set(self.0));
        }
    }
    let old = CURRENT.with(|c| c.replace(NONE));
    let _r = Reset(old);
    f()
}
fn record_alloc(kind: u8, size: usize) {
    if MODE.load(Ordering::Relaxed) != 2 {
        return;
    }
    let _ = CURRENT.try_with(|current| {
        let idx = current.get();
        if idx >= N {
            return;
        }
        let _ = DATA.try_with(|data| {
            if let Ok(mut all) = data.try_borrow_mut() {
                let s = &mut all[idx];
                match kind {
                    0 => {
                        s.alloc_calls = s.alloc_calls.saturating_add(1);
                        s.alloc_requested_bytes =
                            s.alloc_requested_bytes.saturating_add(size as u64);
                    }
                    1 => {
                        s.realloc_calls = s.realloc_calls.saturating_add(1);
                        s.realloc_new_requested_bytes =
                            s.realloc_new_requested_bytes.saturating_add(size as u64);
                    }
                    _ => {
                        s.dealloc_calls = s.dealloc_calls.saturating_add(1);
                        s.dealloc_requested_bytes =
                            s.dealloc_requested_bytes.saturating_add(size as u64);
                    }
                }
            }
        });
    });
}
/// Forward every operation to System unchanged. Counter paths cannot allocate or panic.
struct ObservedSystem;
// SAFETY: layout, pointer ownership, failure behavior, and alignment are forwarded unchanged.
unsafe impl GlobalAlloc for ObservedSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            record_alloc(0, layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            record_alloc(0, layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        record_alloc(2, layout.size());
        unsafe { System.dealloc(p, layout) }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new_size) };
        if !q.is_null() {
            record_alloc(1, new_size);
        }
        q
    }
}
#[global_allocator]
static ALLOCATOR: ObservedSystem = ObservedSystem;

pub fn enabled() -> bool {
    MODE.load(Ordering::Relaxed) != 0
}
pub fn initialize() -> anyhow::Result<()> {
    let Some(path) = std::env::var_os("CODEX_STUDY_REPORT") else {
        return Ok(());
    };
    let every = std::env::var("CODEX_STUDY_SAMPLE_EVERY")
        .unwrap_or_else(|_| "16".into())
        .parse::<u64>()?;
    anyhow::ensure!(
        (1..=1024).contains(&every),
        "sample interval must be 1..1024"
    );
    REPORT
        .set(PathBuf::from(path))
        .map_err(|_| anyhow::anyhow!("study initialized twice"))?;
    START.set(Instant::now()).ok();
    STORE.get_or_init(|| Mutex::new(Store::default()));
    SAMPLE_EVERY.store(every, Ordering::Relaxed);
    MODE.store(
        if std::env::var("CODEX_STUDY_ALLOCATIONS").as_deref() == Ok("1") {
            2
        } else {
            1
        },
        Ordering::Relaxed,
    );
    Ok(())
}

pub struct Scope {
    id: usize,
    old: usize,
    start: Option<Instant>,
    active: bool,
}
pub fn scope(id: usize, bytes: usize) -> Scope {
    if !enabled() || id >= N {
        return Scope {
            id,
            old: NONE,
            start: None,
            active: false,
        };
    }
    let samples = if id < 24 {
        SAMPLE_EVERY.load(Ordering::Relaxed)
    } else {
        1
    };
    let sampled = DATA.with(|d| {
        let mut all = d.borrow_mut();
        let s = &mut all[id];
        s.calls += 1;
        s.input_bytes += bytes as u64;
        s.calls % samples == 1 % samples
    });
    let old = CURRENT.with(|c| c.replace(id));
    Scope {
        id,
        old,
        start: sampled.then(Instant::now),
        active: true,
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let elapsed = self
            .start
            .map(|s| s.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        CURRENT.with(|c| c.set(self.old));
        if let Some(ns) = elapsed {
            DATA.with(|d| {
                let s = &mut d.borrow_mut()[self.id];
                s.samples += 1;
                s.sample_wall_ns += ns;
            });
        }
    }
}
pub fn event(id: usize) {
    if enabled() && id < 7 {
        EVENTS.with(|e| e.borrow_mut()[id] += 1);
    }
}
pub fn family(record: Option<&str>, payload: Option<&str>) -> usize {
    match (record, payload) {
        (Some("response_item"), Some("message")) => 0,
        (Some("response_item"), Some("custom_tool_call")) => 1,
        (Some("response_item"), Some("custom_tool_call_output")) => 2,
        (Some("response_item"), Some("function_call")) => 3,
        (Some("response_item"), Some("function_call_output")) => 4,
        (Some("event_msg"), Some("item_completed")) => 5,
        (Some("event_msg"), Some("token_count")) => 6,
        (Some("event_msg"), _) => 7,
        (Some("session_meta"), _) => 8,
        (Some("turn_context"), _) => 9,
        (Some(_), _) => 10,
        _ => 11,
    }
}

#[derive(Serialize)]
struct NamedStats {
    name: &'static str,
    stats: Stats,
}
fn named(a: &[Stats; N]) -> Vec<NamedStats> {
    a.iter()
        .enumerate()
        .filter(|(_, s)| s.calls + s.alloc_calls + s.realloc_calls + s.dealloc_calls > 0)
        .map(|(i, s)| NamedStats {
            name: LABELS[i],
            stats: *s,
        })
        .collect()
}
#[derive(Serialize)]
struct ThreadRow {
    thread: String,
    worker: Option<usize>,
    scopes: Vec<NamedStats>,
    events: Vec<(&'static str, u64)>,
}
#[derive(Serialize)]
struct TaskRow {
    kind: &'static str,
    path: String,
    thread: String,
    worker: Option<usize>,
    start_ns: u64,
    wall_ns: u64,
    cpu_ns: Option<u64>,
    matched: Option<bool>,
    scopes: Vec<NamedStats>,
    events: Vec<(&'static str, u64)>,
}
#[derive(Default, Serialize)]
struct Store {
    threads: Vec<ThreadRow>,
    tasks: Vec<TaskRow>,
}
fn stamp() -> u64 {
    START
        .get()
        .map(|t| t.elapsed().as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}
fn events_named(e: &[u64; 7]) -> Vec<(&'static str, u64)> {
    EVENT_LABELS
        .iter()
        .copied()
        .zip(e.iter().copied())
        .collect()
}
pub struct Task {
    path: String,
    kind: &'static str,
    start_ns: u64,
    clock: Instant,
    cpu: Option<u64>,
    before: [Stats; N],
    before_events: [u64; 7],
}
pub fn task(path: &Path, kind: &'static str) -> Option<Task> {
    if !enabled() {
        return None;
    }
    Some(suspended(|| Task {
        path: path.to_string_lossy().into_owned(),
        kind,
        start_ns: stamp(),
        clock: Instant::now(),
        cpu: thread_cpu_ns(),
        before: DATA.with(|d| *d.borrow()),
        before_events: EVENTS.with(|e| *e.borrow()),
    }))
}
impl Task {
    pub fn finish(self, matched: Option<bool>) {
        let wall = self.clock.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let cpu = thread_cpu_ns()
            .zip(self.cpu)
            .map(|(a, b)| a.saturating_sub(b));
        suspended(|| {
            let now = DATA.with(|d| *d.borrow());
            let e = EVENTS.with(|v| *v.borrow());
            let delta = std::array::from_fn(|i| now[i].diff(self.before[i]));
            let de = std::array::from_fn(|i| e[i].saturating_sub(self.before_events[i]));
            let row = TaskRow {
                kind: self.kind,
                path: self.path,
                thread: format!("{:?}", std::thread::current().id()),
                worker: rayon::current_thread_index(),
                start_ns: self.start_ns,
                wall_ns: wall,
                cpu_ns: cpu,
                matched,
                scopes: named(&delta),
                events: events_named(&de),
            };
            STORE
                .get()
                .unwrap()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .tasks
                .push(row);
        });
    }
}
pub fn drain_thread() {
    if !enabled() {
        return;
    }
    suspended(|| {
        let a = DATA.with(|d| d.replace([ZERO; N]));
        let e = EVENTS.with(|d| d.replace([0; 7]));
        if a.iter()
            .all(|s| s.calls + s.alloc_calls + s.realloc_calls + s.dealloc_calls == 0)
            && e.iter().all(|&n| n == 0)
        {
            return;
        }
        let row = ThreadRow {
            thread: format!("{:?}", std::thread::current().id()),
            worker: rayon::current_thread_index(),
            scopes: named(&a),
            events: events_named(&e),
        };
        STORE
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .threads
            .push(row);
    });
}
pub fn finish(hotspots: serde_json::Value, status: &str) -> anyhow::Result<()> {
    let Some(path) = REPORT.get() else {
        return Ok(());
    };
    drain_thread();
    let mode = MODE.swap(0, Ordering::Relaxed);
    let store = STORE
        .get()
        .unwrap()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let output = serde_json::json!({"study_schema":"codex-integrated-study-1","status":status,"profile_mode":if mode==2{"allocation+timing"}else{"timing"},"sample_every":SAMPLE_EVERY.load(Ordering::Relaxed),"timing_semantics":"sample_wall_ns is inclusive elapsed work across overlapping scopes and threads; never sum parent/child or equate it to process wall; high-frequency families sampled, other scopes exhaustive","allocation_semantics":"observed successful global System allocator calls in this instrumented binary; exclusive innermost scope attribution; requested cumulative bytes, not live/peak memory, not physical copied bytes, and not source-level allocation guarantees","task_cpu_semantics":"GetThreadTimes on Windows; clock_gettime(CLOCK_THREAD_CPUTIME_ID) on Linux; nullable; Windows reported units are not guaranteed measurement precision","unavailable_metrics":["cache misses","memory bandwidth","per-function CPU samples","Windows context-switch counts","hard-fault-only counts","lock contention attribution"],"records":&*store,"legacy_session_hotspots":hotspots,"process_resources":process_resources()});
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(&output)?)?;
    Ok(())
}

#[cfg(any(windows, feature = "study-windows-api-typecheck"))]
mod platform {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct Ft {
        lo: u32,
        hi: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Memory {
        cb: u32,
        faults: u32,
        peak_ws: usize,
        ws: usize,
        peak_paged: usize,
        paged: usize,
        peak_nonpaged: usize,
        nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Io {
        read_ops: u64,
        write_ops: u64,
        other_ops: u64,
        read_bytes: u64,
        write_bytes: u64,
        other_bytes: u64,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut c_void;
        fn GetCurrentProcess() -> *mut c_void;
        fn GetThreadTimes(h: *mut c_void, c: *mut Ft, e: *mut Ft, k: *mut Ft, u: *mut Ft) -> i32;
        fn GetProcessTimes(h: *mut c_void, c: *mut Ft, e: *mut Ft, k: *mut Ft, u: *mut Ft) -> i32;
        fn K32GetProcessMemoryInfo(h: *mut c_void, m: *mut Memory, cb: u32) -> i32;
        fn GetProcessIoCounters(h: *mut c_void, io: *mut Io) -> i32;
    }
    fn ft(f: &Ft) -> u64 {
        ((f.hi as u64) << 32) | (f.lo as u64)
    }
    pub fn thread_cpu_ns() -> Option<u64> {
        let (mut c, mut e, mut k, mut u) =
            (Ft::default(), Ft::default(), Ft::default(), Ft::default());
        // SAFETY: live output buffers and the documented current-thread pseudo handle.
        (unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) } != 0)
            .then(|| (ft(&k) + ft(&u)) * 100)
    }
    pub fn resources() -> serde_json::Value {
        let (mut c, mut e, mut k, mut u) =
            (Ft::default(), Ft::default(), Ft::default(), Ft::default());
        let mut m = Memory::default();
        m.cb = std::mem::size_of::<Memory>() as u32;
        let mut io = Io::default();
        // SAFETY: each buffer has the documented layout/length; pseudo handle needs no CloseHandle.
        let memory_size = m.cb;
        let (t, mem, i) = unsafe {
            let h = GetCurrentProcess();
            (
                GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0,
                K32GetProcessMemoryInfo(h, &mut m, memory_size) != 0,
                GetProcessIoCounters(h, &mut io) != 0,
            )
        };
        serde_json::json!({"cpu_ns":t.then(||(ft(&k)+ft(&u))*100),"peak_working_set_bytes":mem.then_some(m.peak_ws),"working_set_bytes":mem.then_some(m.ws),"page_faults_total_not_hard_only":mem.then_some(m.faults),"io_read_transfer_bytes_not_physical_disk":i.then_some(io.read_bytes),"io_write_transfer_bytes":i.then_some(io.write_bytes)})
    }
}
#[cfg(all(target_os = "linux", not(feature = "study-windows-api-typecheck")))]
mod platform {
    use std::ffi::{c_int, c_long};
    #[repr(C)]
    struct Ts {
        sec: c_long,
        nsec: c_long,
    }
    unsafe extern "C" {
        fn clock_gettime(clock: c_int, t: *mut Ts) -> c_int;
    }
    pub fn thread_cpu_ns() -> Option<u64> {
        let mut t = Ts { sec: 0, nsec: 0 };
        // SAFETY: Linux clock id 3 is CLOCK_THREAD_CPUTIME_ID and t is a writable timespec.
        (unsafe { clock_gettime(3, &mut t) } == 0)
            .then(|| (t.sec as u64) * 1_000_000_000 + t.nsec as u64)
    }
    pub fn resources() -> serde_json::Value {
        serde_json::json!({"proc_self_status":std::fs::read_to_string("/proc/self/status").ok(),"proc_self_io":std::fs::read_to_string("/proc/self/io").ok()})
    }
}
#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    pub fn thread_cpu_ns() -> Option<u64> {
        None
    }
    pub fn resources() -> serde_json::Value {
        serde_json::Value::Null
    }
}
fn thread_cpu_ns() -> Option<u64> {
    platform::thread_cpu_ns()
}
fn process_resources() -> serde_json::Value {
    platform::resources()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn family_is_total_and_bounded() {
        for r in [None, Some("response_item"), Some("event_msg"), Some("x")] {
            for p in [None, Some("item_completed"), Some("function_call")] {
                assert!(family(r, p) < 12);
            }
        }
    }
    #[test]
    fn diff_saturates() {
        assert_eq!(ZERO.diff(Stats { calls: 1, ..ZERO }).calls, 0);
    }
    #[test]
    fn scope_off_leaves_output_untouched() {
        let _g = scope(0, 19);
        assert_eq!(serde_json::to_string(&vec![1, 2, 3]).unwrap(), "[1,2,3]");
    }
    #[test]
    fn cpu_counter_available() {
        #[cfg(any(windows, target_os = "linux"))]
        assert!(thread_cpu_ns().is_some());
    }
}
