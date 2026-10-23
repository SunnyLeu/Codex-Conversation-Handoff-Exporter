#![deny(unsafe_op_in_unsafe_fn)]

// The presentation boundary keeps the old engineering log unchanged in
// developer mode and captures it instead of leaking it into product screens.
macro_rules! println {
    () => { crate::console_ui::legacy_line(false, format_args!("")) };
    ($($arg:tt)*) => { crate::console_ui::legacy_line(false, format_args!($($arg)*)) };
}
macro_rules! eprintln {
    () => { crate::console_ui::legacy_line(true, format_args!("")) };
    ($($arg:tt)*) => { crate::console_ui::legacy_line(true, format_args!($($arg)*)) };
}
mod cli;
mod codex_paths;
mod console_ui;
mod destination;
mod platform;
mod product;
mod progress;
mod sidebar_targets;
mod source_guard;
mod sqlite_threads;

#[cfg(feature = "integrated-study")]
mod integrated_study;

mod crlf_formatter;
mod jsonl_segments;
mod project_membership;
mod rollout_codec;
mod rollout_continuations;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Datelike, FixedOffset, TimeZone, Timelike, Utc};
use clap::builder::TypedValueParser;
use clap::{Parser, ValueEnum};
use indexmap::IndexMap;
use memchr::memchr_iter;
use rayon::prelude::*;
use regex::{Regex, bytes::Regex as BytesRegex};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use sonic_rs::LazyValue;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::{
    SHCNE_UPDATEDIR, SHCNF_FLUSHNOWAIT, SHCNF_PATHW, SHChangeNotify,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(test)]
const TEST_ATTACHMENT_ROOT: &str = r"C:\Users\ExampleUser\.codex\attachments";
const ATTACHMENT_MIRROR_ENV: &str = "CODEX_EXPORT_BENCHMARK_ATTACHMENT_MIRROR";
#[cfg(test)]
const TEST_TARGET_CWD: &str = r"D:\Synthetic\Project";
#[cfg(test)]
const TEST_OUTPUT_DIRECTORY: &str = r"D:\Synthetic\Exports";
const DEFAULT_WORKERS: usize = 130;
const DEFAULT_WRITE_LANES: usize = 4;
const DEFAULT_CLEANUP_LANES: usize = 2;
const DEFAULT_FINALIZE_LANES: usize = 1;
const DEFAULT_FUSED_REDUCER: bool = true;
const DEFAULT_DIRECT_CANONICAL_INGESTION: bool = true;
const DEFAULT_SHELL_NOTIFY_PREWARM: bool = true;
const MIN_WORKERS: usize = 1;
const MAX_WORKERS: usize = 130;
const MAX_EXPORT_LANES: usize = 32;
const ACTIVITY_DETAIL: &str = "Compact";
const MAX_TS: i64 = i64::MAX;

type Obj = Map<String, Value>;
type Timestamp = i64; // UTC microseconds since Unix epoch.
type Records = Vec<(usize, Value)>;

macro_rules! obj {
    ($($key:expr => $value:expr),* $(,)?) => {{
        let mut map = Obj::new();
        $(map.insert($key.to_string(), $value);)*
        map
    }};
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum SerializerMode {
    Orjson2,
    Compact,
}

#[derive(Parser, Debug)]
#[command(
    name = "Codex-Conversation-Handoff-Exporter",
    version = VERSION,
    about = "Native Rust Codex rollout JSONL to handoff JSON exporter"
)]
struct Args {
    /// Use the legacy engineering CLI rather than the interactive product UI.
    #[arg(long)]
    developer_mode: bool,

    /// Select one independent sidebar conversation by its canonical Thread ID.
    #[arg(long, conflicts_with_all = ["project_id", "developer_mode"])]
    thread_id: Option<String>,

    /// Explicitly skip confirmation; requires both a source and destination.
    #[arg(long, conflicts_with = "developer_mode")]
    yes: bool,

    #[arg(skip)]
    product_mode: bool,
    #[arg(skip)]
    expected_state: Option<String>,

    #[arg(long, default_value_os_t = codex_paths::default_path("sessions"))]
    sessions_root: PathBuf,

    #[arg(long, default_value_os_t = codex_paths::default_path("session_index.jsonl"))]
    session_index: PathBuf,

    #[arg(long, default_value_os_t = codex_paths::default_path("attachments"))]
    attachment_root: PathBuf,

    #[arg(long, default_value = "", conflicts_with = "project_id")]
    target_cwd: String,

    /// Select a registered Codex project by its unique Legacy or App Server ID.
    /// Never resolves a project by name or guesses from a shared worktree path.
    #[arg(long, conflicts_with = "legacy_cwd_only")]
    project_id: Option<String>,

    /// Read-only: show all registered project IDs, names, and root paths.
    #[arg(long, conflicts_with = "project_id")]
    list_projects: bool,

    /// Read-only Codex project registry override; defaults to the state file
    /// in the same .codex folder as the sessions directory.
    #[arg(long, conflicts_with = "legacy_cwd_only")]
    project_state: Option<PathBuf>,

    /// Explicit compatibility mode for historical fixtures and benchmarks.
    /// Never silently fall back to cwd-only when the project registry is absent.
    #[arg(long)]
    legacy_cwd_only: bool,

    /// Disable console wait on non-clean results in unattended invocations.
    #[arg(long)]
    no_pause: bool,

    /// Full-process production wall-time warning threshold, in seconds.
    #[arg(long, default_value_t = 3.0)]
    max_total_seconds: f64,

    #[arg(long, default_value = "", value_parser = clap::builder::OsStringValueParser::new().map(PathBuf::from))]
    output_directory: PathBuf,

    #[arg(long, default_value_t = DEFAULT_WORKERS, value_parser = parse_worker_count)]
    workers: usize,

    /// Compatibility umbrella for export concurrency. When omitted, split production defaults are used.
    /// 0 means unbounded Rayon scheduling.
    #[arg(long, value_parser = parse_export_lane_count)]
    export_lanes: Option<usize>,

    /// Override temp-file write concurrency independently from finalization. 0 means unbounded.
    #[arg(long, value_parser = parse_export_lane_count)]
    write_lanes: Option<usize>,

    /// Override final-rename concurrency independently from temp writes. 0 means unbounded.
    #[arg(long, value_parser = parse_export_lane_count)]
    finalize_lanes: Option<usize>,

    /// Override obsolete-output deletion concurrency independently from final rename. 0 means unbounded.
    #[arg(long, value_parser = parse_export_lane_count)]
    cleanup_lanes: Option<usize>,

    /// Keep planned-current final files out of stale cleanup and delete each immediately before its temp rename.
    #[arg(long)]
    defer_planned_final_delete: bool,

    /// Do not pre-delete planned final paths; let fs::rename replace the existing file directly.
    #[arg(long)]
    replace_existing_final: bool,

    /// Build and serialize each handoff in the same Rayon task, then perform file I/O separately.
    #[arg(long)]
    pre_serialize_candidates: bool,

    /// Write temporary handoffs in a unique sibling staging directory on the same volume.
    #[arg(long)]
    isolated_staging: bool,

    /// Build, serialize, and temp-write each handoff in one Rayon pipeline task.
    #[arg(long)]
    pipeline_build_write: bool,

    /// Pre-warm the Windows Shell change-notification path on a background thread.
    #[arg(long)]
    shell_notify_prewarm: bool,

    /// Begin Shell notification delivery and return as soon as delivery has started.
    #[arg(long)]
    shell_notify_flush_nowait: bool,

    /// Use memchr-based CRLF scanning plus a reserved output buffer for pretty JSON serialization.
    #[arg(long)]
    fast_crlf_serializer: bool,

    /// Diagnostic A/B: retain scalar inclusive-LF splitting of the captured snapshot.
    #[arg(long)]
    legacy_line_splitter: bool,

    /// Diagnostic A/B: use the previous per-fragment CRLF conversion writer.
    #[arg(long)]
    legacy_crlf_writer: bool,

    #[arg(long, value_enum, default_value_t = SerializerMode::Orjson2)]
    serializer: SerializerMode,

    #[arg(long)]
    compare_reference: Option<PathBuf>,

    #[arg(long)]
    single_session: Option<PathBuf>,

    #[arg(long)]
    profile_phases: bool,

    /// Collect low-overhead per-session hotspot phase timings.
    #[arg(long)]
    hotspot_profile: bool,

    /// Number of slowest sessions shown by --hotspot-profile.
    #[arg(long, default_value_t = 20)]
    hotspot_top: usize,

    #[arg(long)]
    no_write: bool,

    #[arg(long, default_value_t = 0)]
    benchmark_runs: usize,

    /// Compatibility alias: the fused prefix reducer is the production default.
    #[arg(long)]
    fused_reducer: bool,

    /// Diagnostic fallback to the legacy retained-record reducer.
    #[arg(long)]
    legacy_reducer: bool,

    /// Use the legacy full-sonic per-line ingestion path for diagnostic A/B only.
    #[arg(long)]
    legacy_ingestion: bool,

    /// Compatibility flag for the production optimized prefix-routed ingestion path.
    #[arg(long)]
    prefix_ingestion: bool,

    /// Compatibility flag: Direct canonical ingestion is the production default.
    #[arg(long, conflicts_with = "legacy_canonical_ingestion")]
    direct_canonical_ingestion: bool,

    /// Diagnostic fallback to the v1.0.31 canonical outer-envelope parse path.
    #[arg(long, conflicts_with = "direct_canonical_ingestion")]
    legacy_canonical_ingestion: bool,

    /// Force the clone-heavy PowerShell-compatible activity sort for diagnostic A/B only.
    #[arg(long)]
    legacy_activity_sort: bool,

    /// Reproduce the r6 source-quality scan that inspects every line and allocates a lowercase copy.
    #[arg(long)]
    legacy_source_quality_scan: bool,

    /// Use the lightweight reference/index activity sort (production default in r7).
    #[arg(long)]
    lightweight_activity_sort: bool,

    /// Recompile activity-classification regexes on every match for diagnostic A/B only.
    #[arg(long)]
    legacy_activity_regex: bool,

    /// Recompile fixed JavaScript tool-parser regexes on every parse for diagnostic A/B only.
    #[arg(long)]
    legacy_js_regex: bool,

    /// Use the pre-v0.6.4 sequential export path that rebuilds each handoff before writing.
    #[arg(long)]
    legacy_export: bool,

    /// Recompile the remaining fixed UI/Goal/attachment regexes on every call for diagnostic A/B only.
    #[arg(long)]
    legacy_residual_regex: bool,

    /// Use full existing-output JSON parsing, post-serialization CRLF copying, and post-rename timestamp opens for diagnostic A/B only.
    #[arg(long)]
    legacy_output_io: bool,

    /// Use repeated filesystem metadata calls during rollout discovery for diagnostic A/B only.
    #[arg(long)]
    legacy_discovery: bool,

    /// Build root handoff JSON sequentially instead of on the existing Rayon pool for diagnostic A/B only.
    #[arg(long)]
    legacy_handoff_build: bool,

    /// Use the pre-v0.6.7 export bookkeeping path with sequential existing-output indexing and repeated metadata probes.
    #[arg(long)]
    legacy_export_bookkeeping: bool,

    #[arg(long)]
    verbose: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SessionHotspotProfile {
    session_id: String,
    path: PathBuf,
    input_bytes: u64,
    retained_records: usize,
    ingestion: Duration,
    metadata: Duration,
    core_scan: Duration,
    core_finalize: Duration,
    finalize_filter_sort: Duration,
    finalize_messages: Duration,
    finalize_turns_goals: Duration,
    finalize_changes: Duration,
    finalize_activities: Duration,
    finalize_assembly: Duration,
    enrichment: Duration,
    total: Duration,
}

static HOTSPOT_ENABLED: AtomicBool = AtomicBool::new(false);
static LEGACY_ACTIVITY_SORT_ENABLED: AtomicBool = AtomicBool::new(false);
static LEGACY_SOURCE_QUALITY_SCAN_ENABLED: AtomicBool = AtomicBool::new(false);
static LEGACY_ACTIVITY_REGEX_ENABLED: AtomicBool = AtomicBool::new(false);
static LEGACY_JS_REGEX_ENABLED: AtomicBool = AtomicBool::new(false);
static LEGACY_RESIDUAL_REGEX_ENABLED: AtomicBool = AtomicBool::new(false);
static LEGACY_OUTPUT_IO_ENABLED: AtomicBool = AtomicBool::new(false);
static LEGACY_EXPORT_BOOKKEEPING_ENABLED: AtomicBool = AtomicBool::new(false);
static LEGACY_LINE_SPLITTER_ENABLED: AtomicBool = AtomicBool::new(false);
static LEGACY_CRLF_WRITER_ENABLED: AtomicBool = AtomicBool::new(false);
static FAST_CRLF_SERIALIZER_ENABLED: AtomicBool = AtomicBool::new(false);
static DIRECT_CANONICAL_INGESTION_ENABLED: AtomicBool =
    AtomicBool::new(DEFAULT_DIRECT_CANONICAL_INGESTION);
static HOTSPOT_PROFILES: OnceLock<Mutex<Vec<SessionHotspotProfile>>> = OnceLock::new();

fn hotspot_profiles() -> &'static Mutex<Vec<SessionHotspotProfile>> {
    HOTSPOT_PROFILES.get_or_init(|| Mutex::new(Vec::new()))
}

fn duration_or_zero(start: Option<Instant>) -> Duration {
    start.map(|value| value.elapsed()).unwrap_or_default()
}

fn parse_worker_count(text: &str) -> std::result::Result<usize, String> {
    let value = text
        .parse::<usize>()
        .map_err(|_| format!("workers must be an integer: {text}"))?;
    if !(MIN_WORKERS..=MAX_WORKERS).contains(&value) {
        return Err(format!("workers must be in {MIN_WORKERS}..={MAX_WORKERS}"));
    }
    Ok(value)
}

fn parse_export_lane_count(text: &str) -> std::result::Result<usize, String> {
    let value = text
        .parse::<usize>()
        .map_err(|_| format!("export-lanes must be an integer: {text}"))?;
    if value > MAX_EXPORT_LANES {
        return Err(format!("export-lanes must be in 0..={MAX_EXPORT_LANES}"));
    }
    Ok(value)
}

fn resolved_export_lanes(args: &Args) -> (usize, usize, usize) {
    let umbrella = args.export_lanes;
    let write_lanes = args.write_lanes.or(umbrella).unwrap_or(DEFAULT_WRITE_LANES);
    let finalize_lanes = args
        .finalize_lanes
        .or(umbrella)
        .unwrap_or(DEFAULT_FINALIZE_LANES);
    let cleanup_lanes = args
        .cleanup_lanes
        .or(args.finalize_lanes)
        .or(umbrella)
        .unwrap_or(DEFAULT_CLEANUP_LANES);
    (write_lanes, cleanup_lanes, finalize_lanes)
}

fn use_fused_reducer(args: &Args) -> bool {
    (DEFAULT_FUSED_REDUCER || args.fused_reducer) && !args.legacy_reducer
}

fn export_lane_ranges(len: usize, lanes: usize) -> Vec<std::ops::Range<usize>> {
    if len == 0 {
        return Vec::new();
    }
    let lanes = lanes.clamp(1, len);
    let mut ranges = Vec::with_capacity(lanes);
    for lane in 0..lanes {
        let start = len * lane / lanes;
        let end = len * (lane + 1) / lanes;
        if start < end {
            ranges.push(start..end);
        }
    }
    ranges
}

fn value_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn raw_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn nonblank_raw_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

fn bool_value(value: Option<&Value>) -> Option<bool> {
    value.and_then(Value::as_bool)
}

fn int_value(value: Option<&Value>, minimum: Option<i64>) -> Option<i64> {
    let parsed = match value? {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|v| i64::try_from(v).ok())),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => None,
    }?;
    if minimum.is_some_and(|min| parsed < min) {
        None
    } else {
        Some(parsed)
    }
}

fn float_value(value: Option<&Value>, minimum: Option<f64>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    if !parsed.is_finite() || minimum.is_some_and(|min| parsed < min) {
        None
    } else {
        Some(parsed)
    }
}

fn parse_datetime_value(value: Option<&Value>) -> Option<Timestamp> {
    let text = value?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp_micros())
}

fn parse_datetime_text(text: &str) -> Option<Timestamp> {
    DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp_micros())
}

fn parse_unix_seconds(value: Option<&Value>) -> Option<Timestamp> {
    let seconds = int_value(value, None)?;
    seconds.checked_mul(1_000_000)
}

fn utc_iso(timestamp: Timestamp) -> String {
    let seconds = timestamp.div_euclid(1_000_000);
    let micros = timestamp.rem_euclid(1_000_000) as u32;
    let dt = Utc
        .timestamp_opt(seconds, micros * 1_000)
        .single()
        .expect("valid timestamp");
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}+00:00",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.timestamp_subsec_millis(),
    )
}

fn filename_timestamp_gmt8(timestamp: Timestamp) -> String {
    let seconds = timestamp.div_euclid(1_000_000);
    let micros = timestamp.rem_euclid(1_000_000) as u32;
    let utc = Utc
        .timestamp_opt(seconds, micros * 1_000)
        .single()
        .expect("valid timestamp");
    let offset = FixedOffset::east_opt(8 * 3600).expect("GMT+8 offset");
    let local = utc.with_timezone(&offset);
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute(),
        local.second()
    )
}

fn round_away(value: f64, places: u32) -> f64 {
    let factor = 10_f64.powi(places as i32);
    if value >= 0.0 {
        (value * factor + 0.5).floor() / factor
    } else {
        (value * factor - 0.5).ceil() / factor
    }
}

fn context_snapshot(window_tokens: i64, used_tokens: i64) -> Option<Value> {
    if window_tokens <= 0 || used_tokens < 0 {
        return None;
    }
    let percent = round_away((used_tokens as f64 / window_tokens as f64) * 100.0, 2);
    let percent_value = if percent.fract() == 0.0 {
        Value::Number(Number::from(percent as i64))
    } else {
        Value::Number(Number::from_f64(percent)?)
    };
    Some(Value::Object(obj!(
        "window_tokens" => Value::Number(window_tokens.into()),
        "used_tokens" => Value::Number(used_tokens.into()),
        "utilization_percent" => percent_value,
    )))
}

fn normalize_windows_path(path: &str) -> String {
    let mut value = path.trim().replace('/', "\\");
    while value.contains("\\\\") {
        value = value.replace("\\\\", "\\");
    }
    let is_drive_root =
        value.len() == 3 && value.as_bytes().get(1) == Some(&b':') && value.ends_with('\\');
    if !is_drive_root {
        while value.ends_with('\\') {
            value.pop();
        }
    }
    value.to_uppercase()
}

fn relative_workspace_path(workspace: &str, item: &str) -> String {
    let base = workspace.trim_end_matches(&['\\', '/'][..]);
    let full = item.replace('/', "\\");
    if full.eq_ignore_ascii_case(base) {
        return ".".to_string();
    }
    let prefix = format!("{base}\\");
    if full
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(&prefix))
    {
        return full[prefix.len()..].to_string();
    }
    item.to_string()
}

fn get_message_text(content: Option<&Value>) -> Option<String> {
    let value = content?;
    let mut parts = Vec::new();
    match value {
        Value::Array(items) => {
            for item in items {
                if let Some(text) = item
                    .as_object()
                    .and_then(|m| m.get("text"))
                    .and_then(Value::as_str)
                {
                    if !text.trim().is_empty() {
                        parts.push(text.to_string());
                    }
                }
            }
        }
        Value::Object(item) => {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
        _ => {}
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n").trim().to_string())
    }
}

/// Tool-call output is a transport stream, not a chat message. Preserve scalar
/// outputs and concatenate text fragments without inserting separators so
/// diagnostic hashes remain tied to the raw logical output.
fn get_tool_output_text(content: Option<&Value>) -> Option<String> {
    let value = content?;
    let mut output = String::new();
    match value {
        Value::String(text) => output.push_str(text),
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(text) => output.push_str(text),
                    Value::Object(map) => {
                        if let Some(text) = map.get("text").and_then(Value::as_str) {
                            output.push_str(text);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                output.push_str(text);
            }
        }
        _ => {}
    }
    (!output.trim().is_empty()).then_some(output)
}

fn text_key(value: Option<&str>) -> String {
    value
        .unwrap_or_default()
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

fn sanitize_filename_part(value: &str, fallback: &str) -> String {
    let source = if value.trim().is_empty() {
        fallback
    } else {
        value
    };
    let mut result = String::with_capacity(source.len());
    let mut previous_space = false;
    for ch in source.chars() {
        if ch <= '\u{1f}' || ch == '\u{7f}' {
            continue;
        }
        let replacement = if matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            '_'
        } else {
            ch
        };
        if replacement.is_whitespace() {
            if !previous_space {
                result.push(' ');
            }
            previous_space = true;
        } else {
            result.push(replacement);
            previous_space = false;
        }
    }
    let mut result = result.trim().trim_end_matches(&['.', ' '][..]).to_string();
    if result.chars().count() > 120 {
        result = result.chars().take(120).collect::<String>();
        result = result.trim_end_matches(&['.', ' '][..]).to_string();
    }
    if result.trim().is_empty() {
        fallback.to_string()
    } else {
        result
    }
}

fn fallback_title(raw_messages: &[RawMessage], session_id: &str) -> String {
    for message in raw_messages {
        if message.role != "user" {
            continue;
        }
        for line in message.content.lines() {
            let candidate = line.trim();
            if !candidate.is_empty() {
                return candidate.to_string();
            }
        }
    }
    session_id.to_string()
}

fn sha256_text(text: Option<&str>) -> String {
    let mut hash = Sha256::new();
    hash.update(text.unwrap_or_default().as_bytes());
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn open_shared_read(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    options
        .open(path)
        .with_context(|| format!("unable to open {}", path.display()))
}

fn read_shared_bytes(path: &Path) -> Result<Vec<u8>> {
    let mut file = open_shared_read(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("unable to read {}", path.display()))?;
    Ok(bytes)
}

#[derive(Debug, Deserialize)]
struct RolloutEnvelope<'a> {
    #[serde(rename = "type", default, borrow)]
    record_type: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    payload: Option<LazyValue<'a>>,
}

#[derive(Debug, Deserialize)]
struct PayloadKind<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Option<Cow<'a, str>>,
}

#[derive(Debug, Default, Deserialize)]
struct SessionMetaSelective {
    id: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,
    originator: Option<String>,
    source: Option<Value>,
    cli_version: Option<String>,
    model_provider: Option<String>,
    git: Option<Value>,
    thread_source: Option<String>,
    parent_thread_id: Option<String>,
    forked_from_id: Option<String>,
    agent_nickname: Option<String>,
    agent_path: Option<String>,
    multi_agent_version: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TurnContextSelective {
    turn_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    summary: Option<String>,
    collaboration_mode: Option<Value>,
    sandbox_policy: Option<Value>,
    multi_agent_mode: Option<String>,
    multi_agent_version: Option<String>,
    approval_policy: Option<String>,
    approvals_reviewer: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TokenUsageSelective {
    total_tokens: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct TokenInfoSelective {
    model_context_window: Option<Value>,
    last_token_usage: Option<TokenUsageSelective>,
    rate_limits: Option<RateLimitsSelective>,
}

#[derive(Debug, Default, Deserialize)]
struct RateLimitPrimarySelective {
    used_percent: Option<Value>,
    resets_at: Option<Value>,
    window_minutes: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RateLimitsSelective {
    limit_id: Option<Value>,
    primary: Option<RateLimitPrimarySelective>,
}

#[derive(Debug, Default, Deserialize)]
struct TokenCountSelective {
    #[serde(rename = "type")]
    kind: Option<String>,
    info: Option<TokenInfoSelective>,
    rate_limits: Option<RateLimitsSelective>,
}

#[derive(Debug, Default, Deserialize)]
struct PatchChangeSelective {
    #[serde(rename = "type")]
    operation: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PatchApplySelective {
    #[serde(rename = "type")]
    kind: Option<String>,
    changes: Option<IndexMap<String, PatchChangeSelective>>,
    turn_id: Option<String>,
    success: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct MessageSelective<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    role: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    phase: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    content: Option<LazyValue<'a>>,
    #[serde(default, borrow)]
    internal_chat_message_metadata_passthrough: Option<LazyValue<'a>>,
    #[serde(default, borrow)]
    author: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    recipient: Option<Cow<'a, str>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallSelective<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    input: Option<LazyValue<'a>>,
    #[serde(default, borrow)]
    arguments: Option<LazyValue<'a>>,
    #[serde(default, borrow)]
    call_id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    internal_chat_message_metadata_passthrough: Option<LazyValue<'a>>,
}

#[derive(Debug, Deserialize)]
struct ToolOutputSelective<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    call_id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    output: Option<LazyValue<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolOutputMode {
    Full,
    ShellSummary,
    Drop,
}

#[derive(Clone, Debug, Default)]
struct SourceDataQuality {
    source_unicode_replacement_chars: usize,
    affected_tool_outputs: usize,
}

impl SourceDataQuality {
    fn to_value(&self) -> Option<Value> {
        if self.source_unicode_replacement_chars == 0 {
            return None;
        }
        Some(Value::Object(obj!(
            "source_unicode_replacement_chars" => Value::Number(
                (self.source_unicode_replacement_chars as u64).into(),
            ),
            "affected_tool_outputs" => Value::Number(
                (self.affected_tool_outputs as u64).into(),
            ),
            "lossy_source_capture" => Value::Bool(true),
        )))
    }
}

#[derive(Default)]
struct RolloutIngestionState {
    custom_output_modes: HashMap<String, ToolOutputMode>,
    function_output_modes: HashMap<String, ToolOutputMode>,
    data_quality: SourceDataQuality,
}

fn insert_optional_owned_string(map: &mut Obj, key: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), Value::String(value));
    }
}

fn insert_optional_owned_value(map: &mut Obj, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_string(), value);
    }
}

fn insert_optional_cow_string(map: &mut Obj, key: &str, value: Option<Cow<'_, str>>) {
    if let Some(value) = value {
        map.insert(key.to_string(), Value::String(value.into_owned()));
    }
}

fn lazy_to_value(value: Option<&LazyValue<'_>>) -> Result<Option<Value>> {
    value
        .map(|value| parse_payload_value(value.as_raw_str()))
        .transpose()
}

fn lazy_to_text(value: Option<&LazyValue<'_>>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let raw = value.as_raw_str();
    if let Ok(text) = sonic_rs::from_str::<String>(raw) {
        return Ok(Some(text));
    }
    let parsed = parse_payload_value(raw)?;
    Ok(get_message_text(Some(&parsed)))
}

/// Match the canonical oracle's message-text semantics for tool outputs.
///
/// Raw function/custom tool output may itself be a scalar JSON string.  The
/// canonical Python exporter deliberately ignores scalar strings in
/// `get_message_text()` and only consumes `text` fields from objects/arrays.
/// Selective ingestion must preserve that distinction: extracting exit codes
/// directly from a scalar string changes ResultMapping from `unknown` to
/// `exact`, which in turn causes Compact activity output to summarize commands
/// that the oracle keeps as individual command activities.
fn lazy_to_message_text(value: Option<&LazyValue<'_>>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = parse_payload_value(value.as_raw_str())?;
    Ok(get_message_text(Some(&parsed)))
}

fn lazy_to_tool_output_text(value: Option<&LazyValue<'_>>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = parse_payload_value(value.as_raw_str())?;
    Ok(get_tool_output_text(Some(&parsed)))
}

fn build_compact_record(record_type: &str, timestamp: Option<&str>, payload: Option<Obj>) -> Value {
    let mut record = obj!("type" => Value::String(record_type.to_string()));
    if let Some(timestamp) = timestamp {
        record.insert(
            "timestamp".to_string(),
            Value::String(timestamp.to_string()),
        );
    }
    if let Some(payload) = payload {
        record.insert("payload".to_string(), Value::Object(payload));
    }
    Value::Object(record)
}

fn parse_payload_value(raw: &str) -> Result<Value> {
    match sonic_rs::from_str::<Value>(raw) {
        Ok(value) => Ok(value),
        Err(fast_error) => serde_json::from_str::<Value>(raw)
            .map_err(|fallback_error| anyhow!("sonic-rs payload parse failed ({fast_error}); serde_json fallback failed ({fallback_error})")),
    }
}

fn compact_payload_keys(raw: &str, keys: &[&str]) -> Result<Obj> {
    let mut value = parse_payload_value(raw)?;
    let source = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("rollout payload is not an object"))?;
    let mut compact = Obj::new();
    for key in keys {
        if let Some(value) = source.remove(*key) {
            compact.insert((*key).to_string(), value);
        }
    }
    Ok(compact)
}

fn compact_session_meta(raw: &str) -> Result<Obj> {
    let parsed: SessionMetaSelective = sonic_rs::from_str(raw)
        .map_err(|error| anyhow!("unable to selectively parse session_meta payload: {error}"))?;
    let mut payload = Obj::new();
    insert_optional_owned_string(&mut payload, "id", parsed.id);
    insert_optional_owned_string(&mut payload, "session_id", parsed.session_id);
    insert_optional_owned_string(&mut payload, "cwd", parsed.cwd);
    insert_optional_owned_string(&mut payload, "timestamp", parsed.timestamp);
    insert_optional_owned_string(&mut payload, "originator", parsed.originator);
    insert_optional_owned_value(&mut payload, "source", parsed.source);
    insert_optional_owned_string(&mut payload, "cli_version", parsed.cli_version);
    insert_optional_owned_string(&mut payload, "model_provider", parsed.model_provider);
    insert_optional_owned_value(&mut payload, "git", parsed.git);
    insert_optional_owned_string(&mut payload, "thread_source", parsed.thread_source);
    insert_optional_owned_string(&mut payload, "parent_thread_id", parsed.parent_thread_id);
    insert_optional_owned_string(&mut payload, "forked_from_id", parsed.forked_from_id);
    insert_optional_owned_string(&mut payload, "agent_nickname", parsed.agent_nickname);
    insert_optional_owned_string(&mut payload, "agent_path", parsed.agent_path);
    insert_optional_owned_string(
        &mut payload,
        "multi_agent_version",
        parsed.multi_agent_version,
    );
    Ok(payload)
}

fn compact_turn_context(raw: &str) -> Result<Obj> {
    let parsed: TurnContextSelective = sonic_rs::from_str(raw)
        .map_err(|error| anyhow!("unable to selectively parse turn_context payload: {error}"))?;
    let mut payload = Obj::new();
    insert_optional_owned_string(&mut payload, "turn_id", parsed.turn_id);
    insert_optional_owned_string(&mut payload, "model", parsed.model);
    insert_optional_owned_string(&mut payload, "effort", parsed.effort);
    insert_optional_owned_string(&mut payload, "summary", parsed.summary);
    insert_optional_owned_value(
        &mut payload,
        "collaboration_mode",
        parsed.collaboration_mode,
    );
    insert_optional_owned_value(&mut payload, "sandbox_policy", parsed.sandbox_policy);
    insert_optional_owned_string(&mut payload, "multi_agent_mode", parsed.multi_agent_mode);
    insert_optional_owned_string(
        &mut payload,
        "multi_agent_version",
        parsed.multi_agent_version,
    );
    insert_optional_owned_string(&mut payload, "approval_policy", parsed.approval_policy);
    insert_optional_owned_string(
        &mut payload,
        "approvals_reviewer",
        parsed.approvals_reviewer,
    );
    Ok(payload)
}

fn compact_rate_limits(rate_limits: RateLimitsSelective) -> Option<Value> {
    let mut rates_map = Obj::new();
    insert_optional_owned_value(&mut rates_map, "limit_id", rate_limits.limit_id);
    if let Some(primary) = rate_limits.primary {
        let mut primary_map = Obj::new();
        insert_optional_owned_value(&mut primary_map, "used_percent", primary.used_percent);
        insert_optional_owned_value(&mut primary_map, "resets_at", primary.resets_at);
        insert_optional_owned_value(&mut primary_map, "window_minutes", primary.window_minutes);
        if !primary_map.is_empty() {
            rates_map.insert("primary".to_string(), Value::Object(primary_map));
        }
    }
    (!rates_map.is_empty()).then_some(Value::Object(rates_map))
}

fn compact_token_count(raw: &str) -> Result<Obj> {
    let parsed: TokenCountSelective = sonic_rs::from_str(raw)
        .map_err(|error| anyhow!("unable to selectively parse token_count payload: {error}"))?;
    let mut payload = Obj::new();
    insert_optional_owned_string(&mut payload, "type", parsed.kind);
    if let Some(info) = parsed.info {
        let mut info_map = Obj::new();
        insert_optional_owned_value(
            &mut info_map,
            "model_context_window",
            info.model_context_window,
        );
        if let Some(usage) = info.last_token_usage {
            let mut usage_map = Obj::new();
            insert_optional_owned_value(&mut usage_map, "total_tokens", usage.total_tokens);
            if !usage_map.is_empty() {
                info_map.insert("last_token_usage".to_string(), Value::Object(usage_map));
            }
        }
        if let Some(rate_limits) = info.rate_limits.and_then(compact_rate_limits) {
            info_map.insert("rate_limits".to_string(), rate_limits);
        }
        if !info_map.is_empty() {
            payload.insert("info".to_string(), Value::Object(info_map));
        }
    }
    if let Some(rate_limits) = parsed.rate_limits.and_then(compact_rate_limits) {
        payload.insert("rate_limits".to_string(), rate_limits);
    }
    Ok(payload)
}

fn compact_patch_apply(raw: &str) -> Result<Obj> {
    let parsed: PatchApplySelective = sonic_rs::from_str(raw)
        .map_err(|error| anyhow!("unable to selectively parse patch_apply_end payload: {error}"))?;
    let mut payload = Obj::new();
    insert_optional_owned_string(&mut payload, "type", parsed.kind);
    if let Some(changes) = parsed.changes {
        let mut change_map = Obj::new();
        for (path, change) in changes {
            let mut detail = Obj::new();
            insert_optional_owned_string(&mut detail, "type", change.operation);
            change_map.insert(path, Value::Object(detail));
        }
        payload.insert("changes".to_string(), Value::Object(change_map));
    }
    insert_optional_owned_string(&mut payload, "turn_id", parsed.turn_id);
    if let Some(success) = parsed.success {
        payload.insert("success".to_string(), Value::Bool(success));
    }
    Ok(payload)
}

fn compact_message_payload(parsed: MessageSelective<'_>) -> Result<Obj> {
    let content = lazy_to_value(parsed.content.as_ref())?;
    let metadata = lazy_to_value(parsed.internal_chat_message_metadata_passthrough.as_ref())?;
    let mut payload = Obj::new();
    insert_optional_cow_string(&mut payload, "type", parsed.kind);
    insert_optional_cow_string(&mut payload, "role", parsed.role);
    insert_optional_cow_string(&mut payload, "phase", parsed.phase);
    insert_optional_owned_value(&mut payload, "content", content);
    insert_optional_owned_value(
        &mut payload,
        "internal_chat_message_metadata_passthrough",
        metadata,
    );
    insert_optional_cow_string(&mut payload, "author", parsed.author);
    insert_optional_cow_string(&mut payload, "recipient", parsed.recipient);
    Ok(payload)
}

fn compact_tool_call_payload(
    parsed: ToolCallSelective<'_>,
    input: Option<Value>,
    arguments: Option<Value>,
    metadata: Option<Value>,
) -> Obj {
    let mut payload = Obj::new();
    insert_optional_cow_string(&mut payload, "type", parsed.kind);
    insert_optional_cow_string(&mut payload, "name", parsed.name);
    insert_optional_owned_value(&mut payload, "input", input);
    insert_optional_owned_value(&mut payload, "arguments", arguments);
    insert_optional_cow_string(&mut payload, "call_id", parsed.call_id);
    insert_optional_owned_value(
        &mut payload,
        "internal_chat_message_metadata_passthrough",
        metadata,
    );
    payload
}

fn compact_tool_output_payload(parsed: ToolOutputSelective<'_>, output: Option<Value>) -> Obj {
    let mut payload = Obj::new();
    insert_optional_cow_string(&mut payload, "type", parsed.kind);
    insert_optional_cow_string(&mut payload, "call_id", parsed.call_id);
    insert_optional_owned_value(&mut payload, "output", output);
    payload
}

fn shell_output_summary(text: Option<&str>) -> (Vec<i64>, bool) {
    let text = text.unwrap_or_default();
    let exit_codes = exit_code_regex()
        .captures_iter(text)
        .filter_map(|captures| captures.get(1))
        .filter_map(|value| value.as_str().parse::<i64>().ok())
        .collect();
    let truncated = truncated_output_regex().is_match(text);
    (exit_codes, truncated)
}

fn command_wrapper_failure_diagnostic(text: Option<&str>, exit_codes: &[i64]) -> Option<Value> {
    if !exit_codes.is_empty() {
        return None;
    }
    let text = text?;
    let first = text.trim_start();
    if first.starts_with("Script failed") || first.starts_with("execution error:") {
        Some(bounded_text(text, 4096))
    } else {
        None
    }
}

fn insert_compact_failure(payload: &mut Obj, diagnostic: Option<Value>) {
    let Some(diagnostic) = diagnostic else {
        return;
    };
    payload.insert(
        "_compact_failure_status".to_string(),
        Value::String("failed".to_string()),
    );
    payload.insert("_compact_failure_diagnostic".to_string(), diagnostic);
}

fn compact_shell_output_payload(
    parsed: ToolOutputSelective<'_>,
    summary_text: Option<&str>,
    exact_text: Option<&str>,
) -> Obj {
    // Preserve v1.0.27 result-mapping semantics for ordinary command output.
    // Exact transport text is consulted only for the no-exit wrapper-failure
    // diagnostic so successful/normal commands do not change compaction.
    let (exit_codes, truncated) = shell_output_summary(summary_text);
    let failure = command_wrapper_failure_diagnostic(exact_text, &exit_codes);
    let mut payload = Obj::new();
    insert_optional_cow_string(&mut payload, "type", parsed.kind);
    insert_optional_cow_string(&mut payload, "call_id", parsed.call_id);
    payload.insert("_compact_shell_summary".to_string(), Value::Bool(true));
    payload.insert(
        "_compact_exit_codes".to_string(),
        Value::Array(
            exit_codes
                .iter()
                .map(|code| Value::Number((*code).into()))
                .collect(),
        ),
    );
    payload.insert(
        "_compact_output_truncated".to_string(),
        Value::Bool(truncated),
    );
    insert_compact_failure(&mut payload, failure);
    payload
}

fn has_attachment_reference(text: &str) -> bool {
    if !attachment_references(text).is_empty() {
        return true;
    }
    text.contains("\\\\") && !attachment_references(&text.replace("\\\\", "\\")).is_empty()
}

fn attachment_read_command(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("get-content") && lower.contains("-raw") && has_attachment_reference(text)
}

fn custom_attachment_read_is_unambiguous(text: &str) -> bool {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(40, 0);

    attachment_read_reference_from_script(text).is_some()
}

fn custom_attachment_classifier_may_match(text: &str) -> bool {
    // Fast negative gate for the expensive JavaScript command parser.  A
    // concrete simple attachment read must expose Get-Content, -Raw, and a
    // configured attachment-root path somewhere in the source text.  Template
    // interpolation or encoded JavaScript escapes can synthesize those bytes,
    // so those shapes conservatively fall back to the authoritative parser.
    let lower = text.to_ascii_lowercase();
    let dynamic = text.contains("${")
        || text.contains(r"\u")
        || text.contains(r"\x")
        || text.contains(r"\0")
        || text.contains(r"\1")
        || text.contains(r"\2")
        || text.contains(r"\3")
        || text.contains(r"\4")
        || text.contains(r"\5")
        || text.contains(r"\6")
        || text.contains(r"\7");
    if (!lower.contains("get-content") || !lower.contains("-raw")) && !dynamic {
        return false;
    }
    if attachment_path_config().regex.is_match(text) {
        return true;
    }
    dynamic
}

fn custom_shell_attachment_read(text: &str) -> bool {
    custom_attachment_classifier_may_match(text) && custom_attachment_read_is_unambiguous(text)
}

fn function_shell_attachment_read(arguments: Option<&Value>) -> bool {
    let arguments = json_object(arguments);
    raw_str(arguments.get("command")).is_some_and(attachment_read_command)
}

// ---------------------------------------------------------------------------
// v0.5 prefix-routed ingestion
//
// Codex-generated rollout JSONL has a stable outer layout in the current
// corpus: timestamp, top-level type, then payload; event/response payloads put
// their type first.  The fast path inspects only the small prefix needed to
// route records.  It deliberately does not validate or structurally scan the
// rest of a large record unless that record is actually needed.
//
// Any unfamiliar ordering or missing prefix field falls back to the complete
// sonic-rs parser below.  The fast path also requires a complete object frame,
// so a truncated physical line is still handled by the existing error path.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrefixDecision {
    Fallback,
    Drop,
    Compacted,
    CustomShellSummary,
    FunctionShellSummary,
    CustomOutputDrop,
    FunctionOutputDrop,
}

#[derive(Clone, Copy, Debug)]
struct PrefixRoute<'a> {
    decision: PrefixDecision,
    timestamp: Option<&'a str>,
    call_id: Option<&'a str>,
    tool_output: bool,
}

#[derive(Clone, Copy, Debug)]
struct CanonicalPrefix<'a> {
    timestamp: &'a str,
    record_type: &'a str,
    payload_type: Option<&'a str>,
    after_payload_type: Option<usize>,
    payload_start: usize,
}

fn parse_plain_json_string_at<'a>(line: &'a [u8], start: usize) -> Option<(&'a str, usize)> {
    let remainder = line.get(start..)?;
    let relative_end = remainder.iter().position(|byte| *byte == b'"')?;
    let raw = remainder.get(..relative_end)?;
    if raw.contains(&b'\\') {
        return None;
    }
    let value = std::str::from_utf8(raw).ok()?;
    Some((value, start + relative_end + 1))
}

fn canonical_prefix_fields<'a>(line: &'a [u8]) -> Option<CanonicalPrefix<'a>> {
    const TIMESTAMP_PREFIX: &[u8] = br#"{"timestamp":""#;
    const ORDINAL_PREFIX: &[u8] = br#","ordinal":"#;
    const RECORD_TYPE_PREFIX: &[u8] = br#","type":""#;
    const PAYLOAD_PREFIX: &[u8] = br#","payload":{"#;
    const PAYLOAD_TYPE_PREFIX: &[u8] = br#""type":""#;

    // The fast router is deliberately strict. Codex-generated rollout JSONL in
    // the current corpus uses this exact compact field order. Any whitespace,
    // reordered property, escape, or incomplete object falls back to sonic-rs.
    if line.first() != Some(&b'{') || line.last() != Some(&b'}') {
        return None;
    }
    if !line.starts_with(TIMESTAMP_PREFIX) {
        return None;
    }

    let (timestamp, mut cursor) = parse_plain_json_string_at(line, TIMESTAMP_PREFIX.len())?;
    if line.get(cursor..)?.starts_with(ORDINAL_PREFIX) {
        cursor += ORDINAL_PREFIX.len();
        let ordinal_start = cursor;
        while line.get(cursor).is_some_and(|byte| byte.is_ascii_digit()) {
            cursor += 1;
        }
        if cursor == ordinal_start {
            return None;
        }
    }
    if !line.get(cursor..)?.starts_with(RECORD_TYPE_PREFIX) {
        return None;
    }
    cursor += RECORD_TYPE_PREFIX.len();

    let (record_type, next) = parse_plain_json_string_at(line, cursor)?;
    cursor = next;
    if !line.get(cursor..)?.starts_with(PAYLOAD_PREFIX) {
        return None;
    }
    cursor += PAYLOAD_PREFIX.len();
    let payload_start = cursor.checked_sub(1)?;

    let remainder = line.get(cursor..)?;
    if !remainder.starts_with(PAYLOAD_TYPE_PREFIX) {
        return Some(CanonicalPrefix {
            timestamp,
            record_type,
            payload_type: None,
            after_payload_type: None,
            payload_start,
        });
    }
    cursor += PAYLOAD_TYPE_PREFIX.len();
    let (payload_type, cursor) = parse_plain_json_string_at(line, cursor)?;
    Some(CanonicalPrefix {
        timestamp,
        record_type,
        payload_type: Some(payload_type),
        after_payload_type: Some(cursor),
        payload_start,
    })
}

fn canonical_payload_raw<'a>(line: &'a [u8], prefix: &CanonicalPrefix<'_>) -> Option<&'a str> {
    // Strict Codex canonical layout is timestamp,type,payload with payload last.
    // Remove only the outer record closing brace; the payload object keeps its
    // own braces. If the suffix is not an object, preserve the legacy parser.
    let outer_end = line.len().checked_sub(1)?;
    let raw = line.get(prefix.payload_start..outer_end)?;
    if raw.first() != Some(&b'{') || raw.last() != Some(&b'}') {
        return None;
    }
    std::str::from_utf8(raw).ok()
}

fn direct_canonical_outer_suffix_fallback(
    prefix: &CanonicalPrefix<'_>,
    error: &anyhow::Error,
) -> bool {
    let message = error.to_string();
    let trailing = message.contains("trailing characters after the value")
        || message.contains("trailing characters at line");
    if !trailing {
        return false;
    }

    // These errors come from the top-level selective payload decode, before
    // any custom/function output-mode state is inserted or removed.
    if message.starts_with("unable to selectively parse ") {
        return true;
    }

    // Non-response records and stateless response kinds cannot have mutated
    // ingestion state before their payload parse failed.
    if prefix.record_type != "response_item" {
        return true;
    }
    !matches!(
        prefix.payload_type,
        Some(
            "custom_tool_call"
                | "custom_tool_call_output"
                | "function_call"
                | "function_call_output"
        )
    )
}

fn direct_call_id_after_payload_type<'a>(
    line: &'a [u8],
    after_payload_type: usize,
) -> Option<&'a str> {
    const ID_PREFIX: &[u8] = br#","id":""#;
    const CALL_ID_PREFIX: &[u8] = br#","call_id":""#;

    let mut cursor = after_payload_type;
    let remainder = line.get(cursor..)?;
    if remainder.starts_with(ID_PREFIX) {
        cursor += ID_PREFIX.len();
        let (_, next) = parse_plain_json_string_at(line, cursor)?;
        cursor = next;
    }

    let remainder = line.get(cursor..)?;
    if !remainder.starts_with(CALL_ID_PREFIX) {
        return None;
    }
    let start = cursor + CALL_ID_PREFIX.len();
    parse_plain_json_string_at(line, start).map(|(call_id, _)| call_id)
}

fn raw_exit_code_regex() -> &'static BytesRegex {
    static RE: OnceLock<BytesRegex> = OnceLock::new();
    RE.get_or_init(|| {
        BytesRegex::new(r"(?i)(?:Exit code:|Process exited with code)(?:\s|\\[nr])*(-?\d+)")
            .unwrap()
    })
}

fn raw_truncated_output_regex() -> &'static BytesRegex {
    static RE: OnceLock<BytesRegex> = OnceLock::new();
    RE.get_or_init(|| {
        BytesRegex::new(r"(?i)truncated output|output truncated|\[truncated").unwrap()
    })
}

fn raw_shell_output_summary(line: &[u8]) -> (Vec<i64>, bool) {
    let exit_codes = raw_exit_code_regex()
        .captures_iter(line)
        .filter_map(|captures| captures.get(1))
        .filter_map(|capture| std::str::from_utf8(capture.as_bytes()).ok())
        .filter_map(|text| text.parse::<i64>().ok())
        .collect();
    let truncated = raw_truncated_output_regex().is_match(line);
    (exit_codes, truncated)
}

/// Return whether the canonical-prefix record stores payload.output as a
/// scalar JSON string rather than an object/array carrying `text` fields.
///
/// `get_message_text()` intentionally ignores scalar strings.  The prefix
/// fast path therefore must not infer exit codes/truncation from bytes inside
/// such a string, even though the raw line contains text like `Exit code: 0`.
fn raw_output_value_is_scalar_string(line: &[u8]) -> bool {
    const OUTPUT_KEY: &[u8] = br#""output""#;
    let Some(start) = line
        .windows(OUTPUT_KEY.len())
        .position(|window| window == OUTPUT_KEY)
    else {
        return false;
    };
    let mut index = start + OUTPUT_KEY.len();
    while line.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if line.get(index) != Some(&b':') {
        return false;
    }
    index += 1;
    while line.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    line.get(index) == Some(&b'"')
}

fn compact_shell_output_payload_from_raw(payload_type: &str, call_id: &str, line: &[u8]) -> Obj {
    let (exit_codes, truncated) = if raw_output_value_is_scalar_string(line) {
        (Vec::new(), false)
    } else {
        raw_shell_output_summary(line)
    };
    // Parse the complete record only for the rare no-exit wrapper-failure path.
    // This keeps the fast path cheap while preserving the exact logical output
    // (including scalar strings) needed for a bounded failure diagnostic.
    let failure = if exit_codes.is_empty()
        && (line
            .windows(b"Script failed".len())
            .any(|w| w == b"Script failed")
            || line
                .windows(b"execution error:".len())
                .any(|w| w == b"execution error:"))
    {
        serde_json::from_slice::<Value>(line)
            .ok()
            .and_then(|record| {
                record
                    .get("payload")
                    .and_then(Value::as_object)
                    .and_then(|payload| get_tool_output_text(payload.get("output")))
            })
            .and_then(|text| command_wrapper_failure_diagnostic(Some(&text), &exit_codes))
    } else {
        None
    };
    let mut payload = Obj::new();
    payload.insert("type".to_string(), Value::String(payload_type.to_string()));
    payload.insert("call_id".to_string(), Value::String(call_id.to_string()));
    payload.insert("_compact_shell_summary".to_string(), Value::Bool(true));
    payload.insert(
        "_compact_exit_codes".to_string(),
        Value::Array(
            exit_codes
                .iter()
                .map(|code| Value::Number((*code).into()))
                .collect(),
        ),
    );
    payload.insert(
        "_compact_output_truncated".to_string(),
        Value::Bool(truncated),
    );
    insert_compact_failure(&mut payload, failure);
    payload
}

fn prefix_route<'a>(line: &'a [u8], state: &RolloutIngestionState) -> PrefixRoute<'a> {
    let Some(prefix) = canonical_prefix_fields(line) else {
        return PrefixRoute {
            decision: PrefixDecision::Fallback,
            timestamp: None,
            call_id: None,
            tool_output: false,
        };
    };

    let timestamp = Some(prefix.timestamp);
    let record_type = prefix.record_type;

    if matches!(
        record_type,
        "world_state" | "inter_agent_communication_metadata"
    ) {
        return PrefixRoute {
            decision: PrefixDecision::Drop,
            timestamp,
            call_id: None,
            tool_output: false,
        };
    }
    if record_type == "compacted" {
        return PrefixRoute {
            decision: PrefixDecision::Compacted,
            timestamp,
            call_id: None,
            tool_output: false,
        };
    }

    let payload_type = prefix.payload_type;
    if record_type == "event_msg"
        && matches!(
            payload_type,
            Some("agent_reasoning" | "agent_message" | "context_compacted" | "mcp_tool_call_end")
        )
    {
        return PrefixRoute {
            decision: PrefixDecision::Drop,
            timestamp,
            call_id: None,
            tool_output: false,
        };
    }
    if record_type == "response_item"
        && matches!(
            payload_type,
            Some("reasoning" | "tool_search_call" | "tool_search_output")
        )
    {
        return PrefixRoute {
            decision: PrefixDecision::Drop,
            timestamp,
            call_id: None,
            tool_output: false,
        };
    }

    if record_type == "response_item" && payload_type == Some("custom_tool_call_output") {
        let Some(after_payload_type) = prefix.after_payload_type else {
            return PrefixRoute {
                decision: PrefixDecision::Fallback,
                timestamp,
                call_id: None,
                tool_output: true,
            };
        };
        let Some(call_id) = direct_call_id_after_payload_type(line, after_payload_type) else {
            return PrefixRoute {
                decision: PrefixDecision::Fallback,
                timestamp,
                call_id: None,
                tool_output: true,
            };
        };
        let mode = state
            .custom_output_modes
            .get(call_id)
            .copied()
            .unwrap_or(ToolOutputMode::Drop);
        let decision = match mode {
            ToolOutputMode::Full => PrefixDecision::Fallback,
            ToolOutputMode::ShellSummary => PrefixDecision::CustomShellSummary,
            ToolOutputMode::Drop => PrefixDecision::CustomOutputDrop,
        };
        return PrefixRoute {
            decision,
            timestamp,
            call_id: Some(call_id),
            tool_output: true,
        };
    }

    if record_type == "response_item" && payload_type == Some("function_call_output") {
        let Some(after_payload_type) = prefix.after_payload_type else {
            return PrefixRoute {
                decision: PrefixDecision::Fallback,
                timestamp,
                call_id: None,
                tool_output: true,
            };
        };
        let Some(call_id) = direct_call_id_after_payload_type(line, after_payload_type) else {
            return PrefixRoute {
                decision: PrefixDecision::Fallback,
                timestamp,
                call_id: None,
                tool_output: true,
            };
        };
        let mode = state
            .function_output_modes
            .get(call_id)
            .copied()
            .unwrap_or(ToolOutputMode::Drop);
        return PrefixRoute {
            decision: match mode {
                ToolOutputMode::Full => PrefixDecision::Fallback,
                ToolOutputMode::ShellSummary => PrefixDecision::FunctionShellSummary,
                ToolOutputMode::Drop => PrefixDecision::FunctionOutputDrop,
            },
            timestamp,
            call_id: Some(call_id),
            tool_output: true,
        };
    }

    PrefixRoute {
        decision: PrefixDecision::Fallback,
        timestamp,
        call_id: None,
        tool_output: false,
    }
}

fn line_may_contain_replacement_character_legacy(line: &[u8]) -> bool {
    line.windows(3).any(|window| window == [0xef, 0xbf, 0xbd])
        || std::str::from_utf8(line)
            .ok()
            .is_some_and(|text| text.to_ascii_lowercase().contains(r"\ufffd"))
}

fn line_may_contain_replacement_character_fast(line: &[u8]) -> bool {
    for index in memchr_iter(0xef, line) {
        if line.get(index + 1) == Some(&0xbf) && line.get(index + 2) == Some(&0xbd) {
            return true;
        }
    }
    for index in memchr_iter(b'\\', line) {
        let Some(window) = line.get(index..index.saturating_add(6)) else {
            continue;
        };
        if window.len() == 6
            && window[1] == b'u'
            && window[2].eq_ignore_ascii_case(&b'f')
            && window[3].eq_ignore_ascii_case(&b'f')
            && window[4].eq_ignore_ascii_case(&b'f')
            && window[5].eq_ignore_ascii_case(&b'd')
        {
            return true;
        }
    }
    false
}

fn line_may_contain_replacement_character(line: &[u8]) -> bool {
    if LEGACY_SOURCE_QUALITY_SCAN_ENABLED.load(AtomicOrdering::Relaxed) {
        line_may_contain_replacement_character_legacy(line)
    } else {
        line_may_contain_replacement_character_fast(line)
    }
}

fn observe_source_data_quality_candidate(line: &[u8], state: &mut RolloutIngestionState) {
    let Ok(record) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    let Some(record_map) = record.as_object() else {
        return;
    };
    if value_str(record_map.get("type")) != Some("response_item") {
        return;
    }
    let Some(payload) = record_map.get("payload").and_then(Value::as_object) else {
        return;
    };
    if !matches!(
        value_str(payload.get("type")),
        Some("function_call_output" | "custom_tool_call_output")
    ) {
        return;
    }
    let count = payload
        .get("output")
        .map(replacement_character_count)
        .unwrap_or(0);
    if count > 0 {
        state.data_quality.source_unicode_replacement_chars += count;
        state.data_quality.affected_tool_outputs += 1;
    }
}

fn observe_source_data_quality(line: &[u8], state: &mut RolloutIngestionState) {
    if line_may_contain_replacement_character(line) {
        observe_source_data_quality_candidate(line, state);
    }
}

fn parse_rollout_line_prefix_mode(
    line: &[u8],
    state: &mut RolloutIngestionState,
    direct_canonical_ingestion: bool,
) -> Result<Option<Value>> {
    let legacy_source_quality = LEGACY_SOURCE_QUALITY_SCAN_ENABLED.load(AtomicOrdering::Relaxed);
    if legacy_source_quality {
        observe_source_data_quality(line, state);
    }
    let route = prefix_route(line, state);
    if !legacy_source_quality
        && (route.tool_output || route.timestamp.is_none())
        && line_may_contain_replacement_character_fast(line)
    {
        observe_source_data_quality_candidate(line, state);
    }
    match route.decision {
        PrefixDecision::Fallback => {
            #[cfg(feature = "integrated-study")]
            integrated_study::event(0);
            if direct_canonical_ingestion {
                if let Some(prefix) = canonical_prefix_fields(line) {
                    let direct_safe = prefix.payload_type.is_some()
                        || matches!(prefix.record_type, "session_meta" | "turn_context");
                    if direct_safe {
                        if let Some(payload_raw) = canonical_payload_raw(line, &prefix) {
                            match parse_rollout_known_fields(
                                prefix.record_type,
                                Some(prefix.timestamp),
                                Some(payload_raw),
                                prefix.payload_type,
                                state,
                            ) {
                                Ok(value) => {
                                    #[cfg(feature = "integrated-study")]
                                    integrated_study::event(2);
                                    return Ok(value);
                                }
                                Err(error)
                                    if direct_canonical_outer_suffix_fallback(&prefix, &error) =>
                                {
                                    #[cfg(feature = "integrated-study")]
                                    integrated_study::event(3);
                                    return parse_rollout_line(line, state);
                                }
                                Err(error) => return Err(error),
                            }
                        }
                    }
                }
            }
            #[cfg(feature = "integrated-study")]
            integrated_study::event(4);
            parse_rollout_line(line, state)
        }
        PrefixDecision::Drop => {
            #[cfg(feature = "integrated-study")]
            integrated_study::event(1);
            Ok(None)
        }
        PrefixDecision::Compacted => Ok(Some(build_compact_record(
            "compacted",
            route.timestamp,
            None,
        ))),
        PrefixDecision::CustomOutputDrop => {
            if let Some(call_id) = route.call_id {
                state.custom_output_modes.remove(call_id);
            }
            Ok(None)
        }
        PrefixDecision::FunctionOutputDrop => {
            if let Some(call_id) = route.call_id {
                state.function_output_modes.remove(call_id);
            }
            Ok(None)
        }
        PrefixDecision::CustomShellSummary => {
            let call_id = route
                .call_id
                .ok_or_else(|| anyhow!("prefix-routed custom output is missing call_id"))?;
            state.custom_output_modes.remove(call_id);
            let payload =
                compact_shell_output_payload_from_raw("custom_tool_call_output", call_id, line);
            Ok(Some(build_compact_record(
                "response_item",
                route.timestamp,
                Some(payload),
            )))
        }
        PrefixDecision::FunctionShellSummary => {
            let call_id = route
                .call_id
                .ok_or_else(|| anyhow!("prefix-routed function output is missing call_id"))?;
            state.function_output_modes.remove(call_id);
            let payload =
                compact_shell_output_payload_from_raw("function_call_output", call_id, line);
            Ok(Some(build_compact_record(
                "response_item",
                route.timestamp,
                Some(payload),
            )))
        }
    }
}

fn parse_rollout_line_prefix(
    line: &[u8],
    state: &mut RolloutIngestionState,
) -> Result<Option<Value>> {
    parse_rollout_line_prefix_mode(
        line,
        state,
        DIRECT_CANONICAL_INGESTION_ENABLED.load(AtomicOrdering::Relaxed),
    )
}

fn parse_rollout_known_fields(
    record_type: &str,
    timestamp: Option<&str>,
    payload_raw: Option<&str>,
    payload_type_text: Option<&str>,
    state: &mut RolloutIngestionState,
) -> Result<Option<Value>> {
    if matches!(
        record_type,
        "world_state" | "inter_agent_communication_metadata"
    ) {
        return Ok(None);
    }
    if matches!(
        (record_type, payload_type_text),
        ("event_msg", Some("agent_reasoning" | "context_compacted"))
            | (
                "response_item",
                Some("reasoning" | "tool_search_call" | "tool_search_output")
            )
            | ("event_msg", Some("mcp_tool_call_end"))
    ) {
        return Ok(None);
    }

    if record_type == "session_meta" {
        let raw = payload_raw.ok_or_else(|| anyhow!("session_meta record is missing payload"))?;
        return Ok(Some(build_compact_record(
            record_type,
            timestamp,
            Some(compact_session_meta(raw)?),
        )));
    }
    if record_type == "compacted" {
        return Ok(Some(build_compact_record(record_type, timestamp, None)));
    }
    if record_type == "turn_context" {
        let raw = payload_raw.ok_or_else(|| anyhow!("turn_context record is missing payload"))?;
        return Ok(Some(build_compact_record(
            record_type,
            timestamp,
            Some(compact_turn_context(raw)?),
        )));
    }
    if record_type == "token_usage_record" {
        let Some(raw) = payload_raw else {
            return Ok(None);
        };
        let payload = compact_payload_keys(raw, &["thread_token_usage"])?;
        if payload.is_empty() {
            return Ok(None);
        }
        return Ok(Some(build_compact_record(
            record_type,
            timestamp,
            Some(payload),
        )));
    }

    if record_type == "event_msg" {
        let Some(raw) = payload_raw else {
            return Ok(None);
        };
        let payload = match payload_type_text {
            Some("token_count") => compact_token_count(raw)?,
            Some("patch_apply_end") => compact_patch_apply(raw)?,
            Some("task_started") => compact_payload_keys(
                raw,
                &[
                    "type",
                    "model_context_window",
                    "turn_id",
                    "collaboration_mode_kind",
                ],
            )?,
            Some("thread_goal_updated") => {
                compact_payload_keys(raw, &["type", "goal", "threadId"])?
            }
            Some("item_completed") => compact_payload_keys(
                raw,
                &[
                    "type",
                    "item",
                    "turn_id",
                    "started_at_ms",
                    "completed_at_ms",
                ],
            )?,
            Some("task_complete") => compact_payload_keys(
                raw,
                &[
                    "type",
                    "turn_id",
                    "error",
                    "completed_at",
                    "started_at",
                    "duration_ms",
                    "time_to_first_token_ms",
                ],
            )?,
            Some("turn_aborted") => {
                compact_payload_keys(raw, &["type", "turn_id", "reason", "duration_ms"])?
            }
            Some("thread_rolled_back") => compact_payload_keys(raw, &["type", "num_turns"])?,
            Some("thread_settings_applied") => {
                compact_payload_keys(raw, &["type", "thread_settings"])?
            }
            Some("user_message") => compact_payload_keys(raw, &["type", "message"])?,
            Some("web_search_end") => compact_payload_keys(raw, &["type", "action", "query"])?,
            Some("sub_agent_activity") => compact_payload_keys(
                raw,
                &[
                    "type",
                    "occurred_at_ms",
                    "agent_thread_id",
                    "agent_path",
                    "kind",
                ],
            )?,
            Some(
                "agent_message" | "agent_reasoning" | "context_compacted" | "mcp_tool_call_end",
            ) => return Ok(None),
            _ => {
                let value = parse_payload_value(raw)?;
                value.as_object().cloned().unwrap_or_default()
            }
        };
        return Ok(Some(build_compact_record(
            record_type,
            timestamp,
            Some(payload),
        )));
    }

    if record_type == "response_item" {
        let Some(raw) = payload_raw else {
            return Ok(None);
        };
        match payload_type_text {
            Some("message") => {
                let parsed: MessageSelective<'_> = sonic_rs::from_str(raw).map_err(|error| {
                    anyhow!("unable to selectively parse message payload: {error}")
                })?;
                let role = parsed.role.as_deref();
                let keep = match role {
                    Some("user" | "assistant") => true,
                    Some("developer") => parsed.content.as_ref().is_some_and(|content| {
                        content
                            .as_raw_str()
                            .contains("You are an agent in a team of agents")
                    }),
                    _ => false,
                };
                if !keep {
                    return Ok(None);
                }
                let payload = compact_message_payload(parsed)?;
                return Ok(Some(build_compact_record(
                    record_type,
                    timestamp,
                    Some(payload),
                )));
            }
            Some("agent_message") => {
                let parsed: MessageSelective<'_> = sonic_rs::from_str(raw).map_err(|error| {
                    anyhow!("unable to selectively parse agent_message payload: {error}")
                })?;
                return Ok(Some(build_compact_record(
                    record_type,
                    timestamp,
                    Some(compact_message_payload(parsed)?),
                )));
            }
            Some("custom_tool_call") => {
                let parsed: ToolCallSelective<'_> = sonic_rs::from_str(raw).map_err(|error| {
                    anyhow!("unable to selectively parse custom_tool_call payload: {error}")
                })?;
                if parsed.name.as_deref() == Some("apply_patch") {
                    return Ok(None);
                }
                let Some(input_text) = lazy_to_text(parsed.input.as_ref())? else {
                    return Ok(None);
                };
                let contains_command = input_text.contains("tools.shell_command")
                    || input_text.contains("tools.exec_command");
                let contains_plan = input_text.contains("tools.update_plan");
                let contains_goal = exec_goal_tool_name(Some(&input_text)).is_some();
                let mode = if custom_shell_attachment_read(&input_text) || contains_goal {
                    Some(ToolOutputMode::Full)
                } else if contains_command {
                    Some(ToolOutputMode::ShellSummary)
                } else if contains_plan || input_text.contains("tools.web__run") {
                    Some(ToolOutputMode::Drop)
                } else {
                    None
                };
                let Some(mode) = mode else { return Ok(None) };
                let call_id = parsed.call_id.as_deref().map(str::to_string);
                if let Some(call_id) = call_id.as_deref() {
                    state.custom_output_modes.insert(call_id.to_string(), mode);
                }
                let arguments = lazy_to_value(parsed.arguments.as_ref())?;
                let metadata =
                    lazy_to_value(parsed.internal_chat_message_metadata_passthrough.as_ref())?;
                let payload = compact_tool_call_payload(
                    parsed,
                    Some(Value::String(input_text)),
                    arguments,
                    metadata,
                );
                return Ok(Some(build_compact_record(
                    record_type,
                    timestamp,
                    Some(payload),
                )));
            }
            Some("custom_tool_call_output") => {
                let parsed: ToolOutputSelective<'_> = sonic_rs::from_str(raw).map_err(|error| {
                    anyhow!("unable to selectively parse custom_tool_call_output payload: {error}")
                })?;
                let mode = parsed
                    .call_id
                    .as_deref()
                    .and_then(|call_id| state.custom_output_modes.remove(call_id))
                    .unwrap_or(ToolOutputMode::Drop);
                let payload = match mode {
                    ToolOutputMode::Full => {
                        let output = lazy_to_value(parsed.output.as_ref())?;
                        compact_tool_output_payload(parsed, output)
                    }
                    ToolOutputMode::ShellSummary => {
                        let summary_text = lazy_to_message_text(parsed.output.as_ref())?;
                        let exact_text = lazy_to_tool_output_text(parsed.output.as_ref())?;
                        compact_shell_output_payload(
                            parsed,
                            summary_text.as_deref(),
                            exact_text.as_deref(),
                        )
                    }
                    ToolOutputMode::Drop => return Ok(None),
                };
                return Ok(Some(build_compact_record(
                    record_type,
                    timestamp,
                    Some(payload),
                )));
            }
            Some("function_call") => {
                let parsed: ToolCallSelective<'_> = sonic_rs::from_str(raw).map_err(|error| {
                    anyhow!("unable to selectively parse function_call payload: {error}")
                })?;
                let name = parsed.name.as_deref().unwrap_or_default();
                let exec_input_text = if name == "exec" {
                    lazy_to_text(parsed.input.as_ref())?
                } else {
                    None
                };
                let exec_goal_wrapper = exec_goal_tool_name(exec_input_text.as_deref()).is_some();
                let potentially_relevant = matches!(
                    name,
                    "create_goal"
                        | "update_goal"
                        | "get_goal"
                        | "update_plan"
                        | "shell_command"
                        | "request_user_input"
                        | "request_user_input_async"
                ) || exec_goal_wrapper
                    || agent_function_name(name);
                if !potentially_relevant {
                    return Ok(None);
                }
                let arguments = lazy_to_value(parsed.arguments.as_ref())?;
                let mode = if matches!(
                    name,
                    "create_goal"
                        | "update_goal"
                        | "get_goal"
                        | "request_user_input"
                        | "request_user_input_async"
                ) || exec_goal_wrapper
                    || agent_function_name(name)
                {
                    Some(ToolOutputMode::Full)
                } else if name == "update_plan" {
                    Some(ToolOutputMode::Drop)
                } else if name == "shell_command"
                    && function_shell_attachment_read(arguments.as_ref())
                {
                    Some(ToolOutputMode::Full)
                } else if name == "shell_command" {
                    Some(ToolOutputMode::ShellSummary)
                } else {
                    None
                };
                let Some(mode) = mode else { return Ok(None) };
                let call_id = parsed.call_id.as_deref().map(str::to_string);
                if let Some(call_id) = call_id.as_deref() {
                    state
                        .function_output_modes
                        .insert(call_id.to_string(), mode);
                }
                let input = if let Some(exec_input_text) = exec_input_text {
                    Some(Value::String(exec_input_text))
                } else {
                    lazy_to_value(parsed.input.as_ref())?
                };
                let metadata =
                    lazy_to_value(parsed.internal_chat_message_metadata_passthrough.as_ref())?;
                let payload = compact_tool_call_payload(parsed, input, arguments, metadata);
                return Ok(Some(build_compact_record(
                    record_type,
                    timestamp,
                    Some(payload),
                )));
            }
            Some("function_call_output") => {
                let parsed: ToolOutputSelective<'_> = sonic_rs::from_str(raw).map_err(|error| {
                    anyhow!("unable to selectively parse function_call_output payload: {error}")
                })?;
                let mode = parsed
                    .call_id
                    .as_deref()
                    .and_then(|call_id| state.function_output_modes.remove(call_id))
                    .unwrap_or(ToolOutputMode::Drop);
                let payload = match mode {
                    ToolOutputMode::Full => {
                        let output = lazy_to_value(parsed.output.as_ref())?;
                        compact_tool_output_payload(parsed, output)
                    }
                    ToolOutputMode::ShellSummary => {
                        let summary_text = lazy_to_message_text(parsed.output.as_ref())?;
                        let exact_text = lazy_to_tool_output_text(parsed.output.as_ref())?;
                        compact_shell_output_payload(
                            parsed,
                            summary_text.as_deref(),
                            exact_text.as_deref(),
                        )
                    }
                    ToolOutputMode::Drop => return Ok(None),
                };
                return Ok(Some(build_compact_record(
                    record_type,
                    timestamp,
                    Some(payload),
                )));
            }
            Some("reasoning" | "tool_search_call" | "tool_search_output") => return Ok(None),
            _ => {
                let value = parse_payload_value(raw)?;
                return Ok(Some(build_compact_record(
                    record_type,
                    timestamp,
                    Some(value.as_object().cloned().unwrap_or_default()),
                )));
            }
        }
    }

    Ok(None)
}

fn parse_rollout_line(line: &[u8], state: &mut RolloutIngestionState) -> Result<Option<Value>> {
    let envelope: RolloutEnvelope<'_> = match sonic_rs::from_slice(line) {
        Ok(value) => value,
        Err(fast_error) => {
            let fallback: Value = serde_json::from_slice(line).map_err(|fallback_error| {
                anyhow!("sonic-rs envelope parse failed ({fast_error}); serde_json fallback failed ({fallback_error})")
            })?;
            return Ok(Some(fallback));
        }
    };
    let Some(record_type) = envelope.record_type.as_deref() else {
        let fallback: Value = parse_payload_value(std::str::from_utf8(line)?)?;
        return Ok(Some(fallback));
    };
    let timestamp = envelope.timestamp.as_deref();
    let payload_raw = envelope.payload.as_ref().map(|value| value.as_raw_str());
    let payload_type = if let Some(raw) = payload_raw {
        sonic_rs::from_str::<PayloadKind<'_>>(raw)
            .ok()
            .and_then(|payload| payload.kind.map(|value| value.into_owned()))
    } else {
        None
    };
    parse_rollout_known_fields(
        record_type,
        timestamp,
        payload_raw,
        payload_type.as_deref(),
        state,
    )
}

fn source_data_quality_from_path(path: &Path) -> Result<SourceDataQuality> {
    let file = open_shared_read(path)?;
    let stored_len = file
        .metadata()
        .with_context(|| format!("unable to inspect {}", path.display()))?
        .len();
    let (mut reader, snapshot_len): (Box<dyn BufRead>, u64) =
        if rollout_codec::is_compressed_rollout(path) {
            let bytes = rollout_codec::read_rollout_snapshot(path)?;
            let len = bytes.len() as u64;
            (Box::new(BufReader::new(std::io::Cursor::new(bytes))), len)
        } else {
            (Box::new(BufReader::new(file.take(stored_len))), stored_len)
        };
    let mut quality = SourceDataQuality::default();
    let mut buffer = Vec::with_capacity(16 * 1024);
    let mut line_number = 0usize;
    let mut bytes_read = 0u64;

    loop {
        buffer.clear();
        let read = reader
            .read_until(b'\n', &mut buffer)
            .with_context(|| format!("unable to stream {}", path.display()))?;
        if read == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(read as u64);
        line_number += 1;
        if buffer.ends_with(b"\n") {
            buffer.pop();
        }
        if buffer.ends_with(b"\r") {
            buffer.pop();
        }
        if buffer.iter().all(u8::is_ascii_whitespace)
            || !line_may_contain_replacement_character(&buffer)
        {
            continue;
        }
        let is_final_physical_line = bytes_read >= snapshot_len;
        let record = match serde_json::from_slice::<Value>(&buffer) {
            Ok(record) => record,
            Err(_) if is_final_physical_line && !rollout_codec::is_compressed_rollout(path) => {
                break;
            }
            Err(error) => {
                bail!(
                    "Invalid JSON at line {line_number} in {} while checking source quality: {error}",
                    path.display()
                )
            }
        };
        let Some(record_map) = record.as_object() else {
            continue;
        };
        if value_str(record_map.get("type")) != Some("response_item") {
            continue;
        }
        let Some(payload) = record_map.get("payload").and_then(Value::as_object) else {
            continue;
        };
        if !matches!(
            value_str(payload.get("type")),
            Some("function_call_output" | "custom_tool_call_output")
        ) {
            continue;
        }
        let count = payload
            .get("output")
            .map(replacement_character_count)
            .unwrap_or(0);
        if count > 0 {
            quality.source_unicode_replacement_chars += count;
            quality.affected_tool_outputs += 1;
        }
    }

    Ok(quality)
}

fn read_rollout_jsonl_legacy(path: &Path) -> Result<Records> {
    let file = open_shared_read(path)?;
    let stored_len = file
        .metadata()
        .with_context(|| format!("unable to inspect {}", path.display()))?
        .len();
    let (mut reader, snapshot_len): (Box<dyn BufRead>, u64) =
        if rollout_codec::is_compressed_rollout(path) {
            let bytes = rollout_codec::read_rollout_snapshot(path)?;
            let len = bytes.len() as u64;
            (Box::new(BufReader::new(std::io::Cursor::new(bytes))), len)
        } else {
            (Box::new(BufReader::new(file.take(stored_len))), stored_len)
        };
    let mut records = Vec::new();
    let mut state = RolloutIngestionState::default();
    let mut buffer = Vec::with_capacity(16 * 1024);
    let mut line_number = 0usize;
    let mut bytes_read = 0u64;

    loop {
        buffer.clear();
        let read = reader
            .read_until(b'\n', &mut buffer)
            .with_context(|| format!("unable to stream {}", path.display()))?;
        if read == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(read as u64);
        line_number += 1;
        if buffer.ends_with(b"\n") {
            buffer.pop();
        }
        if buffer.ends_with(b"\r") {
            buffer.pop();
        }
        if buffer.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let is_final_physical_line = bytes_read >= snapshot_len;
        match parse_rollout_line(&buffer, &mut state) {
            Ok(Some(record)) => records.push((line_number, record)),
            Ok(None) => {}
            Err(_) if is_final_physical_line && !rollout_codec::is_compressed_rollout(path) => {
                eprintln!(
                    "WARNING: Ignoring an incomplete final JSON line in {}.",
                    path.display()
                );
                break;
            }
            Err(error) => bail!(
                "Invalid JSON at line {line_number} in {}: {error}",
                path.display()
            ),
        }
    }
    Ok(records)
}

fn read_rollout_jsonl(path: &Path) -> Result<Records> {
    let bytes = rollout_codec::read_rollout_snapshot(path)?;

    let mut records = Vec::new();
    let mut state = RolloutIngestionState::default();
    let mut line_number = 0usize;
    let mut consumed = 0usize;

    for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
        line_number += 1;
        consumed = consumed.saturating_add(segment.len());
        let mut line = segment;
        if line.ends_with(b"\n") {
            line = &line[..line.len() - 1];
        }
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let is_final_physical_line = consumed >= bytes.len();
        match parse_rollout_line_prefix(line, &mut state) {
            Ok(Some(record)) => records.push((line_number, record)),
            Ok(None) => {}
            Err(_) if is_final_physical_line && !rollout_codec::is_compressed_rollout(path) => {
                eprintln!(
                    "WARNING: Ignoring an incomplete final JSON line in {}.",
                    path.display()
                );
                break;
            }
            Err(error) => {
                bail!(
                    "Invalid JSON at line {line_number} in {}: {error}",
                    path.display()
                )
            }
        }
    }
    Ok(records)
}

fn read_generic_jsonl(path: &Path) -> Result<Records> {
    let bytes = read_shared_bytes(path)?;
    let mut records = Vec::new();
    let mut start = 0usize;
    let mut line_number = 0usize;
    while start < bytes.len() {
        line_number += 1;
        let relative_end = bytes[start..].iter().position(|b| *b == b'\n');
        let (end, has_newline) = match relative_end {
            Some(offset) => (start + offset, true),
            None => (bytes.len(), false),
        };
        let mut line = &bytes[start..end];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if !line.iter().all(u8::is_ascii_whitespace) {
            let is_final_physical_line = !has_newline || end + 1 == bytes.len();
            match serde_json::from_slice::<Value>(line) {
                Ok(Value::Object(map)) => records.push((line_number, Value::Object(map))),
                Ok(_) => bail!(
                    "JSON value at line {line_number} in {} is not an object",
                    path.display()
                ),
                Err(_) if is_final_physical_line => {
                    eprintln!(
                        "WARNING: Ignoring an incomplete final JSON line in {}.",
                        path.display()
                    );
                    break;
                }
                Err(error) => bail!(
                    "Invalid JSON at line {line_number} in {}: {error}",
                    path.display()
                ),
            }
        }
        if !has_newline {
            break;
        }
        start = end + 1;
    }
    Ok(records)
}

fn read_title_index(path: &Path) -> Result<HashMap<String, String>> {
    let mut result = HashMap::new();
    for (_, record) in read_generic_jsonl(path)? {
        let map = record.as_object().expect("object checked");
        let Some(id) = value_str(map.get("id")) else {
            continue;
        };
        let Some(name) = value_str(map.get("thread_name")) else {
            continue;
        };
        result.insert(id.to_string(), name.to_string());
    }
    Ok(result)
}

fn matches_target_session(
    meta: &Obj,
    target_cwd_normalized: &str,
    selected_ids: Option<&HashSet<String>>,
) -> bool {
    if let Some(selected) = selected_ids {
        return value_str(meta.get("id"))
            .or_else(|| value_str(meta.get("session_id")))
            .is_some_and(|id| selected.contains(id));
    }
    value_str(meta.get("cwd"))
        .is_some_and(|cwd| normalize_windows_path(cwd) == target_cwd_normalized)
}

fn matching_session_meta<'a>(
    records: &'a Records,
    target_cwd_normalized: &str,
    selected_ids: Option<&HashSet<String>>,
) -> Option<&'a Obj> {
    for (_, record) in records {
        let map = record.as_object()?;
        if value_str(map.get("type")) != Some("session_meta") {
            continue;
        }
        let candidate = map.get("payload")?.as_object()?;
        return matches_target_session(candidate, target_cwd_normalized, selected_ids)
            .then_some(candidate);
    }
    None
}

fn test_turn_effective(
    turn_id: Option<&str>,
    has_turn_events: bool,
    effective_turns: &HashSet<String>,
) -> bool {
    !has_turn_events || turn_id.is_none() || turn_id.is_some_and(|id| effective_turns.contains(id))
}

fn turn_id_from_message_payload(payload: &Obj) -> Option<String> {
    payload
        .get("internal_chat_message_metadata_passthrough")
        .and_then(Value::as_object)
        .and_then(|metadata| value_str(metadata.get("turn_id")))
        .map(str::to_string)
}

fn turn_id_from_tool_payload(payload: &Obj, fallback: Option<&str>) -> Option<String> {
    turn_id_from_message_payload(payload).or_else(|| fallback.map(str::to_string))
}

#[derive(Clone, Debug)]
struct RawMessage {
    role: String,
    phase: Option<String>,
    turn_id: Option<String>,
    content: String,
    time: Option<Timestamp>,
    sequence: usize,
    source_line: usize,
}

#[derive(Clone, Debug)]
struct PlanItem {
    turn_id: String,
    source_item_id: Option<String>,
    text: String,
    time: Option<Timestamp>,
    sequence: usize,
}

#[derive(Clone, Debug)]
struct TurnInfo {
    turn_id: String,
    status: String,
    start_time: Option<Timestamp>,
    end_time: Option<Timestamp>,
    abort_reason: Option<String>,
    mode: Option<String>,
    duration_ms: Option<i64>,
    time_to_first_token_ms: Option<i64>,
    termination: Option<Value>,
    timestamp_conflict: Option<Value>,
}

#[derive(Clone, Debug)]
struct GoalInterruption {
    sequence: usize,
    turn_id: String,
    interrupted_at: Option<Timestamp>,
    termination: Value,
    resumed_at: Option<Timestamp>,
}

#[derive(Clone, Debug)]
struct GoalInfo {
    thread_id: String,
    created_turn_id: Option<String>,
    last_turn_id: Option<String>,
    objective: Option<String>,
    reported_status: Option<String>,
    status_source: String,
    created_at: Option<Timestamp>,
    updated_at: Option<Timestamp>,
    awaiting_task_start: bool,
    interruptions: Vec<GoalInterruption>,
    sequence: usize,
}

#[derive(Clone, Debug)]
struct RollbackInfo {
    sequence: usize,
    time: Option<Timestamp>,
    num_turns: usize,
    removed_turn_ids: Vec<String>,
}

#[derive(Clone, Debug)]
struct ChangeInfo {
    turn_id: Option<String>,
    time: Option<Timestamp>,
    sequence: usize,
    path: String,
    operation: Option<String>,
    success: bool,
}

#[derive(Clone, Debug)]
struct Activity {
    activity_type: String,
    turn_id: Option<String>,
    time: Option<Timestamp>,
    end_time: Option<Timestamp>,
    sequence: usize,
    executions: Vec<Obj>,
    exit_codes: Vec<i64>,
    output_truncated: bool,
    result_mapping: String,
    invocation_count: usize,
    parse_warnings: Vec<String>,
    source_call_id: Option<String>,
    failure_status: Option<String>,
    failure_diagnostic: Option<Value>,
    action: Option<String>,
    queries: Vec<String>,
    query: Option<String>,
    pattern: Option<String>,
    references: Vec<String>,
    urls: Vec<String>,
    path: Option<String>,
    operation: Option<String>,
    success: Option<bool>,
}

impl Activity {
    fn new(
        activity_type: &str,
        turn_id: Option<String>,
        time: Option<Timestamp>,
        sequence: usize,
    ) -> Self {
        Self {
            activity_type: activity_type.to_string(),
            turn_id,
            time,
            end_time: None,
            sequence,
            executions: Vec::new(),
            exit_codes: Vec::new(),
            output_truncated: false,
            result_mapping: "unknown".to_string(),
            invocation_count: 0,
            parse_warnings: Vec::new(),
            source_call_id: None,
            failure_status: None,
            failure_diagnostic: None,
            action: None,
            queries: Vec::new(),
            query: None,
            pattern: None,
            references: Vec::new(),
            urls: Vec::new(),
            path: None,
            operation: None,
            success: None,
        }
    }
}

#[derive(Clone, Debug)]
struct Session {
    session_id: String,
    root_session_id: String,
    thread_source: String,
    guardian_review: bool,
    parent_thread_id: Option<String>,
    forked_from_id: Option<String>,
    agent_nickname: Option<String>,
    agent_path: Option<String>,
    agent_depth: Option<i64>,
    agent_role: Option<String>,
    multi_agent_version: Option<String>,
    create_time: Option<Timestamp>,
    last_message_time: Timestamp,
    messages: Vec<Value>,
    raw_messages: Vec<RawMessage>,
    plans: Vec<Value>,
    plan_updates: Vec<Value>,
    goals: Vec<Value>,
    goal_updates: Vec<Value>,
    user_input_requests: Vec<Value>,
    agent_events: Vec<Value>,
    delegation: Option<Value>,
    reviews: Vec<Value>,
    memory_citations: Vec<Value>,
    writing_blocks: Vec<Value>,
    ui_actions: Vec<Value>,
    ui_parse_warnings: Vec<Value>,
    rollbacks: Vec<Value>,
    turns: Vec<Value>,
    codex: Value,
    workspace: Value,
    context: Option<Value>,
    usage: Option<Value>,
    activity_detail: String,
    session_state: String,
    changes: Vec<Value>,
    activities: Vec<Value>,
    execution_profile: Option<Value>,
    data_quality: Option<Value>,
    source_path: PathBuf,
    subagent_sessions: Vec<Session>,
    internal_session_ids: Vec<String>,
}

fn is_guardian_review_source(source: &Value) -> bool {
    source
        .as_object()
        .and_then(|source| source.get("subagent"))
        .and_then(Value::as_object)
        .and_then(|subagent| subagent.get("other"))
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("guardian"))
}

// ---------------------------------------------------------------------------
// UI artifact parsing
// ---------------------------------------------------------------------------

fn index_inside_markdown_fence(text: &str, index: usize) -> bool {
    if index == 0 {
        return false;
    }
    let prefix = text[..index].replace("\r\n", "\n").replace('\r', "\n");
    static FENCE_RE: OnceLock<Regex> = OnceLock::new();
    let fence_re = residual_static_regex(&FENCE_RE, r"^[ \t]{0,3}(`{3,}|~{3,})");
    let fence_re = fence_re.as_regex();
    let mut inside = false;
    let mut fence_char = '\0';
    let mut fence_len = 0usize;
    for line in prefix.split('\n') {
        let Some(captures) = fence_re.captures(line) else {
            continue;
        };
        let marker = captures.get(1).expect("capture").as_str();
        let marker_char = marker.chars().next().unwrap_or('\0');
        if !inside {
            inside = true;
            fence_char = marker_char;
            fence_len = marker.len();
        } else if marker_char == fence_char && marker.len() >= fence_len {
            inside = false;
        }
    }
    inside
}

fn index_inside_writing_block(text: &str, index: usize) -> bool {
    if index == 0 {
        return false;
    }
    let prefix = text[..index].replace("\r\n", "\n").replace('\r', "\n");
    static OPENING_RE: OnceLock<Regex> = OnceLock::new();
    static CLOSING_RE: OnceLock<Regex> = OnceLock::new();
    let opening = residual_static_regex(&OPENING_RE, r"^[ \t]*:::writing\{");
    let closing = residual_static_regex(&CLOSING_RE, r"^[ \t]*:::[ \t]*$");
    let opening = opening.as_regex();
    let closing = closing.as_regex();
    let mut inside = false;
    for line in prefix.split('\n') {
        if !inside && opening.is_match(line) {
            inside = true;
        } else if inside && closing.is_match(line) {
            inside = false;
        }
    }
    inside
}

fn tag_is_standalone_line(text: &str, start: usize, length: usize) -> bool {
    let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = text[start + length..]
        .find('\n')
        .map_or(text.len(), |offset| start + length + offset);
    text[line_start..start].trim().is_empty() && text[start + length..line_end].trim().is_empty()
}

fn remove_ranges(text: &str, ranges: &[(usize, usize)]) -> String {
    let mut result = text.to_string();
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|(start, _)| std::cmp::Reverse(*start));
    for (start, length) in sorted {
        if start <= result.len() && start + length <= result.len() {
            result.replace_range(start..start + length, "");
        }
    }
    result.trim().to_string()
}

fn parse_ui_attributes(text: &str) -> (bool, Obj) {
    let mut attrs = Obj::new();
    let mut i = 0usize;
    let mut valid = true;

    while i < text.len() {
        while let Some((ch, len)) = next_char(text, i) {
            if !ch.is_whitespace() {
                break;
            }
            i += len;
        }
        if i >= text.len() {
            break;
        }

        let name_start = i;
        while let Some((ch, len)) = next_char(text, i) {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                i += len;
            } else {
                break;
            }
        }
        if i == name_start {
            valid = false;
            break;
        }
        let name = &text[name_start..i];

        while let Some((ch, len)) = next_char(text, i) {
            if !ch.is_whitespace() {
                break;
            }
            i += len;
        }
        if text.as_bytes().get(i) != Some(&b'=') {
            valid = false;
            break;
        }
        i += 1;

        while let Some((ch, len)) = next_char(text, i) {
            if !ch.is_whitespace() {
                break;
            }
            i += len;
        }
        if i >= text.len() {
            valid = false;
            break;
        }

        let value = if text.as_bytes().get(i) == Some(&b'"') {
            i += 1;
            let mut out = String::new();
            let mut escaped = false;
            let mut closed = false;
            while let Some((ch, len)) = next_char(text, i) {
                if escaped {
                    match ch.to_ascii_lowercase() {
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        other => out.push(other),
                    }
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    i += len;
                    closed = true;
                    break;
                } else {
                    out.push(ch);
                }
                i += len;
            }
            if !closed {
                valid = false;
                break;
            }
            Value::String(out)
        } else {
            let value_start = i;
            while let Some((ch, len)) = next_char(text, i) {
                if ch.is_whitespace() {
                    break;
                }
                i += len;
            }
            let raw = &text[value_start..i];
            if let Ok(number) = raw.parse::<i64>() {
                Value::Number(number.into())
            } else if raw.eq_ignore_ascii_case("true") {
                Value::Bool(true)
            } else if raw.eq_ignore_ascii_case("false") {
                Value::Bool(false)
            } else {
                Value::String(raw.to_string())
            }
        };

        if attrs.contains_key(name) {
            valid = false;
            break;
        }
        attrs.insert(name.to_string(), value);
    }

    (valid, attrs)
}

#[derive(Clone, Debug)]
struct MemoryCitationBlock {
    valid: bool,
    start: usize,
    length: usize,
    entries: Vec<Value>,
    rollout_ids: Vec<String>,
    warning: Option<String>,
}

fn memory_citation_blocks(text: &str) -> Vec<MemoryCitationBlock> {
    let mut result = Vec::new();
    if text.trim().is_empty() {
        return result;
    }
    let open_tag = "<oai-mem-citation>";
    let close_tag = "</oai-mem-citation>";
    static ENTRY_RE: OnceLock<Regex> = OnceLock::new();
    let entry_re = residual_static_regex(
        &ENTRY_RE,
        r"^(?P<path>.+):(?P<start>[0-9]+)(?:-(?P<end>[0-9]+))?\|note=\[(?P<note>.*)\]$",
    );
    let entry_re = entry_re.as_regex();
    let mut search = 0usize;
    while search < text.len() {
        let Some(relative_start) = text[search..].find(open_tag) else {
            break;
        };
        let start = search + relative_start;
        if index_inside_markdown_fence(text, start) || index_inside_writing_block(text, start) {
            search = start + open_tag.len();
            continue;
        }
        let Some(relative_close) = text[start + open_tag.len()..].find(close_tag) else {
            result.push(MemoryCitationBlock {
                valid: false,
                start,
                length: 0,
                entries: Vec::new(),
                rollout_ids: Vec::new(),
                warning: Some("malformed_memory_citation".to_string()),
            });
            break;
        };
        let close = start + open_tag.len() + relative_close;
        let end = close + close_tag.len();
        let block = text[start..end].replace("\r\n", "\n").replace('\r', "\n");
        let (eo, ec, ro, rc) = (
            "<citation_entries>",
            "</citation_entries>",
            "<rollout_ids>",
            "</rollout_ids>",
        );
        let es = block.find(eo);
        let ee = block.find(ec);
        let rs = block.find(ro);
        let re = block.find(rc);
        let mut valid = matches!((es, ee, rs, re), (Some(es), Some(ee), Some(rs), Some(re)) if ee > es && re > rs);
        let mut entries = Vec::new();
        let mut rollout_ids = Vec::new();
        if valid {
            let es = es.unwrap();
            let ee = ee.unwrap();
            let content = &block[es + eo.len()..ee];
            for line in content
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
            {
                let Some(captures) = entry_re.captures(line) else {
                    valid = false;
                    break;
                };
                let line_start = captures["start"].parse::<i64>().unwrap_or(0);
                let line_end = captures
                    .name("end")
                    .and_then(|m| m.as_str().parse::<i64>().ok())
                    .unwrap_or(line_start);
                entries.push(Value::Object(obj!(
                    "path" => Value::String(captures["path"].to_string()),
                    "line_start" => Value::Number(line_start.into()),
                    "line_end" => Value::Number(line_end.into()),
                    "note" => Value::String(captures["note"].to_string()),
                )));
            }
        }
        if valid {
            let rs = rs.unwrap();
            let re = re.unwrap();
            let mut seen = HashSet::new();
            for line in block[rs + ro.len()..re].lines() {
                let item = line.trim();
                if !item.is_empty() && seen.insert(item.to_string()) {
                    rollout_ids.push(item.to_string());
                }
            }
        }
        result.push(MemoryCitationBlock {
            valid,
            start,
            length: end - start,
            entries,
            rollout_ids,
            warning: (!valid).then(|| "malformed_memory_citation".to_string()),
        });
        search = end;
    }
    result
}

#[derive(Clone, Debug)]
struct ProposedPlanBlock {
    valid: bool,
    start: usize,
    length: usize,
    content: Option<String>,
    warning: Option<String>,
}

fn proposed_plan_block(text: &str) -> Option<ProposedPlanBlock> {
    if text.trim().is_empty() {
        return None;
    }
    let open_tag = "<proposed_plan>";
    let close_tag = "</proposed_plan>";
    let mut search = 0usize;
    while search < text.len() {
        let start = search + text[search..].find(open_tag)?;
        if index_inside_markdown_fence(text, start)
            || index_inside_writing_block(text, start)
            || !tag_is_standalone_line(text, start, open_tag.len())
        {
            search = start + open_tag.len();
            continue;
        }
        let Some(relative_close) = text[start + open_tag.len()..].find(close_tag) else {
            return Some(ProposedPlanBlock {
                valid: false,
                start,
                length: 0,
                content: None,
                warning: Some("malformed_proposed_plan".to_string()),
            });
        };
        let close = start + open_tag.len() + relative_close;
        if !tag_is_standalone_line(text, close, close_tag.len()) {
            search = close + close_tag.len();
            continue;
        }
        return Some(ProposedPlanBlock {
            valid: true,
            start,
            length: close + close_tag.len() - start,
            content: Some(text[start + open_tag.len()..close].trim().to_string()),
            warning: None,
        });
    }
    None
}

fn plan_title(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for line in normalized.lines().map(str::trim) {
        if let Some(title) = line.strip_prefix("# ") {
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    normalized
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(200).collect())
}

#[derive(Clone, Debug)]
struct WritingBlock {
    start: usize,
    length: usize,
    content: String,
    attributes: Obj,
    ends_crlf: bool,
    ends_lf: bool,
}

fn writing_blocks_in_text(text: &str) -> (String, Vec<WritingBlock>, Vec<String>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?ms)^[ \t]*:::writing\{(?P<attrs>[^\r\n}]*)\}[ \t]*\r?\n(?P<content>.*?)(?:\r?\n)[ \t]*:::[ \t]*(?:\r?\n|$)").unwrap());
    static OPENING_RE: OnceLock<Regex> = OnceLock::new();
    let opening_re = residual_static_regex(&OPENING_RE, r"(?m)^[ \t]*:::writing\{");
    let opening_re = opening_re.as_regex();
    let mut blocks = Vec::new();
    let mut warnings = Vec::new();
    for captures in re.captures_iter(text) {
        let whole = captures.get(0).unwrap();
        if index_inside_markdown_fence(text, whole.start()) {
            continue;
        }
        let (valid, attrs) = parse_ui_attributes(captures.name("attrs").unwrap().as_str());
        if !valid {
            if !warnings
                .iter()
                .any(|warning| warning == "malformed_writing_block")
            {
                warnings.push("malformed_writing_block".to_string());
            }
            continue;
        }
        let raw = whole.as_str();
        blocks.push(WritingBlock {
            start: whole.start(),
            length: whole.end() - whole.start(),
            content: captures.name("content").unwrap().as_str().to_string(),
            attributes: attrs,
            ends_crlf: raw.ends_with("\r\n"),
            ends_lf: raw.ends_with('\n'),
        });
    }
    let outside_openings = opening_re
        .find_iter(text)
        .filter(|m| !index_inside_markdown_fence(text, m.start()))
        .count();
    if outside_openings > blocks.len()
        && !warnings
            .iter()
            .any(|warning| warning == "malformed_writing_block")
    {
        warnings.push("malformed_writing_block".to_string());
    }
    let mut clean = text.to_string();
    for block in blocks.iter().rev() {
        let mut replacement = block.content.clone();
        if block.ends_crlf {
            replacement.push_str("\r\n");
        } else if block.ends_lf {
            replacement.push('\n');
        }
        clean.replace_range(block.start..block.start + block.length, &replacement);
    }
    (clean.trim().to_string(), blocks, warnings)
}

#[derive(Clone, Debug)]
struct UiAction {
    start: usize,
    length: usize,
    action_type: String,
    attributes: Obj,
    directive_hash: String,
}

fn ui_actions_in_text(text: &str) -> (String, Vec<UiAction>, Vec<String>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?m)^[ \t]*::(?P<name>git-stage|git-commit|git-create-branch|git-push|git-create-pr|created-thread)\{(?P<attrs>[^\r\n}]*)\}[ \t]*(?:\r?\n|$)").unwrap());
    let mut actions = Vec::new();
    let mut warnings = Vec::new();
    for captures in re.captures_iter(text) {
        let whole = captures.get(0).unwrap();
        if index_inside_markdown_fence(text, whole.start())
            || index_inside_writing_block(text, whole.start())
        {
            continue;
        }
        let (valid, attrs) = parse_ui_attributes(captures.name("attrs").unwrap().as_str());
        if !valid {
            warnings.push("malformed_ui_action".to_string());
            continue;
        }
        actions.push(UiAction {
            start: whole.start(),
            length: whole.end() - whole.start(),
            action_type: captures.name("name").unwrap().as_str().replace('-', "_"),
            attributes: attrs,
            directive_hash: sha256_text(Some(whole.as_str().trim())),
        });
    }
    let mut clean = text.to_string();
    for action in actions.iter().rev() {
        clean.replace_range(action.start..action.start + action.length, "");
    }
    (clean.trim().to_string(), actions, warnings)
}

#[derive(Clone, Debug)]
struct DirectiveSpan {
    start: usize,
    end: usize,
    attr_text: String,
    line_start: usize,
    line_end: usize,
}

fn directive_spans(text: &str, marker: &str) -> Vec<DirectiveSpan> {
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    let mut search = 0usize;
    while search < text.len() {
        let Some(relative_start) = text[search..].find(marker) else {
            break;
        };
        let start = search + relative_start;
        if index_inside_markdown_fence(text, start) || index_inside_writing_block(text, start) {
            search = start + marker.len();
            continue;
        }
        let mut i = start + marker.len();
        let mut in_quote = false;
        let mut escaped = false;
        let mut end = None;
        while i < bytes.len() {
            let ch = bytes[i] as char;
            if in_quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_quote = false;
                }
            } else if ch == '"' {
                in_quote = true;
            } else if ch == '}' {
                end = Some(i);
                break;
            }
            i += 1;
        }
        let Some(end) = end else { break };
        let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
        let line_end = text[end + 1..]
            .find('\n')
            .map_or(text.len(), |offset| end + 1 + offset);
        result.push(DirectiveSpan {
            start,
            end,
            attr_text: text[start + marker.len()..end].to_string(),
            line_start,
            line_end,
        });
        search = end + 1;
    }
    result
}

fn code_review_findings(text: &str) -> Vec<Obj> {
    let marker = "::code-comment{";
    let mut result = Vec::new();
    for span in directive_spans(text, marker) {
        if !text[span.line_start..span.start].trim().is_empty()
            || !text[span.end + 1..span.line_end].trim().is_empty()
        {
            continue;
        }
        let (valid, attrs) = parse_ui_attributes(&span.attr_text);
        if !valid {
            continue;
        }
        let Some(title) = value_str(attrs.get("title")) else {
            continue;
        };
        let Some(body) = value_str(attrs.get("body")) else {
            continue;
        };
        let Some(file) = value_str(attrs.get("file")) else {
            continue;
        };
        let mut finding = obj!(
            "title" => Value::String(title.to_string()),
            "body" => Value::String(body.to_string()),
            "file" => Value::String(file.to_string()),
        );
        for name in ["start", "end", "priority"] {
            if let Some(number) = int_value(attrs.get(name), Some(0)) {
                finding.insert(name.to_string(), Value::Number(number.into()));
            }
        }
        result.push(finding);
    }
    result
}

fn remove_code_review_directives(text: &str) -> String {
    let marker = "::code-comment{";
    let mut ranges = Vec::new();
    for span in directive_spans(text, marker) {
        if !text[span.line_start..span.start].trim().is_empty()
            || !text[span.end + 1..span.line_end].trim().is_empty()
        {
            continue;
        }
        let (valid, attrs) = parse_ui_attributes(&span.attr_text);
        let required = valid
            && ["title", "body", "file"]
                .iter()
                .all(|key| value_str(attrs.get(*key)).is_some());
        if required {
            let range_end = if span.line_end < text.len() && text.as_bytes()[span.line_end] == b'\n'
            {
                span.line_end + 1
            } else {
                span.line_end
            };
            ranges.push((span.line_start, range_end - span.line_start));
        }
    }
    remove_ranges(text, &ranges)
}

// ---------------------------------------------------------------------------
// JavaScript shell-command extraction
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct JsLiteral {
    value: String,
    next_index: usize,
    quote: char,
    has_unpaired_surrogate: bool,
}

fn next_char(text: &str, index: usize) -> Option<(char, usize)> {
    let ch = text.get(index..)?.chars().next()?;
    Some((ch, ch.len_utf8()))
}

fn read_js_string(text: &str, start: usize) -> Option<JsLiteral> {
    let (quote, quote_len) = next_char(text, start)?;
    if !matches!(quote, '"' | '\'' | '`') {
        return None;
    }
    let mut out = String::new();
    let mut has_unpaired_surrogate = false;
    let mut index = start + quote_len;
    while index < text.len() {
        let (character, char_len) = next_char(text, index)?;

        if quote == '`' && character == '$' && text.as_bytes().get(index + 1) == Some(&b'{') {
            out.push_str("${");
            index += 2;
            let mut brace_depth = 1i32;
            while index < text.len() && brace_depth > 0 {
                let (inner, inner_len) = next_char(text, index)?;
                if matches!(inner, '"' | '\'' | '`') {
                    let nested = read_js_string(text, index)?;
                    out.push_str(&text[index..nested.next_index]);
                    index = nested.next_index;
                    continue;
                }
                if inner == '/' && index + 1 < text.len() {
                    let next = text.as_bytes()[index + 1] as char;
                    if next == '/' {
                        while index < text.len() {
                            let (comment_character, len) = next_char(text, index)?;
                            out.push(comment_character);
                            index += len;
                            if matches!(comment_character, '\n' | '\r') {
                                break;
                            }
                        }
                        continue;
                    }
                    if next == '*' {
                        out.push_str("/*");
                        index += 2;
                        while index < text.len() {
                            let (comment_character, len) = next_char(text, index)?;
                            out.push(comment_character);
                            index += len;
                            if comment_character == '*' && text.as_bytes().get(index) == Some(&b'/')
                            {
                                out.push('/');
                                index += 1;
                                break;
                            }
                        }
                        continue;
                    }
                    // Regex literals inside template interpolation may
                    // contain quote characters, e.g. replace(/'/g, "''").
                    // Copy them verbatim instead of treating those quotes as
                    // nested JavaScript strings.
                    let previous = out.chars().rev().find(|item| !item.is_whitespace());
                    if previous.is_some_and(|item| "([{=,:;!?&|".contains(item)) {
                        let regex_start = index;
                        index += 1;
                        let mut escaped = false;
                        let mut in_class = false;
                        let mut closed = false;
                        while index < text.len() {
                            let (regex_character, len) = next_char(text, index)?;
                            if escaped {
                                escaped = false;
                                index += len;
                                continue;
                            }
                            if regex_character == '\\' {
                                escaped = true;
                                index += len;
                                continue;
                            }
                            if regex_character == '[' {
                                in_class = true;
                            } else if regex_character == ']' {
                                in_class = false;
                            } else if regex_character == '/' && !in_class {
                                index += len;
                                while index < text.len() {
                                    let (flag, flag_len) = next_char(text, index)?;
                                    if !flag.is_ascii_alphabetic() {
                                        break;
                                    }
                                    index += flag_len;
                                }
                                out.push_str(&text[regex_start..index]);
                                closed = true;
                                break;
                            }
                            index += len;
                        }
                        if !closed {
                            return None;
                        }
                        continue;
                    }
                }
                if inner == '{' {
                    brace_depth += 1;
                    out.push(inner);
                    index += inner_len;
                    continue;
                }
                if inner == '}' {
                    brace_depth -= 1;
                    out.push(inner);
                    index += inner_len;
                    continue;
                }
                out.push(inner);
                index += inner_len;
            }
            if brace_depth != 0 {
                return None;
            }
            continue;
        }

        if character == quote {
            return Some(JsLiteral {
                value: out,
                next_index: index + char_len,
                quote,
                has_unpaired_surrogate,
            });
        }
        if character != '\\' {
            out.push(character);
            index += char_len;
            continue;
        }

        index += char_len;
        if index >= text.len() {
            out.push('\\');
            break;
        }
        let (escaped, escaped_len) = next_char(text, index)?;
        let escaped_key = escaped.to_ascii_lowercase();
        match escaped_key {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000c}'),
            'v' => out.push('\u{000b}'),
            '0' => out.push('\0'),
            'x' => {
                let hex_start = index + escaped_len;
                let hex_end = hex_start + 2;
                if hex_end <= text.len() {
                    let raw = &text[hex_start..hex_end];
                    if raw.bytes().all(|b| b.is_ascii_hexdigit()) {
                        if let Ok(value) = u8::from_str_radix(raw, 16) {
                            out.push(value as char);
                            index = hex_end;
                            continue;
                        }
                    }
                }
                out.push(escaped);
            }
            'u' => {
                let hex_start = index + escaped_len;
                let hex_end = hex_start + 4;
                if hex_end <= text.len() {
                    let raw = &text[hex_start..hex_end];
                    if raw.bytes().all(|b| b.is_ascii_hexdigit()) {
                        let code_unit = u16::from_str_radix(raw, 16).ok()?;
                        if (0xD800..=0xDBFF).contains(&code_unit) {
                            let second_slash = hex_end;
                            if second_slash + 6 <= text.len()
                                && text.as_bytes()[second_slash] == b'\\'
                                && (text.as_bytes()[second_slash + 1] as char)
                                    .eq_ignore_ascii_case(&'u')
                            {
                                let low_raw = &text[second_slash + 2..second_slash + 6];
                                if low_raw.bytes().all(|b| b.is_ascii_hexdigit()) {
                                    let low = u16::from_str_radix(low_raw, 16).ok()?;
                                    if (0xDC00..=0xDFFF).contains(&low) {
                                        let scalar = 0x10000
                                            + (((code_unit - 0xD800) as u32) << 10)
                                            + (low - 0xDC00) as u32;
                                        out.push(char::from_u32(scalar)?);
                                        index = second_slash + 6;
                                        continue;
                                    }
                                }
                            }
                            out.push_str("\\u");
                            out.push_str(raw);
                            has_unpaired_surrogate = true;
                            index = hex_end;
                            continue;
                        }
                        if (0xDC00..=0xDFFF).contains(&code_unit) {
                            out.push_str("\\u");
                            out.push_str(raw);
                            has_unpaired_surrogate = true;
                            index = hex_end;
                            continue;
                        }
                        if let Some(ch) = char::from_u32(code_unit as u32) {
                            out.push(ch);
                            index = hex_end;
                            continue;
                        }
                    }
                }
                out.push(escaped);
            }
            _ if escaped == '\r' => {
                if text.as_bytes().get(index + escaped_len) == Some(&b'\n') {
                    index += 1;
                }
            }
            _ if escaped == '\n' => {}
            _ => out.push(escaped),
        }
        index += escaped_len;
    }
    None
}

fn js_code_mask(text: &str) -> String {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(43, text.len());

    if text.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < text.len() {
        let Some((character, char_len)) = next_char(text, index) else {
            break;
        };
        if matches!(character, '"' | '\'' | '`') {
            if let Some(literal) = read_js_string(text, index) {
                let count = literal.next_index - index;
                if count >= 2 {
                    out.push(character);
                    if count > 2 {
                        out.push_str(&" ".repeat(count - 2));
                    }
                    out.push(character);
                } else {
                    out.push(' ');
                }
                index = literal.next_index;
                continue;
            }
            out.push_str(&" ".repeat(char_len));
            index += char_len;
            continue;
        }
        if character == '/' && index + 1 < text.len() {
            let next = text.as_bytes()[index + 1] as char;
            if next == '/' {
                let start_index = index;
                index += 2;
                while index < text.len() {
                    let (ch, len) = next_char(text, index).unwrap();
                    if matches!(ch, '\n' | '\r') {
                        break;
                    }
                    index += len;
                }
                out.push_str(&" ".repeat(index - start_index));
                continue;
            }
            if next == '*' {
                let start_index = index;
                index += 2;
                while index + 1 < text.len()
                    && !(text.as_bytes()[index] == b'*' && text.as_bytes()[index + 1] == b'/')
                {
                    index += next_char(text, index).map_or(1, |(_, len)| len);
                }
                if index + 1 < text.len() {
                    index += 2;
                }
                out.push_str(&" ".repeat(index - start_index));
                continue;
            }
        }
        out.push(character);
        index += char_len;
    }
    out
}

fn read_identifier(text: &str, start: usize) -> Option<(String, usize)> {
    let mut index = start;
    let (first, first_len) = next_char(text, index)?;
    if !(first.is_ascii_alphabetic() || matches!(first, '_' | '$')) {
        return None;
    }
    index += first_len;
    while index < text.len() {
        let (ch, len) = next_char(text, index)?;
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$') {
            index += len;
        } else {
            break;
        }
    }
    Some((text[start..index].to_string(), index))
}

fn read_number_token(text: &str, start: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut index = start;
    if bytes.get(index) == Some(&b'-') {
        index += 1;
    }
    let digits_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    if index == digits_start {
        return None;
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == fraction_start {
            return None;
        }
    }
    Some((text[start..index].to_string(), index))
}

fn read_js_simple_value(
    text: &str,
    start: usize,
    assignments: &HashMap<String, String>,
) -> Option<(String, usize)> {
    let mut index = start;
    while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
        index += 1;
    }
    if index >= text.len() {
        return None;
    }
    if let Some(literal) = read_js_string(text, index) {
        if literal.has_unpaired_surrogate {
            return None;
        }
        return Some((literal.value, literal.next_index));
    }
    if let Some((name, end)) = read_identifier(text, index) {
        return Some((assignments.get(&name).cloned().unwrap_or(name), end));
    }
    read_number_token(text, index)
}

#[derive(Debug)]
enum RegexHandle {
    Cached(&'static Regex),
    Owned(Regex),
}

impl RegexHandle {
    fn as_regex(&self) -> &Regex {
        match self {
            Self::Cached(regex) => regex,
            Self::Owned(regex) => regex,
        }
    }
}

fn js_static_regex_for_mode(
    cache: &'static OnceLock<Regex>,
    pattern: &'static str,
    legacy_compile_per_call: bool,
) -> RegexHandle {
    if legacy_compile_per_call {
        RegexHandle::Owned(Regex::new(pattern).expect("valid static JavaScript parser regex"))
    } else {
        RegexHandle::Cached(
            cache
                .get_or_init(|| Regex::new(pattern).expect("valid static JavaScript parser regex")),
        )
    }
}

fn js_static_regex(cache: &'static OnceLock<Regex>, pattern: &'static str) -> RegexHandle {
    js_static_regex_for_mode(
        cache,
        pattern,
        LEGACY_JS_REGEX_ENABLED.load(AtomicOrdering::Relaxed),
    )
}

fn residual_static_regex_for_mode(
    cache: &'static OnceLock<Regex>,
    pattern: &'static str,
    legacy_compile_per_call: bool,
) -> RegexHandle {
    if legacy_compile_per_call {
        RegexHandle::Owned(Regex::new(pattern).expect("valid residual static regex"))
    } else {
        RegexHandle::Cached(
            cache.get_or_init(|| Regex::new(pattern).expect("valid residual static regex")),
        )
    }
}

fn residual_static_regex(cache: &'static OnceLock<Regex>, pattern: &'static str) -> RegexHandle {
    residual_static_regex_for_mode(
        cache,
        pattern,
        LEGACY_RESIDUAL_REGEX_ENABLED.load(AtomicOrdering::Relaxed),
    )
}

static JS_TEMPLATE_EXPR_RE: OnceLock<Regex> = OnceLock::new();
static JS_IDENTIFIER_RE: OnceLock<Regex> = OnceLock::new();
static JS_STRING_ASSIGNMENT_RE: OnceLock<Regex> = OnceLock::new();
static JS_ARRAY_ASSIGNMENT_RE: OnceLock<Regex> = OnceLock::new();
static JS_FOR_OF_RE: OnceLock<Regex> = OnceLock::new();
static JS_CLOSE_AND_OPEN_RE: OnceLock<Regex> = OnceLock::new();
static JS_BINDING_IDENTIFIER_RE: OnceLock<Regex> = OnceLock::new();
static JS_PROPERTY_COMMAND_RE: OnceLock<Regex> = OnceLock::new();
static JS_PROPERTY_WORKDIR_RE: OnceLock<Regex> = OnceLock::new();
static JS_PROPERTY_TIMEOUT_RE: OnceLock<Regex> = OnceLock::new();
static JS_PROPERTY_REF_ID_RE: OnceLock<Regex> = OnceLock::new();
static JS_PROPERTY_URL_RE: OnceLock<Regex> = OnceLock::new();
static JS_PROPERTY_OPEN_RE: OnceLock<Regex> = OnceLock::new();
static JS_PROPERTY_CLICK_RE: OnceLock<Regex> = OnceLock::new();

#[derive(Clone, Debug)]
struct JsTemplateExpansion {
    success: bool,
    value: Option<String>,
}

fn expand_js_template(
    template: Option<&str>,
    assignments: &HashMap<String, String>,
) -> JsTemplateExpansion {
    let Some(template) = template else {
        return JsTemplateExpansion {
            success: false,
            value: None,
        };
    };
    let re = js_static_regex(&JS_TEMPLATE_EXPR_RE, r"\$\{([^{}]+)\}");
    let captures: Vec<_> = re.as_regex().captures_iter(template).collect();
    if captures.is_empty() {
        return JsTemplateExpansion {
            success: true,
            value: Some(template.to_string()),
        };
    }
    let mut result = template.to_string();
    let identifier_re = js_static_regex(&JS_IDENTIFIER_RE, r"^[A-Za-z_$][A-Za-z0-9_$]*$");
    for capture in captures {
        let expression = capture.get(1).unwrap().as_str().trim();
        let Some(value) = assignments.get(expression) else {
            return JsTemplateExpansion {
                success: false,
                value: Some(template.to_string()),
            };
        };
        if !identifier_re.as_regex().is_match(expression) {
            return JsTemplateExpansion {
                success: false,
                value: Some(template.to_string()),
            };
        }
        result = result.replace(capture.get(0).unwrap().as_str(), value);
    }
    if result.contains("${") {
        JsTemplateExpansion {
            success: false,
            value: Some(template.to_string()),
        }
    } else {
        JsTemplateExpansion {
            success: true,
            value: Some(result),
        }
    }
}

fn js_string_assignments(text: &str, mask: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    if text.is_empty() || mask.is_empty() {
        return result;
    }
    let re = js_static_regex(
        &JS_STRING_ASSIGNMENT_RE,
        r"(?i)(?:^|[^A-Za-z0-9_$])(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*",
    );
    for captures in re.as_regex().captures_iter(mask) {
        let whole = captures.get(0).unwrap();
        if let Some(literal) = read_js_string(text, whole.end()) {
            if !literal.has_unpaired_surrogate {
                result.insert(captures.get(1).unwrap().as_str().to_string(), literal.value);
            }
        }
    }
    result
}

#[derive(Clone, Debug)]
enum JsArrayValue {
    Scalar(String),
    Array(Vec<JsArrayValue>),
    Unresolved,
}

fn parse_js_array(text: &str, start: usize) -> Option<(Vec<JsArrayValue>, usize)> {
    if text.as_bytes().get(start) != Some(&b'[') {
        return None;
    }
    let mut values = Vec::new();
    let mut index = start + 1;
    while index < text.len() {
        while index < text.len()
            && (text.as_bytes()[index].is_ascii_whitespace() || text.as_bytes()[index] == b',')
        {
            index += 1;
        }
        if index >= text.len() {
            return None;
        }
        if text.as_bytes()[index] == b']' {
            return Some((values, index + 1));
        }
        if text.as_bytes()[index] == b'[' {
            let (nested, end) = parse_js_array(text, index)?;
            values.push(JsArrayValue::Array(nested));
            index = end;
            continue;
        }
        if let Some(literal) = read_js_string(text, index) {
            if literal.has_unpaired_surrogate {
                return None;
            }
            values.push(JsArrayValue::Scalar(literal.value));
            index = literal.next_index;
            continue;
        }
        if let Some((number, end)) = read_number_token(text, index) {
            values.push(JsArrayValue::Scalar(number));
            index = end;
            continue;
        }
        if let Some((identifier, end)) = read_identifier(text, index) {
            values.push(JsArrayValue::Scalar(identifier));
            index = end;
            continue;
        }
        return None;
    }
    None
}

fn js_array_assignments(text: &str, mask: &str) -> HashMap<String, Vec<JsArrayValue>> {
    let mut result = HashMap::new();
    let re = js_static_regex(
        &JS_ARRAY_ASSIGNMENT_RE,
        r"(?i)(?:^|[^A-Za-z0-9_$])(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*\[",
    );
    for captures in re.as_regex().captures_iter(mask) {
        let whole = captures.get(0).unwrap();
        if let Some((values, _)) = parse_js_array(text, whole.end() - 1) {
            result.insert(captures.get(1).unwrap().as_str().to_string(), values);
        }
    }
    result
}

#[derive(Clone, Copy, Debug)]
struct PropertyMatch {
    start: usize,
    end: usize,
}

fn property_regex_for_mode(name: &str, legacy_compile_per_call: bool) -> Option<RegexHandle> {
    if name.trim().is_empty() {
        return None;
    }
    if legacy_compile_per_call {
        let pattern = format!(r"(?i)(?:^|[^A-Za-z0-9_$])({})\s*:\s*", regex::escape(name));
        return Some(RegexHandle::Owned(
            Regex::new(&pattern).expect("valid JavaScript property regex"),
        ));
    }
    let known = match name {
        "command" => Some((
            &JS_PROPERTY_COMMAND_RE,
            r"(?i)(?:^|[^A-Za-z0-9_$])(command)\s*:\s*",
        )),
        "workdir" => Some((
            &JS_PROPERTY_WORKDIR_RE,
            r"(?i)(?:^|[^A-Za-z0-9_$])(workdir)\s*:\s*",
        )),
        "timeout_ms" => Some((
            &JS_PROPERTY_TIMEOUT_RE,
            r"(?i)(?:^|[^A-Za-z0-9_$])(timeout_ms)\s*:\s*",
        )),
        "ref_id" => Some((
            &JS_PROPERTY_REF_ID_RE,
            r"(?i)(?:^|[^A-Za-z0-9_$])(ref_id)\s*:\s*",
        )),
        "url" => Some((&JS_PROPERTY_URL_RE, r"(?i)(?:^|[^A-Za-z0-9_$])(url)\s*:\s*")),
        "open" => Some((
            &JS_PROPERTY_OPEN_RE,
            r"(?i)(?:^|[^A-Za-z0-9_$])(open)\s*:\s*",
        )),
        "click" => Some((
            &JS_PROPERTY_CLICK_RE,
            r"(?i)(?:^|[^A-Za-z0-9_$])(click)\s*:\s*",
        )),
        _ => None,
    };
    if let Some((cache, pattern)) = known {
        return Some(js_static_regex_for_mode(cache, pattern, false));
    }
    let pattern = format!(r"(?i)(?:^|[^A-Za-z0-9_$])({})\s*:\s*", regex::escape(name));
    Some(RegexHandle::Owned(
        Regex::new(&pattern).expect("valid JavaScript property regex"),
    ))
}

fn property_matches(mask: &str, name: &str) -> Vec<PropertyMatch> {
    if mask.is_empty() {
        return Vec::new();
    }
    let Some(re) =
        property_regex_for_mode(name, LEGACY_JS_REGEX_ENABLED.load(AtomicOrdering::Relaxed))
    else {
        return Vec::new();
    };
    re.as_regex()
        .captures_iter(mask)
        .map(|captures| PropertyMatch {
            start: captures.get(1).unwrap().start(),
            end: captures.get(0).unwrap().end(),
        })
        .collect()
}

fn js_property_matches(text: &str, mask: &str, name: &str) -> Vec<PropertyMatch> {
    let mut matches = property_matches(mask, name);
    let mut index = 0usize;
    while index < text.len() {
        if !matches!(text.as_bytes()[index], b'"' | b'\'') {
            index += 1;
            continue;
        }
        let Some(literal) = read_js_string(text, index) else {
            index += 1;
            continue;
        };
        let mut cursor = literal.next_index;
        while cursor < text.len() && text.as_bytes()[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if literal.value.eq_ignore_ascii_case(name) && text.as_bytes().get(cursor) == Some(&b':') {
            cursor += 1;
            while cursor < text.len() && text.as_bytes()[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            matches.push(PropertyMatch {
                start: index,
                end: cursor,
            });
        }
        index = literal.next_index;
    }
    matches.sort_by_key(|item| item.start);
    matches.dedup_by_key(|item| item.start);
    matches
}

fn bracket_end(mask: &str, open_index: usize) -> Option<usize> {
    let mut depth = 1i32;
    let mut index = open_index + 1;
    while index < mask.len() {
        match mask.as_bytes()[index] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn add_unique_warning(warnings: &mut Vec<String>, message: &str) {
    if !message.trim().is_empty() && !warnings.iter().any(|item| item == message) {
        warnings.push(message.to_string());
    }
}

fn read_js_resolved_value(
    text: &str,
    start: usize,
    assignments: &HashMap<String, String>,
    warnings: &mut Vec<String>,
) -> Option<(String, usize)> {
    let mut index = start;
    while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
        index += 1;
    }
    if index >= text.len() {
        return None;
    }
    if let Some(literal) = read_js_string(text, index) {
        if literal.has_unpaired_surrogate {
            add_unique_warning(
                warnings,
                "JavaScript string contained unpaired UTF-16 surrogate escape(s); the value was not emitted as a resolved execution field.",
            );
            return None;
        }
        let mut value = literal.value;
        if literal.quote == '`' {
            let expanded = expand_js_template(Some(&value), assignments);
            if !expanded.success {
                return None;
            }
            value = expanded.value.unwrap_or_default();
        }
        return Some((value, literal.next_index));
    }
    if let Some((name, end)) = read_identifier(text, index) {
        return assignments.get(&name).cloned().map(|value| (value, end));
    }
    read_number_token(text, index)
}

fn new_command_execution(
    text: &str,
    value_start: usize,
    assignments: &HashMap<String, String>,
    concrete_kind: &str,
    workdir: Option<String>,
    timeout: Option<i64>,
    warnings: &mut Vec<String>,
) -> Option<Obj> {
    let mut index = value_start;
    while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
        index += 1;
    }
    if index >= text.len() {
        return None;
    }
    let mut output = Obj::new();
    if let Some(literal) = read_js_string(text, index) {
        let command_text = literal.value;
        if literal.has_unpaired_surrogate {
            output.insert(
                "source_kind".to_string(),
                Value::String("dynamic_script".to_string()),
            );
            output.insert(
                "language".to_string(),
                Value::String("javascript".to_string()),
            );
            output.insert(
                "parse_status".to_string(),
                Value::String("not_fully_resolved".to_string()),
            );
            output.insert("summary".to_string(), Value::String("Command string contained unpaired UTF-16 surrogate escape(s); concrete command text was not emitted.".to_string()));
            add_unique_warning(
                warnings,
                "Command string contained unpaired UTF-16 surrogate escape(s); concrete command text was not emitted.",
            );
        } else if literal.quote == '`' {
            let expanded = expand_js_template(Some(&command_text), assignments);
            if expanded.success && !expanded.value.as_deref().unwrap_or_default().contains("${") {
                output.insert(
                    "source_kind".to_string(),
                    Value::String(
                        if concrete_kind == "expanded_loop" {
                            "expanded_loop"
                        } else {
                            "literal"
                        }
                        .to_string(),
                    ),
                );
                output.insert(
                    "command".to_string(),
                    Value::String(expanded.value.unwrap_or_default()),
                );
            } else {
                output.insert(
                    "source_kind".to_string(),
                    Value::String("dynamic_template".to_string()),
                );
                output.insert("command_template".to_string(), Value::String(command_text));
                add_unique_warning(
                    warnings,
                    "Command contained unresolved JavaScript interpolation and was preserved as a dynamic template.",
                );
            }
        } else {
            output.insert(
                "source_kind".to_string(),
                Value::String("literal".to_string()),
            );
            output.insert("command".to_string(), Value::String(command_text));
        }
    } else {
        let (name, _) = read_identifier(text, index)?;
        if let Some(assigned) = assignments.get(&name) {
            if assigned.contains("${") {
                output.insert(
                    "source_kind".to_string(),
                    Value::String("dynamic_template".to_string()),
                );
                output.insert(
                    "command_template".to_string(),
                    Value::String(assigned.clone()),
                );
                add_unique_warning(
                    warnings,
                    "Command assignment contained unresolved JavaScript interpolation and was preserved as a dynamic template.",
                );
            } else {
                output.insert(
                    "source_kind".to_string(),
                    Value::String(concrete_kind.to_string()),
                );
                output.insert("command".to_string(), Value::String(assigned.clone()));
            }
        } else {
            output.insert(
                "source_kind".to_string(),
                Value::String("dynamic_template".to_string()),
            );
            output.insert("command_template".to_string(), Value::String(name));
            add_unique_warning(
                warnings,
                "Dynamic command expression could not be resolved.",
            );
        }
    }
    if let Some(workdir) = workdir.filter(|value| !value.trim().is_empty()) {
        output.insert("working_directory".to_string(), Value::String(workdir));
    }
    if let Some(timeout) = timeout {
        output.insert("timeout_ms".to_string(), Value::Number(timeout.into()));
    }
    Some(output)
}

fn brace_end(mask: &str, open_index: usize) -> Option<usize> {
    let mut depth = 1i32;
    let mut index = open_index + 1;
    while index < mask.len() {
        match mask.as_bytes()[index] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn loop_ranges_and_executions(
    text: &str,
    mask: &str,
    assignments: &HashMap<String, String>,
    arrays: &HashMap<String, Vec<JsArrayValue>>,
    warnings: &mut Vec<String>,
) -> (Vec<Obj>, Vec<(usize, usize)>) {
    let mut executions = Vec::new();
    let mut ranges = Vec::new();
    let re = js_static_regex(
        &JS_FOR_OF_RE,
        r"(?is)for\s*\(\s*const\s+(\[[^\]]+\]|[A-Za-z_$][A-Za-z0-9_$]*)\s+of\s*",
    );
    for captures in re.as_regex().captures_iter(mask) {
        let whole = captures.get(0).unwrap();
        let binding_text = captures.get(1).unwrap().as_str().trim();
        let mut iterable_start = whole.end();
        while iterable_start < mask.len() && mask.as_bytes()[iterable_start].is_ascii_whitespace() {
            iterable_start += 1;
        }
        let mut iterable_values: Option<Vec<JsArrayValue>> = None;
        let mut iterable_end = iterable_start;
        let mut iterable_resolved = false;
        if mask.as_bytes().get(iterable_start) == Some(&b'[') {
            if let Some((values, end)) = parse_js_array(text, iterable_start) {
                iterable_values = Some(values);
                iterable_end = end;
                iterable_resolved = true;
            }
        } else if let Some((name, end)) = read_identifier(mask, iterable_start) {
            iterable_end = end;
            if let Some(values) = arrays.get(&name) {
                iterable_values = Some(values.clone());
                iterable_resolved = true;
            }
        }
        let close_and_open = js_static_regex(&JS_CLOSE_AND_OPEN_RE, r"^\s*\)\s*\{");
        let Some(body_open_match) = close_and_open.as_regex().find(&mask[iterable_end..]) else {
            continue;
        };
        let body_open = iterable_end + body_open_match.end() - 1;
        let Some(body_close) = brace_end(mask, body_open) else {
            continue;
        };
        ranges.push((whole.start(), body_close + 1));
        let binding_names_re =
            js_static_regex(&JS_BINDING_IDENTIFIER_RE, r"[A-Za-z_$][A-Za-z0-9_$]*");
        let binding_names: Vec<String> = binding_names_re
            .as_regex()
            .find_iter(binding_text)
            .map(|m| m.as_str().to_string())
            .collect();
        if binding_names.is_empty() {
            continue;
        }
        let body_mask = &mask[body_open + 1..body_close];
        let body_text_start = body_open + 1;
        let body_text = &text[body_open + 1..body_close];
        let body_commands = js_property_matches(body_text, body_mask, "command");
        let body_workdirs = js_property_matches(body_text, body_mask, "workdir");
        let body_timeouts = js_property_matches(body_text, body_mask, "timeout_ms");
        if body_commands.is_empty() {
            continue;
        }
        if !iterable_resolved {
            iterable_values = Some(vec![JsArrayValue::Unresolved]);
            add_unique_warning(
                warnings,
                "Loop iterable could not be resolved; command was preserved as a template.",
            );
        }
        for item in iterable_values.unwrap_or_default() {
            let mut local_assignments = assignments.clone();
            let item_unresolved = matches!(item, JsArrayValue::Unresolved);
            if !item_unresolved {
                if binding_names.len() == 1 {
                    let value = match item {
                        JsArrayValue::Scalar(value) => value,
                        JsArrayValue::Array(values) => values
                            .into_iter()
                            .map(|value| match value {
                                JsArrayValue::Scalar(v) => v,
                                _ => String::new(),
                            })
                            .collect::<Vec<_>>()
                            .join(","),
                        JsArrayValue::Unresolved => String::new(),
                    };
                    local_assignments.insert(binding_names[0].clone(), value);
                } else if let JsArrayValue::Array(row) = item {
                    for (binding, value) in binding_names.iter().zip(row.into_iter()) {
                        let text_value = match value {
                            JsArrayValue::Scalar(value) => value,
                            _ => String::new(),
                        };
                        local_assignments.insert(binding.clone(), text_value);
                    }
                }
            }
            for (command_index, command_match) in body_commands.iter().enumerate() {
                let region_start = command_match.start;
                let region_end = body_commands
                    .get(command_index + 1)
                    .map_or(body_mask.len(), |item| item.start);
                let mut working_directory = None;
                for workdir_match in &body_workdirs {
                    if workdir_match.start <= region_start || workdir_match.start >= region_end {
                        continue;
                    }
                    working_directory = read_js_resolved_value(
                        text,
                        body_text_start + workdir_match.end,
                        &local_assignments,
                        warnings,
                    )
                    .map(|value| value.0);
                    break;
                }
                let mut timeout = None;
                for timeout_match in &body_timeouts {
                    if timeout_match.start <= region_start || timeout_match.start >= region_end {
                        continue;
                    }
                    timeout = read_js_resolved_value(
                        text,
                        body_text_start + timeout_match.end,
                        &local_assignments,
                        warnings,
                    )
                    .and_then(|value| value.0.parse::<i64>().ok());
                    break;
                }
                if let Some(execution) = new_command_execution(
                    text,
                    body_text_start + command_match.end,
                    &local_assignments,
                    "expanded_loop",
                    working_directory,
                    timeout,
                    warnings,
                ) {
                    executions.push(execution);
                }
            }
        }
    }
    (executions, ranges)
}

fn index_in_ranges(index: usize, ranges: &[(usize, usize)]) -> bool {
    ranges
        .iter()
        .any(|(start, end)| *start <= index && index < *end)
}

fn fast_literal_shell_command_data(text: &str) -> Option<(Vec<Obj>, Vec<String>)> {
    let marker = "tools.shell_command(";
    if text.is_empty() || text.matches(marker).count() != 1 {
        return None;
    }
    let mut index = text.find(marker)? + marker.len();
    while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
        index += 1;
    }
    if text.as_bytes().get(index) != Some(&b'{') {
        return None;
    }
    index += 1;
    let mut values: HashMap<String, Value> = HashMap::new();
    loop {
        while index < text.len()
            && (text.as_bytes()[index].is_ascii_whitespace() || text.as_bytes()[index] == b',')
        {
            index += 1;
        }
        if index >= text.len() {
            return None;
        }
        if text.as_bytes()[index] == b'}' {
            index += 1;
            break;
        }
        let (key, end) = read_identifier(text, index)?;
        index = end;
        while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
            index += 1;
        }
        if text.as_bytes().get(index) != Some(&b':') {
            return None;
        }
        index += 1;
        while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= text.len() {
            return None;
        }
        let value = if matches!(text.as_bytes()[index], b'"' | b'\'' | b'`') {
            let literal = read_js_string(text, index)?;
            if literal.has_unpaired_surrogate || literal.quote == '`' {
                return None;
            }
            index = literal.next_index;
            Value::String(literal.value)
        } else if let Some((number, end)) = read_number_token(text, index) {
            index = end;
            if number.contains('.') {
                Value::Number(Number::from_f64(number.parse::<f64>().ok()?)?)
            } else {
                Value::Number(number.parse::<i64>().ok()?.into())
            }
        } else if text[index..].starts_with("true") {
            index += 4;
            Value::Bool(true)
        } else if text[index..].starts_with("false") {
            index += 5;
            Value::Bool(false)
        } else if text[index..].starts_with("null") {
            index += 4;
            Value::Null
        } else {
            return None;
        };
        values.insert(key, value);
    }
    while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
        index += 1;
    }
    if text.as_bytes().get(index) != Some(&b')') {
        return None;
    }
    let command = values.get("command")?.as_str()?.to_string();
    let mut execution = obj!(
        "source_kind" => Value::String("literal".to_string()),
        "command" => Value::String(command),
    );
    if let Some(workdir) = values
        .get("workdir")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        execution.insert(
            "working_directory".to_string(),
            Value::String(workdir.to_string()),
        );
    }
    if let Some(timeout) = values.get("timeout_ms").and_then(Value::as_i64) {
        execution.insert("timeout_ms".to_string(), Value::Number(timeout.into()));
    }
    Some((vec![execution], Vec::new()))
}

fn fast_literal_exec_command_data(text: &str) -> Option<(Vec<Obj>, Vec<String>)> {
    // r7: modern Codex emits a large number of simple literal
    // tools.exec_command({ cmd: "...", workdir: "...", timeout_ms: ... })
    // calls.  The legacy path normalizes the entire JavaScript string into the
    // shell-command grammar, which requires a full code mask, multiple regex
    // scans, a full String copy, and then a second parse.  This deliberately
    // narrow parser mirrors fast_literal_shell_command_data() and falls back
    // for every dynamic/quoted/template shape.
    let marker = "tools.exec_command(";
    if text.is_empty() {
        return None;
    }
    // Keep the fast path code-aware. A raw substring search can mistake
    // tools.exec_command(...) text inside a string/comment for an executable
    // call. One code mask is still materially cheaper than the legacy path,
    // which masks for normalization, rewrites a full String, and then masks
    // again inside javascript_command_data().
    let mask = js_code_mask(text);
    if mask.matches(marker).count() != 1 {
        return None;
    }
    let mut index = mask.find(marker)? + marker.len();
    while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
        index += 1;
    }
    if text.as_bytes().get(index) != Some(&b'{') {
        return None;
    }
    index += 1;

    let mut values: HashMap<String, Value> = HashMap::new();
    loop {
        while index < text.len()
            && (text.as_bytes()[index].is_ascii_whitespace() || text.as_bytes()[index] == b',')
        {
            index += 1;
        }
        if index >= text.len() {
            return None;
        }
        if text.as_bytes()[index] == b'}' {
            index += 1;
            break;
        }

        let (key, end) = read_identifier(text, index)?;
        index = end;
        while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
            index += 1;
        }
        if text.as_bytes().get(index) != Some(&b':') {
            return None;
        }
        index += 1;
        while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= text.len() {
            return None;
        }

        let value = if matches!(text.as_bytes()[index], b'"' | b'\'' | b'`') {
            let literal = read_js_string(text, index)?;
            if literal.has_unpaired_surrogate || literal.quote == '`' {
                return None;
            }
            index = literal.next_index;
            Value::String(literal.value)
        } else if let Some((number, end)) = read_number_token(text, index) {
            index = end;
            if number.contains('.') {
                Value::Number(Number::from_f64(number.parse::<f64>().ok()?)?)
            } else {
                Value::Number(number.parse::<i64>().ok()?.into())
            }
        } else if text[index..].starts_with("true") {
            index += 4;
            Value::Bool(true)
        } else if text[index..].starts_with("false") {
            index += 5;
            Value::Bool(false)
        } else if text[index..].starts_with("null") {
            index += 4;
            Value::Null
        } else {
            return None;
        };
        values.insert(key, value);
    }

    while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
        index += 1;
    }
    if text.as_bytes().get(index) != Some(&b')') {
        return None;
    }

    let command = values.get("cmd")?.as_str()?.to_string();
    let mut execution = obj!(
        "source_kind" => Value::String("literal".to_string()),
        "command" => Value::String(command),
    );
    if let Some(workdir) = values
        .get("workdir")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        execution.insert(
            "working_directory".to_string(),
            Value::String(workdir.to_string()),
        );
    }
    if let Some(timeout) = values.get("timeout_ms").and_then(Value::as_i64) {
        execution.insert("timeout_ms".to_string(), Value::Number(timeout.into()));
    }
    Some((vec![execution], Vec::new()))
}

fn javascript_command_data(text: &str) -> (Vec<Obj>, Vec<String>) {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(42, text.len());

    if let Some(result) = fast_literal_shell_command_data(text) {
        return result;
    }
    let mut executions = Vec::new();
    let mut warnings = Vec::new();
    if text.trim().is_empty() {
        return (executions, warnings);
    }
    let mask = js_code_mask(text);
    let assignments = js_string_assignments(text, &mask);
    let arrays = js_array_assignments(text, &mask);
    let (loop_executions, loop_ranges) =
        loop_ranges_and_executions(text, &mask, &assignments, &arrays, &mut warnings);
    executions.extend(loop_executions);
    let command_matches = js_property_matches(text, &mask, "command");
    let workdir_matches = js_property_matches(text, &mask, "workdir");
    let timeout_matches = js_property_matches(text, &mask, "timeout_ms");
    for (command_index, command_match) in command_matches.iter().enumerate() {
        if index_in_ranges(command_match.start, &loop_ranges) {
            continue;
        }
        let region_start = command_match.start;
        let region_end = command_matches
            .get(command_index + 1)
            .map_or(text.len(), |item| item.start);
        let mut workdir = None;
        for workdir_match in &workdir_matches {
            if workdir_match.start <= region_start || workdir_match.start >= region_end {
                continue;
            }
            workdir = read_js_resolved_value(text, workdir_match.end, &assignments, &mut warnings)
                .map(|value| value.0);
            break;
        }
        let mut timeout = None;
        for timeout_match in &timeout_matches {
            if timeout_match.start <= region_start || timeout_match.start >= region_end {
                continue;
            }
            timeout = read_js_resolved_value(text, timeout_match.end, &assignments, &mut warnings)
                .and_then(|value| value.0.parse::<i64>().ok());
            break;
        }
        if let Some(execution) = new_command_execution(
            text,
            command_match.end,
            &assignments,
            "literal",
            workdir,
            timeout,
            &mut warnings,
        ) {
            executions.push(execution);
        }
    }
    (executions, warnings)
}

fn normalize_exec_command_script(text: &str) -> String {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(44, text.len());

    if text.is_empty() || !text.contains("tools.exec_command") {
        return text.to_string();
    }
    #[derive(Debug)]
    struct Replacement {
        start: usize,
        end: usize,
        value: &'static str,
    }
    static EXEC_MARKER_RE: OnceLock<Regex> = OnceLock::new();
    static EXEC_CALL_RE: OnceLock<Regex> = OnceLock::new();
    static CMD_SHORTHAND_RE: OnceLock<Regex> = OnceLock::new();
    let marker_re = EXEC_MARKER_RE.get_or_init(|| {
        Regex::new(r"(?i)tools\s*\.\s*exec_command").expect("valid exec marker regex")
    });
    let call_re = EXEC_CALL_RE.get_or_init(|| {
        Regex::new(r"(?i)tools\s*\.\s*exec_command\s*\(").expect("valid exec call regex")
    });
    let shorthand_re = CMD_SHORTHAND_RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|,)\s*(?P<name>cmd)\s*(?:,|})").expect("valid cmd shorthand regex")
    });
    let mask = js_code_mask(text);
    let mut replacements = Vec::new();
    for matched in marker_re.find_iter(&mask) {
        replacements.push(Replacement {
            start: matched.start(),
            end: matched.end(),
            value: "tools.shell_command",
        });
    }
    for property_match in js_property_matches(text, &mask, "cmd") {
        let segment = &mask[property_match.start..property_match.end];
        if let Some(colon) = segment.rfind(':') {
            replacements.push(Replacement {
                start: property_match.start,
                end: property_match.start + colon,
                value: "command",
            });
        }
    }
    for call in call_re.find_iter(&mask) {
        let mut index = call.end();
        while index < mask.len() && mask.as_bytes()[index].is_ascii_whitespace() {
            index += 1;
        }
        if mask.as_bytes().get(index) != Some(&b'{') {
            continue;
        }
        let Some(close) = brace_end(&mask, index) else {
            continue;
        };
        for shorthand in shorthand_re.captures_iter(&mask[index + 1..=close]) {
            let Some(name) = shorthand.name("name") else {
                continue;
            };
            let start = index + 1 + name.start();
            replacements.push(Replacement {
                start,
                end: start + 3,
                value: "command: cmd",
            });
        }
    }
    replacements.sort_by(|left, right| {
        right
            .start
            .cmp(&left.start)
            .then_with(|| (right.end - right.start).cmp(&(left.end - left.start)))
    });
    let mut output = text.to_string();
    let mut boundary = text.len() + 1;
    for replacement in replacements {
        if replacement.end > boundary {
            continue;
        }
        output.replace_range(replacement.start..replacement.end, replacement.value);
        boundary = replacement.start;
    }
    output
}

fn javascript_exec_command_data_legacy(text: &str) -> (Vec<Obj>, Vec<String>) {
    if text.trim().is_empty() || !text.contains("tools.exec_command") {
        return (Vec::new(), Vec::new());
    }
    javascript_command_data(&normalize_exec_command_script(text))
}

fn javascript_exec_command_data(text: &str) -> (Vec<Obj>, Vec<String>) {
    if let Some(result) = fast_literal_exec_command_data(text) {
        return result;
    }
    javascript_exec_command_data_legacy(text)
}

fn javascript_plan_steps(text: &str, mask: &str, array_open: usize) -> Option<(Vec<Value>, usize)> {
    let array_close = bracket_end(mask, array_open)?;
    let mut steps = Vec::new();
    let mut index = array_open + 1;
    while index < array_close {
        if mask.as_bytes()[index] != b'{' {
            index += 1;
            continue;
        }
        let object_close = brace_end(mask, index)?;
        if object_close > array_close {
            return None;
        }
        let object_text = &text[index..=object_close];
        let object_mask = &mask[index..=object_close];
        let assignments = js_string_assignments(object_text, object_mask);
        let step_matches = js_property_matches(object_text, object_mask, "step");
        let status_matches = js_property_matches(object_text, object_mask, "status");
        if let Some(step_match) = step_matches.first() {
            if let Some((step_text, _)) =
                read_js_simple_value(object_text, step_match.end, &assignments)
            {
                if !step_text.trim().is_empty() {
                    let mut step = obj!("text" => Value::String(step_text));
                    if let Some(status_match) = status_matches.first() {
                        if let Some((status, _)) =
                            read_js_simple_value(object_text, status_match.end, &assignments)
                        {
                            if !status.trim().is_empty() {
                                step.insert("status".to_string(), Value::String(status));
                            }
                        }
                    }
                    steps.push(Value::Object(step));
                }
            }
        }
        index = object_close + 1;
    }
    Some((steps, array_close + 1))
}

fn javascript_plan_updates(text: &str) -> Vec<Obj> {
    if text.trim().is_empty() || !text.contains("tools.update_plan") {
        return Vec::new();
    }
    let mask = js_code_mask(text);
    let assignments = js_string_assignments(text, &mask);
    static UPDATE_PLAN_RE: OnceLock<Regex> = OnceLock::new();
    let marker_re = js_static_regex(&UPDATE_PLAN_RE, r"(?i)tools\s*\.\s*update_plan\s*\(");
    let mut updates = Vec::new();
    for marker in marker_re.as_regex().find_iter(&mask) {
        let Some(relative_open) = mask[marker.end()..].find('{') else {
            continue;
        };
        let object_open = marker.end() + relative_open;
        let Some(object_close) = brace_end(&mask, object_open) else {
            continue;
        };
        let object_text = &text[object_open..=object_close];
        let object_mask = &mask[object_open..=object_close];
        let mut update = Obj::new();
        if let Some(item) = js_property_matches(object_text, object_mask, "explanation").first() {
            if let Some((explanation, _)) =
                read_js_simple_value(object_text, item.end, &assignments)
            {
                if !explanation.trim().is_empty() {
                    update.insert("explanation".to_string(), Value::String(explanation));
                }
            }
        }
        if let Some(item) = js_property_matches(object_text, object_mask, "plan").first() {
            if let Some(relative_array) = object_mask[item.end..].find('[') {
                let array_open = item.end + relative_array;
                if let Some((steps, _)) =
                    javascript_plan_steps(object_text, object_mask, array_open)
                {
                    if !steps.is_empty() {
                        update.insert("steps".to_string(), Value::Array(steps));
                    }
                }
            }
        }
        if !update.is_empty() {
            updates.push(update);
        }
    }
    updates
}

fn javascript_property_string_values(text: &str, name: &str) -> Vec<String> {
    let mask = js_code_mask(text);
    let assignments = js_string_assignments(text, &mask);
    js_property_matches(text, &mask, name)
        .into_iter()
        .filter_map(|item| read_js_simple_value(text, item.end, &assignments).map(|value| value.0))
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn input_has_js_property(text: &str, name: &str) -> bool {
    let mask = js_code_mask(text);
    !js_property_matches(text, &mask, name).is_empty()
}

// ---------------------------------------------------------------------------
// Command output and activity compaction
// ---------------------------------------------------------------------------

fn command_execution_text(execution: &Obj) -> String {
    raw_str(execution.get("command"))
        .filter(|value| !value.trim().is_empty())
        .or_else(|| raw_str(execution.get("command_template")))
        .unwrap_or_default()
        .to_string()
}

fn working_directory_is_default(workdir: Option<&str>, workspace: &str) -> bool {
    let Some(workdir) = workdir.filter(|value| !value.trim().is_empty()) else {
        return true;
    };
    workdir
        .trim_end_matches(&['\\', '/'][..])
        .eq_ignore_ascii_case(workspace.trim_end_matches(&['\\', '/'][..]))
}

fn activity_regex_is_match(
    cache: &'static OnceLock<Regex>,
    pattern: &'static str,
    text: &str,
    legacy_compile_per_call: bool,
) -> bool {
    if legacy_compile_per_call {
        Regex::new(pattern)
            .expect("valid static regex")
            .is_match(text)
    } else {
        cache
            .get_or_init(|| Regex::new(pattern).expect("valid static regex"))
            .is_match(text)
    }
}

fn has_single_redirect(command: &str) -> bool {
    let bytes = command.as_bytes();
    for index in 0..bytes.len() {
        if bytes[index] != b'>' {
            continue;
        }
        let previous = index.checked_sub(1).and_then(|i| bytes.get(i)).copied();
        let next = bytes.get(index + 1).copied();
        if previous != Some(b'<') && next != Some(b'>') {
            return true;
        }
    }
    false
}

const ACTIVITY_VALIDATION_PATTERN: &str = r"(?i)(?:^|[;&|]\s*)(?:uv\s+run\s+)?(?:pytest|pyright|ruff|mypy)\b|(?:^|[;&|]\s*)python\s+-m\s+pytest\b|\bgit\s+diff\b[^\r\n;]*--check\b|\buv\s+lock\s+--check\b|\bcompileall\b";
const ACTIVITY_UNSAFE_PATTERN: &str = r"(?im)\b(?:Set-Content|Add-Content|Clear-Content|Out-File|New-Item|Set-Item|Remove-Item|Move-Item|Copy-Item|Rename-Item|Invoke-WebRequest|Invoke-RestMethod|Start-Process)\b|\bgit\s+(?:add|commit|push|reset|restore|checkout|clean|rebase|merge|branch|tag|switch|cherry-pick|revert)\b|\b(?:apply_patch|writealltext|writeallbytes|write_text|write_bytes|mkdir|touch|del|erase|rm|mv|cp)\b|(?:::|\.)(?:Delete|Move|Copy|Create|Append|Write)\s*\(|(?:^|[;&|]\s*)(?:python|py|node|pwsh|powershell|cmd|bash|sh)\b";
const ACTIVITY_GIT_INSPECTION_PATTERN: &str = r"(?i)\bgit\s+(?:status|diff|log|show|rev-parse|ls-files|check-ignore|config|remote|cat-file|ls-tree)\b";
const ACTIVITY_FILE_READ_PATTERN: &str = r"(?i)\b(?:Get-Content|Get-Item|Get-ChildItem|Resolve-Path|Test-Path|Select-String|Get-FileHash|Measure-Object|ConvertFrom-Json|Get-Command|rg\b|findstr\b|grep\b|cat\b|type\b|head\b|tail\b|wc\b|dir\b|ls\b)";

static ACTIVITY_VALIDATION_RE: OnceLock<Regex> = OnceLock::new();
static ACTIVITY_UNSAFE_RE: OnceLock<Regex> = OnceLock::new();
static ACTIVITY_GIT_INSPECTION_RE: OnceLock<Regex> = OnceLock::new();
static ACTIVITY_FILE_READ_RE: OnceLock<Regex> = OnceLock::new();

fn compact_command_category_impl(command: &str, legacy_compile_per_call: bool) -> &'static str {
    if command.trim().is_empty() {
        return "other";
    }
    if activity_regex_is_match(
        &ACTIVITY_VALIDATION_RE,
        ACTIVITY_VALIDATION_PATTERN,
        command,
        legacy_compile_per_call,
    ) {
        return "validation";
    }
    let unsafe_match = activity_regex_is_match(
        &ACTIVITY_UNSAFE_RE,
        ACTIVITY_UNSAFE_PATTERN,
        command,
        legacy_compile_per_call,
    ) || has_single_redirect(command);
    if unsafe_match {
        return "other";
    }
    if activity_regex_is_match(
        &ACTIVITY_GIT_INSPECTION_RE,
        ACTIVITY_GIT_INSPECTION_PATTERN,
        command,
        legacy_compile_per_call,
    ) {
        return "git_inspection";
    }
    if activity_regex_is_match(
        &ACTIVITY_FILE_READ_RE,
        ACTIVITY_FILE_READ_PATTERN,
        command,
        legacy_compile_per_call,
    ) {
        return "file_read_or_search";
    }
    "other"
}

fn compact_command_category(command: &str) -> &'static str {
    compact_command_category_impl(
        command,
        LEGACY_ACTIVITY_REGEX_ENABLED.load(AtomicOrdering::Relaxed),
    )
}

fn compact_execution_category(execution: &Obj, workspace: &str) -> Option<&'static str> {
    if value_str(execution.get("source_kind")) == Some("dynamic_template") {
        return None;
    }
    if !working_directory_is_default(value_str(execution.get("working_directory")), workspace) {
        return None;
    }
    if int_value(execution.get("exit_code"), None) != Some(0) {
        return None;
    }
    let category = compact_command_category(&command_execution_text(execution));
    (category != "other").then_some(category)
}

fn compact_execution_output(execution: &Obj, workspace: &str) -> Value {
    let mut output = Obj::new();
    let source_kind = value_str(execution.get("source_kind")).unwrap_or("literal");
    if source_kind != "literal" {
        output.insert(
            "source_kind".to_string(),
            Value::String(source_kind.to_string()),
        );
    }
    if let Some(command) = raw_str(execution.get("command")) {
        output.insert("command".to_string(), Value::String(command.to_string()));
    } else if let Some(template) = raw_str(execution.get("command_template")) {
        output.insert(
            "command_template".to_string(),
            Value::String(template.to_string()),
        );
    }
    if let Some(workdir) = raw_str(execution.get("working_directory")) {
        if !workdir.trim().is_empty() && !working_directory_is_default(Some(workdir), workspace) {
            output.insert(
                "working_directory".to_string(),
                Value::String(workdir.to_string()),
            );
        }
    }
    if let Some(timeout) = int_value(execution.get("timeout_ms"), None) {
        if timeout != 10_000 {
            output.insert("timeout_ms".to_string(), Value::Number(timeout.into()));
        }
    }
    if let Some(exit_code) = int_value(execution.get("exit_code"), None) {
        output.insert("exit_code".to_string(), Value::Number(exit_code.into()));
    }
    Value::Object(output)
}

fn complete_command_activity(
    activity: &mut Activity,
    exit_codes: Vec<i64>,
    output_truncated: bool,
) {
    activity.exit_codes = exit_codes;
    activity.output_truncated = output_truncated;
    let dynamic = activity
        .executions
        .iter()
        .any(|item| value_str(item.get("source_kind")) == Some("dynamic_template"));
    let mut mapping = "unknown";
    if !activity.exit_codes.is_empty() {
        if activity.executions.len() == 1 && activity.exit_codes.len() == 1 {
            mapping = "exact";
        } else if activity.executions.len() == activity.exit_codes.len()
            && !dynamic
            && !output_truncated
        {
            mapping = "ordered";
        } else {
            mapping = "aggregate";
        }
    }
    if matches!(mapping, "exact" | "ordered") {
        for (execution, code) in activity
            .executions
            .iter_mut()
            .zip(activity.exit_codes.iter())
        {
            execution.insert("exit_code".to_string(), Value::Number((*code).into()));
        }
    }
    if !activity.exit_codes.is_empty() && activity.executions.len() != activity.exit_codes.len() {
        add_unique_warning(
            &mut activity.parse_warnings,
            "Command and exit-code counts do not match.",
        );
    }
    if dynamic {
        add_unique_warning(
            &mut activity.parse_warnings,
            "One or more commands are preserved as dynamic templates.",
        );
    }
    if output_truncated {
        add_unique_warning(
            &mut activity.parse_warnings,
            "Command output was truncated.",
        );
    }
    activity.result_mapping = mapping.to_string();
    activity.invocation_count = activity.executions.len().max(activity.exit_codes.len());
}

fn direct_shell_execution(arguments: &Obj) -> Option<Obj> {
    let command = raw_str(arguments.get("command"))?.to_string();
    if command.trim().is_empty() {
        return None;
    }
    let mut execution = obj!(
        "source_kind" => Value::String("literal".to_string()),
        "command" => Value::String(command),
    );
    if let Some(workdir) =
        raw_str(arguments.get("workdir")).filter(|value| !value.trim().is_empty())
    {
        execution.insert(
            "working_directory".to_string(),
            Value::String(workdir.to_string()),
        );
    }
    if let Some(timeout) = int_value(arguments.get("timeout_ms"), Some(0)) {
        execution.insert("timeout_ms".to_string(), Value::Number(timeout.into()));
    }
    Some(execution)
}

fn command_output_summary(payload: &Obj) -> (Vec<i64>, bool, Option<String>, Option<Value>) {
    if bool_value(payload.get("_compact_shell_summary")) == Some(true) {
        let exit_codes: Vec<i64> = sequence_values(payload.get("_compact_exit_codes"))
            .into_iter()
            .filter_map(|value| int_value(Some(value), None))
            .collect();
        let truncated = bool_value(payload.get("_compact_output_truncated")).unwrap_or(false);
        let failure_status = value_str(payload.get("_compact_failure_status")).map(str::to_string);
        let failure_diagnostic = payload.get("_compact_failure_diagnostic").cloned();
        return (exit_codes, truncated, failure_status, failure_diagnostic);
    }
    let summary_text =
        get_message_text(payload.get("output")).filter(|text| !text.trim().is_empty());
    let exact_text =
        get_tool_output_text(payload.get("output")).filter(|text| !text.trim().is_empty());
    let exit_codes: Vec<i64> = summary_text
        .as_deref()
        .map(|text| {
            exit_code_regex()
                .captures_iter(text)
                .filter_map(|captures| captures.get(1))
                .filter_map(|code| code.as_str().parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default();
    let truncated = summary_text
        .as_deref()
        .is_some_and(|text| truncated_output_regex().is_match(text));
    let failure_diagnostic = command_wrapper_failure_diagnostic(exact_text.as_deref(), &exit_codes);
    let failure_status = failure_diagnostic.as_ref().map(|_| "failed".to_string());
    (exit_codes, truncated, failure_status, failure_diagnostic)
}

fn complete_command_activity_from_payload(
    activity: &mut Activity,
    payload: &Obj,
    record_time: Option<Timestamp>,
) {
    let (exit_codes, truncated, failure_status, failure_diagnostic) =
        command_output_summary(payload);
    complete_command_activity(activity, exit_codes, truncated);
    activity.failure_status = failure_status;
    activity.failure_diagnostic = failure_diagnostic;
    activity.end_time = record_time;
}

fn activity_to_output(activity: &Activity, workspace: &str, compact_command: bool) -> Value {
    let mut output = Obj::new();
    if let Some(turn_id) = activity
        .turn_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        output.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
    }
    output.insert(
        "type".to_string(),
        Value::String(activity.activity_type.clone()),
    );
    output.insert(
        "create_time".to_string(),
        activity
            .time
            .map(utc_iso)
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    if activity.activity_type == "command" {
        output.insert(
            "result_mapping".to_string(),
            Value::String(activity.result_mapping.clone()),
        );
        if activity.failure_status.as_deref() == Some("failed") {
            if let Some(call_id) = activity
                .source_call_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                output.insert(
                    "source_call_id".to_string(),
                    Value::String(call_id.to_string()),
                );
            }
        }
        let executions = if compact_command {
            activity
                .executions
                .iter()
                .map(|item| compact_execution_output(item, workspace))
                .collect()
        } else {
            activity
                .executions
                .iter()
                .cloned()
                .map(Value::Object)
                .collect()
        };
        output.insert("executions".to_string(), Value::Array(executions));
        if compact_command {
            if activity.output_truncated {
                output.insert("output_truncated".to_string(), Value::Bool(true));
            }
        } else {
            output.insert(
                "output_truncated".to_string(),
                Value::Bool(activity.output_truncated),
            );
        }
        if matches!(activity.result_mapping.as_str(), "aggregate" | "unknown") {
            let mut aggregate = obj!(
                "invocation_count" => Value::Number((activity.invocation_count as u64).into()),
            );
            if !activity.exit_codes.is_empty() {
                aggregate.insert(
                    "exit_codes".to_string(),
                    Value::Array(
                        activity
                            .exit_codes
                            .iter()
                            .map(|code| Value::Number((*code).into()))
                            .collect(),
                    ),
                );
            }
            if let Some(status) = activity
                .failure_status
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                aggregate.insert("status".to_string(), Value::String(status.to_string()));
            }
            if let Some(diagnostic) = &activity.failure_diagnostic {
                aggregate.insert(
                    "diagnostics".to_string(),
                    Value::Object(obj!("output" => diagnostic.clone())),
                );
            }
            output.insert("aggregate_result".to_string(), Value::Object(aggregate));
        }
        if !activity.parse_warnings.is_empty() {
            output.insert(
                "parse_warnings".to_string(),
                Value::Array(
                    activity
                        .parse_warnings
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        if let Some(end_time) = activity.end_time {
            output.insert("completed_at".to_string(), Value::String(utc_iso(end_time)));
        }
    } else if activity.activity_type == "file_edit" {
        output.insert(
            "path".to_string(),
            activity
                .path
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        output.insert(
            "operation".to_string(),
            activity
                .operation
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        output.insert(
            "success".to_string(),
            Value::Bool(activity.success.unwrap_or(false)),
        );
    } else {
        if let Some(action) = activity
            .action
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            output.insert("action".to_string(), Value::String(action.to_string()));
        }
        if !activity.queries.is_empty() {
            output.insert(
                "queries".to_string(),
                Value::Array(
                    activity
                        .queries
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        } else if let Some(query) = activity
            .query
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            output.insert("query".to_string(), Value::String(query.to_string()));
        }
        if let Some(pattern) = activity
            .pattern
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            output.insert("pattern".to_string(), Value::String(pattern.to_string()));
        }
        if !activity.references.is_empty() {
            output.insert(
                "references".to_string(),
                Value::Array(
                    activity
                        .references
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        if !activity.urls.is_empty() {
            output.insert(
                "urls".to_string(),
                Value::Array(activity.urls.iter().cloned().map(Value::String).collect()),
            );
        }
    }
    Value::Object(output)
}

fn changes_for_output(changes: &[ChangeInfo], detail_mode: &str) -> Vec<Value> {
    let mut sorted = changes.to_vec();
    sorted.sort_by_key(|item| (item.time.unwrap_or(MAX_TS), item.sequence));
    if detail_mode == "Full" {
        return sorted
            .into_iter()
            .map(|item| {
                Value::Object(obj!(
                    "turn_id" => item.turn_id.map(Value::String).unwrap_or(Value::Null),
                    "create_time" => item.time.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
                    "path" => Value::String(item.path),
                    "operation" => item.operation.map(Value::String).unwrap_or(Value::Null),
                    "success" => Value::Bool(item.success),
                ))
            })
            .collect();
    }
    #[derive(Clone)]
    struct Group {
        turn_id: String,
        path: String,
        operation: String,
        first_time: Option<Timestamp>,
        last_time: Option<Timestamp>,
        sequence: usize,
        edit_count: usize,
        failed_count: usize,
    }
    let mut groups = Vec::<Group>::new();
    let mut indexes = HashMap::<(String, String, String), usize>::new();
    for change in sorted {
        let key = (
            change.turn_id.clone().unwrap_or_default(),
            change.path.clone(),
            change.operation.clone().unwrap_or_default(),
        );
        let index = if let Some(index) = indexes.get(&key).copied() {
            index
        } else {
            let index = groups.len();
            indexes.insert(key.clone(), index);
            groups.push(Group {
                turn_id: key.0.clone(),
                path: key.1.clone(),
                operation: key.2.clone(),
                first_time: change.time,
                last_time: change.time,
                sequence: change.sequence,
                edit_count: 0,
                failed_count: 0,
            });
            index
        };
        let group = &mut groups[index];
        group.edit_count += 1;
        if !change.success {
            group.failed_count += 1;
        }
        if let Some(time) = change.time {
            group.first_time = Some(group.first_time.map_or(time, |current| current.min(time)));
            group.last_time = Some(group.last_time.map_or(time, |current| current.max(time)));
        }
    }
    groups.sort_by_key(|item| (item.first_time.unwrap_or(MAX_TS), item.sequence));
    groups
        .into_iter()
        .map(|group| {
            let mut output = obj!(
                "turn_id" => Value::String(group.turn_id),
                "first_time" => group.first_time.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
                "last_time" => group.last_time.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
                "path" => Value::String(group.path),
                "operation" => Value::String(group.operation),
                "edit_count" => Value::Number((group.edit_count as u64).into()),
                "success" => Value::Bool(group.failed_count == 0),
            );
            if group.failed_count > 0 {
                output.insert("failed_count".to_string(), Value::Number((group.failed_count as u64).into()));
            }
            Value::Object(output)
        })
        .collect()
}

fn append_unique(values: &mut Vec<String>, seen: &mut HashSet<String>, value: Option<&str>) {
    let Some(value) = value.filter(|item| !item.trim().is_empty()) else {
        return;
    };
    if seen.insert(value.to_string()) {
        values.push(value.to_string());
    }
}

fn powershell_unstable_sort<T: Clone, F>(values: &[T], compare: F) -> Vec<T>
where
    F: Fn(&T, &T) -> Ordering + Copy,
{
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(32, 0);

    let mut items = values.to_vec();
    if items.len() < 2 {
        return items;
    }

    fn swap_if_greater<T, F>(items: &mut [T], a: usize, b: usize, compare: F)
    where
        F: Fn(&T, &T) -> Ordering + Copy,
    {
        if a != b && compare(&items[a], &items[b]) == Ordering::Greater {
            items.swap(a, b);
        }
    }

    fn heap_sort<T: Clone, F>(items: &mut [T], left: usize, right: usize, compare: F)
    where
        F: Fn(&T, &T) -> Ordering + Copy,
    {
        fn down_heap<T: Clone, F>(
            items: &mut [T],
            mut index: usize,
            count: usize,
            offset: usize,
            compare: F,
        ) where
            F: Fn(&T, &T) -> Ordering + Copy,
        {
            let value = items[offset + index - 1].clone();
            while index <= count / 2 {
                let mut child = 2 * index;
                if child < count
                    && compare(&items[offset + child - 1], &items[offset + child]) == Ordering::Less
                {
                    child += 1;
                }
                if compare(&value, &items[offset + child - 1]) != Ordering::Less {
                    break;
                }
                items[offset + index - 1] = items[offset + child - 1].clone();
                index = child;
            }
            items[offset + index - 1] = value;
        }
        let count = right - left + 1;
        for index in (1..=count / 2).rev() {
            down_heap(items, index, count, left, compare);
        }
        for index in (2..=count).rev() {
            items.swap(left, left + index - 1);
            down_heap(items, 1, index - 1, left, compare);
        }
    }

    fn quick_sort<T: Clone, F>(
        items: &mut [T],
        mut left: usize,
        mut right: usize,
        mut depth_limit: usize,
        compare: F,
    ) where
        F: Fn(&T, &T) -> Ordering + Copy,
    {
        while left < right {
            if depth_limit == 0 {
                heap_sort(items, left, right, compare);
                return;
            }
            let mut i = left;
            let mut j = right;
            let middle = i + ((j - i) >> 1);
            swap_if_greater(items, i, middle, compare);
            swap_if_greater(items, i, j, compare);
            swap_if_greater(items, middle, j, compare);
            let pivot = items[middle].clone();
            loop {
                while compare(&items[i], &pivot) == Ordering::Less {
                    i += 1;
                }
                while compare(&pivot, &items[j]) == Ordering::Less {
                    if j == 0 {
                        break;
                    }
                    j -= 1;
                }
                if i > j {
                    break;
                }
                if i < j {
                    items.swap(i, j);
                }
                i += 1;
                if j == 0 {
                    break;
                }
                j -= 1;
                if i > j {
                    break;
                }
            }
            depth_limit -= 1;
            let left_size = j.saturating_sub(left);
            let right_size = right.saturating_sub(i);
            if left_size <= right_size {
                if left < j {
                    quick_sort(items, left, j, depth_limit, compare);
                }
                left = i;
            } else {
                if i < right {
                    quick_sort(items, i, right, depth_limit, compare);
                }
                right = j;
            }
        }
    }

    let last = items.len() - 1;
    quick_sort(&mut items, 0, last, 32, compare);
    items
}

#[derive(Clone)]
struct ActivityEntry {
    time: Option<Timestamp>,
    sequence: usize,
    output: Value,
}

fn activities_for_output_impl(
    activities: &[Activity],
    workspace: &str,
    detail_mode: &str,
    legacy_clone_sort: bool,
) -> Vec<Value> {
    // PowerShell-compatible ordering is required for parity, but sorting whole Activity values
    // is extremely expensive because the compatibility sorter clones pivots/items repeatedly.
    // Keep the exact same comparator/algorithm while sorting lightweight references instead.
    let legacy_sorted_storage = if legacy_clone_sort {
        Some(powershell_unstable_sort(activities, |left, right| {
            (left.time.unwrap_or(MAX_TS), left.sequence)
                .cmp(&(right.time.unwrap_or(MAX_TS), right.sequence))
        }))
    } else {
        None
    };
    let sorted_activities: Vec<&Activity> = if let Some(storage) = legacy_sorted_storage.as_ref() {
        storage.iter().collect()
    } else {
        let refs: Vec<&Activity> = activities.iter().collect();
        powershell_unstable_sort(&refs, |left, right| {
            (left.time.unwrap_or(MAX_TS), left.sequence)
                .cmp(&(right.time.unwrap_or(MAX_TS), right.sequence))
        })
    };
    let mut entries = Vec::<ActivityEntry>::new();
    if detail_mode == "Full" {
        for activity in sorted_activities {
            entries.push(ActivityEntry {
                time: activity.time,
                sequence: activity.sequence,
                output: activity_to_output(activity, workspace, false),
            });
        }
    } else {
        #[derive(Clone)]
        struct InspectionGroup {
            turn_id: String,
            first_time: Option<Timestamp>,
            last_time: Option<Timestamp>,
            sequence: usize,
            execution_count: usize,
            file_read_count: usize,
            git_inspection_count: usize,
            custom_timeout_count: usize,
            commands: HashSet<String>,
        }
        #[derive(Clone)]
        struct ValidationCheck {
            command: String,
            timeout: Option<i64>,
            last_time: Option<Timestamp>,
            sequence: usize,
            run_count: usize,
            final_exit_code: i64,
        }
        #[derive(Clone)]
        struct ValidationGroup {
            turn_id: String,
            first_time: Option<Timestamp>,
            last_time: Option<Timestamp>,
            sequence: usize,
            checks: Vec<ValidationCheck>,
            check_indexes: HashMap<(String, Option<i64>), usize>,
        }
        #[derive(Clone)]
        struct WebGroup {
            turn_id: String,
            first_time: Option<Timestamp>,
            last_time: Option<Timestamp>,
            sequence: usize,
            search_count: usize,
            open_count: usize,
            find_count: usize,
            click_count: usize,
            other_count: usize,
            queries: Vec<String>,
            query_set: HashSet<String>,
            patterns: Vec<String>,
            pattern_set: HashSet<String>,
            references: Vec<String>,
            reference_set: HashSet<String>,
            urls: Vec<String>,
            url_set: HashSet<String>,
        }

        let mut inspection_groups = Vec::<InspectionGroup>::new();
        let mut inspection_indexes = HashMap::<String, usize>::new();
        let mut validation_groups = Vec::<ValidationGroup>::new();
        let mut validation_indexes = HashMap::<String, usize>::new();
        let mut web_groups = Vec::<WebGroup>::new();
        let mut web_indexes = HashMap::<String, usize>::new();

        for activity in sorted_activities {
            if activity.activity_type == "file_edit" {
                continue;
            }
            let turn_id = activity.turn_id.clone().unwrap_or_default();
            let turn_key = if turn_id.trim().is_empty() {
                "<none>".to_string()
            } else {
                turn_id.clone()
            };
            if activity.activity_type == "command" {
                let can_partition = matches!(activity.result_mapping.as_str(), "exact" | "ordered")
                    && !activity.output_truncated
                    && activity.parse_warnings.is_empty();
                if !can_partition {
                    entries.push(ActivityEntry {
                        time: activity.time,
                        sequence: activity.sequence,
                        output: activity_to_output(activity, workspace, true),
                    });
                    continue;
                }
                let mut remaining = Vec::<Obj>::new();
                for execution in &activity.executions {
                    match compact_execution_category(execution, workspace) {
                        Some(category @ ("git_inspection" | "file_read_or_search")) => {
                            let index =
                                if let Some(index) = inspection_indexes.get(&turn_key).copied() {
                                    index
                                } else {
                                    let index = inspection_groups.len();
                                    inspection_indexes.insert(turn_key.clone(), index);
                                    inspection_groups.push(InspectionGroup {
                                        turn_id: turn_id.clone(),
                                        first_time: activity.time,
                                        last_time: activity.end_time,
                                        sequence: activity.sequence,
                                        execution_count: 0,
                                        file_read_count: 0,
                                        git_inspection_count: 0,
                                        custom_timeout_count: 0,
                                        commands: HashSet::new(),
                                    });
                                    index
                                };
                            let group = &mut inspection_groups[index];
                            group.commands.insert(command_execution_text(execution));
                            group.execution_count += 1;
                            if category == "git_inspection" {
                                group.git_inspection_count += 1;
                            } else {
                                group.file_read_count += 1;
                            }
                            if int_value(execution.get("timeout_ms"), None)
                                .is_some_and(|value| value != 10_000)
                            {
                                group.custom_timeout_count += 1;
                            }
                            if let Some(time) = activity.time {
                                group.first_time = Some(
                                    group.first_time.map_or(time, |current| current.min(time)),
                                );
                            }
                            if let Some(last) = activity.end_time.or(activity.time) {
                                group.last_time =
                                    Some(group.last_time.map_or(last, |current| current.max(last)));
                            }
                            group.sequence = group.sequence.min(activity.sequence);
                        }
                        Some("validation") => {
                            let index =
                                if let Some(index) = validation_indexes.get(&turn_key).copied() {
                                    index
                                } else {
                                    let index = validation_groups.len();
                                    validation_indexes.insert(turn_key.clone(), index);
                                    validation_groups.push(ValidationGroup {
                                        turn_id: turn_id.clone(),
                                        first_time: activity.time,
                                        last_time: activity.end_time,
                                        sequence: activity.sequence,
                                        checks: Vec::new(),
                                        check_indexes: HashMap::new(),
                                    });
                                    index
                                };
                            let group = &mut validation_groups[index];
                            let command = command_execution_text(execution);
                            let timeout = int_value(execution.get("timeout_ms"), None);
                            let key = (command.clone(), timeout);
                            let check_index =
                                if let Some(index) = group.check_indexes.get(&key).copied() {
                                    index
                                } else {
                                    let check_index = group.checks.len();
                                    group.check_indexes.insert(key, check_index);
                                    group.checks.push(ValidationCheck {
                                        command,
                                        timeout,
                                        last_time: activity.end_time,
                                        sequence: activity.sequence,
                                        run_count: 0,
                                        final_exit_code: 0,
                                    });
                                    check_index
                                };
                            let check = &mut group.checks[check_index];
                            check.run_count += 1;
                            check.final_exit_code =
                                int_value(execution.get("exit_code"), None).unwrap_or(0);
                            if let Some(last) = activity.end_time.or(activity.time) {
                                check.last_time = Some(last);
                                group.last_time =
                                    Some(group.last_time.map_or(last, |current| current.max(last)));
                            }
                            if let Some(time) = activity.time {
                                group.first_time = Some(
                                    group.first_time.map_or(time, |current| current.min(time)),
                                );
                            }
                            group.sequence = group.sequence.min(activity.sequence);
                        }
                        _ => remaining.push(execution.clone()),
                    }
                }
                if !remaining.is_empty() {
                    let exit_codes: Vec<i64> = remaining
                        .iter()
                        .filter_map(|execution| int_value(execution.get("exit_code"), None))
                        .collect();
                    let mut partial = (*activity).clone();
                    partial.executions = remaining;
                    partial.exit_codes = exit_codes;
                    partial.output_truncated = false;
                    partial.result_mapping = if partial.executions.len() == 1 {
                        "exact"
                    } else {
                        "ordered"
                    }
                    .to_string();
                    partial.invocation_count = partial.executions.len();
                    partial.parse_warnings.clear();
                    entries.push(ActivityEntry {
                        time: partial.time,
                        sequence: partial.sequence,
                        output: activity_to_output(&partial, workspace, true),
                    });
                }
                continue;
            }
            if activity.activity_type.starts_with("web_") {
                let index = if let Some(index) = web_indexes.get(&turn_key).copied() {
                    index
                } else {
                    let index = web_groups.len();
                    web_indexes.insert(turn_key.clone(), index);
                    web_groups.push(WebGroup {
                        turn_id: turn_id.clone(),
                        first_time: activity.time,
                        last_time: activity.time,
                        sequence: activity.sequence,
                        search_count: 0,
                        open_count: 0,
                        find_count: 0,
                        click_count: 0,
                        other_count: 0,
                        queries: Vec::new(),
                        query_set: HashSet::new(),
                        patterns: Vec::new(),
                        pattern_set: HashSet::new(),
                        references: Vec::new(),
                        reference_set: HashSet::new(),
                        urls: Vec::new(),
                        url_set: HashSet::new(),
                    });
                    index
                };
                let group = &mut web_groups[index];
                match activity.activity_type.as_str() {
                    "web_search" => group.search_count += 1,
                    "web_open" => group.open_count += 1,
                    "web_find" => group.find_count += 1,
                    "web_click" => group.click_count += 1,
                    _ => group.other_count += 1,
                }
                for query in &activity.queries {
                    append_unique(&mut group.queries, &mut group.query_set, Some(query));
                }
                append_unique(
                    &mut group.queries,
                    &mut group.query_set,
                    activity.query.as_deref(),
                );
                append_unique(
                    &mut group.patterns,
                    &mut group.pattern_set,
                    activity.pattern.as_deref(),
                );
                for reference in &activity.references {
                    append_unique(
                        &mut group.references,
                        &mut group.reference_set,
                        Some(reference),
                    );
                }
                for url in &activity.urls {
                    append_unique(&mut group.urls, &mut group.url_set, Some(url));
                }
                if let Some(time) = activity.time {
                    group.first_time =
                        Some(group.first_time.map_or(time, |current| current.min(time)));
                    group.last_time =
                        Some(group.last_time.map_or(time, |current| current.max(time)));
                }
                group.sequence = group.sequence.min(activity.sequence);
                continue;
            }
            entries.push(ActivityEntry {
                time: activity.time,
                sequence: activity.sequence,
                output: activity_to_output(activity, workspace, true),
            });
        }

        for group in inspection_groups {
            let mut categories = Obj::new();
            if group.file_read_count > 0 {
                categories.insert(
                    "file_read_or_search".to_string(),
                    Value::Number((group.file_read_count as u64).into()),
                );
            }
            if group.git_inspection_count > 0 {
                categories.insert(
                    "git_inspection".to_string(),
                    Value::Number((group.git_inspection_count as u64).into()),
                );
            }
            let mut output = Obj::new();
            if !group.turn_id.trim().is_empty() {
                output.insert("turn_id".to_string(), Value::String(group.turn_id));
            }
            output.insert(
                "type".to_string(),
                Value::String("inspection_summary".to_string()),
            );
            output.insert(
                "create_time".to_string(),
                group
                    .first_time
                    .map(utc_iso)
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            if let Some(last) = group.last_time {
                output.insert("completed_at".to_string(), Value::String(utc_iso(last)));
            }
            output.insert(
                "execution_count".to_string(),
                Value::Number((group.execution_count as u64).into()),
            );
            output.insert(
                "unique_command_count".to_string(),
                Value::Number((group.commands.len() as u64).into()),
            );
            output.insert("categories".to_string(), Value::Object(categories));
            if group.custom_timeout_count > 0 {
                output.insert(
                    "custom_timeout_count".to_string(),
                    Value::Number((group.custom_timeout_count as u64).into()),
                );
            }
            entries.push(ActivityEntry {
                time: group.first_time,
                sequence: group.sequence,
                output: Value::Object(output),
            });
        }

        for group in validation_groups {
            let sorted_checks = powershell_unstable_sort(&group.checks, |left, right| {
                left.sequence.cmp(&right.sequence)
            });
            let mut checks = Vec::new();
            for check in sorted_checks {
                let mut output_check = obj!(
                    "command" => Value::String(check.command),
                    "run_count" => Value::Number((check.run_count as u64).into()),
                    "final_exit_code" => Value::Number(check.final_exit_code.into()),
                );
                if check.timeout.is_some_and(|timeout| timeout != 10_000) {
                    output_check.insert(
                        "timeout_ms".to_string(),
                        Value::Number(check.timeout.unwrap().into()),
                    );
                }
                checks.push(Value::Object(output_check));
            }
            let mut output = Obj::new();
            if !group.turn_id.trim().is_empty() {
                output.insert("turn_id".to_string(), Value::String(group.turn_id));
            }
            output.insert(
                "type".to_string(),
                Value::String("validation_summary".to_string()),
            );
            output.insert(
                "create_time".to_string(),
                group
                    .first_time
                    .map(utc_iso)
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            if let Some(last) = group.last_time {
                output.insert("completed_at".to_string(), Value::String(utc_iso(last)));
            }
            output.insert("checks".to_string(), Value::Array(checks));
            entries.push(ActivityEntry {
                time: group.first_time,
                sequence: group.sequence,
                output: Value::Object(output),
            });
        }

        for group in web_groups {
            let mut output = Obj::new();
            if !group.turn_id.trim().is_empty() {
                output.insert("turn_id".to_string(), Value::String(group.turn_id));
            }
            output.insert(
                "type".to_string(),
                Value::String("web_research".to_string()),
            );
            output.insert(
                "create_time".to_string(),
                group
                    .first_time
                    .map(utc_iso)
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            if let Some(last) = group.last_time {
                output.insert("completed_at".to_string(), Value::String(utc_iso(last)));
            }
            for (field, count) in [
                ("search_count", group.search_count),
                ("open_count", group.open_count),
                ("find_count", group.find_count),
                ("click_count", group.click_count),
                ("other_count", group.other_count),
            ] {
                if count > 0 {
                    output.insert(field.to_string(), Value::Number((count as u64).into()));
                }
            }
            for (field, values) in [
                ("queries", group.queries),
                ("patterns", group.patterns),
                ("references", group.references),
                ("urls", group.urls),
            ] {
                if !values.is_empty() {
                    output.insert(
                        field.to_string(),
                        Value::Array(values.into_iter().map(Value::String).collect()),
                    );
                }
            }
            entries.push(ActivityEntry {
                time: group.first_time,
                sequence: group.sequence,
                output: Value::Object(output),
            });
        }
    }

    let sorted_entries = if legacy_clone_sort {
        powershell_unstable_sort(&entries, |left, right| {
            (left.time.unwrap_or(MAX_TS), left.sequence)
                .cmp(&(right.time.unwrap_or(MAX_TS), right.sequence))
        })
    } else {
        // Sorting ActivityEntry itself clones the complete JSON output at every pivot/move.
        // Sort only indexes with the same compatibility algorithm, then move each entry once.
        let indexes: Vec<usize> = (0..entries.len()).collect();
        let order = powershell_unstable_sort(&indexes, |left, right| {
            let left = &entries[*left];
            let right = &entries[*right];
            (left.time.unwrap_or(MAX_TS), left.sequence)
                .cmp(&(right.time.unwrap_or(MAX_TS), right.sequence))
        });
        let mut slots: Vec<Option<ActivityEntry>> = entries.into_iter().map(Some).collect();
        order
            .into_iter()
            .map(|index| slots[index].take().expect("activity entry index is unique"))
            .collect()
    };
    sorted_entries
        .into_iter()
        .enumerate()
        .map(|(index, mut entry)| {
            if let Some(output) = entry.output.as_object_mut() {
                let mut numbered = Obj::new();
                numbered.insert(
                    "id".to_string(),
                    Value::String(format!("act{:02}", index + 1)),
                );
                for (key, value) in std::mem::take(output) {
                    numbered.insert(key, value);
                }
                Value::Object(numbered)
            } else {
                entry.output
            }
        })
        .collect()
}

fn activities_for_output(
    activities: &[Activity],
    workspace: &str,
    detail_mode: &str,
) -> Vec<Value> {
    activities_for_output_impl(
        activities,
        workspace,
        detail_mode,
        LEGACY_ACTIVITY_SORT_ENABLED.load(AtomicOrdering::Relaxed),
    )
}

#[derive(Clone, Debug)]
struct MessageOutputs {
    messages: Vec<Value>,
    plans: Vec<Value>,
    reviews: Vec<Value>,
    memory_citations: Vec<Value>,
    writing_blocks: Vec<Value>,
    ui_actions: Vec<Value>,
    ui_parse_warnings: Vec<Value>,
    last_message_time: Option<Timestamp>,
}

fn next_counter(counters: &mut HashMap<String, usize>, name: &str) -> usize {
    let value = counters.entry(name.to_string()).or_insert(0);
    *value += 1;
    *value
}

fn add_ui_warning(
    counters: &mut HashMap<String, usize>,
    output_warnings: &mut Vec<Value>,
    message_id: &str,
    turn_id: Option<&str>,
    warning_type: &str,
) -> String {
    let warning_id = format!("ui-warning{:02}", next_counter(counters, "warning"));
    let mut warning = obj!(
        "id" => Value::String(warning_id.clone()),
        "message_id" => Value::String(message_id.to_string()),
        "type" => Value::String(warning_type.to_string()),
    );
    if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
        warning.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
    }
    output_warnings.push(Value::Object(warning));
    warning_id
}

fn build_output_messages(
    sorted_messages: &[RawMessage],
    effective_plan_items: &[PlanItem],
) -> MessageOutputs {
    let mut output_messages = Vec::new();
    let mut output_plans = Vec::new();
    let mut output_reviews = Vec::new();
    let mut output_memory_citations = Vec::new();
    let mut output_writing_blocks = Vec::new();
    let mut output_ui_actions = Vec::new();
    let mut output_ui_warnings = Vec::new();
    let mut used_plan_sequences = HashSet::<usize>::new();
    let mut counters = HashMap::<String, usize>::new();
    let mut last_message_time = None;
    let mut sorted_plan_items = effective_plan_items.to_vec();
    sorted_plan_items.sort_by_key(|item| item.sequence);

    for message in sorted_messages {
        let role = message.role.as_str();
        let role_count = next_counter(&mut counters, role);
        let message_id = format!(
            "{}{:02}",
            if role == "user" { "u" } else { "a" },
            role_count
        );
        let turn_id = message.turn_id.as_deref();
        let mut output_message = obj!("id" => Value::String(message_id.clone()));
        if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
            output_message.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
        }
        output_message.insert("role".to_string(), Value::String(role.to_string()));
        if role == "assistant" {
            output_message.insert(
                "phase".to_string(),
                message
                    .phase
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
        }
        output_message.insert(
            "create_time".to_string(),
            message
                .time
                .map(utc_iso)
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        let mut clean = message.content.clone();
        let mut citation_ids = Vec::<String>::new();
        let mut writing_ids = Vec::<String>::new();
        let mut action_ids = Vec::<String>::new();
        let mut warning_ids = Vec::<String>::new();

        if role == "assistant" {
            let mut citation_ranges = Vec::<(usize, usize)>::new();
            for block in memory_citation_blocks(&clean) {
                if block.valid {
                    let citation_id = format!(
                        "memory-citation{:02}",
                        next_counter(&mut counters, "citation")
                    );
                    let mut output = obj!("id" => Value::String(citation_id.clone()));
                    if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
                        output.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
                    }
                    output.insert("message_id".to_string(), Value::String(message_id.clone()));
                    output.insert("entries".to_string(), Value::Array(block.entries));
                    output.insert(
                        "rollout_ids".to_string(),
                        Value::Array(block.rollout_ids.into_iter().map(Value::String).collect()),
                    );
                    output_memory_citations.push(Value::Object(output));
                    citation_ids.push(citation_id);
                    citation_ranges.push((block.start, block.length));
                } else {
                    warning_ids.push(add_ui_warning(
                        &mut counters,
                        &mut output_ui_warnings,
                        &message_id,
                        turn_id,
                        block
                            .warning
                            .as_deref()
                            .unwrap_or("malformed_memory_citation"),
                    ));
                }
            }
            if !citation_ranges.is_empty() {
                clean = remove_ranges(&clean, &citation_ranges);
            }

            let (new_clean, actions, action_warnings) = ui_actions_in_text(&clean);
            clean = new_clean;
            for action in actions {
                let action_id = format!("ui-action{:02}", next_counter(&mut counters, "action"));
                let mut output = obj!("id" => Value::String(action_id.clone()));
                if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
                    output.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
                }
                output.insert("message_id".to_string(), Value::String(message_id.clone()));
                output.insert(
                    "create_time".to_string(),
                    message
                        .time
                        .map(utc_iso)
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
                output.insert("type".to_string(), Value::String(action.action_type));
                if !action.attributes.is_empty() {
                    output.insert("attributes".to_string(), Value::Object(action.attributes));
                }
                output.insert(
                    "source_directive_sha256".to_string(),
                    Value::String(action.directive_hash),
                );
                output_ui_actions.push(Value::Object(output));
                action_ids.push(action_id);
            }
            for warning in action_warnings {
                warning_ids.push(add_ui_warning(
                    &mut counters,
                    &mut output_ui_warnings,
                    &message_id,
                    turn_id,
                    &warning,
                ));
            }

            if message.phase.as_deref() == Some("final_answer") {
                let findings = code_review_findings(&clean);
                if !findings.is_empty() {
                    let review_id = format!("review{:02}", next_counter(&mut counters, "review"));
                    let mut output_findings = Vec::new();
                    for (index, finding) in findings.into_iter().enumerate() {
                        let mut output_finding = obj!(
                            "id" => Value::String(format!("{review_id}-finding{:02}", index + 1)),
                            "title" => finding.get("title").cloned().unwrap_or(Value::Null),
                            "body" => finding.get("body").cloned().unwrap_or(Value::Null),
                            "file" => finding.get("file").cloned().unwrap_or(Value::Null),
                        );
                        for name in ["start", "end", "priority"] {
                            if let Some(value) = finding.get(name) {
                                output_finding.insert(name.to_string(), value.clone());
                            }
                        }
                        output_findings.push(Value::Object(output_finding));
                    }
                    let mut output_review = obj!("id" => Value::String(review_id.clone()));
                    if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
                        output_review
                            .insert("turn_id".to_string(), Value::String(turn_id.to_string()));
                    }
                    output_review
                        .insert("message_id".to_string(), Value::String(message_id.clone()));
                    output_review.insert(
                        "create_time".to_string(),
                        message
                            .time
                            .map(utc_iso)
                            .map(Value::String)
                            .unwrap_or(Value::Null),
                    );
                    output_review.insert("findings".to_string(), Value::Array(output_findings));
                    output_reviews.push(Value::Object(output_review));
                    clean = remove_code_review_directives(&clean);
                    output_message.insert(
                        "content_type".to_string(),
                        Value::String("code_review".to_string()),
                    );
                    output_message.insert("review_id".to_string(), Value::String(review_id));
                }
            }

            let (new_clean, blocks, writing_warnings) = writing_blocks_in_text(&clean);
            clean = new_clean;
            for block in blocks {
                let writing_id = format!("writing{:02}", next_counter(&mut counters, "writing"));
                let mut output = obj!("id" => Value::String(writing_id.clone()));
                if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
                    output.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
                }
                output.insert("message_id".to_string(), Value::String(message_id.clone()));
                if let Some(source_id) = block.attributes.get("id") {
                    output.insert("source_id".to_string(), source_id.clone());
                }
                if let Some(variant) = block.attributes.get("variant") {
                    output.insert("variant".to_string(), variant.clone());
                }
                output.insert(
                    "content_sha256".to_string(),
                    Value::String(sha256_text(Some(&block.content))),
                );
                output_writing_blocks.push(Value::Object(output));
                writing_ids.push(writing_id);
            }
            for warning in writing_warnings {
                warning_ids.push(add_ui_warning(
                    &mut counters,
                    &mut output_ui_warnings,
                    &message_id,
                    turn_id,
                    &warning,
                ));
            }

            if message.phase.as_deref() == Some("final_answer") {
                let mut block = proposed_plan_block(&clean);
                if block.as_ref().is_some_and(|item| !item.valid) {
                    let warning = block
                        .as_ref()
                        .and_then(|item| item.warning.as_deref())
                        .unwrap_or("malformed_proposed_plan");
                    warning_ids.push(add_ui_warning(
                        &mut counters,
                        &mut output_ui_warnings,
                        &message_id,
                        turn_id,
                        warning,
                    ));
                    block = None;
                }
                let mut matching_index = None;
                let mut exact_index = None;
                for (index, candidate) in sorted_plan_items.iter().enumerate() {
                    if used_plan_sequences.contains(&candidate.sequence)
                        || candidate.turn_id != turn_id.unwrap_or_default()
                    {
                        continue;
                    }
                    if matching_index.is_none() {
                        matching_index = Some(index);
                    }
                    let candidate_key = text_key(Some(&candidate.text));
                    if block.as_ref().is_some_and(|item| {
                        item.valid && candidate_key == text_key(item.content.as_deref())
                    }) || candidate_key == text_key(Some(&clean))
                    {
                        exact_index = Some(index);
                        break;
                    }
                }
                if exact_index.is_some() {
                    matching_index = exact_index;
                }
                if matching_index.is_some() || block.as_ref().is_some_and(|item| item.valid) {
                    let plan_id = format!("plan{:02}", next_counter(&mut counters, "plan"));
                    let plan_text = matching_index
                        .map(|index| sorted_plan_items[index].text.clone())
                        .or_else(|| block.as_ref().and_then(|item| item.content.clone()))
                        .expect("matching or block exists");
                    let mut message_match = false;
                    if let Some(block) = block.as_ref().filter(|item| item.valid) {
                        message_match =
                            text_key(block.content.as_deref()) == text_key(Some(&plan_text));
                        let replacement = if message_match {
                            plan_text.clone()
                        } else {
                            block.content.clone().unwrap_or_default()
                        };
                        let outside = format!(
                            "{}{}",
                            &clean[..block.start],
                            &clean[block.start + block.length..]
                        );
                        if outside.trim().is_empty() {
                            clean = replacement;
                        } else {
                            clean = format!(
                                "{}{}{}",
                                &clean[..block.start],
                                replacement,
                                &clean[block.start + block.length..]
                            )
                            .trim()
                            .to_string();
                        }
                    } else if text_key(Some(&clean)) == text_key(Some(&plan_text)) {
                        message_match = true;
                        clean = plan_text.clone();
                    }
                    let mut output_plan = obj!("id" => Value::String(plan_id.clone()));
                    if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
                        output_plan
                            .insert("turn_id".to_string(), Value::String(turn_id.to_string()));
                    }
                    output_plan.insert("message_id".to_string(), Value::String(message_id.clone()));
                    if let Some(index) = matching_index {
                        let matching = &sorted_plan_items[index];
                        used_plan_sequences.insert(matching.sequence);
                        if let Some(source_id) = matching
                            .source_item_id
                            .as_deref()
                            .filter(|value| !value.trim().is_empty())
                        {
                            output_plan.insert(
                                "source_item_id".to_string(),
                                Value::String(source_id.to_string()),
                            );
                        }
                        output_plan.insert(
                            "create_time".to_string(),
                            matching
                                .time
                                .map(utc_iso)
                                .map(Value::String)
                                .unwrap_or(Value::Null),
                        );
                    } else {
                        output_plan.insert(
                            "create_time".to_string(),
                            message
                                .time
                                .map(utc_iso)
                                .map(Value::String)
                                .unwrap_or(Value::Null),
                        );
                    }
                    output_plan.insert("status".to_string(), Value::String("proposed".to_string()));
                    if let Some(title) = plan_title(&plan_text) {
                        output_plan.insert("title".to_string(), Value::String(title));
                    }
                    output_plan.insert("message_match".to_string(), Value::Bool(message_match));
                    if !message_match {
                        output_plan.insert("content".to_string(), Value::String(plan_text));
                    }
                    output_plans.push(Value::Object(output_plan));
                    output_message.insert(
                        "content_type".to_string(),
                        Value::String("proposed_plan".to_string()),
                    );
                    output_message.insert("plan_id".to_string(), Value::String(plan_id));
                }
            }
        }

        if !citation_ids.is_empty() {
            output_message.insert(
                "memory_citation_ids".to_string(),
                Value::Array(citation_ids.into_iter().map(Value::String).collect()),
            );
        }
        if !writing_ids.is_empty() {
            output_message.insert(
                "writing_block_ids".to_string(),
                Value::Array(writing_ids.into_iter().map(Value::String).collect()),
            );
        }
        if !action_ids.is_empty() {
            output_message.insert(
                "ui_action_ids".to_string(),
                Value::Array(action_ids.into_iter().map(Value::String).collect()),
            );
        }
        if !warning_ids.is_empty() {
            output_message.insert(
                "ui_parse_warning_ids".to_string(),
                Value::Array(warning_ids.into_iter().map(Value::String).collect()),
            );
        }
        output_message.insert(
            "content".to_string(),
            Value::String(clean.trim().to_string()),
        );
        output_messages.push(Value::Object(output_message));
        if let Some(time) = message.time {
            last_message_time =
                Some(last_message_time.map_or(time, |current: Timestamp| current.max(time)));
        }
    }

    for item in sorted_plan_items {
        if used_plan_sequences.contains(&item.sequence) {
            continue;
        }
        let plan_id = format!("plan{:02}", next_counter(&mut counters, "plan"));
        let mut output_plan = obj!(
            "id" => Value::String(plan_id),
            "turn_id" => Value::String(item.turn_id),
        );
        if let Some(source_id) = item.source_item_id.filter(|value| !value.trim().is_empty()) {
            output_plan.insert("source_item_id".to_string(), Value::String(source_id));
        }
        output_plan.insert(
            "create_time".to_string(),
            item.time
                .map(utc_iso)
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        output_plan.insert("status".to_string(), Value::String("proposed".to_string()));
        if let Some(title) = plan_title(&item.text) {
            output_plan.insert("title".to_string(), Value::String(title));
        }
        output_plan.insert("message_match".to_string(), Value::Bool(false));
        output_plan.insert("content".to_string(), Value::String(item.text));
        output_plans.push(Value::Object(output_plan));
    }

    MessageOutputs {
        messages: output_messages,
        plans: output_plans,
        reviews: output_reviews,
        memory_citations: output_memory_citations,
        writing_blocks: output_writing_blocks,
        ui_actions: output_ui_actions,
        ui_parse_warnings: output_ui_warnings,
        last_message_time,
    }
}

// ---------------------------------------------------------------------------
// Attachments, Goal transport, execution profiles, recursive helpers
// ---------------------------------------------------------------------------

fn goal_context_objective(text: Option<&str>) -> Option<String> {
    let text = text?;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?is)<codex_internal_context\b[^>]*\bsource=["']goal["'][^>]*>.*?<objective>\s*(.*?)\s*</objective>"#).unwrap()
    });
    re.captures(text)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().trim().to_string())
        .filter(|value| !value.is_empty())
}

fn goal_text_key(text: Option<&str>) -> Option<String> {
    let text = text.filter(|value| !value.trim().is_empty())?;
    let references = attachment_references(text);
    if !references.is_empty() {
        return Some(format!(
            "attachment:{}",
            references
                .iter()
                .map(|reference| reference.normalized_path.as_str())
                .collect::<Vec<_>>()
                .join("|")
        ));
    }
    let mut normalized = text.trim().to_string();
    if normalized.to_ascii_lowercase().starts_with("/goal") {
        normalized = normalized[5..].trim_start().to_string();
    }
    static WHITESPACE_RE: OnceLock<Regex> = OnceLock::new();
    let whitespace = residual_static_regex(&WHITESPACE_RE, r"\s+");
    normalized = whitespace
        .as_regex()
        .replace_all(&normalized, " ")
        .to_string();
    Some(format!("text:{}", sha256_text(Some(&normalized))))
}

fn goal_update_is_fresh_input(update: &Obj) -> bool {
    let event_time = value_str(update.get("create_time")).and_then(parse_datetime_text);
    let created_at = value_str(update.get("created_at")).and_then(parse_datetime_text);
    matches!((event_time, created_at), (Some(event), Some(created)) if (event - created).abs() <= 2_000_000)
}

fn json_object(value: Option<&Value>) -> Obj {
    let Some(value) = value else {
        return Obj::new();
    };
    if let Some(map) = value.as_object() {
        return map.clone();
    }
    let Some(text) = value.as_str().filter(|text| !text.trim().is_empty()) else {
        return Obj::new();
    };
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn build_user_input_request(
    payload: &Obj,
    record_time: Option<Timestamp>,
    current_turn_id: Option<&str>,
) -> Option<(String, Value)> {
    if !matches!(
        value_str(payload.get("name")),
        Some("request_user_input" | "request_user_input_async")
    ) {
        return None;
    }
    let call_id = value_str(payload.get("call_id"))
        .filter(|value| !value.trim().is_empty())?
        .to_string();
    let arguments = json_object(payload.get("arguments"));
    let questions = arguments.get("questions")?.as_array()?.clone();
    let mut request = obj!(
        "call_id" => Value::String(call_id.clone()),
        "status" => Value::String("pending".to_string()),
        "questions" => Value::Array(questions),
    );
    if value_str(payload.get("name")) == Some("request_user_input_async") {
        request.insert(
            "request_mode".to_string(),
            Value::String("async".to_string()),
        );
    }
    if let Some(turn_id) = turn_id_from_tool_payload(payload, current_turn_id) {
        request.insert("turn_id".to_string(), Value::String(turn_id));
    }
    if let Some(record_time) = record_time {
        request.insert(
            "create_time".to_string(),
            Value::String(utc_iso(record_time)),
        );
    }
    Some((call_id, Value::Object(request)))
}

fn complete_user_input_request(request: &mut Value, payload: &Obj, record_time: Option<Timestamp>) {
    let Some(request) = request.as_object_mut() else {
        return;
    };
    if value_str(request.get("request_mode")) == Some("async") {
        let output = json_object(payload.get("output"));
        if let Some(record_time) = record_time {
            request.insert(
                "acknowledged_at".to_string(),
                Value::String(utc_iso(record_time)),
            );
        }
        let status = if let Some(accepted) = output.get("accepted").and_then(Value::as_bool) {
            request.insert("accepted".to_string(), Value::Bool(accepted));
            if accepted {
                "awaiting_response"
            } else {
                "delivery_rejected"
            }
        } else {
            "acknowledgment_unparsed"
        };
        request.insert("status".to_string(), Value::String(status.to_string()));
        return;
    }
    if let Some(record_time) = record_time {
        request.insert(
            "answered_at".to_string(),
            Value::String(utc_iso(record_time)),
        );
    }
    let output = json_object(payload.get("output"));
    if let Some(answers) = output.get("answers").and_then(Value::as_object) {
        request.insert("status".to_string(), Value::String("answered".to_string()));
        request.insert("answers".to_string(), Value::Object(answers.clone()));
    } else {
        request.insert(
            "status".to_string(),
            Value::String("answer_unparsed".to_string()),
        );
        if let Some(raw_output) = payload.get("output") {
            request.insert("raw_output".to_string(), raw_output.clone());
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AttachmentReference {
    path: String,
    normalized_path: String,
    attachment_id: String,
    filename: String,
}

#[derive(Debug)]
struct AttachmentPathConfig {
    normalized_root: String,
    regex: Regex,
}

static ATTACHMENT_PATH_CONFIG: OnceLock<AttachmentPathConfig> = OnceLock::new();

fn separator_tolerant_path_pattern(value: &str) -> String {
    value
        .trim_end_matches(&['\\', '/'][..])
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(r"[\\/]+")
}

fn build_attachment_path_config(root: &str) -> Result<AttachmentPathConfig> {
    let root = root.trim();
    if root.is_empty() {
        bail!("attachment root must not be empty");
    }
    let root_pattern = separator_tolerant_path_pattern(root);
    if root_pattern.is_empty() {
        bail!("attachment root could not be normalized");
    }
    let regex = Regex::new(&format!(
        r#"(?i)(?P<path>{root_pattern}[\\/]+(?P<attachment_id>[0-9a-f]{{8}}-[0-9a-f]{{4}}-[0-9a-f]{{4}}-[0-9a-f]{{4}}-[0-9a-f]{{12}})[\\/]+(?P<filename>[^\r\n\\/:*?"<>|]+?\.[A-Za-z0-9]{{1,16}}))(?:$|[\s'"`\)\]\}},;:.，。；：！？、])"#,
    ))
    .map_err(|error| anyhow!("unable to compile attachment path matcher: {error}"))?;
    Ok(AttachmentPathConfig {
        normalized_root: normalize_windows_path(root),
        regex,
    })
}

fn configure_attachment_root(root: &Path) -> Result<()> {
    let config = build_attachment_path_config(&root.to_string_lossy())?;
    ATTACHMENT_PATH_CONFIG
        .set(config)
        .map_err(|_| anyhow!("attachment root was configured more than once"))
}

fn attachment_path_config() -> &'static AttachmentPathConfig {
    ATTACHMENT_PATH_CONFIG.get_or_init(|| {
        #[cfg(test)]
        let root = PathBuf::from(TEST_ATTACHMENT_ROOT);
        #[cfg(not(test))]
        let root = codex_paths::default_path("attachments");
        build_attachment_path_config(&root.to_string_lossy())
            .expect("attachment root must be configured before parsing")
    })
}

fn normalized_attachment_path_with_root(value: &str, normalized_root: &str) -> Option<String> {
    let stripped = value.trim().trim_matches(&['\'', '"', '`'][..]);
    let normalized = normalize_windows_path(stripped);
    let prefix = format!("{normalized_root}\\");
    if !normalized.starts_with(&prefix) {
        return None;
    }
    let relative = &normalized[prefix.len()..];
    let parts: Vec<_> = relative
        .split('\\')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() != 2 || parts.iter().any(|part| matches!(*part, "." | "..")) {
        return None;
    }
    static UUID_RE: OnceLock<Regex> = OnceLock::new();
    let uuid_re = residual_static_regex(
        &UUID_RE,
        r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
    );
    if !uuid_re.as_regex().is_match(parts[0]) {
        return None;
    }
    Some(normalized)
}

fn attachment_references_with_config(
    text: &str,
    config: &AttachmentPathConfig,
) -> Vec<AttachmentReference> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut references = Vec::new();
    let mut seen = HashSet::new();
    for captures in config.regex.captures_iter(text) {
        let Some(path_match) = captures.name("path") else {
            continue;
        };
        let whole = path_match.as_str();
        let Some(normalized) = normalized_attachment_path_with_root(whole, &config.normalized_root)
        else {
            continue;
        };
        if !seen.insert(normalized.clone()) {
            continue;
        }
        references.push(AttachmentReference {
            path: whole.to_string(),
            normalized_path: normalized,
            attachment_id: captures
                .name("attachment_id")
                .expect("attachment id capture")
                .as_str()
                .to_string(),
            filename: captures
                .name("filename")
                .expect("attachment filename capture")
                .as_str()
                .to_string(),
        });
    }
    references
}

#[cfg(test)]
fn attachment_references_for_root(text: &str, root: &str) -> Result<Vec<AttachmentReference>> {
    let config = build_attachment_path_config(root)?;
    Ok(attachment_references_with_config(text, &config))
}

fn attachment_references(text: &str) -> Vec<AttachmentReference> {
    attachment_references_with_config(text, attachment_path_config())
}

fn exit_code_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?im)(?:Exit code:|Process exited with code)\s*(-?\d+)").unwrap()
    })
}

fn truncated_output_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)truncated output|output truncated|\[truncated").unwrap())
}

fn command_output_marker_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?im)^(?:Final output|Output):[ \t]*\r?\n").unwrap())
}

fn agent_message_type_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?im)^Message Type:\s*([^\r\n]+)").unwrap())
}

fn agent_task_name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?im)^Task name:\s*([^\r\n]+)").unwrap())
}

fn remove_one_tool_transport_newline(mut content: String) -> String {
    if content.ends_with("\r\n") {
        content.truncate(content.len() - 2);
    } else if content.ends_with('\n') || content.ends_with('\r') {
        content.pop();
    }
    content
}

fn replacement_character_count(value: &Value) -> usize {
    match value {
        Value::String(text) => text.matches('\u{fffd}').count(),
        Value::Array(values) => values.iter().map(replacement_character_count).sum(),
        Value::Object(map) => map.values().map(replacement_character_count).sum(),
        _ => 0,
    }
}

fn tool_output_text_segments(value: &Value) -> Vec<&str> {
    match value {
        Value::String(text) => vec![text.as_str()],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.as_object()
                    .and_then(|map| map.get("text"))
                    .and_then(Value::as_str)
            })
            .collect(),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn attachment_output_is_truncated(text: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    truncated_output_regex().is_match(text)
        || RE
            .get_or_init(|| {
                Regex::new(
                    r"(?i)(?:…|\.\.\.)\s*\d+\s+(?:tokens|characters)\s+truncated\s*(?:…|\.\.\.)",
                )
                .unwrap()
            })
            .is_match(text)
}

fn extract_successful_command_output(value: Option<&Value>) -> Option<String> {
    let segments = tool_output_text_segments(value?);
    let mut marker_segment = None;
    let mut marker_end = 0usize;
    for (index, segment) in segments.iter().enumerate() {
        if let Some(marker) = command_output_marker_regex().find_iter(segment).last() {
            marker_segment = Some(index);
            marker_end = marker.end();
        }
    }
    let index = marker_segment?;
    let mut pieces = vec![&segments[index][marker_end..]];
    pieces.extend(segments[index + 1..].iter().copied());
    let nonempty: Vec<&str> = pieces
        .into_iter()
        .filter(|piece| !piece.trim().is_empty())
        .collect();
    let mut command_results = Vec::<Obj>::new();
    let mut all_structured = true;
    for piece in &nonempty {
        match serde_json::from_str::<Value>(piece) {
            Ok(Value::Object(map))
                if map.get("output").is_some_and(Value::is_string)
                    && map.get("exit_code").and_then(Value::as_i64).is_some() =>
            {
                command_results.push(map);
            }
            Ok(_) => {}
            Err(_) => all_structured = false,
        }
    }
    if !command_results.is_empty() {
        // Separate rendering envelopes are not one stdout. Reject two owners,
        // including byte-identical candidates, rather than guessing ownership.
        if command_results.len() != 1 || !all_structured {
            return None;
        }
        let result = &command_results[0];
        if result.get("exit_code").and_then(Value::as_i64) != Some(0)
            || result.get("truncated").and_then(Value::as_bool) == Some(true)
            || result.get("output_truncated").and_then(Value::as_bool) == Some(true)
        {
            return None;
        }
        let content = raw_str(result.get("output"))?;
        if attachment_output_is_truncated(content) {
            return None;
        }
        return Some(remove_one_tool_transport_newline(content.to_string()));
    }
    if nonempty.len() != 1 {
        return None;
    }
    let diagnostic = segments[..=index].join("\n");
    let exit = exit_code_regex().captures(&diagnostic)?;
    if exit.get(1)?.as_str() != "0" || attachment_output_is_truncated(nonempty[0]) {
        return None;
    }
    Some(remove_one_tool_transport_newline(nonempty[0].to_string()))
}

fn attachment_metadata(
    reference: &AttachmentReference,
    content: Option<&str>,
    restoration_source: &str,
    restoration_status: &str,
) -> Obj {
    let mut output = obj!(
        "attachment_id" => Value::String(reference.attachment_id.clone()),
        "filename" => Value::String(reference.filename.clone()),
        "restoration_source" => Value::String(restoration_source.to_string()),
        "restoration_status" => Value::String(restoration_status.to_string()),
    );
    if let Some(content) = content {
        let bytes = content.as_bytes();
        output.insert(
            "sha256".to_string(),
            Value::String(sha256_text(Some(content))),
        );
        output.insert(
            "byte_length".to_string(),
            Value::Number((bytes.len() as u64).into()),
        );
        output.insert(
            "character_length".to_string(),
            Value::Number((content.chars().count() as u64).into()),
        );
    }
    output
}

fn read_attachment_from_disk(
    reference: &AttachmentReference,
    attachment_root: &Path,
) -> (Option<String>, String) {
    let path = if attachment_root.as_os_str().is_empty() {
        PathBuf::from(&reference.path)
    } else {
        attachment_root
            .join(&reference.attachment_id)
            .join(&reference.filename)
    };
    if !path.is_file() {
        return (None, "missing".to_string());
    }
    let Ok(data) = read_shared_bytes(&path) else {
        return (None, "unreadable".to_string());
    };
    match String::from_utf8(data) {
        Ok(mut text) => {
            if text.starts_with('\u{feff}') {
                text.remove(0);
            }
            (Some(text), "restored".to_string())
        }
        Err(_) => (None, "decode_error".to_string()),
    }
}

#[derive(Clone, Debug)]
struct AttachmentEntry {
    content: Option<String>,
    metadata: Obj,
}

type AttachmentRegistry = HashMap<String, AttachmentEntry>;

fn powershell_tokens(command: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut chars = command.chars().peekable();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|ch| ch.is_whitespace()) {
            chars.next();
        }
        let Some(first) = chars.next() else { break };
        if matches!(first, '\'' | '"' | '`') {
            let quote = first;
            let mut token = String::new();
            let mut closed = false;
            while let Some(ch) = chars.next() {
                if ch == quote {
                    if quote == '\'' && chars.peek() == Some(&'\'') {
                        token.push('\'');
                        chars.next();
                        continue;
                    }
                    closed = true;
                    break;
                }
                token.push(ch);
            }
            if !closed {
                return None;
            }
            tokens.push(token);
            continue;
        }
        let mut token = String::new();
        token.push(first);
        while chars.peek().is_some_and(|ch| !ch.is_whitespace()) {
            token.push(chars.next().expect("peeked token character"));
        }
        tokens.push(token);
    }
    Some(tokens)
}

fn simple_attachment_read_reference(command: &str) -> Option<AttachmentReference> {
    if command.trim().is_empty()
        || command
            .chars()
            .any(|ch| matches!(ch, '\r' | '\n' | ';' | '|' | '&'))
    {
        return None;
    }
    let references = attachment_references(command);
    if references.len() != 1 {
        return None;
    }
    let reference = references.into_iter().next()?;
    let tokens = powershell_tokens(command.trim())?;
    if tokens
        .first()
        .is_none_or(|token| !token.eq_ignore_ascii_case("get-content"))
    {
        return None;
    }
    let mut saw_raw = false;
    let mut saw_path = false;
    let mut index = 1usize;
    while index < tokens.len() {
        let token = &tokens[index];
        if token.eq_ignore_ascii_case("-raw") {
            saw_raw = true;
            index += 1;
            continue;
        }
        if token.eq_ignore_ascii_case("-literalpath") || token.eq_ignore_ascii_case("-path") {
            index += 1;
            let candidate = tokens.get(index)?;
            if normalized_attachment_path_with_root(
                candidate,
                &attachment_path_config().normalized_root,
            )
            .as_deref()
                != Some(reference.normalized_path.as_str())
            {
                return None;
            }
            saw_path = true;
            index += 1;
            continue;
        }
        if token.eq_ignore_ascii_case("-encoding") {
            index += 2;
            if index > tokens.len() {
                return None;
            }
            continue;
        }
        if !saw_path
            && normalized_attachment_path_with_root(
                token,
                &attachment_path_config().normalized_root,
            )
            .as_deref()
                == Some(reference.normalized_path.as_str())
        {
            saw_path = true;
            index += 1;
            continue;
        }
        return None;
    }
    (saw_raw && saw_path).then_some(reference)
}

fn attachment_read_reference_from_script(script: &str) -> Option<AttachmentReference> {
    let lower = script.to_ascii_lowercase();
    let invocation_count =
        lower.matches("tools.shell_command").count() + lower.matches("tools.exec_command").count();
    if invocation_count != 1 {
        return None;
    }
    let (executions, warnings) = if lower.contains("tools.exec_command") {
        javascript_exec_command_data(script)
    } else {
        javascript_command_data(script)
    };
    if !warnings.is_empty() || executions.len() != 1 {
        return None;
    }
    let command = value_str(executions[0].get("command"))?;
    simple_attachment_read_reference(command)
}

fn attachment_read_reference_from_call(payload: &Obj) -> Option<AttachmentReference> {
    match value_str(payload.get("name")) {
        Some("shell_command") => {
            let arguments = json_object(payload.get("arguments"));
            let command = raw_str(arguments.get("command"))?;
            simple_attachment_read_reference(command)
        }
        Some("exec") => attachment_read_reference_from_script(raw_str(payload.get("input"))?),
        _ => None,
    }
}

fn attachment_entry_from_values(
    reference: &AttachmentReference,
    values: Vec<String>,
    attachment_root: &Path,
) -> AttachmentEntry {
    let mut unique_values = Vec::<String>::new();
    let mut digests = HashSet::<String>::new();
    for value in values {
        let digest = sha256_text(Some(&value));
        if digests.insert(digest) {
            unique_values.push(value);
        }
    }
    if unique_values.len() == 1 {
        let content = unique_values.pop().expect("one attachment candidate");
        return AttachmentEntry {
            content: Some(content.clone()),
            metadata: attachment_metadata(
                reference,
                Some(&content),
                "rollout_tool_output",
                "restored",
            ),
        };
    }
    if unique_values.len() > 1 {
        return AttachmentEntry {
            content: None,
            metadata: attachment_metadata(
                reference,
                None,
                "rollout_tool_output",
                "content_conflict",
            ),
        };
    }
    let (content, status) = read_attachment_from_disk(reference, attachment_root);
    let source = if content.is_some() {
        "attachment_file"
    } else {
        "attachment_reference"
    };
    AttachmentEntry {
        metadata: attachment_metadata(reference, content.as_deref(), source, &status),
        content,
    }
}

fn collect_attachment_contents(records: &Records, attachment_root: &Path) -> AttachmentRegistry {
    let mut pending_reads = HashMap::<String, AttachmentReference>::new();
    let mut candidates = HashMap::<String, Vec<String>>::new();
    let mut references_by_path = HashMap::<String, AttachmentReference>::new();
    for (_, record) in records {
        let Some(record_map) = record.as_object() else {
            continue;
        };
        let Some(payload) = record_map.get("payload").and_then(Value::as_object) else {
            continue;
        };
        let payload_type = value_str(payload.get("type"));
        if value_str(record_map.get("type")) == Some("response_item")
            && matches!(payload_type, Some("function_call" | "custom_tool_call"))
        {
            if let (Some(reference), Some(call_id)) = (
                attachment_read_reference_from_call(payload),
                value_str(payload.get("call_id")),
            ) {
                references_by_path.insert(reference.normalized_path.clone(), reference.clone());
                pending_reads.insert(call_id.to_string(), reference);
            }
            continue;
        }
        if value_str(record_map.get("type")) == Some("response_item")
            && matches!(
                payload_type,
                Some("function_call_output" | "custom_tool_call_output")
            )
        {
            let Some(call_id) = value_str(payload.get("call_id")) else {
                continue;
            };
            let Some(reference) = pending_reads.remove(call_id) else {
                continue;
            };
            if let Some(content) = extract_successful_command_output(payload.get("output")) {
                candidates
                    .entry(reference.normalized_path.clone())
                    .or_default()
                    .push(content);
            }
        }
    }

    let mut all_references = references_by_path;
    for (_, record) in records {
        let Some(payload) = record
            .as_object()
            .and_then(|map| map.get("payload"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        if let Some(message) = get_message_text(payload.get("message")) {
            for reference in attachment_references(&message) {
                all_references
                    .entry(reference.normalized_path.clone())
                    .or_insert(reference);
            }
        }
        if let Some(goal) = payload.get("goal").and_then(Value::as_object) {
            if let Some(objective) = get_message_text(goal.get("objective")) {
                for reference in attachment_references(&objective) {
                    all_references
                        .entry(reference.normalized_path.clone())
                        .or_insert(reference);
                }
            }
        }
        if value_str(payload.get("type")) == Some("message") {
            if let Some(content) = get_message_text(payload.get("content")) {
                for reference in attachment_references(&content) {
                    all_references
                        .entry(reference.normalized_path.clone())
                        .or_insert(reference);
                }
            }
        }
    }

    let mut registry = HashMap::new();
    for (normalized, reference) in all_references {
        let values = candidates.remove(&normalized).unwrap_or_default();
        registry.insert(
            normalized,
            attachment_entry_from_values(&reference, values, attachment_root),
        );
    }
    registry
}

fn pasted_text_request_heading_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?im)^## My request:[ \t]*(?:\r\n|\n|\r)?").unwrap())
}

fn pasted_text_semantic_content(
    original: &str,
    references: &[AttachmentReference],
    registry: &AttachmentRegistry,
) -> Option<String> {
    if !original.contains("# Files pasted by the user:")
        || !original.contains("Pasted text contains the user's request.")
    {
        return None;
    }
    let mut pasted_contents = Vec::new();
    for reference in references {
        if !reference.filename.eq_ignore_ascii_case("pasted-text.txt") {
            continue;
        }
        let entry = registry.get(&reference.normalized_path)?;
        pasted_contents.push(entry.content.clone()?);
    }
    if pasted_contents.is_empty() {
        return None;
    }
    let trailing_request = pasted_text_request_heading_regex()
        .find(original)
        .map(|matched| &original[matched.end()..])
        .unwrap_or_default();
    let mut restored = pasted_contents.join("\n\n");
    if !trailing_request.trim().is_empty() {
        restored.push_str("\n\n");
        restored.push_str(trailing_request);
    }
    Some(restored)
}

fn message_attachment_transport_is_restorable(message: &Obj) -> bool {
    // Assistant output can legitimately document or link attachment paths.
    // Only user messages use those paths as Codex transport placeholders.
    value_str(message.get("role")) == Some("user")
}

fn replace_attachment_transport(
    text: &str,
    registry: &AttachmentRegistry,
) -> (String, Vec<Value>, bool) {
    let original = text.to_string();
    let references = attachment_references(&original);
    let mut metadata = Vec::new();
    for reference in &references {
        if let Some(entry) = registry.get(&reference.normalized_path) {
            metadata.push(Value::Object(entry.metadata.clone()));
        } else {
            metadata.push(Value::Object(attachment_metadata(
                reference,
                None,
                "attachment_reference",
                "missing",
            )));
        }
    }

    if let Some(restored) = pasted_text_semantic_content(&original, &references, registry) {
        return (restored, metadata, true);
    }

    let mut result = original.clone();
    let mut restored_any = false;
    for reference in &references {
        let Some(entry) = registry.get(&reference.normalized_path) else {
            continue;
        };
        let Some(content) = entry.content.as_deref() else {
            continue;
        };
        let escaped_path = regex::escape(&reference.path);
        let directive = Regex::new(&format!(
            r"(?is)(?:/goal\s+)?Read\s+the\s+Codex\b.*?\bfile\s+at\s+{}\s+before\s+continuing\.?",
            escaped_path
        ))
        .unwrap();
        let updated = directive
            .replacen(&result, 1, regex::NoExpand(content))
            .to_string();
        if updated != result {
            result = updated;
            restored_any = true;
            continue;
        }
        let path_re = Regex::new(&format!(r"(?i){}", escaped_path)).unwrap();
        let updated = path_re
            .replacen(&result, 1, regex::NoExpand(content))
            .to_string();
        if updated != result {
            result = updated;
            restored_any = true;
        }
    }
    (result, metadata, restored_any)
}

fn record_time_is_local(record: &Obj, create_time: Option<Timestamp>, is_subagent: bool) -> bool {
    if !is_subagent || create_time.is_none() {
        return true;
    }
    parse_datetime_value(record.get("timestamp")).is_some_and(|time| time >= create_time.unwrap())
}

fn call_turn_id(payload: &Obj, current_turn_id: Option<&str>) -> Option<String> {
    turn_id_from_message_payload(payload).or_else(|| current_turn_id.map(str::to_string))
}

fn profile_from_turn_context(payload: &Obj) -> Obj {
    let mut profile = Obj::new();
    let collaboration_mode = payload
        .get("collaboration_mode")
        .and_then(|value| {
            value
                .as_object()
                .and_then(|map| value_str(map.get("mode")))
                .or_else(|| value_str(Some(value)))
        })
        .map(str::to_string);
    let sandbox = payload.get("sandbox_policy").and_then(Value::as_object);
    let sandbox_type = sandbox
        .and_then(|map| value_str(map.get("type")))
        .map(str::to_string);
    let network_access = sandbox.and_then(|map| bool_value(map.get("network_access")));
    for (key, value) in [
        ("model", value_str(payload.get("model")).map(str::to_string)),
        (
            "reasoning_effort",
            value_str(payload.get("effort")).map(str::to_string),
        ),
        (
            "reasoning_summary",
            value_str(payload.get("summary")).map(str::to_string),
        ),
        ("collaboration_mode", collaboration_mode),
        (
            "multi_agent_mode",
            value_str(payload.get("multi_agent_mode")).map(str::to_string),
        ),
        (
            "multi_agent_version",
            value_str(payload.get("multi_agent_version")).map(str::to_string),
        ),
        (
            "approval_policy",
            value_str(payload.get("approval_policy")).map(str::to_string),
        ),
        (
            "approvals_reviewer",
            value_str(payload.get("approvals_reviewer")).map(str::to_string),
        ),
        ("sandbox_type", sandbox_type),
    ] {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            profile.insert(key.to_string(), Value::String(value));
        }
    }
    if let Some(network_access) = network_access {
        profile.insert("network_access".to_string(), Value::Bool(network_access));
    }
    profile
}

fn execution_profile_summary(turns: &[Value]) -> Option<Value> {
    let profiles: Vec<&Obj> = turns
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|turn| turn.get("execution_profile").and_then(Value::as_object))
        .collect();
    if profiles.is_empty() {
        return None;
    }
    let mut output = Obj::new();
    for (field, prefix, observed_key) in [
        (
            "reasoning_effort",
            "reasoning_effort",
            "reasoning_efforts_observed",
        ),
        (
            "reasoning_summary",
            "reasoning_summary",
            "reasoning_summaries_observed",
        ),
        ("model", "model", "models_observed"),
    ] {
        let mut observed = Vec::<String>::new();
        for profile in &profiles {
            if let Some(value) = value_str(profile.get(field)) {
                if !observed.iter().any(|item| item == value) {
                    observed.push(value.to_string());
                }
            }
        }
        if !observed.is_empty() {
            output.insert(
                format!("initial_{prefix}"),
                Value::String(observed[0].clone()),
            );
            output.insert(
                format!("final_{prefix}"),
                Value::String(observed.last().unwrap().clone()),
            );
            output.insert(
                observed_key.to_string(),
                Value::Array(observed.into_iter().map(Value::String).collect()),
            );
        }
    }
    for field in [
        "collaboration_mode",
        "multi_agent_mode",
        "multi_agent_version",
        "approval_policy",
        "approvals_reviewer",
        "sandbox_type",
        "network_access",
    ] {
        let mut observed = Vec::<Value>::new();
        for profile in &profiles {
            if let Some(value) = profile.get(field).filter(|value| !value.is_null()) {
                if !observed.iter().any(|item| item == value) {
                    observed.push(value.clone());
                }
            }
        }
        if observed.len() == 1 {
            output.insert(field.to_string(), observed.remove(0));
        } else if !observed.is_empty() {
            output.insert(format!("{field}s_observed"), Value::Array(observed));
        }
    }
    (!output.is_empty()).then_some(Value::Object(output))
}

fn renumber_messages(messages: &[Value]) -> (Vec<Value>, HashMap<String, String>) {
    let mut counters = HashMap::<String, usize>::new();
    let mut output = Vec::new();
    let mut id_map = HashMap::new();
    for message in messages {
        let Some(map) = message.as_object() else {
            continue;
        };
        let mut item = map.clone();
        let role = value_str(item.get("role")).unwrap_or_default();
        let prefix = if role == "user" { "u" } else { "a" };
        let count = next_counter(&mut counters, prefix);
        let new_id = format!("{prefix}{count:02}");
        if let Some(old_id) = raw_str(item.get("id")) {
            id_map.insert(old_id.to_string(), new_id.clone());
        }
        item.insert("id".to_string(), Value::String(new_id));
        output.push(Value::Object(item));
    }
    (output, id_map)
}

fn replace_message_ids(value: &Value, id_map: &HashMap<String, String>) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| replace_message_ids(item, id_map))
                .collect(),
        ),
        Value::Object(map) => {
            let mut output = Obj::new();
            for (key, item) in map {
                if let Some(text) = item
                    .as_str()
                    .filter(|_| key == "message_id" || key.ends_with("_message_id"))
                {
                    output.insert(
                        key.clone(),
                        Value::String(
                            id_map
                                .get(text)
                                .cloned()
                                .unwrap_or_else(|| text.to_string()),
                        ),
                    );
                } else {
                    output.insert(key.clone(), replace_message_ids(item, id_map));
                }
            }
            Value::Object(output)
        }
        _ => value.clone(),
    }
}

fn clean_dynamic_command_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => {
            Value::Array(items.iter().map(clean_dynamic_command_value).collect())
        }
        Value::Object(map) => {
            let mut output = Obj::new();
            for (key, item) in map {
                output.insert(key.clone(), clean_dynamic_command_value(item));
            }
            let template = value_str(output.get("command_template"));
            let source_kind = value_str(output.get("source_kind"));
            let unresolved = source_kind == Some("dynamic_template")
                || template
                    .is_some_and(|template| template.trim().len() <= 1 || template.contains("${"));
            if unresolved {
                output.insert(
                    "source_kind".to_string(),
                    Value::String("dynamic_script".to_string()),
                );
                output.insert(
                    "language".to_string(),
                    Value::String("javascript".to_string()),
                );
                output.insert(
                    "parse_status".to_string(),
                    Value::String("not_fully_resolved".to_string()),
                );
                output.insert(
                    "summary".to_string(),
                    Value::String(
                        "Dynamic command script; concrete invocations could not be fully resolved."
                            .to_string(),
                    ),
                );
                output.remove("command_template");
                if output.contains_key("parse_warnings") {
                    output.insert(
                        "parse_warnings".to_string(),
                        Value::Array(vec![Value::String("Dynamic script was summarized because concrete commands were not reliably recoverable.".to_string())]),
                    );
                }
            }
            Value::Object(output)
        }
        _ => value.clone(),
    }
}

fn max_timestamp_in_value(value: &Value, key_hint: &str) -> Option<Timestamp> {
    match value {
        Value::Object(map) => {
            let mut latest = None;
            for (key, item) in map {
                let normalized_key = key.to_ascii_lowercase();
                if matches!(
                    normalized_key.as_str(),
                    "retry_at" | "scheduled_at" | "next_run_at" | "expires_at" | "expiration_time"
                ) {
                    continue;
                }
                if let Some(candidate) = max_timestamp_in_value(item, key) {
                    latest =
                        Some(latest.map_or(candidate, |current: Timestamp| current.max(candidate)));
                }
            }
            latest
        }
        Value::Array(items) => items
            .iter()
            .filter_map(|item| max_timestamp_in_value(item, key_hint))
            .max(),
        Value::String(text) => {
            let normalized = key_hint.to_ascii_lowercase();
            if normalized.ends_with("time")
                || normalized.ends_with("_at")
                || normalized.ends_with("started")
                || normalized.ends_with("completed")
                || normalized.ends_with("updated")
                || matches!(
                    normalized.as_str(),
                    "create_time" | "update_time" | "start_time" | "end_time" | "compacted_at"
                )
            {
                parse_datetime_text(text)
            } else {
                None
            }
        }
        _ => None,
    }
}

// Traverse the existing Session payload by reference; previously both finalize
// paths built a temporary serde_json::Value and deeply cloned its arrays.
// This retains identical key hints, excluded timestamp fields and max semantics.
fn latest_session_payload_timestamp(session: &Session) -> Option<Timestamp> {
    let mut latest = None;
    for (hint, items) in [
        ("turns", &session.turns),
        ("messages", &session.messages),
        ("goals", &session.goals),
        ("goal_updates", &session.goal_updates),
        ("plan_updates", &session.plan_updates),
        ("user_input_requests", &session.user_input_requests),
        ("activities", &session.activities),
        ("changes", &session.changes),
        ("agent_events", &session.agent_events),
    ] {
        for item in items {
            if let Some(candidate) = max_timestamp_in_value(item, hint) {
                latest =
                    Some(latest.map_or(candidate, |current: Timestamp| current.max(candidate)));
            }
        }
    }
    for (hint, item) in [
        ("usage", session.usage.as_ref()),
        ("context", session.context.as_ref()),
    ] {
        if let Some(item) = item {
            if let Some(candidate) = max_timestamp_in_value(item, hint) {
                latest =
                    Some(latest.map_or(candidate, |current: Timestamp| current.max(candidate)));
            }
        }
    }
    latest
}

#[derive(Clone, Debug)]
struct UsageLimit {
    limit_id: Option<String>,
    first_reached_at: Option<Timestamp>,
    reset_at: Option<Timestamp>,
    window_minutes: Option<i64>,
    used_percent: Option<f64>,
}

impl Default for UsageLimit {
    fn default() -> Self {
        Self {
            limit_id: None,
            first_reached_at: None,
            reset_at: None,
            window_minutes: None,
            used_percent: None,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingCompaction {
    sequence: usize,
    compacted_at: Option<Timestamp>,
    before: Option<Value>,
}

#[derive(Clone, Debug)]
struct PendingWebCall {
    turn_id: Option<String>,
    input: String,
}

#[derive(Clone, Debug)]
struct PendingUnparsedCommand {
    turn_id: Option<String>,
    time: Option<Timestamp>,
    sequence: usize,
    parse_warnings: Vec<String>,
}

fn resolve_task_complete_end_time(
    effective_start: Option<Timestamp>,
    record_time: Option<Timestamp>,
    payload_completed_at: Option<Timestamp>,
) -> (Option<Timestamp>, Option<Value>) {
    let selected = record_time.or(payload_completed_at);
    let (Some(start), Some(record_end), Some(payload_end)) =
        (effective_start, record_time, payload_completed_at)
    else {
        return (selected, None);
    };
    if record_end < start && payload_end >= start {
        let conflict = obj!(
            "record_time" => Value::String(utc_iso(record_end)),
            "payload_completed_at" => Value::String(utc_iso(payload_end)),
            "selected" => Value::String("payload_completed_at".to_string()),
        );
        (Some(payload_end), Some(Value::Object(conflict)))
    } else {
        (selected, None)
    }
}

fn new_turn(
    turn_id: &str,
    status: &str,
    _sequence: usize,
    start_time: Option<Timestamp>,
    end_time: Option<Timestamp>,
    abort_reason: Option<String>,
    mode: Option<String>,
    duration_ms: Option<i64>,
    time_to_first_token_ms: Option<i64>,
) -> TurnInfo {
    TurnInfo {
        turn_id: turn_id.to_string(),
        status: status.to_string(),
        start_time,
        end_time,
        abort_reason,
        mode,
        duration_ms,
        time_to_first_token_ms,
        termination: None,
        timestamp_conflict: None,
    }
}

fn goal_is_effective(goal: &GoalInfo, has_turn_events: bool, effective: &HashSet<String>) -> bool {
    if !has_turn_events {
        return true;
    }
    goal.created_turn_id
        .as_ref()
        .is_some_and(|id| effective.contains(id))
        || goal
            .last_turn_id
            .as_ref()
            .is_some_and(|id| effective.contains(id))
}

// ---------------------------------------------------------------------------
// v0.4 fused streaming session builder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamControl {
    Continue,
    Stop,
}

fn for_each_rollout_record<F>(path: &Path, callback: F) -> Result<SourceDataQuality>
where
    F: FnMut(usize, Value) -> Result<StreamControl>,
{
    #[cfg(feature = "integrated-study")]
    let study_read = integrated_study::scope(24, 0);
    let bytes = rollout_codec::read_rollout_snapshot(path)?;

    #[cfg(feature = "integrated-study")]
    drop(study_read);

    if LEGACY_LINE_SPLITTER_ENABLED.load(AtomicOrdering::Relaxed) {
        consume_rollout_segments(
            path,
            bytes.len(),
            bytes.split_inclusive(|byte| *byte == b'\n'),
            callback,
        )
    } else {
        consume_rollout_segments(
            path,
            bytes.len(),
            jsonl_segments::JsonlSegments::new(&bytes),
            callback,
        )
    }
}

fn consume_rollout_segments<'a, F, I>(
    path: &Path,
    snapshot_len: usize,
    segments: I,
    mut callback: F,
) -> Result<SourceDataQuality>
where
    F: FnMut(usize, Value) -> Result<StreamControl>,
    I: Iterator<Item = &'a [u8]>,
{
    let mut ingestion = RolloutIngestionState::default();
    let mut line_number = 0usize;
    let mut consumed = 0usize;
    for segment in segments {
        line_number += 1;
        consumed = consumed.saturating_add(segment.len());
        let mut line = segment;
        if line.ends_with(b"\n") {
            line = &line[..line.len() - 1];
        }
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let is_final_physical_line = consumed >= snapshot_len;
        #[cfg(feature = "integrated-study")]
        let study_family = if integrated_study::enabled() {
            let p = canonical_prefix_fields(line);
            integrated_study::family(
                p.as_ref().map(|v| v.record_type),
                p.as_ref().and_then(|v| v.payload_type),
            )
        } else {
            11
        };
        let record = {
            #[cfg(feature = "integrated-study")]
            let _study_scope = integrated_study::scope(study_family, line.len());
            match parse_rollout_line_prefix(line, &mut ingestion) {
                Ok(Some(record)) => record,
                Ok(None) => continue,
                Err(_) if is_final_physical_line && !rollout_codec::is_compressed_rollout(path) => {
                    eprintln!(
                        "WARNING: Ignoring an incomplete final JSON line in {}.",
                        path.display()
                    );
                    break;
                }
                Err(error) => {
                    bail!(
                        "Invalid JSON at line {line_number} in {}: {error}",
                        path.display()
                    )
                }
            }
        };
        #[cfg(feature = "integrated-study")]
        integrated_study::event(5);
        #[cfg(feature = "integrated-study")]
        let _study_reduce = integrated_study::scope(study_family + 12, line.len());
        if callback(line_number, record)? == StreamControl::Stop {
            break;
        }
    }
    Ok(ingestion.data_quality)
}

#[derive(Default)]
struct AttachmentCollector {
    pending_reads: HashMap<String, AttachmentReference>,
    candidates: HashMap<String, Vec<String>>,
    references_by_path: HashMap<String, AttachmentReference>,
}

impl AttachmentCollector {
    fn observe(&mut self, record: &Value) {
        let Some(record_map) = record.as_object() else {
            return;
        };
        let Some(payload) = record_map.get("payload").and_then(Value::as_object) else {
            return;
        };
        let record_type = value_str(record_map.get("type"));
        let payload_type = value_str(payload.get("type"));

        if record_type == Some("response_item")
            && matches!(payload_type, Some("function_call" | "custom_tool_call"))
        {
            if let (Some(reference), Some(call_id)) = (
                attachment_read_reference_from_call(payload),
                value_str(payload.get("call_id")),
            ) {
                self.references_by_path
                    .insert(reference.normalized_path.clone(), reference.clone());
                self.pending_reads.insert(call_id.to_string(), reference);
            }
        }

        if record_type == Some("response_item")
            && matches!(
                payload_type,
                Some("function_call_output" | "custom_tool_call_output")
            )
        {
            if let Some(call_id) = value_str(payload.get("call_id")) {
                if let Some(reference) = self.pending_reads.remove(call_id) {
                    if let Some(content) = extract_successful_command_output(payload.get("output"))
                    {
                        self.candidates
                            .entry(reference.normalized_path.clone())
                            .or_default()
                            .push(content);
                    }
                }
            }
        }

        if let Some(message) = get_message_text(payload.get("message")) {
            self.record_references(&message);
        }
        if let Some(goal) = payload.get("goal").and_then(Value::as_object) {
            if let Some(objective) = get_message_text(goal.get("objective")) {
                self.record_references(&objective);
            }
        }
        if payload_type == Some("message") {
            if let Some(content) = get_message_text(payload.get("content")) {
                self.record_references(&content);
            }
        }
    }

    fn record_references(&mut self, text: &str) {
        for reference in attachment_references(text) {
            self.references_by_path
                .entry(reference.normalized_path.clone())
                .or_insert(reference);
        }
    }

    fn finish(mut self, attachment_root: &Path) -> AttachmentRegistry {
        let mut registry = HashMap::new();
        for (normalized, reference) in self.references_by_path {
            let values = self.candidates.remove(&normalized).unwrap_or_default();
            registry.insert(
                normalized,
                attachment_entry_from_values(&reference, values, attachment_root),
            );
        }
        registry
    }
}

#[derive(Default)]
struct CoreAccumulator {
    models: Vec<String>,
    model_set: HashSet<String>,
    messages: Vec<RawMessage>,
    plan_items: Vec<PlanItem>,
    goals: Vec<GoalInfo>,
    goal_indexes: HashMap<String, usize>,
    pending_goal_thread_id: Option<String>,
    usage_limits: HashMap<String, UsageLimit>,
    rollbacks: Vec<RollbackInfo>,
    changes: Vec<ChangeInfo>,
    activities: Vec<Activity>,
    pending_user_turns: HashMap<String, VecDeque<String>>,
    command_calls: HashMap<String, usize>,
    pending_unparsed_command_calls: HashMap<String, PendingUnparsedCommand>,
    user_input_requests: Vec<Value>,
    pending_user_input_requests: HashMap<String, usize>,
    pending_web_calls: VecDeque<PendingWebCall>,
    turn_stack: Vec<String>,
    turn_info: HashMap<String, TurnInfo>,
    has_turn_events: bool,
    current_turn_id: Option<String>,
    sequence: HashMap<String, usize>,
    context_window_tokens: Option<i64>,
    latest_context_used_tokens: Option<i64>,
    last_context_snapshot: Option<Value>,
    context_compactions: Vec<Value>,
    pending_compaction: Option<PendingCompaction>,
}

impl CoreAccumulator {
    fn consume(&mut self, line_number: usize, record: &Value, cwd: &str) -> Result<()> {
        let Some(record_map) = record.as_object() else {
            return Ok(());
        };
        let record_type = value_str(record_map.get("type")).unwrap_or_default();
        let payload_empty = Obj::new();
        let payload = record_map
            .get("payload")
            .and_then(Value::as_object)
            .unwrap_or(&payload_empty);
        let payload_type = value_str(payload.get("type")).unwrap_or_default();
        let ignored = matches!(
            (record_type, payload_type),
            ("event_msg", "agent_reasoning")
                | ("response_item", "reasoning")
                | ("event_msg", "agent_message")
                | ("event_msg", "context_compacted")
        ) || matches!(record_type, "world_state" | "session_meta");
        if ignored {
            return Ok(());
        }
        let record_time = parse_datetime_value(record_map.get("timestamp"));

        if record_type == "compacted" {
            if let Some(pending) = self.pending_compaction.take() {
                let mut unfinished = obj!(
                    "sequence" => Value::Number((pending.sequence as u64).into()),
                    "compacted_at" => pending.compacted_at.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
                );
                if let Some(before) = pending.before {
                    unfinished.insert("before".to_string(), before);
                }
                self.context_compactions.push(Value::Object(unfinished));
            }
            let compaction_sequence = next_counter(&mut self.sequence, "compaction");
            self.pending_compaction = Some(PendingCompaction {
                sequence: compaction_sequence,
                compacted_at: record_time,
                before: self.last_context_snapshot.clone(),
            });
            return Ok(());
        }

        if record_type == "event_msg" && payload_type == "task_started" {
            if let Some(window) = int_value(payload.get("model_context_window"), Some(1)) {
                self.context_window_tokens = Some(window);
            }
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                self.has_turn_events = true;
                self.current_turn_id = Some(turn_id.to_string());
                let mode = value_str(payload.get("collaboration_mode_kind")).map(str::to_string);
                if !self.turn_info.contains_key(turn_id) {
                    let turn_sequence = next_counter(&mut self.sequence, "turn");
                    self.turn_info.insert(
                        turn_id.to_string(),
                        new_turn(
                            turn_id,
                            "in_progress",
                            turn_sequence,
                            record_time,
                            None,
                            None,
                            mode,
                            None,
                            None,
                        ),
                    );
                } else if let Some(mode) = mode {
                    self.turn_info.get_mut(turn_id).unwrap().mode = Some(mode);
                }
                if !self.turn_stack.iter().any(|item| item == turn_id) {
                    self.turn_stack.push(turn_id.to_string());
                }
                if let Some(thread_id) = self.pending_goal_thread_id.clone() {
                    if let Some(index) = self.goal_indexes.get(&thread_id).copied() {
                        let goal = &mut self.goals[index];
                        if goal
                            .created_turn_id
                            .as_deref()
                            .unwrap_or_default()
                            .trim()
                            .is_empty()
                        {
                            goal.created_turn_id = Some(turn_id.to_string());
                        }
                        goal.last_turn_id = Some(turn_id.to_string());
                        goal.awaiting_task_start = false;
                        self.pending_goal_thread_id = None;
                    }
                }
            }
            return Ok(());
        }

        if record_type == "event_msg" && payload_type == "token_count" {
            if let Some(info) = payload.get("info").and_then(Value::as_object) {
                let parsed_window = int_value(info.get("model_context_window"), Some(1));
                let parsed_used = info
                    .get("last_token_usage")
                    .and_then(Value::as_object)
                    .and_then(|usage| int_value(usage.get("total_tokens"), Some(0)));
                if parsed_window.is_some() {
                    self.context_window_tokens = parsed_window;
                }
                if parsed_used.is_some() {
                    self.latest_context_used_tokens = parsed_used;
                }
                if let (Some(window), Some(used)) = (parsed_window, parsed_used) {
                    let snapshot = context_snapshot(window, used);
                    self.last_context_snapshot = snapshot.clone();
                    if let (Some(pending), Some(snapshot)) =
                        (self.pending_compaction.take(), snapshot)
                    {
                        let mut completed = obj!(
                            "sequence" => Value::Number((pending.sequence as u64).into()),
                            "compacted_at" => pending.compacted_at.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
                        );
                        if let Some(before) = pending.before {
                            completed.insert("before".to_string(), before);
                        }
                        completed.insert("after".to_string(), snapshot);
                        self.context_compactions.push(Value::Object(completed));
                    }
                }
                let rate_limits = payload
                    .get("rate_limits")
                    .and_then(Value::as_object)
                    .or_else(|| info.get("rate_limits").and_then(Value::as_object));
                if let (Some(rate_limits), Some(turn_id)) =
                    (rate_limits, self.current_turn_id.as_deref())
                {
                    if let Some(primary) = rate_limits.get("primary").and_then(Value::as_object) {
                        let limit = self.usage_limits.entry(turn_id.to_string()).or_default();
                        limit.limit_id = value_str(rate_limits.get("limit_id"))
                            .map(str::to_string)
                            .or(limit.limit_id.take());
                        if let Some(used) = float_value(primary.get("used_percent"), Some(0.0)) {
                            limit.used_percent =
                                Some(limit.used_percent.map_or(used, |current| current.max(used)));
                            if used >= 100.0 && limit.first_reached_at.is_none() {
                                limit.first_reached_at = record_time;
                            }
                        }
                        if let Some(reset) = parse_unix_seconds(primary.get("resets_at")) {
                            limit.reset_at = Some(reset);
                        }
                        if let Some(window) = int_value(primary.get("window_minutes"), Some(1)) {
                            limit.window_minutes = Some(window);
                        }
                    }
                }
            }
            return Ok(());
        }

        if record_type == "event_msg" && payload_type == "thread_goal_updated" {
            let Some(goal_payload) = payload.get("goal").and_then(Value::as_object) else {
                return Ok(());
            };
            let thread_id = value_str(payload.get("threadId"))
                .or_else(|| value_str(goal_payload.get("threadId")));
            let Some(thread_id) = thread_id else {
                return Ok(());
            };
            let active_turn = self
                .current_turn_id
                .as_deref()
                .filter(|turn_id| {
                    self.turn_info
                        .get(*turn_id)
                        .is_some_and(|turn| turn.status == "in_progress")
                })
                .map(str::to_string);
            let index = if let Some(index) = self.goal_indexes.get(thread_id).copied() {
                index
            } else {
                let goal_sequence = next_counter(&mut self.sequence, "goal");
                let index = self.goals.len();
                self.goals.push(GoalInfo {
                    thread_id: thread_id.to_string(),
                    created_turn_id: active_turn.clone(),
                    last_turn_id: active_turn.clone(),
                    objective: None,
                    reported_status: None,
                    status_source: "thread_goal_updated".to_string(),
                    created_at: None,
                    updated_at: None,
                    awaiting_task_start: active_turn.is_none(),
                    interruptions: Vec::new(),
                    sequence: goal_sequence,
                });
                self.goal_indexes.insert(thread_id.to_string(), index);
                index
            };
            let goal = &mut self.goals[index];
            let created_at = parse_unix_seconds(goal_payload.get("createdAt"));
            if let (Some(created_at), Some(record_time), Some(existing)) =
                (created_at, record_time, goal.created_at)
            {
                if created_at != existing && (record_time - created_at).abs() <= 2_000_000 {
                    goal.created_turn_id = active_turn.clone();
                    goal.last_turn_id = active_turn.clone();
                    goal.objective = None;
                    goal.reported_status = None;
                    goal.created_at = Some(created_at);
                    goal.updated_at = None;
                    goal.awaiting_task_start = active_turn.is_none();
                    goal.interruptions.clear();
                }
            }
            if let Some(active_turn) = active_turn {
                if goal
                    .created_turn_id
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                {
                    goal.created_turn_id = Some(active_turn.clone());
                }
                goal.last_turn_id = Some(active_turn);
                goal.awaiting_task_start = false;
                if self.pending_goal_thread_id.as_deref() == Some(thread_id) {
                    self.pending_goal_thread_id = None;
                }
            } else {
                self.pending_goal_thread_id = Some(thread_id.to_string());
                goal.awaiting_task_start = true;
            }
            if let Some(objective) = nonblank_raw_str(goal_payload.get("objective")) {
                goal.objective = Some(objective.to_string());
            }
            if let Some(status) = value_str(goal_payload.get("status")) {
                let status_lower = status.to_ascii_lowercase();
                goal.reported_status = Some(status_lower.clone());
                if status_lower == "active" {
                    if let Some(record_time) = record_time {
                        if let Some(interruption) =
                            goal.interruptions.iter_mut().rev().find(|item| {
                                item.resumed_at.is_none()
                                    && item.interrupted_at.is_some_and(|at| record_time > at)
                            })
                        {
                            interruption.resumed_at = Some(record_time);
                        }
                    }
                }
            }
            if let Some(created_at) = created_at {
                goal.created_at = Some(created_at);
            }
            goal.updated_at = parse_unix_seconds(goal_payload.get("updatedAt"))
                .or(record_time)
                .or(goal.updated_at);
            return Ok(());
        }

        if record_type == "event_msg" && payload_type == "item_completed" {
            if let Some(item) = payload.get("item").and_then(Value::as_object) {
                if value_str(item.get("type")) == Some("Plan") {
                    if let (Some(text), Some(turn_id)) = (
                        nonblank_raw_str(item.get("text")),
                        value_str(payload.get("turn_id")),
                    ) {
                        let plan_sequence = next_counter(&mut self.sequence, "plan");
                        self.plan_items.push(PlanItem {
                            turn_id: turn_id.to_string(),
                            source_item_id: value_str(item.get("id")).map(str::to_string),
                            text: text.to_string(),
                            time: record_time,
                            sequence: plan_sequence,
                        });
                        return Ok(());
                    }
                }
            }
        }

        if record_type == "event_msg" && payload_type == "task_complete" {
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                self.has_turn_events = true;
                let failed = payload.get("error").is_some_and(|value| !value.is_null());
                let start_time = parse_unix_seconds(payload.get("started_at"));
                let payload_completed_at = parse_unix_seconds(payload.get("completed_at"));
                let effective_start = self
                    .turn_info
                    .get(turn_id)
                    .and_then(|info| info.start_time)
                    .or(start_time);
                let (end_time, timestamp_conflict) = resolve_task_complete_end_time(
                    effective_start,
                    record_time,
                    payload_completed_at,
                );
                let duration = int_value(payload.get("duration_ms"), Some(0));
                let first_token = int_value(payload.get("time_to_first_token_ms"), Some(0));
                if !self.turn_info.contains_key(turn_id) {
                    let turn_sequence = next_counter(&mut self.sequence, "turn");
                    let mut info = new_turn(
                        turn_id,
                        if failed { "failed" } else { "completed" },
                        turn_sequence,
                        start_time,
                        end_time,
                        None,
                        None,
                        duration,
                        first_token,
                    );
                    info.timestamp_conflict = timestamp_conflict;
                    self.turn_info.insert(turn_id.to_string(), info);
                    if !self.turn_stack.iter().any(|item| item == turn_id) {
                        self.turn_stack.push(turn_id.to_string());
                    }
                } else {
                    let info = self.turn_info.get_mut(turn_id).unwrap();
                    info.status = if failed { "failed" } else { "completed" }.to_string();
                    info.end_time = end_time;
                    if timestamp_conflict.is_some() {
                        info.timestamp_conflict = timestamp_conflict;
                    }
                    if info.start_time.is_none() {
                        info.start_time = start_time;
                    }
                    if duration.is_some() {
                        info.duration_ms = duration;
                    }
                    if first_token.is_some() {
                        info.time_to_first_token_ms = first_token;
                    }
                }
                if failed {
                    let error_code = payload
                        .get("error")
                        .and_then(Value::as_object)
                        .and_then(|error| value_str(error.get("codex_error_info")))
                        .map(str::to_string);
                    let termination_type = if error_code.as_deref() == Some("usage_limit_exceeded")
                    {
                        "usage_limit"
                    } else {
                        "runtime_error"
                    };
                    let mut termination =
                        obj!("type" => Value::String(termination_type.to_string()));
                    if let Some(code) = error_code {
                        termination.insert("code".to_string(), Value::String(code));
                    }
                    if termination_type == "usage_limit" {
                        if let Some(usage) = self.usage_limits.get(turn_id) {
                            if let Some(time) = usage.first_reached_at {
                                termination.insert(
                                    "limit_reached_at".to_string(),
                                    Value::String(utc_iso(time)),
                                );
                            }
                            if let Some(time) = usage.reset_at {
                                termination
                                    .insert("retry_at".to_string(), Value::String(utc_iso(time)));
                            }
                        }
                    }
                    let termination_value = Value::Object(termination.clone());
                    self.turn_info.get_mut(turn_id).unwrap().termination =
                        Some(termination_value.clone());
                    for goal in &mut self.goals {
                        if goal.last_turn_id.as_deref() == Some(turn_id)
                            && goal.reported_status.as_deref() == Some("active")
                        {
                            goal.interruptions.push(GoalInterruption {
                                sequence: goal.interruptions.len() + 1,
                                turn_id: turn_id.to_string(),
                                interrupted_at: end_time,
                                termination: termination_value.clone(),
                                resumed_at: None,
                            });
                        }
                    }
                }
            }
            return Ok(());
        }

        if record_type == "event_msg" && payload_type == "turn_aborted" {
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                self.has_turn_events = true;
                let duration = int_value(payload.get("duration_ms"), Some(0));
                if !self.turn_info.contains_key(turn_id) {
                    let turn_sequence = next_counter(&mut self.sequence, "turn");
                    self.turn_info.insert(
                        turn_id.to_string(),
                        new_turn(
                            turn_id,
                            "aborted",
                            turn_sequence,
                            None,
                            record_time,
                            raw_str(payload.get("reason")).map(str::to_string),
                            None,
                            duration,
                            None,
                        ),
                    );
                    if !self.turn_stack.iter().any(|item| item == turn_id) {
                        self.turn_stack.push(turn_id.to_string());
                    }
                } else {
                    let info = self.turn_info.get_mut(turn_id).unwrap();
                    info.status = "aborted".to_string();
                    info.end_time = record_time;
                    info.abort_reason = raw_str(payload.get("reason")).map(str::to_string);
                    if duration.is_some() {
                        info.duration_ms = duration;
                    }
                }
            }
            return Ok(());
        }

        if record_type == "event_msg" && payload_type == "thread_rolled_back" {
            let rollback_count = int_value(payload.get("num_turns"), Some(0)).unwrap_or(0) as usize;
            let mut removed = Vec::new();
            for _ in 0..rollback_count {
                let Some(rolled) = self.turn_stack.pop() else {
                    break;
                };
                removed.push(rolled.clone());
                if let Some(info) = self.turn_info.get_mut(&rolled) {
                    info.status = "rolled_back".to_string();
                    info.end_time = record_time;
                }
                self.pending_web_calls
                    .retain(|call| call.turn_id.as_deref() != Some(&rolled));
                self.command_calls.retain(|_, index| {
                    self.activities
                        .get(*index)
                        .and_then(|activity| activity.turn_id.as_deref())
                        != Some(&rolled)
                });
            }
            let rollback_sequence = next_counter(&mut self.sequence, "rollback");
            self.rollbacks.push(RollbackInfo {
                sequence: rollback_sequence,
                time: record_time,
                num_turns: rollback_count,
                removed_turn_ids: removed,
            });
            self.current_turn_id = self.turn_stack.last().cloned();
            return Ok(());
        }

        if record_type == "turn_context" {
            if let Some(model) = value_str(payload.get("model")) {
                if self.model_set.insert(model.to_string()) {
                    self.models.push(model.to_string());
                }
            }
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                self.current_turn_id = Some(turn_id.to_string());
            }
            return Ok(());
        }

        if record_type == "event_msg" && payload_type == "thread_settings_applied" {
            if let Some(model) = payload
                .get("thread_settings")
                .and_then(Value::as_object)
                .and_then(|settings| value_str(settings.get("model")))
            {
                if self.model_set.insert(model.to_string()) {
                    self.models.push(model.to_string());
                }
            }
            return Ok(());
        }

        if record_type == "response_item" && payload_type == "message" {
            let role = value_str(payload.get("role")).unwrap_or_default();
            let content = get_message_text(payload.get("content"));
            let turn_id =
                turn_id_from_message_payload(payload).or_else(|| self.current_turn_id.clone());
            if role == "user" {
                if let Some(content) = content.filter(|content| !content.trim().is_empty()) {
                    if let Some(turn_id) = turn_id {
                        self.pending_user_turns
                            .entry(text_key(Some(&content)))
                            .or_default()
                            .push_back(turn_id);
                    }
                }
                return Ok(());
            }
            if role == "assistant" {
                let phase = value_str(payload.get("phase"));
                if let (Some(content), Some(phase)) =
                    (content.filter(|content| !content.trim().is_empty()), phase)
                {
                    if matches!(phase, "commentary" | "final_answer") {
                        let message_sequence = next_counter(&mut self.sequence, "message");
                        self.messages.push(RawMessage {
                            role: "assistant".to_string(),
                            phase: Some(phase.to_string()),
                            turn_id,
                            content,
                            time: record_time,
                            sequence: message_sequence,
                            source_line: line_number,
                        });
                    }
                }
            }
            return Ok(());
        }

        if record_type == "event_msg" && payload_type == "user_message" {
            if let Some(content) =
                raw_str(payload.get("message")).filter(|content| !content.trim().is_empty())
            {
                let message_sequence = next_counter(&mut self.sequence, "message");
                let key = text_key(Some(content));
                let mut turn_id = self
                    .pending_user_turns
                    .get_mut(&key)
                    .and_then(VecDeque::pop_front);
                if self
                    .pending_user_turns
                    .get(&key)
                    .is_some_and(VecDeque::is_empty)
                {
                    self.pending_user_turns.remove(&key);
                }
                if turn_id.is_none() {
                    turn_id = self.current_turn_id.clone();
                }
                self.messages.push(RawMessage {
                    role: "user".to_string(),
                    phase: None,
                    turn_id,
                    content: content.trim().to_string(),
                    time: record_time,
                    sequence: message_sequence,
                    source_line: line_number,
                });
            }
            return Ok(());
        }

        if record_type == "response_item" && payload_type == "function_call" {
            if matches!(
                value_str(payload.get("name")),
                Some("request_user_input" | "request_user_input_async")
            ) {
                if let Some((call_id, request)) =
                    build_user_input_request(payload, record_time, self.current_turn_id.as_deref())
                {
                    let index = self.user_input_requests.len();
                    self.user_input_requests.push(request);
                    self.pending_user_input_requests.insert(call_id, index);
                }
                return Ok(());
            }
            if value_str(payload.get("name")) != Some("shell_command") {
                return Ok(());
            }
            let arguments = json_object(payload.get("arguments"));
            let turn_id = turn_id_from_tool_payload(payload, self.current_turn_id.as_deref());
            let call_id = value_str(payload.get("call_id")).map(str::to_string);
            let Some(execution) = direct_shell_execution(&arguments) else {
                if let Some(call_id) = call_id {
                    let sequence = next_counter(&mut self.sequence, "activity");
                    self.pending_unparsed_command_calls.insert(
                        call_id,
                        PendingUnparsedCommand {
                            turn_id,
                            time: record_time,
                            sequence,
                            parse_warnings: vec!["Shell command arguments could not be structurally resolved before execution.".to_string()],
                        },
                    );
                }
                return Ok(());
            };
            let activity_sequence = next_counter(&mut self.sequence, "activity");
            let mut activity = Activity::new("command", turn_id, record_time, activity_sequence);
            activity.source_call_id = call_id.clone();
            activity.executions = vec![execution];
            activity.invocation_count = 1;
            let index = self.activities.len();
            self.activities.push(activity);
            if let Some(call_id) = call_id {
                self.command_calls.insert(call_id, index);
            }
            return Ok(());
        }

        if record_type == "response_item" && payload_type == "function_call_output" {
            if let Some(call_id) = value_str(payload.get("call_id")) {
                if let Some(index) = self.pending_user_input_requests.remove(call_id) {
                    if let Some(request) = self.user_input_requests.get_mut(index) {
                        complete_user_input_request(request, payload, record_time);
                    }
                    return Ok(());
                }
                if let Some(index) = self.command_calls.remove(call_id) {
                    if let Some(activity) = self.activities.get_mut(index) {
                        complete_command_activity_from_payload(activity, payload, record_time);
                    }
                } else if let Some(pending) = self.pending_unparsed_command_calls.remove(call_id) {
                    let (exit_codes, truncated, failure_status, failure_diagnostic) =
                        command_output_summary(payload);
                    if failure_status.as_deref() == Some("failed") {
                        let mut activity = Activity::new(
                            "command",
                            pending.turn_id,
                            pending.time,
                            pending.sequence,
                        );
                        activity.source_call_id = Some(call_id.to_string());
                        activity.parse_warnings = pending.parse_warnings;
                        activity.failure_status = failure_status;
                        activity.failure_diagnostic = failure_diagnostic;
                        complete_command_activity(&mut activity, exit_codes, truncated);
                        activity.end_time = record_time;
                        self.activities.push(activity);
                    }
                }
            }
            return Ok(());
        }

        if record_type == "response_item" && payload_type == "custom_tool_call" {
            let Some(tool_input) =
                raw_str(payload.get("input")).filter(|input| !input.trim().is_empty())
            else {
                return Ok(());
            };
            let call_id = value_str(payload.get("call_id")).map(str::to_string);
            let turn_id = turn_id_from_tool_payload(payload, self.current_turn_id.as_deref());
            let mut executions = Vec::new();
            let mut warnings = Vec::new();
            if tool_input.contains("tools.shell_command") {
                let (values, value_warnings) = javascript_command_data(tool_input);
                executions.extend(values);
                warnings.extend(value_warnings);
            }
            if tool_input.contains("tools.exec_command") {
                let (values, value_warnings) = javascript_exec_command_data(tool_input);
                executions.extend(values);
                warnings.extend(value_warnings);
            }
            let commandish = tool_input.contains("tools.shell_command")
                || tool_input.contains("tools.exec_command");
            if !executions.is_empty() {
                let activity_sequence = next_counter(&mut self.sequence, "activity");
                let mut activity =
                    Activity::new("command", turn_id.clone(), record_time, activity_sequence);
                activity.source_call_id = call_id.clone();
                activity.executions = executions;
                activity.invocation_count = activity.executions.len();
                for warning in warnings {
                    add_unique_warning(&mut activity.parse_warnings, &warning);
                }
                let index = self.activities.len();
                self.activities.push(activity);
                if let Some(call_id) = call_id.clone() {
                    self.command_calls.insert(call_id, index);
                }
            } else if commandish {
                if let Some(call_id) = call_id.clone() {
                    let activity_sequence = next_counter(&mut self.sequence, "activity");
                    let mut parse_warnings = warnings;
                    add_unique_warning(
                        &mut parse_warnings,
                        "Command wrapper could not be structurally resolved before execution.",
                    );
                    self.pending_unparsed_command_calls.insert(
                        call_id,
                        PendingUnparsedCommand {
                            turn_id: turn_id.clone(),
                            time: record_time,
                            sequence: activity_sequence,
                            parse_warnings,
                        },
                    );
                }
            }
            if tool_input.contains("tools.web__run") {
                self.pending_web_calls.push_back(PendingWebCall {
                    turn_id,
                    input: tool_input.to_string(),
                });
            }
            return Ok(());
        }

        if record_type == "response_item" && payload_type == "custom_tool_call_output" {
            if let Some(call_id) = value_str(payload.get("call_id")) {
                if let Some(index) = self.command_calls.remove(call_id) {
                    if let Some(activity) = self.activities.get_mut(index) {
                        complete_command_activity_from_payload(activity, payload, record_time);
                    }
                } else if let Some(pending) = self.pending_unparsed_command_calls.remove(call_id) {
                    let (exit_codes, truncated, failure_status, failure_diagnostic) =
                        command_output_summary(payload);
                    if failure_status.as_deref() == Some("failed") {
                        let mut activity = Activity::new(
                            "command",
                            pending.turn_id,
                            pending.time,
                            pending.sequence,
                        );
                        activity.source_call_id = Some(call_id.to_string());
                        activity.parse_warnings = pending.parse_warnings;
                        activity.failure_status = failure_status;
                        activity.failure_diagnostic = failure_diagnostic;
                        complete_command_activity(&mut activity, exit_codes, truncated);
                        activity.end_time = record_time;
                        self.activities.push(activity);
                    }
                }
            }
            return Ok(());
        }

        if record_type == "event_msg" && payload_type == "web_search_end" {
            let web_call = self.pending_web_calls.pop_front();
            let mut turn_id = self.current_turn_id.clone();
            let mut web_input = String::new();
            if let Some(call) = web_call {
                turn_id = call.turn_id;
                web_input = call.input;
            }
            let action_object = payload.get("action").and_then(Value::as_object);
            let mut action_type = action_object
                .and_then(|action| value_str(action.get("type")))
                .unwrap_or_default()
                .to_string();
            let activity_type = if action_type == "search" {
                "web_search"
            } else if action_type == "find_in_page" {
                "web_find"
            } else if !web_input.is_empty() && input_has_js_property(&web_input, "open") {
                action_type = "open".to_string();
                "web_open"
            } else if !web_input.is_empty() && input_has_js_property(&web_input, "click") {
                action_type = "click".to_string();
                "web_click"
            } else {
                "web_activity"
            };
            let queries = action_object
                .and_then(|action| action.get("queries"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let references = if web_input.is_empty() {
                Vec::new()
            } else {
                javascript_property_string_values(&web_input, "ref_id")
            };
            let urls = if web_input.is_empty() {
                Vec::new()
            } else {
                javascript_property_string_values(&web_input, "url")
            };
            let activity_sequence = next_counter(&mut self.sequence, "activity");
            let mut activity =
                Activity::new(activity_type, turn_id, record_time, activity_sequence);
            activity.action = Some(action_type);
            activity.queries = queries;
            activity.query = raw_str(payload.get("query")).map(str::to_string);
            activity.pattern = action_object
                .and_then(|action| raw_str(action.get("pattern")))
                .map(str::to_string);
            activity.references = references;
            activity.urls = urls;
            self.activities.push(activity);
            return Ok(());
        }

        if record_type == "event_msg" && payload_type == "patch_apply_end" {
            let Some(change_map) = payload.get("changes").and_then(Value::as_object) else {
                return Ok(());
            };
            let turn_id = value_str(payload.get("turn_id"))
                .map(str::to_string)
                .or_else(|| self.current_turn_id.clone());
            let success = bool_value(payload.get("success")).unwrap_or(false);
            for (item_path, detail) in change_map {
                let operation = detail
                    .as_object()
                    .and_then(|detail| value_str(detail.get("type")))
                    .map(str::to_string);
                let relative = relative_workspace_path(cwd, item_path);
                let change_sequence = next_counter(&mut self.sequence, "change");
                self.changes.push(ChangeInfo {
                    turn_id: turn_id.clone(),
                    time: record_time,
                    sequence: change_sequence,
                    path: relative.clone(),
                    operation: operation.clone(),
                    success,
                });
                let activity_sequence = next_counter(&mut self.sequence, "activity");
                let mut activity =
                    Activity::new("file_edit", turn_id.clone(), record_time, activity_sequence);
                activity.path = Some(relative);
                activity.operation = operation;
                activity.success = Some(success);
                self.activities.push(activity);
            }
        }

        Ok(())
    }
}

fn structured_item_text(item: &Obj) -> Option<String> {
    let content = item.get("content").and_then(Value::as_array)?;
    let mut text = String::new();
    for part in content {
        let Some(part) = part.as_object() else {
            continue;
        };
        if let Some(value) = raw_str(part.get("text")) {
            text.push_str(value);
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

// E05/E06: bounded excerpts retain Unicode-scalar offsets and hashes of the
// original text. No command result is reclassified from exit code alone.
fn bounded_text(text: &str, limit: usize) -> Value {
    let chars: Vec<char> = text.chars().collect();
    let size = chars.len();
    let spans = if size <= limit {
        vec![(0, size)]
    } else {
        let head = limit / 2;
        vec![(0, head), (size - (limit - head), size)]
    };
    let segments = spans
        .into_iter()
        .map(|(start, end)| {
            Value::Object(obj!(
                "start" => Value::Number(start.into()),
                "end" => Value::Number(end.into()),
                "text" => Value::String(chars[start..end].iter().collect()),
            ))
        })
        .collect();
    Value::Object(obj!(
        "sha256" => Value::String(sha256_text(Some(text))),
        "byte_length" => Value::Number(text.len().into()),
        "character_length" => Value::Number(size.into()),
        "truncated" => Value::Bool(size > limit),
        "segments" => Value::Array(segments),
    ))
}

fn tool_binary_data_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"data:[^,\s]*;base64,[A-Za-z0-9+/=\r\n]+").unwrap())
}

fn compact_tool_arguments(value: &Value) -> Value {
    fn visit(
        value: &Value,
        depth: usize,
        remaining: &mut usize,
        nodes: &mut usize,
        truncated: &mut bool,
        omitted_binary: &mut bool,
    ) -> Value {
        if depth > 6 || *nodes == 0 || *remaining == 0 {
            *truncated = true;
            return Value::String("[omitted]".to_string());
        }
        *nodes -= 1;
        match value {
            Value::String(text) => {
                let cleaned = tool_binary_data_regex().replace_all(text, "[binary omitted]");
                *omitted_binary |= cleaned.as_ref() != text;
                let take = (*remaining).min(512);
                let result: String = cleaned.chars().take(take).collect();
                *truncated |= cleaned.chars().count() > take;
                *remaining -= result.chars().count();
                Value::String(result)
            }
            Value::Object(map) => {
                let mut result = Obj::new();
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for key in keys {
                    if result.len() >= 32 || *remaining == 0 || *nodes == 0 {
                        *truncated = true;
                        break;
                    }
                    if matches!(
                        key.to_ascii_lowercase().as_str(),
                        "_meta" | "base64" | "screenshot" | "image" | "image_data" | "data"
                    ) {
                        *omitted_binary = true;
                        continue;
                    }
                    let key_len = key.chars().count();
                    if key_len > *remaining {
                        *truncated = true;
                        break;
                    }
                    *remaining -= key_len;
                    result.insert(
                        key.clone(),
                        visit(
                            &map[key],
                            depth + 1,
                            remaining,
                            nodes,
                            truncated,
                            omitted_binary,
                        ),
                    );
                }
                Value::Object(result)
            }
            Value::Array(values) => {
                let mut result = Vec::new();
                for child in values.iter().take(16) {
                    if *remaining == 0 || *nodes == 0 {
                        *truncated = true;
                        break;
                    }
                    result.push(visit(
                        child,
                        depth + 1,
                        remaining,
                        nodes,
                        truncated,
                        omitted_binary,
                    ));
                }
                *truncated |= result.len() != values.len();
                Value::Array(result)
            }
            _ => value.clone(),
        }
    }
    let mut remaining = 2048;
    let mut nodes = 128;
    let mut truncated = false;
    let mut omitted_binary = false;
    let result = visit(
        value,
        0,
        &mut remaining,
        &mut nodes,
        &mut truncated,
        &mut omitted_binary,
    );
    Value::Object(obj!(
        "value" => result,
        "truncated" => Value::Bool(truncated),
        "omitted_binary" => Value::Bool(omitted_binary),
    ))
}

fn typed_web_activity(
    item: &Obj,
    turn_id: Option<&str>,
    record_time: Option<Timestamp>,
) -> Option<Value> {
    let source_kind = value_str(item.get("type"))?;
    if source_kind == "Extension" && value_str(item.get("kind")) != Some("web.search") {
        return None;
    }
    if !matches!(source_kind, "WebSearch" | "Extension") {
        return None;
    }

    let action_object = item.get("action").and_then(Value::as_object);
    let action = action_object
        .and_then(|value| value_str(value.get("type")))
        .unwrap_or("other");

    let mut queries = Vec::<String>::new();
    let mut query_set = HashSet::<String>::new();
    append_unique(
        &mut queries,
        &mut query_set,
        nonblank_raw_str(item.get("query")),
    );
    if let Some(action_object) = action_object {
        append_unique(
            &mut queries,
            &mut query_set,
            nonblank_raw_str(action_object.get("query")),
        );
        if let Some(values) = action_object.get("queries").and_then(Value::as_array) {
            for value in values {
                append_unique(
                    &mut queries,
                    &mut query_set,
                    value.as_str().filter(|value| !value.trim().is_empty()),
                );
            }
        }
    }

    let mut references = Vec::<String>::new();
    let mut reference_set = HashSet::<String>::new();
    let mut urls = Vec::<String>::new();
    let mut url_set = HashSet::<String>::new();
    if let Some(results) = item.get("results").and_then(Value::as_array) {
        for result in results {
            let Some(result) = result.as_object() else {
                continue;
            };
            append_unique(
                &mut references,
                &mut reference_set,
                nonblank_raw_str(result.get("ref_id")),
            );
            append_unique(&mut urls, &mut url_set, nonblank_raw_str(result.get("url")));
        }
    }

    let mut activity = obj!(
        "type" => Value::String("web_research".to_string()),
        "source_kind" => Value::String(source_kind.to_string()),
    );
    insert_optional_string(&mut activity, "turn_id", turn_id);
    if let Some(time) = record_time {
        activity.insert("create_time".to_string(), Value::String(utc_iso(time)));
        activity.insert("completed_at".to_string(), Value::String(utc_iso(time)));
    }
    if let Some(source_item_id) = nonblank_raw_str(item.get("id")) {
        activity.insert(
            "source_item_id".to_string(),
            Value::String(source_item_id.to_string()),
        );
    }
    activity.insert("action".to_string(), Value::String(action.to_string()));
    let counter_name = match action {
        "search" => "search_count",
        "open" => "open_count",
        "find" | "find_in_page" => "find_count",
        "click" => "click_count",
        _ => "other_count",
    };
    activity.insert(counter_name.to_string(), Value::Number(1_u64.into()));
    if !queries.is_empty() {
        activity.insert(
            "queries".to_string(),
            Value::Array(queries.into_iter().map(Value::String).collect()),
        );
    }
    if !references.is_empty() {
        activity.insert(
            "references".to_string(),
            Value::Array(references.into_iter().map(Value::String).collect()),
        );
    }
    if !urls.is_empty() {
        activity.insert(
            "urls".to_string(),
            Value::Array(urls.into_iter().map(Value::String).collect()),
        );
    }
    Some(Value::Object(activity))
}

fn typed_image_view_activity(
    item: &Obj,
    turn_id: Option<&str>,
    record_time: Option<Timestamp>,
) -> Option<Value> {
    if value_str(item.get("type")) != Some("ImageView") {
        return None;
    }
    let path = nonblank_raw_str(item.get("path"))?;
    let mut activity = obj!(
        "type" => Value::String("image_view".to_string()),
        "source_kind" => Value::String("ImageView".to_string()),
        "path" => Value::String(path.to_string()),
    );
    insert_optional_string(&mut activity, "turn_id", turn_id);
    if let Some(time) = record_time {
        activity.insert("create_time".to_string(), Value::String(utc_iso(time)));
        activity.insert("completed_at".to_string(), Value::String(utc_iso(time)));
    }
    if let Some(source_item_id) = nonblank_raw_str(item.get("id")) {
        activity.insert(
            "source_item_id".to_string(),
            Value::String(source_item_id.to_string()),
        );
    }
    Some(Value::Object(activity))
}

fn typed_tool_activity(
    item: &Obj,
    turn_id: Option<&str>,
    record_time: Option<Timestamp>,
    payload: &Obj,
) -> Value {
    let mut activity = obj!(
        "type" => Value::String("tool".to_string()),
        "source_kind" => Value::String(value_str(item.get("type")).unwrap_or_default().to_string()),
    );
    insert_optional_string(&mut activity, "turn_id", turn_id);
    if let Some(time) = record_time {
        activity.insert("create_time".to_string(), Value::String(utc_iso(time)));
        activity.insert("completed_at".to_string(), Value::String(utc_iso(time)));
    }
    for (dest, src) in [
        ("source_item_id", "id"),
        ("call_id", "call_id"),
        ("namespace", "namespace"),
        ("server", "server"),
        ("tool", "tool"),
        ("status", "status"),
    ] {
        if let Some(value) = nonblank_raw_str(item.get(src)) {
            activity.insert(dest.to_string(), Value::String(value.to_string()));
        }
    }
    for (dest, src) in [("success", "success"), ("read_only_hint", "readOnlyHint")] {
        if let Some(value) = item.get(src).and_then(Value::as_bool) {
            activity.insert(dest.to_string(), Value::Bool(value));
        }
    }
    let result = item.get("result").and_then(Value::as_object);
    if let Some(value) = result
        .and_then(|map| map.get("isError"))
        .and_then(Value::as_bool)
    {
        activity.insert("is_error".to_string(), Value::Bool(value));
    }
    if let Some(arguments) = item.get("arguments") {
        activity.insert(
            "input_summary".to_string(),
            compact_tool_arguments(arguments),
        );
    }
    let content = if value_str(item.get("type")) == Some("DynamicToolCall") {
        item.get("content_items")
    } else {
        result.and_then(|map| map.get("content"))
    };
    let mut parts = Vec::<String>::new();
    let mut types = Vec::<String>::new();
    let mut omitted = 0usize;
    if let Some(content) = content.and_then(Value::as_array) {
        for part in content {
            let Some(part) = part.as_object() else {
                omitted += 1;
                continue;
            };
            let kind = value_str(part.get("type")).unwrap_or_default();
            if !kind.is_empty() && !types.iter().any(|item| item == kind) {
                types.push(kind.to_string());
            }
            if matches!(
                kind.to_ascii_lowercase().as_str(),
                "text" | "inputtext" | "input_text" | "output_text"
            ) {
                if let Some(text) = raw_str(part.get("text")) {
                    parts.push(text.to_string());
                    continue;
                }
            }
            omitted += 1;
        }
    }
    if !types.is_empty() {
        activity.insert(
            "result_content_types".to_string(),
            Value::Array(types.into_iter().map(Value::String).collect()),
        );
    }
    if omitted != 0 {
        activity.insert(
            "omitted_nontext_items".to_string(),
            Value::Number(omitted.into()),
        );
    }
    if !parts.is_empty() {
        let original = parts.join("\n");
        let cleaned = tool_binary_data_regex().replace_all(&original, "[binary omitted]");
        let mut summary = bounded_text(&cleaned, 4096);
        if cleaned.as_ref() != original {
            let map = summary.as_object_mut().unwrap();
            map.insert(
                "scope".to_string(),
                Value::String("binary_sanitized_text".to_string()),
            );
            map.insert(
                "original_sha256".to_string(),
                Value::String(sha256_text(Some(&original))),
            );
        }
        activity.insert("result_summary".to_string(), summary);
    }
    let started = int_value(payload.get("started_at_ms"), Some(0));
    let completed = int_value(payload.get("completed_at_ms"), Some(0));
    if let Some(value) = started {
        activity.insert("started_at_ms".to_string(), Value::Number(value.into()));
    }
    if let Some(value) = completed {
        activity.insert("completed_at_ms".to_string(), Value::Number(value.into()));
    }
    if let (Some(start), Some(end)) = (started, completed) {
        if end >= start {
            activity.insert(
                "duration_ms".to_string(),
                Value::Number((end - start).into()),
            );
        }
    }
    Value::Object(activity)
}

fn async_delivery(
    item: &Obj,
    turn_id: Option<&str>,
    record_time: Option<Timestamp>,
) -> Option<Value> {
    if value_str(item.get("type")) != Some("AgentMessage")
        || value_str(item.get("delivery")) != Some("async")
    {
        return None;
    }
    let id = raw_str(item.get("id"))?;
    let questions = item.get("questions")?.as_array()?.clone();
    let mut delivery = obj!(
        "source_item_id" => Value::String(id.to_string()),
        "mode" => Value::String("async".to_string()),
        "questions" => Value::Array(questions),
    );
    insert_optional_string(&mut delivery, "turn_id", turn_id);
    if let Some(time) = record_time {
        delivery.insert("observed_at".to_string(), Value::String(utc_iso(time)));
    }
    if let Some(phase) = raw_str(item.get("phase")) {
        delivery.insert("phase".to_string(), Value::String(phase.to_string()));
    }
    if let Some(text) = structured_item_text(item) {
        delivery.insert("content".to_string(), Value::String(text));
    }
    Some(Value::Object(delivery))
}

fn attach_async_deliveries(requests: &mut Vec<Value>, deliveries: Vec<Value>) {
    for delivery in deliveries {
        let Some(map) = delivery.as_object() else {
            continue;
        };
        let Some(id) = raw_str(map.get("source_item_id")) else {
            continue;
        };
        let index = requests.iter().position(|request| {
            let Some(request) = request.as_object() else {
                return false;
            };
            value_str(request.get("request_mode")) == Some("async")
                && (raw_str(request.get("call_id")) == Some(id)
                    || raw_str(request.get("source_item_id")) == Some(id))
        });
        let index = index.unwrap_or_else(|| {
            let mut request = obj!(
                "request_mode" => Value::String("async".to_string()),
                "source_item_id" => Value::String(id.to_string()),
                "status" => Value::String("delivery_observed".to_string()),
                "questions" => map.get("questions").cloned().unwrap_or(Value::Array(Vec::new())),
            );
            insert_optional_string(&mut request, "turn_id", value_str(map.get("turn_id")));
            insert_optional_string(
                &mut request,
                "create_time",
                value_str(map.get("observed_at")),
            );
            requests.push(Value::Object(request));
            requests.len() - 1
        });
        if let Some(request) = requests[index].as_object_mut() {
            request.insert("delivery".to_string(), delivery);
        }
    }
}

fn extract_goal_observation_result(value: Option<&Value>) -> Option<Obj> {
    fn visit(value: &Value, depth: usize, found: &mut Vec<Obj>) {
        if depth > 16 {
            return;
        }
        match value {
            Value::Object(map) => {
                if let Some(goal) = map.get("goal").and_then(Value::as_object) {
                    if raw_str(goal.get("threadId")).is_some()
                        && parse_unix_seconds(goal.get("createdAt")).is_some()
                    {
                        found.push(map.clone());
                    }
                    return;
                }
                if map.contains_key("exit_code") && map.contains_key("output") {
                    return;
                }
                for key in ["value", "result", "data", "text"] {
                    if let Some(child) = map.get(key) {
                        visit(child, depth + 1, found);
                    }
                }
            }
            Value::Array(values) => {
                for child in values {
                    visit(child, depth + 1, found);
                }
            }
            Value::String(text) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    visit(&parsed, depth + 1, found);
                } else {
                    for line in text.lines() {
                        if line.trim_start().starts_with(['{', '[']) {
                            if let Ok(parsed) = serde_json::from_str::<Value>(line) {
                                visit(&parsed, depth + 1, found);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    visit(value?, 0, &mut found);
    if found.len() == 1 { found.pop() } else { None }
}

fn projectable_goal_updates(updates: &[Value]) -> Vec<Value> {
    let mut projected = Vec::<Value>::new();
    let mut generation: Option<String> = None;
    for value in updates {
        let Some(update) = value.as_object() else {
            continue;
        };
        let created = raw_str(update.get("created_at"));
        if update.get("observation").and_then(Value::as_bool) != Some(true) {
            if let Some(created) = created {
                generation = Some(created.to_string());
            }
            projected.push(value.clone());
            continue;
        }
        let Some(created) = created else { continue };
        if generation
            .as_deref()
            .is_some_and(|current| current != created)
        {
            continue;
        }
        let previous = projected
            .iter()
            .rev()
            .filter_map(Value::as_object)
            .find(|map| raw_str(map.get("created_at")) == Some(created));
        if let Some(previous) = previous {
            if let (Some(before), Some(after)) = (
                raw_str(previous.get("updated_at")),
                raw_str(update.get("updated_at")),
            ) {
                if after < before {
                    continue;
                }
            }
        }
        generation = Some(created.to_string());
        let mut merged = previous.cloned().unwrap_or_default();
        merged.extend(update.clone());
        projected.push(Value::Object(merged));
    }
    projected
}

fn typed_command_execution_output(
    item: &Obj,
    turn_id: Option<&str>,
    record_time: Option<Timestamp>,
    payload: &Obj,
) -> Value {
    let mut execution = Obj::new();
    execution.insert(
        "source_kind".to_string(),
        Value::String("structured_execution".to_string()),
    );
    if let Some(item_id) = nonblank_raw_str(item.get("id")) {
        execution.insert(
            "source_item_id".to_string(),
            Value::String(item_id.to_string()),
        );
    }

    if let Some(argv_values) = item.get("command").and_then(Value::as_array) {
        let argv: Vec<String> = argv_values
            .iter()
            .filter_map(|value| match value {
                Value::String(text) => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                Value::Bool(flag) => Some(flag.to_string()),
                _ => None,
            })
            .collect();
        if !argv.is_empty() {
            execution.insert(
                "argv".to_string(),
                Value::Array(argv.iter().cloned().map(Value::String).collect()),
            );
            let command = if argv.len() >= 3
                && matches!(
                    argv[argv.len() - 2].to_ascii_lowercase().as_str(),
                    "-command" | "-c"
                ) {
                argv.last().cloned().unwrap_or_default()
            } else {
                argv.join(" ")
            };
            if !command.trim().is_empty() {
                execution.insert("command".to_string(), Value::String(command));
            }
        }
    } else if let Some(command) = nonblank_raw_str(item.get("command")) {
        execution.insert("command".to_string(), Value::String(command.to_string()));
    }

    if let Some(cwd) = nonblank_raw_str(item.get("cwd")) {
        execution.insert(
            "working_directory".to_string(),
            Value::String(cwd.to_string()),
        );
    }
    if let Some(status) = nonblank_raw_str(item.get("status")) {
        execution.insert("status".to_string(), Value::String(status.to_string()));
    }
    if let Some(exit_code) = int_value(item.get("exit_code"), None) {
        execution.insert("exit_code".to_string(), Value::Number(exit_code.into()));
    }
    if let Some(process_id) = item.get("process_id") {
        match process_id {
            Value::String(value) if !value.trim().is_empty() => {
                execution.insert("process_id".to_string(), Value::String(value.clone()));
            }
            Value::Number(value) => {
                execution.insert("process_id".to_string(), Value::String(value.to_string()));
            }
            _ => {}
        }
    }

    let started_ms = int_value(payload.get("started_at_ms"), None).filter(|value| *value >= 0);
    let completed_ms = int_value(payload.get("completed_at_ms"), None).filter(|value| *value >= 0);
    if let Some(started_ms) = started_ms {
        execution.insert(
            "started_at_ms".to_string(),
            Value::Number(started_ms.into()),
        );
    }
    if let Some(completed_ms) = completed_ms {
        execution.insert(
            "completed_at_ms".to_string(),
            Value::Number(completed_ms.into()),
        );
    }
    if let (Some(started_ms), Some(completed_ms)) = (started_ms, completed_ms) {
        if completed_ms >= started_ms {
            execution.insert(
                "duration_ms".to_string(),
                Value::Number((completed_ms - started_ms).into()),
            );
        }
    }
    if let Some(stderr) = nonblank_raw_str(item.get("stderr")) {
        execution.insert("stderr".to_string(), Value::String(stderr.to_string()));
    }
    if let Some(stdout) = raw_str(item.get("stdout")) {
        execution.insert(
            "stdout_present".to_string(),
            Value::Bool(!stdout.is_empty()),
        );
    }
    if int_value(item.get("exit_code"), None).is_some_and(|code| code != 0)
        || value_str(item.get("status")).is_some_and(|status| {
            matches!(status.to_ascii_lowercase().as_str(), "failed" | "error")
        })
    {
        let mut diagnostics = Obj::new();
        for name in ["stdout", "stderr"] {
            if let Some(text) = nonblank_raw_str(item.get(name)) {
                diagnostics.insert(name.to_string(), bounded_text(text, 4096));
            }
        }
        if !diagnostics.is_empty() {
            execution.insert("diagnostics".to_string(), Value::Object(diagnostics));
        }
    }
    if let Some(parsed_cmd) = item.get("parsed_cmd").and_then(Value::as_array) {
        let canonical_command = execution
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string);
        let values: Vec<Value> = parsed_cmd
            .iter()
            .filter_map(|value| {
                let mut parsed = value.as_object()?.clone();
                if canonical_command.as_deref().is_some_and(|command| {
                    parsed.get("cmd").and_then(Value::as_str) == Some(command)
                }) {
                    parsed.remove("cmd");
                }
                (!parsed.is_empty()).then_some(Value::Object(parsed))
            })
            .collect();
        if !values.is_empty() {
            execution.insert("parsed_cmd".to_string(), Value::Array(values));
        }
    }

    let mut activity = Obj::new();
    if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
        activity.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
    }
    activity.insert("type".to_string(), Value::String("command".to_string()));
    if let Some(record_time) = record_time {
        activity.insert(
            "create_time".to_string(),
            Value::String(utc_iso(record_time)),
        );
    }
    activity.insert(
        "result_mapping".to_string(),
        Value::String("exact".to_string()),
    );
    activity.insert(
        "executions".to_string(),
        Value::Array(vec![Value::Object(execution)]),
    );
    if let Some(record_time) = record_time {
        activity.insert(
            "completed_at".to_string(),
            Value::String(utc_iso(record_time)),
        );
    }
    Value::Object(activity)
}

fn typed_file_change_outputs(
    item: &Obj,
    turn_id: Option<&str>,
    record_time: Option<Timestamp>,
) -> Vec<Value> {
    let Some(changes) = item.get("changes").and_then(Value::as_object) else {
        return Vec::new();
    };
    let status = nonblank_raw_str(item.get("status")).unwrap_or("unknown");
    let success = status == "completed";
    let item_id = nonblank_raw_str(item.get("id"));
    let mut outputs = Vec::new();
    for (path, detail) in changes {
        if path.trim().is_empty() {
            continue;
        }
        let Some(detail) = detail.as_object() else {
            continue;
        };
        let mut output = Obj::new();
        if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
            output.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
        }
        if let Some(record_time) = record_time {
            output.insert(
                "create_time".to_string(),
                Value::String(utc_iso(record_time)),
            );
        }
        output.insert("path".to_string(), Value::String(path.clone()));
        output.insert(
            "operation".to_string(),
            Value::String(
                nonblank_raw_str(detail.get("type"))
                    .unwrap_or("unknown")
                    .to_string(),
            ),
        );
        if let Some(move_path) = nonblank_raw_str(detail.get("move_path")) {
            output.insert(
                "move_path".to_string(),
                Value::String(move_path.to_string()),
            );
        }
        output.insert("status".to_string(), Value::String(status.to_string()));
        output.insert("success".to_string(), Value::Bool(success));
        if let Some(item_id) = item_id {
            output.insert(
                "source_item_id".to_string(),
                Value::String(item_id.to_string()),
            );
        }
        outputs.push(Value::Object(output));
    }
    outputs
}

fn extract_goal_result(value: Option<&Value>) -> Option<Obj> {
    let value = value?;
    match value {
        Value::Object(map) => {
            if map.get("goal").and_then(Value::as_object).is_some() {
                return Some(map.clone());
            }
            for key in ["value", "result", "data"] {
                if let Some(found) = extract_goal_result(map.get(key)) {
                    return Some(found);
                }
            }
            for child in map.values() {
                if let Some(found) = extract_goal_result(Some(child)) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(values) => {
            for child in values {
                if let Some(found) = extract_goal_result(Some(child)) {
                    return Some(found);
                }
            }
            None
        }
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            let mut candidates = vec![text.to_string()];
            candidates.extend(
                text.lines()
                    .map(str::trim)
                    .filter(|line| line.starts_with('{') || line.starts_with('['))
                    .map(str::to_string),
            );
            for candidate in candidates.into_iter().rev() {
                let Ok(parsed) = serde_json::from_str::<Value>(&candidate) else {
                    continue;
                };
                if let Some(found) = extract_goal_result(Some(&parsed)) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn exec_goal_tool_name(tool_input: Option<&str>) -> Option<&'static str> {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(41, 0);

    static GOAL_TOOL_RE: OnceLock<Regex> = OnceLock::new();
    let tool_input = tool_input?;
    let regex = GOAL_TOOL_RE.get_or_init(|| {
        Regex::new(r"tools\.(create_goal|update_goal|get_goal)\s*\(")
            .expect("goal wrapper regex is valid")
    });
    let mask = js_code_mask(tool_input);
    let mut names = HashSet::new();
    let mut occurrences = 0usize;
    for captures in regex.captures_iter(&mask) {
        occurrences += 1;
        if let Some(name) = captures.get(1).map(|value| value.as_str()) {
            names.insert(name);
        }
    }
    if names.len() != 1 || (names.contains("get_goal") && occurrences != 1) {
        return None;
    }
    match names.into_iter().next()? {
        "create_goal" => Some("create_goal"),
        "update_goal" => Some("update_goal"),
        "get_goal" => Some("get_goal"),
        _ => None,
    }
}

fn thread_usage_summary(payload: &Obj, record_time: Option<Timestamp>) -> Option<Value> {
    let raw = payload
        .get("thread_token_usage")
        .and_then(Value::as_object)?;
    let mut usage = Obj::new();
    usage.insert(
        "scope".to_string(),
        Value::String("thread_cumulative".to_string()),
    );
    usage.insert(
        "source".to_string(),
        Value::String("token_usage_record".to_string()),
    );
    if let Some(record_time) = record_time {
        usage.insert(
            "observed_at".to_string(),
            Value::String(utc_iso(record_time)),
        );
    }
    for key in [
        "input_tokens",
        "cached_input_tokens",
        "cache_write_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ] {
        if let Some(value) = int_value(raw.get(key), None).filter(|value| *value >= 0) {
            usage.insert(key.to_string(), Value::Number(value.into()));
        }
    }
    if usage.len() <= 2 {
        return None;
    }
    Some(Value::Object(obj!(
        "thread_cumulative" => Value::Object(usage)
    )))
}

fn user_candidate_output(
    turn_id: Option<&str>,
    record_time: Option<Timestamp>,
    content: &str,
) -> Value {
    let mut output = Obj::new();
    if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
        output.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
    }
    if let Some(record_time) = record_time {
        output.insert(
            "create_time".to_string(),
            Value::String(utc_iso(record_time)),
        );
    }
    output.insert("content".to_string(), Value::String(content.to_string()));
    Value::Object(output)
}

fn structured_user_item_output(
    item: &Obj,
    turn_id: Option<&str>,
    record_time: Option<Timestamp>,
) -> Option<Value> {
    let content = structured_item_text(item)?;
    let mut output = Obj::new();
    if let Some(turn_id) = turn_id.filter(|value| !value.trim().is_empty()) {
        output.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
    }
    if let Some(record_time) = record_time {
        output.insert(
            "create_time".to_string(),
            Value::String(utc_iso(record_time)),
        );
    }
    output.insert(
        "content".to_string(),
        Value::String(content.trim().to_string()),
    );
    if let Some(source_item_id) = nonblank_raw_str(item.get("id")) {
        output.insert(
            "source_item_id".to_string(),
            Value::String(source_item_id.to_string()),
        );
    }
    if let Some(client_id) = nonblank_raw_str(item.get("client_id")) {
        output.insert(
            "client_id".to_string(),
            Value::String(client_id.to_string()),
        );
    }
    Some(Value::Object(output))
}

fn merge_verified_structured_users(
    response_user_candidates: &mut HashMap<(String, String), VecDeque<Value>>,
    structured_user_items: &[Value],
) -> Vec<Value> {
    let mut outputs = Vec::new();
    for item in structured_user_items {
        let Some(item_map) = item.as_object() else {
            continue;
        };
        let Some(content) = raw_str(item_map.get("content")) else {
            continue;
        };
        let turn_id = value_str(item_map.get("turn_id")).unwrap_or_default();
        let key = (turn_id.to_string(), text_key(Some(content)));
        let Some(candidates) = response_user_candidates.get_mut(&key) else {
            continue;
        };
        let Some(source) = candidates.pop_front() else {
            continue;
        };
        let source_map = source.as_object();
        let mut message = Obj::new();
        message.insert(
            "id".to_string(),
            Value::String(format!(
                "structured-user-{}",
                value_str(item_map.get("source_item_id")).unwrap_or("unknown")
            )),
        );
        if !turn_id.is_empty() {
            message.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
        }
        if let Some(create_time) = source_map
            .and_then(|map| map.get("create_time"))
            .filter(|value| !value.is_null())
            .cloned()
            .or_else(|| item_map.get("create_time").cloned())
        {
            message.insert("create_time".to_string(), create_time);
        }
        message.insert("role".to_string(), Value::String("user".to_string()));
        message.insert("content".to_string(), Value::String(content.to_string()));
        message.insert(
            "message_origin".to_string(),
            Value::String("item_completed.UserMessage".to_string()),
        );
        for key in ["source_item_id", "client_id"] {
            if let Some(value) = item_map.get(key).cloned() {
                message.insert(key.to_string(), value);
            }
        }
        outputs.push(Value::Object(message));
    }
    outputs
}

struct EnrichmentAccumulator {
    session_id: String,
    is_subagent: bool,
    create_time: Option<Timestamp>,
    actor_path: String,
    local_turn_ids: Vec<String>,
    turn_profiles: HashMap<String, Obj>,
    plan_updates: Vec<Value>,
    goal_updates: Vec<Value>,
    pending_goal_update_indexes: Vec<usize>,
    pending_goal_calls: HashMap<String, Obj>,
    goal_contexts_by_turn: HashMap<String, Vec<String>>,
    agent_events: Vec<Value>,
    pending_agent_calls: HashMap<String, usize>,
    current_turn_id: Option<String>,
    plan_sequence: usize,
    goal_sequence: usize,
    agent_sequence: usize,
    local_start_line: Option<usize>,
    message_start_line: Option<usize>,
    active_task_start_line: Option<usize>,
    delegation_seen: bool,
    latest_local_record_time: Option<Timestamp>,
    prelocal_records: Vec<(usize, Value)>,
    response_user_candidates: HashMap<(String, String), VecDeque<Value>>,
    structured_user_items: Vec<Value>,
    structured_command_activities: Vec<Value>,
    structured_tool_activities: Vec<Value>,
    structured_web_activities: Vec<Value>,
    structured_image_activities: Vec<Value>,
    async_deliveries: Vec<Value>,
    structured_changes: Vec<Value>,
    latest_thread_usage: Option<Value>,
}

impl EnrichmentAccumulator {
    fn new(
        session_id: String,
        is_subagent: bool,
        create_time: Option<Timestamp>,
        actor_path: String,
    ) -> Self {
        Self {
            session_id,
            is_subagent,
            create_time,
            actor_path,
            local_turn_ids: Vec::new(),
            turn_profiles: HashMap::new(),
            plan_updates: Vec::new(),
            goal_updates: Vec::new(),
            pending_goal_update_indexes: Vec::new(),
            pending_goal_calls: HashMap::new(),
            goal_contexts_by_turn: HashMap::new(),
            agent_events: Vec::new(),
            pending_agent_calls: HashMap::new(),
            current_turn_id: None,
            plan_sequence: 0,
            goal_sequence: 0,
            agent_sequence: 0,
            local_start_line: None,
            message_start_line: None,
            active_task_start_line: None,
            delegation_seen: false,
            latest_local_record_time: None,
            prelocal_records: Vec::new(),
            response_user_candidates: HashMap::new(),
            structured_user_items: Vec::new(),
            structured_command_activities: Vec::new(),
            structured_tool_activities: Vec::new(),
            structured_web_activities: Vec::new(),
            structured_image_activities: Vec::new(),
            async_deliveries: Vec::new(),
            structured_changes: Vec::new(),
            latest_thread_usage: None,
        }
    }

    fn activate_local_from(
        &mut self,
        start_line: usize,
        current_record: Option<&Value>,
    ) -> Result<()> {
        self.local_start_line = Some(start_line);
        let records = std::mem::take(&mut self.prelocal_records);
        for (line_number, record) in records {
            if line_number >= start_line {
                self.consume_local(&record)?;
            }
        }
        if let Some(record) = current_record {
            self.consume_local(record)?;
        }
        Ok(())
    }

    fn observe(&mut self, line_number: usize, record: &Value) -> Result<()> {
        if !self.is_subagent {
            return self.consume_local(record);
        }
        if self.local_start_line.is_some() {
            if self.message_start_line.is_none() {
                let is_next_task = record
                    .as_object()
                    .and_then(|map| map.get("payload"))
                    .and_then(Value::as_object)
                    .is_some_and(|payload| {
                        record
                            .as_object()
                            .and_then(|map| value_str(map.get("type")))
                            == Some("event_msg")
                            && value_str(payload.get("type")) == Some("task_started")
                    });
                if is_exportable_assistant_record(record) || is_next_task {
                    self.message_start_line = Some(line_number);
                }
            }
            return self.consume_local(record);
        }
        let Some(record_map) = record.as_object() else {
            self.prelocal_records.push((line_number, record.clone()));
            return Ok(());
        };
        let payload = record_map.get("payload").and_then(Value::as_object);
        if let Some(payload) = payload {
            let record_type = value_str(record_map.get("type"));
            let payload_type = value_str(payload.get("type"));
            if record_type == Some("event_msg") && payload_type == Some("task_started") {
                if self.delegation_seen {
                    self.message_start_line = Some(line_number);
                    return self.activate_local_from(line_number, Some(record));
                }
                self.active_task_start_line = Some(line_number);
                self.prelocal_records.push((line_number, record.clone()));
                return Ok(());
            }
            if record_type == Some("response_item")
                && payload_type == Some("message")
                && value_str(payload.get("role")) == Some("developer")
            {
                let developer_text = get_message_text(payload.get("content")).unwrap_or_default();
                if developer_text.contains("You are an agent in a team of agents") {
                    self.delegation_seen = true;
                    self.prelocal_records.push((line_number, record.clone()));
                    return Ok(());
                }
            }
            if self.delegation_seen && is_exportable_assistant_record(record) {
                let start_line = self.active_task_start_line.unwrap_or(line_number);
                self.message_start_line = Some(line_number);
                return self.activate_local_from(start_line, Some(record));
            }
            if record_type == Some("event_msg")
                && matches!(payload_type, Some("task_complete" | "turn_aborted"))
            {
                self.active_task_start_line = None;
            }
        }
        self.prelocal_records.push((line_number, record.clone()));
        Ok(())
    }

    fn local_message_start_line(&self) -> Option<usize> {
        if self.is_subagent && self.local_start_line.is_some() {
            Some(self.message_start_line.unwrap_or(usize::MAX))
        } else {
            None
        }
    }

    fn resolve_prelocal(&mut self) -> Result<()> {
        if !self.is_subagent || self.local_start_line.is_some() {
            self.prelocal_records.clear();
            return Ok(());
        }
        let records = std::mem::take(&mut self.prelocal_records);
        for (_, record) in records {
            let local = record
                .as_object()
                .is_some_and(|map| record_time_is_local(map, self.create_time, true));
            if local {
                self.consume_local(&record)?;
            }
        }
        Ok(())
    }

    fn consume_local(&mut self, record: &Value) -> Result<()> {
        let Some(record_map) = record.as_object() else {
            return Ok(());
        };
        let Some(payload) = record_map.get("payload").and_then(Value::as_object) else {
            return Ok(());
        };
        let record_type = value_str(record_map.get("type"));
        let payload_type = value_str(payload.get("type"));
        let record_time = parse_datetime_value(record_map.get("timestamp"));
        if let Some(record_time) = record_time {
            self.latest_local_record_time = Some(
                self.latest_local_record_time
                    .map_or(record_time, |current| current.max(record_time)),
            );
        }
        if record_type == Some("event_msg") && payload_type == Some("task_started") {
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                self.current_turn_id = Some(turn_id.to_string());
                if !self.local_turn_ids.iter().any(|value| value == turn_id) {
                    self.local_turn_ids.push(turn_id.to_string());
                }
                let pending_indexes = std::mem::take(&mut self.pending_goal_update_indexes);
                for index in pending_indexes {
                    if let Some(update) = self
                        .goal_updates
                        .get_mut(index)
                        .and_then(Value::as_object_mut)
                    {
                        update.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
                    }
                }
            }
            return Ok(());
        }
        if record_type == Some("event_msg")
            && matches!(payload_type, Some("task_complete" | "turn_aborted"))
        {
            // Use core's terminal-only local identity without rewriting IDs.
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                if !self.local_turn_ids.iter().any(|value| value == turn_id) {
                    self.local_turn_ids.push(turn_id.to_string());
                }
            }
            if value_str(payload.get("turn_id")) == self.current_turn_id.as_deref() {
                self.current_turn_id = None;
            }
        }
        if record_type == Some("token_usage_record") {
            if let Some(usage) = thread_usage_summary(payload, record_time) {
                self.latest_thread_usage = Some(usage);
            }
            return Ok(());
        }

        if record_type == Some("event_msg") && payload_type == Some("item_completed") {
            if let Some(item) = payload.get("item").and_then(Value::as_object) {
                let item_turn_id =
                    value_str(payload.get("turn_id")).or(self.current_turn_id.as_deref());
                match value_str(item.get("type")) {
                    Some("UserMessage") => {
                        if let Some(output) =
                            structured_user_item_output(item, item_turn_id, record_time)
                        {
                            self.structured_user_items.push(output);
                        }
                        return Ok(());
                    }
                    Some("CommandExecution") => {
                        self.structured_command_activities
                            .push(typed_command_execution_output(
                                item,
                                item_turn_id,
                                record_time,
                                payload,
                            ));
                        return Ok(());
                    }
                    Some("WebSearch" | "Extension") => {
                        if let Some(activity) = typed_web_activity(item, item_turn_id, record_time)
                        {
                            self.structured_web_activities.push(activity);
                        }
                        return Ok(());
                    }
                    Some("ImageView") => {
                        if let Some(activity) =
                            typed_image_view_activity(item, item_turn_id, record_time)
                        {
                            self.structured_image_activities.push(activity);
                        }
                        return Ok(());
                    }
                    Some("McpToolCall" | "DynamicToolCall") => {
                        self.structured_tool_activities.push(typed_tool_activity(
                            item,
                            item_turn_id,
                            record_time,
                            payload,
                        ));
                        return Ok(());
                    }
                    Some("AgentMessage") => {
                        if let Some(delivery) = async_delivery(item, item_turn_id, record_time) {
                            self.async_deliveries.push(delivery);
                        }
                        return Ok(());
                    }
                    Some("FileChange") => {
                        self.structured_changes.extend(typed_file_change_outputs(
                            item,
                            item_turn_id,
                            record_time,
                        ));
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

        if record_type == Some("turn_context") {
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                let profile = profile_from_turn_context(payload);
                if !profile.is_empty() {
                    self.turn_profiles.insert(turn_id.to_string(), profile);
                }
            }
            return Ok(());
        }

        if record_type == Some("event_msg") && payload_type == Some("thread_goal_updated") {
            let Some(goal_payload) = payload.get("goal").and_then(Value::as_object) else {
                return Ok(());
            };
            let thread_id = value_str(payload.get("threadId"))
                .or_else(|| value_str(goal_payload.get("threadId")));
            if thread_id != Some(self.session_id.as_str()) {
                return Ok(());
            }
            self.goal_sequence += 1;
            let mut update =
                value_object_with_sequence(self.goal_sequence, Some("thread_goal_updated"));
            if let Some(record_time) = record_time {
                update.insert(
                    "create_time".to_string(),
                    Value::String(utc_iso(record_time)),
                );
            }
            if let Some(objective) = nonblank_raw_str(goal_payload.get("objective")) {
                update.insert(
                    "objective".to_string(),
                    Value::String(objective.to_string()),
                );
            }
            if let Some(status) = value_str(goal_payload.get("status")) {
                update.insert(
                    "reported_status".to_string(),
                    Value::String(status.to_ascii_lowercase()),
                );
            }
            let created_at = parse_unix_seconds(goal_payload.get("createdAt"));
            let updated_at = parse_unix_seconds(goal_payload.get("updatedAt")).or(record_time);
            if let Some(created_at) = created_at {
                update.insert("created_at".to_string(), Value::String(utc_iso(created_at)));
            }
            if let Some(updated_at) = updated_at {
                update.insert("updated_at".to_string(), Value::String(utc_iso(updated_at)));
            }
            for (output_key, source_key) in [
                ("tokens_used", "tokensUsed"),
                ("token_budget", "tokenBudget"),
                ("time_used_seconds", "timeUsedSeconds"),
            ] {
                if let Some(value) = int_value(goal_payload.get(source_key), Some(0)) {
                    update.insert(output_key.to_string(), Value::Number(value.into()));
                }
            }
            if let Some(turn_id) = self.current_turn_id.as_deref() {
                update.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
            }
            let update_index = self.goal_updates.len();
            self.goal_updates.push(Value::Object(update));
            if self.current_turn_id.is_none() {
                self.pending_goal_update_indexes.push(update_index);
            }
            return Ok(());
        }

        if record_type == Some("response_item")
            && payload_type == Some("message")
            && value_str(payload.get("role")) == Some("user")
        {
            let content = get_message_text(payload.get("content"));
            let turn_id = call_turn_id(payload, self.current_turn_id.as_deref());
            if let (Some(turn_id), Some(content)) = (turn_id.as_deref(), content.as_deref()) {
                if !content.trim().is_empty() {
                    self.response_user_candidates
                        .entry((turn_id.to_string(), text_key(Some(content))))
                        .or_default()
                        .push_back(user_candidate_output(Some(turn_id), record_time, content));
                }
            }
            let objective = goal_context_objective(content.as_deref());
            if let (Some(turn_id), Some(objective)) = (turn_id, objective) {
                self.goal_contexts_by_turn
                    .entry(turn_id)
                    .or_default()
                    .push(objective);
            }
            return Ok(());
        }

        if record_type == Some("event_msg") && payload_type == Some("sub_agent_activity") {
            self.agent_sequence += 1;
            let mut event = value_object_with_sequence(self.agent_sequence, None);
            event.insert(
                "type".to_string(),
                Value::String("agent_activity".to_string()),
            );
            let occurred_seconds = float_value(payload.get("occurred_at_ms"), None)
                .map(|value| (value as i64).div_euclid(1000));
            let occurred = occurred_seconds
                .and_then(|seconds| seconds.checked_mul(1_000_000))
                .or(record_time);
            if let Some(occurred) = occurred {
                event.insert("create_time".to_string(), Value::String(utc_iso(occurred)));
            }
            event.insert("actor".to_string(), Value::String(self.actor_path.clone()));
            for (output_key, source_key) in [
                ("target_thread_id", "agent_thread_id"),
                ("target", "agent_path"),
                ("status", "kind"),
            ] {
                if let Some(value) = value_str(payload.get(source_key)) {
                    event.insert(output_key.to_string(), Value::String(value.to_string()));
                }
            }
            self.agent_events.push(Value::Object(event));
            return Ok(());
        }

        if record_type == Some("response_item")
            && matches!(payload_type, Some("function_call" | "custom_tool_call"))
        {
            let name = value_str(payload.get("name")).unwrap_or_default();
            let arguments = json_object(payload.get("arguments"));
            let turn_id = call_turn_id(payload, self.current_turn_id.as_deref());
            if matches!(name, "create_goal" | "update_goal" | "get_goal") {
                if let Some(call_id) = value_str(payload.get("call_id")) {
                    let mut pending_call = Obj::new();
                    pending_call.insert("source".to_string(), Value::String(name.to_string()));
                    insert_optional_string(&mut pending_call, "turn_id", turn_id.as_deref());
                    self.pending_goal_calls
                        .insert(call_id.to_string(), pending_call);
                }
                return Ok(());
            }
            if name == "update_plan" {
                self.plan_sequence += 1;
                let mut update = value_object_with_sequence(self.plan_sequence, None);
                insert_optional_string(&mut update, "turn_id", turn_id.as_deref());
                if let Some(record_time) = record_time {
                    update.insert(
                        "create_time".to_string(),
                        Value::String(utc_iso(record_time)),
                    );
                }
                if let Some(explanation) = value_str(arguments.get("explanation")) {
                    update.insert(
                        "explanation".to_string(),
                        Value::String(explanation.to_string()),
                    );
                }
                let mut steps = Vec::new();
                for plan_item in sequence_values(arguments.get("plan")) {
                    let Some(plan_item) = plan_item.as_object() else {
                        continue;
                    };
                    let Some(step_text) = value_str(plan_item.get("step")) else {
                        continue;
                    };
                    let mut step = obj!("text" => Value::String(step_text.to_string()));
                    if let Some(status) = value_str(plan_item.get("status")) {
                        step.insert("status".to_string(), Value::String(status.to_string()));
                    }
                    steps.push(Value::Object(step));
                }
                if !steps.is_empty() {
                    update.insert("steps".to_string(), Value::Array(steps));
                }
                self.plan_updates.push(Value::Object(update));
                return Ok(());
            }
            if name == "exec" {
                if let Some(tool_input) = raw_str(payload.get("input")) {
                    if let Some(goal_tool_name) = exec_goal_tool_name(Some(tool_input)) {
                        if let Some(call_id) = value_str(payload.get("call_id")) {
                            let mut pending_call = Obj::new();
                            pending_call.insert(
                                "source".to_string(),
                                Value::String(format!("exec:{goal_tool_name}")),
                            );
                            insert_optional_string(
                                &mut pending_call,
                                "turn_id",
                                turn_id.as_deref(),
                            );
                            self.pending_goal_calls
                                .insert(call_id.to_string(), pending_call);
                        }
                    }
                    for parsed in javascript_plan_updates(tool_input) {
                        self.plan_sequence += 1;
                        let mut update = value_object_with_sequence(self.plan_sequence, None);
                        insert_optional_string(&mut update, "turn_id", turn_id.as_deref());
                        if let Some(record_time) = record_time {
                            update.insert(
                                "create_time".to_string(),
                                Value::String(utc_iso(record_time)),
                            );
                        }
                        if let Some(explanation) = parsed.get("explanation") {
                            update.insert("explanation".to_string(), explanation.clone());
                        }
                        if let Some(steps) = parsed.get("steps") {
                            update.insert("steps".to_string(), steps.clone());
                        }
                        self.plan_updates.push(Value::Object(update));
                    }
                }
                return Ok(());
            }
            if agent_function_name(name) {
                self.agent_sequence += 1;
                let event_type = match name {
                    "spawn_agent" => "spawn",
                    "followup_task" => "followup_task",
                    "send_message" => "message_sent",
                    "wait_agent" => "wait",
                    "list_agents" => "list",
                    "interrupt_agent" => "interrupt",
                    _ => "agent_call",
                };
                let mut event = value_object_with_sequence(self.agent_sequence, None);
                event.insert("type".to_string(), Value::String(event_type.to_string()));
                event.insert("actor".to_string(), Value::String(self.actor_path.clone()));
                insert_optional_string(&mut event, "turn_id", turn_id.as_deref());
                if let Some(record_time) = record_time {
                    event.insert(
                        "create_time".to_string(),
                        Value::String(utc_iso(record_time)),
                    );
                }
                for (output_key, source_key) in [
                    ("target", "target"),
                    ("task_name", "task_name"),
                    ("fork_turns", "fork_turns"),
                    ("timeout_ms", "timeout_ms"),
                ] {
                    if let Some(value) = arguments.get(source_key) {
                        let valid = value.as_str().is_some_and(|text| !text.trim().is_empty())
                            || value.as_i64().is_some()
                            || value.as_u64().is_some();
                        if valid {
                            event.insert(output_key.to_string(), value.clone());
                        }
                    }
                }
                if let Some(message) = raw_str(arguments.get("message")) {
                    event.insert(
                        "payload_encrypted".to_string(),
                        Value::Bool(message.starts_with("gAAAA")),
                    );
                }
                let event_index = self.agent_events.len();
                self.agent_events.push(Value::Object(event));
                if let Some(call_id) = value_str(payload.get("call_id")) {
                    self.pending_agent_calls
                        .insert(call_id.to_string(), event_index);
                }
                return Ok(());
            }
        }

        if record_type == Some("response_item")
            && matches!(
                payload_type,
                Some("function_call_output" | "custom_tool_call_output")
            )
        {
            let call_id = value_str(payload.get("call_id")).map(str::to_string);
            if let Some(call_id) = call_id.as_deref() {
                if let Some(pending_call) = self.pending_goal_calls.remove(call_id) {
                    let observation = matches!(
                        value_str(pending_call.get("source")),
                        Some("get_goal" | "exec:get_goal")
                    );
                    let result = if observation {
                        extract_goal_observation_result(payload.get("output")).unwrap_or_default()
                    } else {
                        extract_goal_result(payload.get("output")).unwrap_or_else(|| {
                            let output_text = raw_str(payload.get("output"))
                                .map(str::to_string)
                                .or_else(|| get_message_text(payload.get("output")));
                            let output_value = output_text.map(Value::String);
                            json_object(output_value.as_ref())
                        })
                    };
                    if let Some(goal_payload) = result.get("goal").and_then(Value::as_object) {
                        if value_str(goal_payload.get("threadId")) == Some(self.session_id.as_str())
                        {
                            self.goal_sequence += 1;
                            let source =
                                value_str(pending_call.get("source")).unwrap_or("goal_tool");
                            let mut update =
                                value_object_with_sequence(self.goal_sequence, Some(source));
                            if observation {
                                update.insert("observation".to_string(), Value::Bool(true));
                                update.insert(
                                    "source_call_id".to_string(),
                                    Value::String(call_id.to_string()),
                                );
                                if let Some(time) = record_time {
                                    update.insert(
                                        "observed_at".to_string(),
                                        Value::String(utc_iso(time)),
                                    );
                                }
                            }
                            if let Some(record_time) = record_time {
                                update.insert(
                                    "create_time".to_string(),
                                    Value::String(utc_iso(record_time)),
                                );
                            }
                            insert_optional_string(
                                &mut update,
                                "turn_id",
                                value_str(pending_call.get("turn_id")),
                            );
                            if let Some(objective) = nonblank_raw_str(goal_payload.get("objective"))
                            {
                                update.insert(
                                    "objective".to_string(),
                                    Value::String(objective.to_string()),
                                );
                            }
                            if let Some(status) = value_str(goal_payload.get("status")) {
                                update.insert(
                                    "reported_status".to_string(),
                                    Value::String(status.to_ascii_lowercase()),
                                );
                            }
                            if let Some(created_at) =
                                parse_unix_seconds(goal_payload.get("createdAt"))
                            {
                                update.insert(
                                    "created_at".to_string(),
                                    Value::String(utc_iso(created_at)),
                                );
                            }
                            if let Some(updated_at) =
                                parse_unix_seconds(goal_payload.get("updatedAt")).or(record_time)
                            {
                                update.insert(
                                    "updated_at".to_string(),
                                    Value::String(utc_iso(updated_at)),
                                );
                            }
                            for (output_key, source_key) in [
                                ("tokens_used", "tokensUsed"),
                                ("token_budget", "tokenBudget"),
                                ("time_used_seconds", "timeUsedSeconds"),
                            ] {
                                if let Some(value) =
                                    int_value(goal_payload.get(source_key), Some(0))
                                {
                                    update.insert(
                                        output_key.to_string(),
                                        Value::Number(value.into()),
                                    );
                                }
                            }
                            if let Some(remaining) =
                                int_value(result.get("remainingTokens"), Some(0))
                            {
                                update.insert(
                                    "remaining_tokens".to_string(),
                                    Value::Number(remaining.into()),
                                );
                            }
                            self.goal_updates.push(Value::Object(update));
                        }
                    }
                    return Ok(());
                }
            }

            let Some(call_id) = call_id else {
                return Ok(());
            };
            let Some(event_index) = self.pending_agent_calls.remove(&call_id) else {
                return Ok(());
            };
            let output_text = raw_str(payload.get("output"))
                .map(str::to_string)
                .or_else(|| get_message_text(payload.get("output")));
            let output_value = output_text.map(Value::String);
            let result = json_object(output_value.as_ref());
            let Some(event) = self
                .agent_events
                .get_mut(event_index)
                .and_then(Value::as_object_mut)
            else {
                return Ok(());
            };
            if let Some(task_path) = value_str(result.get("task_name")) {
                event.insert("target".to_string(), Value::String(task_path.to_string()));
            }
            if let Some(timed_out) = bool_value(result.get("timed_out")) {
                event.insert("timed_out".to_string(), Value::Bool(timed_out));
            }
            let mut statuses = Vec::new();
            for agent in sequence_values(result.get("agents")) {
                let Some(agent) = agent.as_object() else {
                    continue;
                };
                if let (Some(name), Some(status)) = (
                    value_str(agent.get("agent_name")),
                    value_str(agent.get("agent_status")),
                ) {
                    statuses.push(Value::Object(obj!(
                        "agent" => Value::String(name.to_string()),
                        "status" => Value::String(status.to_string()),
                    )));
                }
            }
            if !statuses.is_empty() {
                event.insert("agents".to_string(), Value::Array(statuses));
            }
            if let Some(record_time) = record_time {
                event.insert(
                    "completed_at".to_string(),
                    Value::String(utc_iso(record_time)),
                );
            }
            return Ok(());
        }

        if record_type == Some("response_item") && payload_type == Some("agent_message") {
            self.agent_sequence += 1;
            let mut event = value_object_with_sequence(self.agent_sequence, None);
            event.insert(
                "type".to_string(),
                Value::String("message_received".to_string()),
            );
            if let Some(record_time) = record_time {
                event.insert(
                    "create_time".to_string(),
                    Value::String(utc_iso(record_time)),
                );
            }
            insert_optional_string(&mut event, "actor", value_str(payload.get("author")));
            insert_optional_string(&mut event, "target", value_str(payload.get("recipient")));
            let content = get_message_text(payload.get("content")).unwrap_or_default();
            if let Some(captures) = agent_message_type_regex().captures(&content) {
                event.insert(
                    "message_type".to_string(),
                    Value::String(captures[1].trim().to_string()),
                );
            }
            if let Some(captures) = agent_task_name_regex().captures(&content) {
                event.insert(
                    "task_name".to_string(),
                    Value::String(captures[1].trim().to_string()),
                );
            }
            event.insert(
                "payload_encrypted".to_string(),
                Value::Bool(content.contains("gAAAA") || content.contains("Message Type:")),
            );
            self.agent_events.push(Value::Object(event));
        }

        Ok(())
    }
}

fn hydrate_goal_update_attachments(goal_updates: &mut [Value], registry: &AttachmentRegistry) {
    for update in goal_updates {
        let Some(update) = update.as_object_mut() else {
            continue;
        };
        let Some(objective) = nonblank_raw_str(update.get("objective")).map(str::to_string) else {
            continue;
        };
        let (restored, attachments, restored_any) =
            replace_attachment_transport(&objective, registry);
        update.insert("objective".to_string(), Value::String(restored));
        if !attachments.is_empty() {
            update.insert("attachments".to_string(), Value::Array(attachments));
        }
        if restored_any {
            update.insert(
                "objective_source".to_string(),
                Value::String("codex_attachment".to_string()),
            );
            update.insert("transport_objective".to_string(), Value::String(objective));
        }
    }
}

fn finalize_streaming_enrichment(
    mut state: EnrichmentAccumulator,
    session: &mut Session,
    registry: &AttachmentRegistry,
) -> Result<()> {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(35, 0);

    state.resolve_prelocal()?;
    let EnrichmentAccumulator {
        session_id,
        is_subagent,
        create_time,
        actor_path: _,
        local_turn_ids,
        turn_profiles,
        plan_updates,
        mut goal_updates,
        mut pending_goal_update_indexes,
        pending_goal_calls: _,
        goal_contexts_by_turn,
        agent_events,
        pending_agent_calls: _,
        current_turn_id: _,
        plan_sequence: _,
        goal_sequence: _,
        agent_sequence: _,
        local_start_line: _,
        message_start_line: _,
        active_task_start_line: _,
        delegation_seen: _,
        latest_local_record_time: _,
        prelocal_records: _,
        mut response_user_candidates,
        structured_user_items,
        structured_command_activities,
        structured_tool_activities,
        structured_web_activities,
        structured_image_activities,
        async_deliveries,
        structured_changes,
        latest_thread_usage,
    } = state;
    hydrate_goal_update_attachments(&mut goal_updates, registry);
    if !pending_goal_update_indexes.is_empty() {
        if let Some(last_local_turn_id) = local_turn_ids.last() {
            for index in pending_goal_update_indexes.drain(..) {
                if let Some(update) = goal_updates.get_mut(index).and_then(Value::as_object_mut) {
                    update.insert(
                        "turn_id".to_string(),
                        Value::String(last_local_turn_id.clone()),
                    );
                }
            }
        }
    }

    let effective_ids: HashSet<String> = session
        .turns
        .iter()
        .filter_map(|turn| {
            turn.get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let has_effective_lifecycle = !session.turns.is_empty() || !session.rollbacks.is_empty();
    let async_deliveries = async_deliveries
        .into_iter()
        .filter(|delivery| {
            test_turn_effective(
                delivery.get("turn_id").and_then(Value::as_str),
                has_effective_lifecycle,
                &effective_ids,
            )
        })
        .collect();
    attach_async_deliveries(&mut session.user_input_requests, async_deliveries);
    if is_subagent {
        let local_set: HashSet<String> = local_turn_ids.into_iter().collect();
        session.turns.retain(|turn| {
            turn.as_object()
                .and_then(|map| value_str(map.get("turn_id")))
                .is_some_and(|turn_id| local_set.contains(turn_id))
        });
        session.messages.retain(|message| {
            message
                .as_object()
                .and_then(|map| value_str(map.get("turn_id")))
                .is_some_and(|turn_id| local_set.contains(turn_id))
        });
        session.raw_messages.retain(|message| {
            message
                .turn_id
                .as_ref()
                .is_some_and(|turn_id| local_set.contains(turn_id))
        });
        filter_values_by_turn(&mut session.plans, &local_set);
        filter_values_by_turn(&mut session.reviews, &local_set);
        filter_values_by_turn(&mut session.memory_citations, &local_set);
        filter_values_by_turn(&mut session.writing_blocks, &local_set);
        filter_values_by_turn(&mut session.ui_actions, &local_set);
        filter_values_by_turn(&mut session.ui_parse_warnings, &local_set);
        filter_values_by_turn(&mut session.user_input_requests, &local_set);
        filter_values_by_turn(&mut session.activities, &local_set);
        filter_values_by_turn(&mut session.changes, &local_set);
        session.goals.retain(|goal| {
            goal.as_object()
                .and_then(|map| value_str(map.get("thread_id")))
                == Some(session_id.as_str())
        });
    }

    if !structured_command_activities.is_empty() {
        session.activities.retain(|activity| {
            let Some(map) = activity.as_object() else {
                return true;
            };
            if value_str(map.get("type")) != Some("command") {
                return true;
            }
            map.get("aggregate_result")
                .and_then(Value::as_object)
                .and_then(|aggregate| value_str(aggregate.get("status")))
                == Some("failed")
        });
        session.activities.extend(structured_command_activities);
        session.activities.sort_by(|left, right| {
            let left_time = left
                .as_object()
                .and_then(|map| value_str(map.get("create_time")))
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            let right_time = right
                .as_object()
                .and_then(|map| value_str(map.get("create_time")))
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            left_time.cmp(right_time)
        });
    }
    if !structured_tool_activities.is_empty() {
        session
            .activities
            .extend(structured_tool_activities.into_iter().filter(|activity| {
                test_turn_effective(
                    activity.get("turn_id").and_then(Value::as_str),
                    has_effective_lifecycle,
                    &effective_ids,
                )
            }));
        session.activities.sort_by(|left, right| {
            let a = left
                .get("create_time")
                .and_then(Value::as_str)
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            let b = right
                .get("create_time")
                .and_then(Value::as_str)
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            a.cmp(b)
        });
    }
    if !structured_web_activities.is_empty() || !structured_image_activities.is_empty() {
        session.activities.extend(
            structured_web_activities
                .into_iter()
                .chain(structured_image_activities)
                .filter(|activity| {
                    test_turn_effective(
                        activity.get("turn_id").and_then(Value::as_str),
                        has_effective_lifecycle,
                        &effective_ids,
                    )
                }),
        );
        session.activities.sort_by(|left, right| {
            let a = left
                .get("create_time")
                .and_then(Value::as_str)
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            let b = right
                .get("create_time")
                .and_then(Value::as_str)
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            a.cmp(b)
        });
    }
    if !structured_changes.is_empty() {
        session.changes = structured_changes;
        session.changes.sort_by(|left, right| {
            let left_time = left
                .as_object()
                .and_then(|map| value_str(map.get("create_time")))
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            let right_time = right
                .as_object()
                .and_then(|map| value_str(map.get("create_time")))
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            left_time.cmp(right_time)
        });
    }
    if let Some(usage) = latest_thread_usage {
        session.usage = Some(usage);
    }

    let structured_user_messages =
        merge_verified_structured_users(&mut response_user_candidates, &structured_user_items);
    let mut source_messages: Vec<Value> = session
        .messages
        .iter()
        .filter_map(|message| message.as_object().cloned().map(Value::Object))
        .collect();
    source_messages.extend(structured_user_messages);
    let mut represented_goal_keys = HashSet::new();
    for message in &source_messages {
        let Some(map) = message.as_object() else {
            continue;
        };
        if value_str(map.get("role")) != Some("user") {
            continue;
        }
        if let Some(key) = goal_text_key(raw_str(map.get("content"))) {
            represented_goal_keys.insert(key);
        }
    }

    for update_value in &goal_updates {
        let Some(update) = update_value.as_object() else {
            continue;
        };
        let objective_value =
            raw_str(update.get("transport_objective")).or_else(|| raw_str(update.get("objective")));
        let objective_key = goal_text_key(objective_value);
        let turn_id = value_str(update.get("turn_id"));
        let eligible = objective_key
            .as_ref()
            .is_some_and(|key| !represented_goal_keys.contains(key))
            && value_str(update.get("source")) == Some("thread_goal_updated")
            && value_str(update.get("reported_status")) == Some("active")
            && turn_id.is_some()
            && goal_update_is_fresh_input(update);
        if !eligible {
            continue;
        }
        let objective_key = objective_key.expect("checked");
        let turn_id = turn_id.expect("checked");
        let context_matches = goal_contexts_by_turn.get(turn_id).is_some_and(|contexts| {
            contexts
                .iter()
                .any(|objective| goal_text_key(Some(objective)) == Some(objective_key.clone()))
        });
        let Some(content_value) = nonblank_raw_str(update.get("objective")) else {
            continue;
        };
        if !context_matches {
            continue;
        }
        let mut synthetic = obj!(
            "id" => Value::String(format!("goal-update-{}", int_value(update.get("sequence"), None).unwrap_or_default())),
            "turn_id" => Value::String(turn_id.to_string()),
            "role" => Value::String("user".to_string()),
            "create_time" => update.get("create_time").cloned().unwrap_or(Value::Null),
            "content" => Value::String(content_value.to_string()),
            "content_source" => Value::String(value_str(update.get("objective_source")).unwrap_or("thread_goal_updated").to_string()),
            "message_mode" => Value::String("goal".to_string()),
        );
        if let Some(attachments) = update.get("attachments").and_then(Value::as_array) {
            let values: Vec<Value> = attachments
                .iter()
                .filter_map(|item| item.as_object().cloned().map(Value::Object))
                .collect();
            if !values.is_empty() {
                synthetic.insert("attachments".to_string(), Value::Array(values));
            }
        }
        if let Some(transport) = raw_str(update.get("transport_objective")) {
            synthetic.insert(
                "transport_content".to_string(),
                Value::String(transport.to_string()),
            );
        }
        synthetic.insert(
            "message_origin".to_string(),
            Value::String(
                value_str(update.get("source"))
                    .unwrap_or("thread_goal_updated")
                    .to_string(),
            ),
        );
        source_messages.push(Value::Object(synthetic));
        represented_goal_keys.insert(objective_key);
    }

    let mut indexed_messages: Vec<(usize, Value)> =
        source_messages.into_iter().enumerate().collect();
    indexed_messages.sort_by(|left, right| {
        let left_time = left
            .1
            .as_object()
            .and_then(|map| value_str(map.get("create_time")))
            .unwrap_or("9999-12-31T23:59:59.999+00:00");
        let right_time = right
            .1
            .as_object()
            .and_then(|map| value_str(map.get("create_time")))
            .unwrap_or("9999-12-31T23:59:59.999+00:00");
        left_time.cmp(right_time).then_with(|| left.0.cmp(&right.0))
    });
    let sorted_messages: Vec<Value> = indexed_messages
        .into_iter()
        .map(|(_, value)| value)
        .collect();
    let (messages, id_map) = renumber_messages(&sorted_messages);
    let mut attachment_message_by_path: HashMap<String, String> = HashMap::new();
    let mut goal_message_by_key: HashMap<String, String> = HashMap::new();
    let mut restored_messages = Vec::with_capacity(messages.len());
    for message in messages {
        let Some(message_map) = message.as_object() else {
            restored_messages.push(message);
            continue;
        };
        let Some(content) = raw_str(message_map.get("content")) else {
            restored_messages.push(message);
            continue;
        };
        let existing_attachments = message_map.get("attachments").and_then(Value::as_array);
        let updated = if existing_attachments.is_some()
            || !message_attachment_transport_is_restorable(message_map)
        {
            message_map.clone()
        } else {
            let (restored, attachments, restored_any) =
                replace_attachment_transport(content, registry);
            if attachments.is_empty() {
                message_map.clone()
            } else {
                let mut output = Obj::new();
                for (key, value) in message_map {
                    if key == "content" {
                        output.insert("content".to_string(), Value::String(restored.clone()));
                        output.insert(
                            "content_source".to_string(),
                            Value::String(
                                if restored_any {
                                    "codex_attachment"
                                } else {
                                    "attachment_reference"
                                }
                                .to_string(),
                            ),
                        );
                        let goal_mode = content.to_ascii_lowercase().contains("/goal")
                            || attachments.iter().any(|item| {
                                item.as_object()
                                    .and_then(|map| value_str(map.get("filename")))
                                    == Some("goal-objective.md")
                            });
                        output.insert(
                            "message_mode".to_string(),
                            Value::String(if goal_mode { "goal" } else { "standard" }.to_string()),
                        );
                        output.insert("attachments".to_string(), Value::Array(attachments.clone()));
                        output.insert(
                            "transport_content".to_string(),
                            Value::String(content.to_string()),
                        );
                    } else {
                        output.insert(key.clone(), value.clone());
                    }
                }
                output
            }
        };
        let message_id = value_str(updated.get("id")).unwrap_or_default().to_string();
        if value_str(updated.get("role")) == Some("user") {
            for reference_text in [raw_str(updated.get("transport_content")), Some(content)] {
                let Some(reference_text) = reference_text else {
                    continue;
                };
                for reference in attachment_references(reference_text) {
                    attachment_message_by_path
                        .insert(reference.normalized_path, message_id.clone());
                }
            }
        }
        if value_str(updated.get("role")) == Some("user")
            && value_str(updated.get("message_mode")) == Some("goal")
        {
            for key_text in [
                raw_str(updated.get("transport_content")),
                raw_str(updated.get("content")),
            ] {
                if let Some(key) = goal_text_key(key_text) {
                    goal_message_by_key
                        .entry(key)
                        .or_insert_with(|| message_id.clone());
                }
            }
        }
        restored_messages.push(Value::Object(updated));
    }
    session.messages = restored_messages;

    session.plans = session
        .plans
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();
    session.reviews = session
        .reviews
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();
    session.memory_citations = session
        .memory_citations
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();
    session.writing_blocks = session
        .writing_blocks
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();
    session.ui_actions = session
        .ui_actions
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();
    session.ui_parse_warnings = session
        .ui_parse_warnings
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();

    for update_value in &mut goal_updates {
        let Some(update) = update_value.as_object_mut() else {
            continue;
        };
        let objective_owned = raw_str(update.get("transport_objective"))
            .or_else(|| raw_str(update.get("objective")))
            .map(str::to_string);
        let Some(objective) = objective_owned else {
            continue;
        };
        let mut matched_message_id = None;
        for reference in attachment_references(&objective) {
            if let Some(message_id) = attachment_message_by_path.get(&reference.normalized_path) {
                matched_message_id = Some(message_id.clone());
                break;
            }
        }
        if matched_message_id.is_none() {
            if let Some(key) = goal_text_key(Some(&objective)) {
                matched_message_id = goal_message_by_key.get(&key).cloned();
            }
        }
        if let Some(message_id) = matched_message_id {
            update.remove("objective");
            update.insert(
                "objective_message_id".to_string(),
                Value::String(message_id),
            );
        }
    }

    let mut local_goals = Vec::new();
    for goal in &session.goals {
        let Some(goal_map) = goal.as_object() else {
            continue;
        };
        let mut item = goal_map.clone();
        let objective_owned = raw_str(item.get("objective")).map(str::to_string);
        if let Some(objective) = objective_owned {
            let references = attachment_references(&objective);
            let (restored, attachments, restored_any) =
                replace_attachment_transport(&objective, registry);
            let mut matched_message_id = None;
            for reference in references {
                if let Some(message_id) = attachment_message_by_path.get(&reference.normalized_path)
                {
                    matched_message_id = Some(message_id.clone());
                    break;
                }
            }
            if matched_message_id.is_none() {
                if let Some(key) = goal_text_key(Some(&objective)) {
                    matched_message_id = goal_message_by_key.get(&key).cloned();
                }
            }
            if let Some(message_id) = matched_message_id {
                item.remove("objective");
                item.insert(
                    "objective_message_id".to_string(),
                    Value::String(message_id),
                );
            } else {
                item.insert("objective".to_string(), Value::String(restored));
                if !attachments.is_empty() {
                    item.insert("attachments".to_string(), Value::Array(attachments));
                }
                if restored_any {
                    item.insert(
                        "objective_source".to_string(),
                        Value::String("codex_attachment".to_string()),
                    );
                    item.insert("transport_objective".to_string(), Value::String(objective));
                }
            }
        }
        local_goals.push(Value::Object(item));
    }

    let projected_goal_updates = projectable_goal_updates(&goal_updates);
    if let Some(latest_update_value) = projected_goal_updates.last() {
        let latest_update = latest_update_value.as_object().expect("goal update object");
        let latest_created_at = raw_str(latest_update.get("created_at")).map(str::to_string);
        let generation_updates: Vec<&Obj> = if let Some(created_at) = latest_created_at.as_deref() {
            projected_goal_updates
                .iter()
                .filter_map(Value::as_object)
                .filter(|update| raw_str(update.get("created_at")) == Some(created_at))
                .collect()
        } else {
            vec![latest_update]
        };
        let empty_creation = Obj::new();
        let first_generation_update = generation_updates
            .iter()
            .copied()
            .find(|update| update.get("observation").and_then(Value::as_bool) != Some(true))
            .unwrap_or(&empty_creation);
        let mut current_goal = local_goals
            .iter()
            .filter_map(Value::as_object)
            .find(|goal| {
                value_str(goal.get("thread_id")) == Some(session_id.as_str())
                    && latest_created_at.as_deref().is_none_or(|created_at| {
                        raw_str(goal.get("created_at")) == Some(created_at)
                    })
            })
            .cloned()
            .unwrap_or_default();
        current_goal.insert("id".to_string(), Value::String("goal01".to_string()));
        current_goal.insert("thread_id".to_string(), Value::String(session_id.clone()));
        if let Some(created_turn_id) = value_str(first_generation_update.get("turn_id")) {
            current_goal.insert(
                "created_turn_id".to_string(),
                Value::String(created_turn_id.to_string()),
            );
        } else {
            current_goal.remove("created_turn_id");
        }
        let objective_update = generation_updates
            .iter()
            .rev()
            .find(|candidate| {
                candidate.contains_key("objective_message_id")
                    || candidate.contains_key("objective")
                    || candidate.contains_key("attachments")
            })
            .copied();
        for key in [
            "objective",
            "objective_message_id",
            "attachments",
            "objective_source",
            "transport_objective",
        ] {
            current_goal.remove(key);
        }
        if let Some(objective_update) = objective_update {
            if let Some(message_id) = value_str(objective_update.get("objective_message_id")) {
                current_goal.insert(
                    "objective_message_id".to_string(),
                    Value::String(message_id.to_string()),
                );
            } else {
                for key in [
                    "objective",
                    "attachments",
                    "objective_source",
                    "transport_objective",
                ] {
                    if let Some(value) = objective_update.get(key) {
                        current_goal.insert(key.to_string(), value.clone());
                    }
                }
            }
        }
        let reported = value_str(latest_update.get("reported_status"))
            .unwrap_or("unknown")
            .to_string();
        current_goal.insert(
            "reported_status".to_string(),
            Value::String(reported.clone()),
        );
        current_goal.insert(
            "status_source".to_string(),
            Value::String(
                value_str(latest_update.get("source"))
                    .unwrap_or("thread_goal_updated")
                    .to_string(),
            ),
        );
        if let Some(created_at) = latest_created_at {
            current_goal.insert("created_at".to_string(), Value::String(created_at));
        } else {
            current_goal.remove("created_at");
        }
        if let Some(updated_at) = raw_str(latest_update.get("updated_at")) {
            current_goal.insert(
                "updated_at".to_string(),
                Value::String(updated_at.to_string()),
            );
        } else {
            current_goal.remove("updated_at");
        }
        for key in [
            "tokens_used",
            "token_budget",
            "time_used_seconds",
            "remaining_tokens",
        ] {
            if let Some(value) = latest_update.get(key) {
                current_goal.insert(key.to_string(), value.clone());
            } else {
                current_goal.remove(key);
            }
        }
        let session_turns: Vec<&Obj> = session
            .turns
            .iter()
            .filter_map(Value::as_object)
            .filter(|turn| value_str(turn.get("turn_id")).is_some())
            .collect();
        let last_turn_id = value_str(latest_update.get("turn_id"))
            .map(str::to_string)
            .or_else(|| {
                session_turns
                    .last()
                    .and_then(|turn| value_str(turn.get("turn_id")))
                    .map(str::to_string)
            });
        let turn_by_id: HashMap<String, &Obj> = session_turns
            .iter()
            .filter_map(|turn| {
                value_str(turn.get("turn_id")).map(|turn_id| (turn_id.to_string(), *turn))
            })
            .collect();
        let last_turn = last_turn_id
            .as_ref()
            .and_then(|turn_id| turn_by_id.get(turn_id).copied());
        if let Some(last_turn_id) = last_turn_id.as_deref() {
            current_goal.insert(
                "last_turn_id".to_string(),
                Value::String(last_turn_id.to_string()),
            );
        } else {
            current_goal.remove("last_turn_id");
        }
        if let Some(last_turn) = last_turn {
            let last_status = value_str(last_turn.get("status"));
            if let Some(last_status) = last_status {
                current_goal.insert(
                    "last_turn_status".to_string(),
                    Value::String(last_status.to_string()),
                );
            } else {
                current_goal.remove("last_turn_status");
            }
            if last_status == Some("completed") {
                if let Some(end_time) = raw_str(last_turn.get("end_time")) {
                    current_goal.insert(
                        "last_task_completed_at".to_string(),
                        Value::String(end_time.to_string()),
                    );
                }
            } else {
                current_goal.remove("last_task_completed_at");
            }
            let execution_state = match last_status {
                Some("in_progress") => "running",
                Some("completed") => "idle",
                Some("failed" | "aborted") => "interrupted",
                Some("rolled_back") => "idle",
                _ => "unknown",
            };
            current_goal.insert(
                "execution_state".to_string(),
                Value::String(execution_state.to_string()),
            );
        } else {
            current_goal.remove("last_turn_status");
            current_goal.remove("last_task_completed_at");
            let terminal = matches!(
                reported.as_str(),
                "complete" | "completed" | "cancelled" | "failed" | "paused" | "blocked"
            );
            current_goal.insert(
                "execution_state".to_string(),
                Value::String(if terminal { "idle" } else { "unknown" }.to_string()),
            );
        }
        local_goals = vec![Value::Object(current_goal)];
    }
    session.goals = local_goals;

    for turn in &mut session.turns {
        let Some(turn_map) = turn.as_object_mut() else {
            continue;
        };
        let Some(turn_id) = value_str(turn_map.get("turn_id")).map(str::to_string) else {
            continue;
        };
        if let Some(profile) = turn_profiles.get(&turn_id) {
            turn_map.insert(
                "execution_profile".to_string(),
                Value::Object(profile.clone()),
            );
        }
    }
    session.execution_profile = execution_profile_summary(&session.turns);
    session.plan_updates = plan_updates;
    session.goal_updates = goal_updates;
    session.agent_events = agent_events;

    if is_subagent {
        let mut delegation = Obj::new();
        insert_optional_string(
            &mut delegation,
            "parent_thread_id",
            session.parent_thread_id.as_deref(),
        );
        insert_optional_string(
            &mut delegation,
            "forked_from_id",
            session.forked_from_id.as_deref(),
        );
        insert_optional_string(
            &mut delegation,
            "agent_name",
            session.agent_nickname.as_deref(),
        );
        insert_optional_string(&mut delegation, "path", session.agent_path.as_deref());
        insert_optional_string(&mut delegation, "role", session.agent_role.as_deref());
        if let Some(path) = session
            .agent_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
        {
            let normalized = path.trim_end_matches(&['/', '\\'][..]).replace('\\', "/");
            if let Some(task_name) = normalized
                .rsplit('/')
                .next()
                .filter(|value| !value.is_empty())
            {
                delegation.insert(
                    "task_name".to_string(),
                    Value::String(task_name.to_string()),
                );
            }
        }
        if let Some(create_time) = create_time {
            delegation.insert(
                "assigned_at".to_string(),
                Value::String(utc_iso(create_time)),
            );
        }
        delegation.insert("payload_encrypted".to_string(), Value::Bool(true));
        session.delegation = Some(Value::Object(delegation));
    }

    session.activities = session
        .activities
        .iter()
        .map(clean_dynamic_command_value)
        .collect();
    session.session_state = if session.turns.iter().any(|turn| {
        turn.as_object()
            .and_then(|map| value_str(map.get("status")))
            == Some("in_progress")
    }) {
        "active_at_export".to_string()
    } else {
        "idle".to_string()
    };

    if let Some(latest) = latest_session_payload_timestamp(&session) {
        session.last_message_time = session.last_message_time.max(latest);
    }

    Ok(())
}

struct StreamingSessionBuilder {
    cwd: String,
    session_id: String,
    root_session_id: String,
    thread_source: String,
    parent_thread_id: Option<String>,
    forked_from_id: Option<String>,
    agent_nickname: Option<String>,
    agent_path: Option<String>,
    agent_depth: Option<i64>,
    agent_role: Option<String>,
    multi_agent_version: Option<String>,
    create_time: Option<Timestamp>,
    originator: Option<String>,
    source_value: Value,
    cli_version: Option<String>,
    model_provider: Option<String>,
    git: Option<Obj>,
    source_path: PathBuf,
    core: CoreAccumulator,
    enrichment: EnrichmentAccumulator,
    attachments: AttachmentCollector,
    data_quality: SourceDataQuality,
}

impl StreamingSessionBuilder {
    fn from_session_meta(
        session_meta: &Obj,
        path: &Path,
        target_cwd_normalized: &str,
        selected_ids: Option<&HashSet<String>>,
    ) -> Result<Option<Self>> {
        let cwd = value_str(session_meta.get("cwd"))
            .ok_or_else(|| anyhow!("Session cwd not found in {}", path.display()))?
            .to_string();
        if !matches_target_session(session_meta, target_cwd_normalized, selected_ids) {
            return Ok(None);
        }
        let session_id = value_str(session_meta.get("id"))
            .or_else(|| value_str(session_meta.get("session_id")))
            .ok_or_else(|| anyhow!("Session ID not found in {}", path.display()))?
            .to_string();
        let create_time = parse_datetime_value(session_meta.get("timestamp"));
        let originator = value_str(session_meta.get("originator")).map(str::to_string);
        let source_value = session_meta.get("source").cloned().unwrap_or(Value::Null);
        let cli_version = value_str(session_meta.get("cli_version")).map(str::to_string);
        let model_provider = value_str(session_meta.get("model_provider")).map(str::to_string);
        let git = session_meta.get("git").and_then(Value::as_object).cloned();
        let thread_source = value_str(session_meta.get("thread_source"))
            .unwrap_or("user")
            .to_string();
        let root_session_id = value_str(session_meta.get("session_id"))
            .unwrap_or(&session_id)
            .to_string();
        let parent_thread_id = value_str(session_meta.get("parent_thread_id")).map(str::to_string);
        let forked_from_id = value_str(session_meta.get("forked_from_id")).map(str::to_string);
        let agent_nickname = value_str(session_meta.get("agent_nickname")).map(str::to_string);
        let agent_path = value_str(session_meta.get("agent_path")).map(str::to_string);
        let multi_agent_version =
            value_str(session_meta.get("multi_agent_version")).map(str::to_string);
        let mut agent_depth = None;
        let mut agent_role = None;
        if let Some(subagent) = source_value
            .as_object()
            .and_then(|source| source.get("subagent"))
            .and_then(Value::as_object)
            .and_then(|subagent| subagent.get("thread_spawn"))
            .and_then(Value::as_object)
        {
            agent_depth = int_value(subagent.get("depth"), Some(0));
            agent_role = value_str(subagent.get("agent_role")).map(str::to_string);
        }
        let actor_path = agent_path
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("/root")
            .to_string();
        let enrichment = EnrichmentAccumulator::new(
            session_id.clone(),
            thread_source == "subagent",
            create_time,
            actor_path,
        );
        Ok(Some(Self {
            cwd,
            session_id,
            root_session_id,
            thread_source,
            parent_thread_id,
            forked_from_id,
            agent_nickname,
            agent_path,
            agent_depth,
            agent_role,
            multi_agent_version,
            create_time,
            originator,
            source_value,
            cli_version,
            model_provider,
            git,
            source_path: path.to_path_buf(),
            core: CoreAccumulator::default(),
            enrichment,
            attachments: AttachmentCollector::default(),
            data_quality: SourceDataQuality::default(),
        }))
    }

    fn consume(&mut self, line_number: usize, record: &Value) -> Result<()> {
        self.attachments.observe(record);
        self.core.consume(line_number, record, &self.cwd)?;
        self.enrichment.observe(line_number, record)?;
        Ok(())
    }

    fn finish(self, attachment_root: &Path) -> Result<Session> {
        let StreamingSessionBuilder {
            cwd,
            session_id,
            root_session_id,
            thread_source,
            parent_thread_id,
            forked_from_id,
            agent_nickname,
            agent_path,
            agent_depth,
            agent_role,
            multi_agent_version,
            create_time,
            originator,
            source_value,
            cli_version,
            model_provider,
            git,
            source_path,
            core,
            enrichment,
            attachments,
            data_quality,
        } = self;
        let registry = attachments.finish(attachment_root);
        let subagent_message_start_line = enrichment.local_message_start_line();
        let activity_only_subagent =
            thread_source == "subagent" && enrichment.local_start_line.is_some();
        let activity_only_update_time = if activity_only_subagent {
            enrichment.latest_local_record_time.or(create_time)
        } else {
            None
        };
        let CoreAccumulator {
            models,
            model_set: _,
            messages,
            plan_items,
            goals,
            goal_indexes: _,
            pending_goal_thread_id: _,
            usage_limits: _,
            mut rollbacks,
            changes,
            activities,
            pending_user_turns: _,
            command_calls: _,
            pending_unparsed_command_calls: _,
            user_input_requests,
            pending_user_input_requests: _,
            pending_web_calls: _,
            turn_stack,
            turn_info,
            has_turn_events,
            current_turn_id: _,
            sequence: _,
            context_window_tokens,
            latest_context_used_tokens,
            last_context_snapshot: _,
            mut context_compactions,
            mut pending_compaction,
        } = core;
        let path = source_path.clone();
        if let Some(pending) = pending_compaction.take() {
            let mut unfinished = obj!(
                "sequence" => Value::Number((pending.sequence as u64).into()),
                "compacted_at" => pending.compacted_at.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
            );
            if let Some(before) = pending.before {
                unfinished.insert("before".to_string(), before);
            }
            context_compactions.push(Value::Object(unfinished));
        }

        let effective_turn_set: HashSet<String> = turn_stack.iter().cloned().collect();
        let mut effective_messages: Vec<RawMessage> = messages
            .into_iter()
            .filter(|message| {
                test_turn_effective(
                    message.turn_id.as_deref(),
                    has_turn_events,
                    &effective_turn_set,
                ) && subagent_message_start_line
                    .is_none_or(|start_line| message.source_line >= start_line)
            })
            .collect();
        if effective_messages.is_empty() && !activity_only_subagent {
            bail!("No exportable messages found in {}", path.display());
        }
        effective_messages.sort_by_key(|item| (item.time.unwrap_or(MAX_TS), item.sequence));
        let effective_plan_items: Vec<PlanItem> = plan_items
            .into_iter()
            .filter(|item| {
                test_turn_effective(Some(&item.turn_id), has_turn_events, &effective_turn_set)
            })
            .collect();
        let effective_goals: Vec<GoalInfo> = goals
            .into_iter()
            .filter(|goal| goal_is_effective(goal, has_turn_events, &effective_turn_set))
            .collect();
        let message_outputs = build_output_messages(&effective_messages, &effective_plan_items);
        let last_message_time = message_outputs
            .last_message_time
            .or(activity_only_update_time)
            .ok_or_else(|| {
                anyhow!(
                    "No valid message/activity timestamp found in {}",
                    path.display()
                )
            })?;

        let mut output_turns = Vec::<Value>::new();
        for turn_id in &turn_stack {
            let Some(info) = turn_info.get(turn_id) else {
                continue;
            };
            let mut output = obj!(
                "turn_id" => Value::String(turn_id.clone()),
                "status" => Value::String(info.status.clone()),
                "start_time" => info.start_time.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
            );
            if let Some(end_time) = info.end_time {
                output.insert("end_time".to_string(), Value::String(utc_iso(end_time)));
            }
            if let Some(reason) = info
                .abort_reason
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                output.insert(
                    "abort_reason".to_string(),
                    Value::String(reason.to_string()),
                );
            }
            if let Some(mode) = info
                .mode
                .as_deref()
                .filter(|value| !value.trim().is_empty() && *value != "default")
            {
                output.insert("mode".to_string(), Value::String(mode.to_string()));
            }
            if matches!(info.status.as_str(), "aborted" | "failed") {
                let duration =
                    info.duration_ms
                        .or_else(|| match (info.start_time, info.end_time) {
                            (Some(start), Some(end)) => Some(((end - start).max(0) + 500) / 1_000),
                            _ => None,
                        });
                if let Some(duration) = duration {
                    output.insert("duration_ms".to_string(), Value::Number(duration.into()));
                }
            }
            if info.status == "failed" {
                if let Some(first_token) = info.time_to_first_token_ms {
                    output.insert(
                        "time_to_first_token_ms".to_string(),
                        Value::Number(first_token.into()),
                    );
                }
            }
            if let Some(termination) = &info.termination {
                output.insert("termination".to_string(), termination.clone());
            }
            if let Some(conflict) = &info.timestamp_conflict {
                output.insert(
                    "data_quality".to_string(),
                    Value::Object(obj!("timestamp_conflict" => conflict.clone())),
                );
            }
            output_turns.push(Value::Object(output));
        }
        let session_state = if output_turns.iter().any(|turn| {
            turn.as_object()
                .and_then(|turn| value_str(turn.get("status")))
                == Some("in_progress")
        }) {
            "active_at_export"
        } else {
            "idle"
        };

        let mut sorted_goals = effective_goals;
        sorted_goals.sort_by_key(|goal| goal.sequence);
        let mut output_goals = Vec::<Value>::new();
        for (index, goal) in sorted_goals.into_iter().enumerate() {
            let mut output = obj!(
                "id" => Value::String(format!("goal{:02}", index + 1)),
                "thread_id" => Value::String(goal.thread_id.clone()),
            );
            if let Some(turn_id) = goal
                .created_turn_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                output.insert(
                    "created_turn_id".to_string(),
                    Value::String(turn_id.to_string()),
                );
            }
            if let Some(objective) = goal
                .objective
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                output.insert(
                    "objective".to_string(),
                    Value::String(objective.to_string()),
                );
            }
            let reported = goal
                .reported_status
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            output.insert(
                "reported_status".to_string(),
                Value::String(reported.clone()),
            );
            output.insert(
                "status_source".to_string(),
                Value::String(goal.status_source.clone()),
            );
            let last_info = goal
                .last_turn_id
                .as_deref()
                .and_then(|id| turn_info.get(id));
            let execution_state = if goal.awaiting_task_start {
                "idle"
            } else if let Some(last_info) = last_info {
                match last_info.status.as_str() {
                    "in_progress" => "running",
                    "completed" | "rolled_back" => "idle",
                    "failed" | "aborted" => "interrupted",
                    _ => "unknown",
                }
            } else if matches!(reported.as_str(), "completed" | "cancelled" | "failed") {
                "idle"
            } else {
                "unknown"
            };
            output.insert(
                "execution_state".to_string(),
                Value::String(execution_state.to_string()),
            );
            if let Some(last_info) = last_info {
                output.insert(
                    "last_turn_id".to_string(),
                    Value::String(last_info.turn_id.clone()),
                );
                output.insert(
                    "last_turn_status".to_string(),
                    Value::String(last_info.status.clone()),
                );
                if last_info.status == "completed" {
                    if let Some(end_time) = last_info.end_time {
                        output.insert(
                            "last_task_completed_at".to_string(),
                            Value::String(utc_iso(end_time)),
                        );
                    }
                }
            } else if let Some(last_turn_id) = goal
                .last_turn_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                output.insert(
                    "last_turn_id".to_string(),
                    Value::String(last_turn_id.to_string()),
                );
            }
            if let Some(created_at) = goal.created_at {
                output.insert("created_at".to_string(), Value::String(utc_iso(created_at)));
            }
            if let Some(updated_at) = goal.updated_at {
                output.insert("updated_at".to_string(), Value::String(utc_iso(updated_at)));
            }
            let mut interruptions = goal.interruptions;
            interruptions.sort_by_key(|item| item.sequence);
            if !interruptions.is_empty() {
                let values = interruptions
                .into_iter()
                .map(|item| {
                    let mut output = obj!(
                        "turn_id" => Value::String(item.turn_id),
                        "interrupted_at" => item.interrupted_at.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
                    );
                    if let Some(termination) = item.termination.as_object() {
                        for key in ["type", "code", "limit_reached_at", "retry_at"] {
                            if let Some(value) = termination.get(key) {
                                output.insert(key.to_string(), value.clone());
                            }
                        }
                    }
                    if let Some(resumed_at) = item.resumed_at {
                        output.insert("resumed_at".to_string(), Value::String(utc_iso(resumed_at)));
                    }
                    Value::Object(output)
                })
                .collect();
                output.insert("interruptions".to_string(), Value::Array(values));
            }
            output_goals.push(Value::Object(output));
        }

        let mut output_rollbacks = Vec::new();
        rollbacks.sort_by_key(|item| item.sequence);
        for rollback in rollbacks {
            let mut output = obj!(
                "sequence" => Value::Number((rollback.sequence as u64).into()),
                "rolled_back_at" => rollback.time.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
                "num_turns" => Value::Number((rollback.num_turns as u64).into()),
            );
            if !rollback.removed_turn_ids.is_empty() {
                output.insert(
                    "removed_turn_ids".to_string(),
                    Value::Array(
                        rollback
                            .removed_turn_ids
                            .into_iter()
                            .map(Value::String)
                            .collect(),
                    ),
                );
            }
            output_rollbacks.push(Value::Object(output));
        }

        let effective_changes: Vec<ChangeInfo> = changes
            .into_iter()
            .filter(|item| {
                test_turn_effective(
                    item.turn_id.as_deref(),
                    has_turn_events,
                    &effective_turn_set,
                )
            })
            .collect();
        let effective_activities: Vec<Activity> = activities
            .into_iter()
            .filter(|item| {
                test_turn_effective(
                    item.turn_id.as_deref(),
                    has_turn_events,
                    &effective_turn_set,
                )
            })
            .collect();
        let output_changes = changes_for_output(&effective_changes, ACTIVITY_DETAIL);
        let output_activities = activities_for_output(&effective_activities, &cwd, ACTIVITY_DETAIL);

        let guardian_review = is_guardian_review_source(&source_value);
        let mut codex_info = Obj::new();
        if let Some(value) = originator {
            codex_info.insert("originator".to_string(), Value::String(value));
        }
        if let Some(value) = source_value
            .as_str()
            .filter(|value| !value.trim().is_empty())
        {
            codex_info.insert("source".to_string(), Value::String(value.to_string()));
        }
        if let Some(value) = cli_version {
            codex_info.insert("cli_version".to_string(), Value::String(value));
        }
        if let Some(value) = model_provider {
            codex_info.insert("model_provider".to_string(), Value::String(value));
        }
        codex_info.insert(
            "models".to_string(),
            Value::Array(models.into_iter().map(Value::String).collect()),
        );

        let mut workspace = obj!("cwd" => Value::String(cwd.clone()));
        if let Some(git) = git {
            let mut repository = Obj::new();
            for (output_key, source_key) in [
                ("url", "repository_url"),
                ("branch", "branch"),
                ("initial_commit", "commit_hash"),
            ] {
                if let Some(value) = value_str(git.get(source_key)) {
                    repository.insert(output_key.to_string(), Value::String(value.to_string()));
                }
            }
            if !repository.is_empty() {
                workspace.insert("repository".to_string(), Value::Object(repository));
            }
        }

        let latest_snapshot = match (context_window_tokens, latest_context_used_tokens) {
            (Some(window), Some(used)) => context_snapshot(window, used),
            _ => None,
        };
        let context = if latest_snapshot.is_some() || !context_compactions.is_empty() {
            let mut output = latest_snapshot
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            if !context_compactions.is_empty() {
                output.insert("compactions".to_string(), Value::Array(context_compactions));
            }
            Some(Value::Object(output))
        } else {
            None
        };

        let mut session = Session {
            session_id,
            root_session_id,
            thread_source,
            guardian_review,
            parent_thread_id,
            forked_from_id,
            agent_nickname,
            agent_path,
            agent_depth,
            agent_role,
            multi_agent_version,
            create_time,
            last_message_time,
            messages: message_outputs.messages,
            raw_messages: effective_messages,
            plans: message_outputs.plans,
            plan_updates: Vec::new(),
            goals: output_goals,
            goal_updates: Vec::new(),
            user_input_requests,
            agent_events: Vec::new(),
            delegation: None,
            reviews: message_outputs.reviews,
            memory_citations: message_outputs.memory_citations,
            writing_blocks: message_outputs.writing_blocks,
            ui_actions: message_outputs.ui_actions,
            ui_parse_warnings: message_outputs.ui_parse_warnings,
            rollbacks: output_rollbacks,
            turns: output_turns,
            codex: Value::Object(codex_info),
            workspace: Value::Object(workspace),
            context,
            usage: None,
            activity_detail: ACTIVITY_DETAIL.to_ascii_lowercase(),
            session_state: session_state.to_string(),
            changes: output_changes,
            activities: output_activities,
            execution_profile: None,
            data_quality: data_quality.to_value(),
            source_path: path.to_path_buf(),
            subagent_sessions: Vec::new(),
            internal_session_ids: Vec::new(),
        };

        finalize_streaming_enrichment(enrichment, &mut session, &registry)?;
        Ok(session)
    }
}

fn convert_codex_session_file_streaming(
    path: &Path,
    target_cwd_normalized: &str,
    attachment_root: &Path,
) -> Result<Option<Session>> {
    convert_codex_session_file_streaming_scoped(path, target_cwd_normalized, attachment_root, None)
}

// Some Codex Guardian rollouts contain only inspection bookkeeping with no
// exportable user/assistant messages. They are intentionally excluded from
// Handoff content; only this exact no-content Guardian condition may be
// skipped. Other parse failures remain fatal, including real subagents.
fn finish_streaming_session_or_skip_empty_guardian(
    builder: StreamingSessionBuilder,
    attachment_root: &Path,
) -> Result<Option<Session>> {
    let guardian = builder.thread_source == "guardian_review"
        || is_guardian_review_source(&builder.source_value);
    match builder.finish(attachment_root) {
        Ok(session) => Ok(Some(session)),
        Err(error)
            if guardian
                && error
                    .to_string()
                    .starts_with("No exportable messages found in ") =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn convert_codex_session_file_streaming_scoped(
    path: &Path,
    target_cwd_normalized: &str,
    attachment_root: &Path,
    selected_ids: Option<&HashSet<String>>,
) -> Result<Option<Session>> {
    // Keep the production fused path byte-for-byte simple when profiling is disabled.
    // The diagnostic branch below is intentionally isolated so hotspot instrumentation
    // cannot add per-record Instant calls to daily-use exports.
    if !HOTSPOT_ENABLED.load(AtomicOrdering::Relaxed) {
        let mut builder: Option<StreamingSessionBuilder> = None;
        let mut rejected = false;
        let data_quality = for_each_rollout_record(path, |line_number, record| {
            let record_type = record
                .as_object()
                .and_then(|map| value_str(map.get("type")));
            if record_type == Some("session_meta") && builder.is_none() {
                let session_meta = record
                    .as_object()
                    .and_then(|map| map.get("payload"))
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        anyhow!("session_meta payload is missing in {}", path.display())
                    })?;
                match StreamingSessionBuilder::from_session_meta(
                    session_meta,
                    path,
                    target_cwd_normalized,
                    selected_ids,
                )? {
                    Some(value) => builder = Some(value),
                    None => {
                        rejected = true;
                        return Ok(StreamControl::Stop);
                    }
                }
                return Ok(StreamControl::Continue);
            }
            if let Some(builder) = builder.as_mut() {
                builder.consume(line_number, &record)?;
            }
            Ok(StreamControl::Continue)
        })?;
        if rejected {
            return Ok(None);
        }
        let Some(mut builder) = builder else {
            return Ok(None);
        };
        builder.data_quality = data_quality;
        return finish_streaming_session_or_skip_empty_guardian(builder, attachment_root);
    }

    // Profiling path for the actual production fused streaming reducer.  r8 only
    // instrumented convert_codex_session_file_with_ingestion(), so production-shape
    // hotspot runs reported zero matched profiles.  This branch measures file/prefix
    // ingestion separately from reducer/enrichment callback work and times fused
    // finalization as a coarse phase.
    let total_start = Instant::now();
    let input_bytes = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    let mut builder: Option<StreamingSessionBuilder> = None;
    let mut rejected = false;
    let mut metadata_time = Duration::ZERO;
    let mut core_scan_time = Duration::ZERO;
    let mut enrichment_time = Duration::ZERO;
    let mut parsed_records = 0usize;
    let stream_start = Instant::now();
    let data_quality = for_each_rollout_record(path, |line_number, record| {
        parsed_records = parsed_records.saturating_add(1);
        let record_type = record
            .as_object()
            .and_then(|map| value_str(map.get("type")));
        if record_type == Some("session_meta") && builder.is_none() {
            let session_meta = record
                .as_object()
                .and_then(|map| map.get("payload"))
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("session_meta payload is missing in {}", path.display()))?;
            let started = Instant::now();
            let built = StreamingSessionBuilder::from_session_meta(
                session_meta,
                path,
                target_cwd_normalized,
                selected_ids,
            )?;
            metadata_time = metadata_time.saturating_add(started.elapsed());
            match built {
                Some(value) => builder = Some(value),
                None => {
                    rejected = true;
                    return Ok(StreamControl::Stop);
                }
            }
            return Ok(StreamControl::Continue);
        }
        if let Some(builder) = builder.as_mut() {
            let core_started = Instant::now();
            builder.attachments.observe(&record);
            builder.core.consume(line_number, &record, &builder.cwd)?;
            core_scan_time = core_scan_time.saturating_add(core_started.elapsed());

            let enrichment_started = Instant::now();
            builder.enrichment.observe(line_number, &record)?;
            enrichment_time = enrichment_time.saturating_add(enrichment_started.elapsed());
        }
        Ok(StreamControl::Continue)
    })?;
    let stream_total = stream_start.elapsed();
    if rejected {
        return Ok(None);
    }
    let Some(mut builder) = builder else {
        return Ok(None);
    };
    builder.data_quality = data_quality;
    let session_id = builder.session_id.clone();
    let callback_work = metadata_time
        .saturating_add(core_scan_time)
        .saturating_add(enrichment_time);
    let ingestion_time = stream_total.saturating_sub(callback_work);
    let finalize_started = Instant::now();
    let session = finish_streaming_session_or_skip_empty_guardian(builder, attachment_root)?;
    let finalize_time = finalize_started.elapsed();

    let profile = SessionHotspotProfile {
        session_id,
        path: path.to_path_buf(),
        input_bytes,
        retained_records: parsed_records,
        ingestion: ingestion_time,
        metadata: metadata_time,
        core_scan: core_scan_time,
        core_finalize: finalize_time,
        finalize_filter_sort: Duration::ZERO,
        finalize_messages: Duration::ZERO,
        finalize_turns_goals: Duration::ZERO,
        finalize_changes: Duration::ZERO,
        finalize_activities: Duration::ZERO,
        finalize_assembly: Duration::ZERO,
        enrichment: enrichment_time,
        total: total_start.elapsed(),
    };
    hotspot_profiles()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(profile);
    Ok(session)
}

fn convert_codex_session_file(
    path: &Path,
    target_cwd_normalized: &str,
    attachment_root: &Path,
) -> Result<Option<Session>> {
    convert_codex_session_file_with_ingestion(path, target_cwd_normalized, attachment_root, false)
}

fn convert_codex_session_file_with_ingestion(
    path: &Path,
    target_cwd_normalized: &str,
    attachment_root: &Path,
    legacy_ingestion: bool,
) -> Result<Option<Session>> {
    convert_codex_session_file_with_ingestion_scoped(
        path,
        target_cwd_normalized,
        attachment_root,
        legacy_ingestion,
        None,
    )
}

fn convert_codex_session_file_with_ingestion_scoped(
    path: &Path,
    target_cwd_normalized: &str,
    attachment_root: &Path,
    legacy_ingestion: bool,
    selected_ids: Option<&HashSet<String>>,
) -> Result<Option<Session>> {
    let hotspot_enabled = HOTSPOT_ENABLED.load(AtomicOrdering::Relaxed);
    let hotspot_total_start = hotspot_enabled.then(Instant::now);
    let hotspot_input_bytes = if hotspot_enabled {
        fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or_default()
    } else {
        0
    };
    let hotspot_ingestion_start = hotspot_enabled.then(Instant::now);

    // v0.2 streams a fixed-length snapshot of each rollout, drops irrelevant
    // records before allocation, and retains only reducer-visible payload fields.
    // Both lifecycle and enrichment passes share the compact immutable record set.
    let records = if legacy_ingestion {
        read_rollout_jsonl_legacy(path)?
    } else {
        read_rollout_jsonl(path)?
    };
    let hotspot_ingestion = duration_or_zero(hotspot_ingestion_start);
    let hotspot_metadata_start = hotspot_enabled.then(Instant::now);
    let Some(session_meta) = matching_session_meta(&records, target_cwd_normalized, selected_ids)
    else {
        return Ok(None);
    };
    let cwd = value_str(session_meta.get("cwd"))
        .ok_or_else(|| anyhow!("Session cwd not found in {}", path.display()))?
        .to_string();
    // Membership already passed the shared ProjectResolver or explicit legacy filter.
    let session_id = value_str(session_meta.get("id"))
        .or_else(|| value_str(session_meta.get("session_id")))
        .ok_or_else(|| anyhow!("Session ID not found in {}", path.display()))?
        .to_string();
    let create_time = parse_datetime_value(session_meta.get("timestamp"));
    let originator = value_str(session_meta.get("originator")).map(str::to_string);
    let source_value = session_meta.get("source").cloned().unwrap_or(Value::Null);
    let cli_version = value_str(session_meta.get("cli_version")).map(str::to_string);
    let model_provider = value_str(session_meta.get("model_provider")).map(str::to_string);
    let git = session_meta.get("git").and_then(Value::as_object).cloned();

    let thread_source = value_str(session_meta.get("thread_source"))
        .unwrap_or("user")
        .to_string();
    let root_session_id = value_str(session_meta.get("session_id"))
        .unwrap_or(&session_id)
        .to_string();
    let parent_thread_id = value_str(session_meta.get("parent_thread_id")).map(str::to_string);
    let forked_from_id = value_str(session_meta.get("forked_from_id")).map(str::to_string);
    let agent_nickname = value_str(session_meta.get("agent_nickname")).map(str::to_string);
    let agent_path = value_str(session_meta.get("agent_path")).map(str::to_string);
    let multi_agent_version =
        value_str(session_meta.get("multi_agent_version")).map(str::to_string);
    let mut agent_depth = None;
    let mut agent_role = None;
    if let Some(subagent) = source_value
        .as_object()
        .and_then(|source| source.get("subagent"))
        .and_then(Value::as_object)
        .and_then(|subagent| subagent.get("thread_spawn"))
        .and_then(Value::as_object)
    {
        agent_depth = int_value(subagent.get("depth"), Some(0));
        agent_role = value_str(subagent.get("agent_role")).map(str::to_string);
    }

    let hotspot_metadata = duration_or_zero(hotspot_metadata_start);
    let hotspot_core_scan_start = hotspot_enabled.then(Instant::now);

    let mut models = Vec::<String>::new();
    let mut model_set = HashSet::<String>::new();
    let mut messages = Vec::<RawMessage>::new();
    let mut plan_items = Vec::<PlanItem>::new();
    let mut goals = Vec::<GoalInfo>::new();
    let mut goal_indexes = HashMap::<String, usize>::new();
    let mut pending_goal_thread_id = None::<String>;
    let mut usage_limits = HashMap::<String, UsageLimit>::new();
    let mut rollbacks = Vec::<RollbackInfo>::new();
    let mut changes = Vec::<ChangeInfo>::new();
    let mut activities = Vec::<Activity>::new();
    let mut pending_user_turns = HashMap::<String, VecDeque<String>>::new();
    let mut command_calls = HashMap::<String, usize>::new();
    let mut pending_unparsed_command_calls = HashMap::<String, PendingUnparsedCommand>::new();
    let mut user_input_requests = Vec::<Value>::new();
    let mut pending_user_input_requests = HashMap::<String, usize>::new();
    let mut pending_web_calls = VecDeque::<PendingWebCall>::new();
    let mut turn_stack = Vec::<String>::new();
    let mut turn_info = HashMap::<String, TurnInfo>::new();
    let mut has_turn_events = false;
    let mut current_turn_id = None::<String>;
    let mut sequence = HashMap::<String, usize>::new();
    let mut context_window_tokens = None::<i64>;
    let mut latest_context_used_tokens = None::<i64>;
    let mut last_context_snapshot = None::<Value>;
    let mut context_compactions = Vec::<Value>::new();
    let mut pending_compaction = None::<PendingCompaction>;
    let subagent_boundaries = if thread_source == "subagent" {
        subagent_local_boundaries(&records)
    } else {
        SubagentLocalBoundaries::default()
    };
    let subagent_message_start_line = subagent_boundaries.message_start_line;

    for (line_number, record) in &records {
        let Some(record_map) = record.as_object() else {
            continue;
        };
        let record_type = value_str(record_map.get("type")).unwrap_or_default();
        let payload_empty = Obj::new();
        let payload = record_map
            .get("payload")
            .and_then(Value::as_object)
            .unwrap_or(&payload_empty);
        let payload_type = value_str(payload.get("type")).unwrap_or_default();
        let ignored = matches!(
            (record_type, payload_type),
            ("event_msg", "agent_reasoning")
                | ("response_item", "reasoning")
                | ("event_msg", "agent_message")
                | ("event_msg", "context_compacted")
        ) || matches!(record_type, "world_state" | "session_meta");
        if ignored {
            continue;
        }
        let record_time = parse_datetime_value(record_map.get("timestamp"));

        if record_type == "compacted" {
            if let Some(pending) = pending_compaction.take() {
                let mut unfinished = obj!(
                    "sequence" => Value::Number((pending.sequence as u64).into()),
                    "compacted_at" => pending.compacted_at.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
                );
                if let Some(before) = pending.before {
                    unfinished.insert("before".to_string(), before);
                }
                context_compactions.push(Value::Object(unfinished));
            }
            let compaction_sequence = next_counter(&mut sequence, "compaction");
            pending_compaction = Some(PendingCompaction {
                sequence: compaction_sequence,
                compacted_at: record_time,
                before: last_context_snapshot.clone(),
            });
            continue;
        }

        if record_type == "event_msg" && payload_type == "task_started" {
            if let Some(window) = int_value(payload.get("model_context_window"), Some(1)) {
                context_window_tokens = Some(window);
            }
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                has_turn_events = true;
                current_turn_id = Some(turn_id.to_string());
                let mode = value_str(payload.get("collaboration_mode_kind")).map(str::to_string);
                if !turn_info.contains_key(turn_id) {
                    let turn_sequence = next_counter(&mut sequence, "turn");
                    turn_info.insert(
                        turn_id.to_string(),
                        new_turn(
                            turn_id,
                            "in_progress",
                            turn_sequence,
                            record_time,
                            None,
                            None,
                            mode,
                            None,
                            None,
                        ),
                    );
                } else if let Some(mode) = mode {
                    turn_info.get_mut(turn_id).unwrap().mode = Some(mode);
                }
                if !turn_stack.iter().any(|item| item == turn_id) {
                    turn_stack.push(turn_id.to_string());
                }
                if let Some(thread_id) = pending_goal_thread_id.clone() {
                    if let Some(index) = goal_indexes.get(&thread_id).copied() {
                        let goal = &mut goals[index];
                        if goal
                            .created_turn_id
                            .as_deref()
                            .unwrap_or_default()
                            .trim()
                            .is_empty()
                        {
                            goal.created_turn_id = Some(turn_id.to_string());
                        }
                        goal.last_turn_id = Some(turn_id.to_string());
                        goal.awaiting_task_start = false;
                        pending_goal_thread_id = None;
                    }
                }
            }
            continue;
        }

        if record_type == "event_msg" && payload_type == "token_count" {
            if let Some(info) = payload.get("info").and_then(Value::as_object) {
                let parsed_window = int_value(info.get("model_context_window"), Some(1));
                let parsed_used = info
                    .get("last_token_usage")
                    .and_then(Value::as_object)
                    .and_then(|usage| int_value(usage.get("total_tokens"), Some(0)));
                if parsed_window.is_some() {
                    context_window_tokens = parsed_window;
                }
                if parsed_used.is_some() {
                    latest_context_used_tokens = parsed_used;
                }
                if let (Some(window), Some(used)) = (parsed_window, parsed_used) {
                    let snapshot = context_snapshot(window, used);
                    last_context_snapshot = snapshot.clone();
                    if let (Some(pending), Some(snapshot)) = (pending_compaction.take(), snapshot) {
                        let mut completed = obj!(
                            "sequence" => Value::Number((pending.sequence as u64).into()),
                            "compacted_at" => pending.compacted_at.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
                        );
                        if let Some(before) = pending.before {
                            completed.insert("before".to_string(), before);
                        }
                        completed.insert("after".to_string(), snapshot);
                        context_compactions.push(Value::Object(completed));
                    }
                }
                let rate_limits = payload
                    .get("rate_limits")
                    .and_then(Value::as_object)
                    .or_else(|| info.get("rate_limits").and_then(Value::as_object));
                if let (Some(rate_limits), Some(turn_id)) =
                    (rate_limits, current_turn_id.as_deref())
                {
                    if let Some(primary) = rate_limits.get("primary").and_then(Value::as_object) {
                        let limit = usage_limits.entry(turn_id.to_string()).or_default();
                        limit.limit_id = value_str(rate_limits.get("limit_id"))
                            .map(str::to_string)
                            .or(limit.limit_id.take());
                        if let Some(used) = float_value(primary.get("used_percent"), Some(0.0)) {
                            limit.used_percent =
                                Some(limit.used_percent.map_or(used, |current| current.max(used)));
                            if used >= 100.0 && limit.first_reached_at.is_none() {
                                limit.first_reached_at = record_time;
                            }
                        }
                        if let Some(reset) = parse_unix_seconds(primary.get("resets_at")) {
                            limit.reset_at = Some(reset);
                        }
                        if let Some(window) = int_value(primary.get("window_minutes"), Some(1)) {
                            limit.window_minutes = Some(window);
                        }
                    }
                }
            }
            continue;
        }

        if record_type == "event_msg" && payload_type == "thread_goal_updated" {
            let Some(goal_payload) = payload.get("goal").and_then(Value::as_object) else {
                continue;
            };
            let thread_id = value_str(payload.get("threadId"))
                .or_else(|| value_str(goal_payload.get("threadId")));
            let Some(thread_id) = thread_id else { continue };
            let active_turn = current_turn_id
                .as_deref()
                .filter(|turn_id| {
                    turn_info
                        .get(*turn_id)
                        .is_some_and(|turn| turn.status == "in_progress")
                })
                .map(str::to_string);
            let index = if let Some(index) = goal_indexes.get(thread_id).copied() {
                index
            } else {
                let goal_sequence = next_counter(&mut sequence, "goal");
                let index = goals.len();
                goals.push(GoalInfo {
                    thread_id: thread_id.to_string(),
                    created_turn_id: active_turn.clone(),
                    last_turn_id: active_turn.clone(),
                    objective: None,
                    reported_status: None,
                    status_source: "thread_goal_updated".to_string(),
                    created_at: None,
                    updated_at: None,
                    awaiting_task_start: active_turn.is_none(),
                    interruptions: Vec::new(),
                    sequence: goal_sequence,
                });
                goal_indexes.insert(thread_id.to_string(), index);
                index
            };
            let goal = &mut goals[index];
            let created_at = parse_unix_seconds(goal_payload.get("createdAt"));
            if let (Some(created_at), Some(record_time), Some(existing)) =
                (created_at, record_time, goal.created_at)
            {
                if created_at != existing && (record_time - created_at).abs() <= 2_000_000 {
                    goal.created_turn_id = active_turn.clone();
                    goal.last_turn_id = active_turn.clone();
                    goal.objective = None;
                    goal.reported_status = None;
                    goal.created_at = Some(created_at);
                    goal.updated_at = None;
                    goal.awaiting_task_start = active_turn.is_none();
                    goal.interruptions.clear();
                }
            }
            if let Some(active_turn) = active_turn {
                if goal
                    .created_turn_id
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                {
                    goal.created_turn_id = Some(active_turn.clone());
                }
                goal.last_turn_id = Some(active_turn);
                goal.awaiting_task_start = false;
                if pending_goal_thread_id.as_deref() == Some(thread_id) {
                    pending_goal_thread_id = None;
                }
            } else {
                pending_goal_thread_id = Some(thread_id.to_string());
                goal.awaiting_task_start = true;
            }
            if let Some(objective) = nonblank_raw_str(goal_payload.get("objective")) {
                goal.objective = Some(objective.to_string());
            }
            if let Some(status) = value_str(goal_payload.get("status")) {
                let status_lower = status.to_ascii_lowercase();
                goal.reported_status = Some(status_lower.clone());
                if status_lower == "active" {
                    if let Some(record_time) = record_time {
                        if let Some(interruption) =
                            goal.interruptions.iter_mut().rev().find(|item| {
                                item.resumed_at.is_none()
                                    && item.interrupted_at.is_some_and(|at| record_time > at)
                            })
                        {
                            interruption.resumed_at = Some(record_time);
                        }
                    }
                }
            }
            if let Some(created_at) = created_at {
                goal.created_at = Some(created_at);
            }
            goal.updated_at = parse_unix_seconds(goal_payload.get("updatedAt"))
                .or(record_time)
                .or(goal.updated_at);
            continue;
        }

        if record_type == "event_msg" && payload_type == "item_completed" {
            if let Some(item) = payload.get("item").and_then(Value::as_object) {
                if value_str(item.get("type")) == Some("Plan") {
                    if let (Some(text), Some(turn_id)) = (
                        nonblank_raw_str(item.get("text")),
                        value_str(payload.get("turn_id")),
                    ) {
                        let plan_sequence = next_counter(&mut sequence, "plan");
                        plan_items.push(PlanItem {
                            turn_id: turn_id.to_string(),
                            source_item_id: value_str(item.get("id")).map(str::to_string),
                            text: text.to_string(),
                            time: record_time,
                            sequence: plan_sequence,
                        });
                        continue;
                    }
                }
            }
        }

        if record_type == "event_msg" && payload_type == "task_complete" {
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                has_turn_events = true;
                let failed = payload.get("error").is_some_and(|value| !value.is_null());
                let start_time = parse_unix_seconds(payload.get("started_at"));
                let payload_completed_at = parse_unix_seconds(payload.get("completed_at"));
                let effective_start = turn_info
                    .get(turn_id)
                    .and_then(|info| info.start_time)
                    .or(start_time);
                let (end_time, timestamp_conflict) = resolve_task_complete_end_time(
                    effective_start,
                    record_time,
                    payload_completed_at,
                );
                let duration = int_value(payload.get("duration_ms"), Some(0));
                let first_token = int_value(payload.get("time_to_first_token_ms"), Some(0));
                if !turn_info.contains_key(turn_id) {
                    let turn_sequence = next_counter(&mut sequence, "turn");
                    let mut info = new_turn(
                        turn_id,
                        if failed { "failed" } else { "completed" },
                        turn_sequence,
                        start_time,
                        end_time,
                        None,
                        None,
                        duration,
                        first_token,
                    );
                    info.timestamp_conflict = timestamp_conflict;
                    turn_info.insert(turn_id.to_string(), info);
                    if !turn_stack.iter().any(|item| item == turn_id) {
                        turn_stack.push(turn_id.to_string());
                    }
                } else {
                    let info = turn_info.get_mut(turn_id).unwrap();
                    info.status = if failed { "failed" } else { "completed" }.to_string();
                    info.end_time = end_time;
                    if timestamp_conflict.is_some() {
                        info.timestamp_conflict = timestamp_conflict;
                    }
                    if info.start_time.is_none() {
                        info.start_time = start_time;
                    }
                    if duration.is_some() {
                        info.duration_ms = duration;
                    }
                    if first_token.is_some() {
                        info.time_to_first_token_ms = first_token;
                    }
                }
                if failed {
                    let error_code = payload
                        .get("error")
                        .and_then(Value::as_object)
                        .and_then(|error| value_str(error.get("codex_error_info")))
                        .map(str::to_string);
                    let termination_type = if error_code.as_deref() == Some("usage_limit_exceeded")
                    {
                        "usage_limit"
                    } else {
                        "runtime_error"
                    };
                    let mut termination =
                        obj!("type" => Value::String(termination_type.to_string()));
                    if let Some(code) = error_code {
                        termination.insert("code".to_string(), Value::String(code));
                    }
                    if termination_type == "usage_limit" {
                        if let Some(usage) = usage_limits.get(turn_id) {
                            if let Some(time) = usage.first_reached_at {
                                termination.insert(
                                    "limit_reached_at".to_string(),
                                    Value::String(utc_iso(time)),
                                );
                            }
                            if let Some(time) = usage.reset_at {
                                termination
                                    .insert("retry_at".to_string(), Value::String(utc_iso(time)));
                            }
                        }
                    }
                    let termination_value = Value::Object(termination.clone());
                    turn_info.get_mut(turn_id).unwrap().termination =
                        Some(termination_value.clone());
                    for goal in &mut goals {
                        if goal.last_turn_id.as_deref() == Some(turn_id)
                            && goal.reported_status.as_deref() == Some("active")
                        {
                            goal.interruptions.push(GoalInterruption {
                                sequence: goal.interruptions.len() + 1,
                                turn_id: turn_id.to_string(),
                                interrupted_at: end_time,
                                termination: termination_value.clone(),
                                resumed_at: None,
                            });
                        }
                    }
                }
            }
            continue;
        }

        if record_type == "event_msg" && payload_type == "turn_aborted" {
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                has_turn_events = true;
                let duration = int_value(payload.get("duration_ms"), Some(0));
                if !turn_info.contains_key(turn_id) {
                    let turn_sequence = next_counter(&mut sequence, "turn");
                    turn_info.insert(
                        turn_id.to_string(),
                        new_turn(
                            turn_id,
                            "aborted",
                            turn_sequence,
                            None,
                            record_time,
                            raw_str(payload.get("reason")).map(str::to_string),
                            None,
                            duration,
                            None,
                        ),
                    );
                    if !turn_stack.iter().any(|item| item == turn_id) {
                        turn_stack.push(turn_id.to_string());
                    }
                } else {
                    let info = turn_info.get_mut(turn_id).unwrap();
                    info.status = "aborted".to_string();
                    info.end_time = record_time;
                    info.abort_reason = raw_str(payload.get("reason")).map(str::to_string);
                    if duration.is_some() {
                        info.duration_ms = duration;
                    }
                }
            }
            continue;
        }

        if record_type == "event_msg" && payload_type == "thread_rolled_back" {
            let rollback_count = int_value(payload.get("num_turns"), Some(0)).unwrap_or(0) as usize;
            let mut removed = Vec::new();
            for _ in 0..rollback_count {
                let Some(rolled) = turn_stack.pop() else {
                    break;
                };
                removed.push(rolled.clone());
                if let Some(info) = turn_info.get_mut(&rolled) {
                    info.status = "rolled_back".to_string();
                    info.end_time = record_time;
                }
                pending_web_calls.retain(|call| call.turn_id.as_deref() != Some(&rolled));
                command_calls.retain(|_, index| {
                    activities
                        .get(*index)
                        .and_then(|activity| activity.turn_id.as_deref())
                        != Some(&rolled)
                });
            }
            let rollback_sequence = next_counter(&mut sequence, "rollback");
            rollbacks.push(RollbackInfo {
                sequence: rollback_sequence,
                time: record_time,
                num_turns: rollback_count,
                removed_turn_ids: removed,
            });
            current_turn_id = turn_stack.last().cloned();
            continue;
        }

        if record_type == "turn_context" {
            if let Some(model) = value_str(payload.get("model")) {
                if model_set.insert(model.to_string()) {
                    models.push(model.to_string());
                }
            }
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                current_turn_id = Some(turn_id.to_string());
            }
            continue;
        }

        if record_type == "event_msg" && payload_type == "thread_settings_applied" {
            if let Some(model) = payload
                .get("thread_settings")
                .and_then(Value::as_object)
                .and_then(|settings| value_str(settings.get("model")))
            {
                if model_set.insert(model.to_string()) {
                    models.push(model.to_string());
                }
            }
            continue;
        }

        if record_type == "response_item" && payload_type == "message" {
            let role = value_str(payload.get("role")).unwrap_or_default();
            let content = get_message_text(payload.get("content"));
            let turn_id = turn_id_from_message_payload(payload).or_else(|| current_turn_id.clone());
            if role == "user" {
                if let Some(content) = content.filter(|content| !content.trim().is_empty()) {
                    if let Some(turn_id) = turn_id {
                        pending_user_turns
                            .entry(text_key(Some(&content)))
                            .or_default()
                            .push_back(turn_id);
                    }
                }
                continue;
            }
            if role == "assistant" {
                let phase = value_str(payload.get("phase"));
                if let (Some(content), Some(phase)) =
                    (content.filter(|content| !content.trim().is_empty()), phase)
                {
                    if matches!(phase, "commentary" | "final_answer") {
                        let message_sequence = next_counter(&mut sequence, "message");
                        messages.push(RawMessage {
                            role: "assistant".to_string(),
                            phase: Some(phase.to_string()),
                            turn_id,
                            content,
                            time: record_time,
                            sequence: message_sequence,
                            source_line: *line_number,
                        });
                    }
                }
            }
            continue;
        }

        if record_type == "event_msg" && payload_type == "user_message" {
            if let Some(content) =
                raw_str(payload.get("message")).filter(|content| !content.trim().is_empty())
            {
                let message_sequence = next_counter(&mut sequence, "message");
                let key = text_key(Some(content));
                let mut turn_id = pending_user_turns
                    .get_mut(&key)
                    .and_then(VecDeque::pop_front);
                if pending_user_turns.get(&key).is_some_and(VecDeque::is_empty) {
                    pending_user_turns.remove(&key);
                }
                if turn_id.is_none() {
                    turn_id = current_turn_id.clone();
                }
                messages.push(RawMessage {
                    role: "user".to_string(),
                    phase: None,
                    turn_id,
                    content: content.trim().to_string(),
                    time: record_time,
                    sequence: message_sequence,
                    source_line: *line_number,
                });
            }
            continue;
        }

        if record_type == "response_item" && payload_type == "function_call" {
            if matches!(
                value_str(payload.get("name")),
                Some("request_user_input" | "request_user_input_async")
            ) {
                if let Some((call_id, request)) =
                    build_user_input_request(payload, record_time, current_turn_id.as_deref())
                {
                    let index = user_input_requests.len();
                    user_input_requests.push(request);
                    pending_user_input_requests.insert(call_id, index);
                }
                continue;
            }
            if value_str(payload.get("name")) != Some("shell_command") {
                continue;
            }
            let arguments = json_object(payload.get("arguments"));
            let turn_id = turn_id_from_tool_payload(payload, current_turn_id.as_deref());
            let call_id = value_str(payload.get("call_id")).map(str::to_string);
            let Some(execution) = direct_shell_execution(&arguments) else {
                if let Some(call_id) = call_id {
                    let sequence = next_counter(&mut sequence, "activity");
                    pending_unparsed_command_calls.insert(
                        call_id,
                        PendingUnparsedCommand {
                            turn_id,
                            time: record_time,
                            sequence,
                            parse_warnings: vec!["Shell command arguments could not be structurally resolved before execution.".to_string()],
                        },
                    );
                }
                continue;
            };
            let activity_sequence = next_counter(&mut sequence, "activity");
            let mut activity = Activity::new("command", turn_id, record_time, activity_sequence);
            activity.source_call_id = call_id.clone();
            activity.executions = vec![execution];
            activity.invocation_count = 1;
            let index = activities.len();
            activities.push(activity);
            if let Some(call_id) = call_id {
                command_calls.insert(call_id, index);
            }
            continue;
        }

        if record_type == "response_item" && payload_type == "function_call_output" {
            if let Some(call_id) = value_str(payload.get("call_id")) {
                if let Some(index) = pending_user_input_requests.remove(call_id) {
                    if let Some(request) = user_input_requests.get_mut(index) {
                        complete_user_input_request(request, payload, record_time);
                    }
                    continue;
                }
                if let Some(index) = command_calls.remove(call_id) {
                    if let Some(activity) = activities.get_mut(index) {
                        complete_command_activity_from_payload(activity, payload, record_time);
                    }
                } else if let Some(pending) = pending_unparsed_command_calls.remove(call_id) {
                    let (exit_codes, truncated, failure_status, failure_diagnostic) =
                        command_output_summary(payload);
                    if failure_status.as_deref() == Some("failed") {
                        let mut activity = Activity::new(
                            "command",
                            pending.turn_id,
                            pending.time,
                            pending.sequence,
                        );
                        activity.source_call_id = Some(call_id.to_string());
                        activity.parse_warnings = pending.parse_warnings;
                        activity.failure_status = failure_status;
                        activity.failure_diagnostic = failure_diagnostic;
                        complete_command_activity(&mut activity, exit_codes, truncated);
                        activity.end_time = record_time;
                        activities.push(activity);
                    }
                }
            }
            continue;
        }

        if record_type == "response_item" && payload_type == "custom_tool_call" {
            let Some(tool_input) =
                raw_str(payload.get("input")).filter(|input| !input.trim().is_empty())
            else {
                continue;
            };
            let call_id = value_str(payload.get("call_id")).map(str::to_string);
            let turn_id = turn_id_from_tool_payload(payload, current_turn_id.as_deref());
            let mut executions = Vec::new();
            let mut warnings = Vec::new();
            if tool_input.contains("tools.shell_command") {
                let (values, value_warnings) = javascript_command_data(tool_input);
                executions.extend(values);
                warnings.extend(value_warnings);
            }
            if tool_input.contains("tools.exec_command") {
                let (values, value_warnings) = javascript_exec_command_data(tool_input);
                executions.extend(values);
                warnings.extend(value_warnings);
            }
            let commandish = tool_input.contains("tools.shell_command")
                || tool_input.contains("tools.exec_command");
            if !executions.is_empty() {
                let activity_sequence = next_counter(&mut sequence, "activity");
                let mut activity =
                    Activity::new("command", turn_id.clone(), record_time, activity_sequence);
                activity.source_call_id = call_id.clone();
                activity.executions = executions;
                activity.invocation_count = activity.executions.len();
                for warning in warnings {
                    add_unique_warning(&mut activity.parse_warnings, &warning);
                }
                let index = activities.len();
                activities.push(activity);
                if let Some(call_id) = call_id.clone() {
                    command_calls.insert(call_id, index);
                }
            } else if commandish {
                if let Some(call_id) = call_id.clone() {
                    let activity_sequence = next_counter(&mut sequence, "activity");
                    let mut parse_warnings = warnings;
                    add_unique_warning(
                        &mut parse_warnings,
                        "Command wrapper could not be structurally resolved before execution.",
                    );
                    pending_unparsed_command_calls.insert(
                        call_id,
                        PendingUnparsedCommand {
                            turn_id: turn_id.clone(),
                            time: record_time,
                            sequence: activity_sequence,
                            parse_warnings,
                        },
                    );
                }
            }
            if tool_input.contains("tools.web__run") {
                pending_web_calls.push_back(PendingWebCall {
                    turn_id,
                    input: tool_input.to_string(),
                });
            }
            continue;
        }

        if record_type == "response_item" && payload_type == "custom_tool_call_output" {
            if let Some(call_id) = value_str(payload.get("call_id")) {
                if let Some(index) = command_calls.remove(call_id) {
                    if let Some(activity) = activities.get_mut(index) {
                        complete_command_activity_from_payload(activity, payload, record_time);
                    }
                } else if let Some(pending) = pending_unparsed_command_calls.remove(call_id) {
                    let (exit_codes, truncated, failure_status, failure_diagnostic) =
                        command_output_summary(payload);
                    if failure_status.as_deref() == Some("failed") {
                        let mut activity = Activity::new(
                            "command",
                            pending.turn_id,
                            pending.time,
                            pending.sequence,
                        );
                        activity.source_call_id = Some(call_id.to_string());
                        activity.parse_warnings = pending.parse_warnings;
                        activity.failure_status = failure_status;
                        activity.failure_diagnostic = failure_diagnostic;
                        complete_command_activity(&mut activity, exit_codes, truncated);
                        activity.end_time = record_time;
                        activities.push(activity);
                    }
                }
            }
            continue;
        }

        if record_type == "event_msg" && payload_type == "web_search_end" {
            let web_call = pending_web_calls.pop_front();
            let mut turn_id = current_turn_id.clone();
            let mut web_input = String::new();
            if let Some(call) = web_call {
                turn_id = call.turn_id;
                web_input = call.input;
            }
            let action_object = payload.get("action").and_then(Value::as_object);
            let mut action_type = action_object
                .and_then(|action| value_str(action.get("type")))
                .unwrap_or_default()
                .to_string();
            let activity_type = if action_type == "search" {
                "web_search"
            } else if action_type == "find_in_page" {
                "web_find"
            } else if !web_input.is_empty() && input_has_js_property(&web_input, "open") {
                action_type = "open".to_string();
                "web_open"
            } else if !web_input.is_empty() && input_has_js_property(&web_input, "click") {
                action_type = "click".to_string();
                "web_click"
            } else {
                "web_activity"
            };
            let queries = action_object
                .and_then(|action| action.get("queries"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let references = if web_input.is_empty() {
                Vec::new()
            } else {
                javascript_property_string_values(&web_input, "ref_id")
            };
            let urls = if web_input.is_empty() {
                Vec::new()
            } else {
                javascript_property_string_values(&web_input, "url")
            };
            let activity_sequence = next_counter(&mut sequence, "activity");
            let mut activity =
                Activity::new(activity_type, turn_id, record_time, activity_sequence);
            activity.action = Some(action_type);
            activity.queries = queries;
            activity.query = raw_str(payload.get("query")).map(str::to_string);
            activity.pattern = action_object
                .and_then(|action| raw_str(action.get("pattern")))
                .map(str::to_string);
            activity.references = references;
            activity.urls = urls;
            activities.push(activity);
            continue;
        }

        if record_type == "event_msg" && payload_type == "patch_apply_end" {
            let Some(change_map) = payload.get("changes").and_then(Value::as_object) else {
                continue;
            };
            let turn_id = value_str(payload.get("turn_id"))
                .map(str::to_string)
                .or_else(|| current_turn_id.clone());
            let success = bool_value(payload.get("success")).unwrap_or(false);
            for (item_path, detail) in change_map {
                let operation = detail
                    .as_object()
                    .and_then(|detail| value_str(detail.get("type")))
                    .map(str::to_string);
                let relative = relative_workspace_path(&cwd, item_path);
                let change_sequence = next_counter(&mut sequence, "change");
                changes.push(ChangeInfo {
                    turn_id: turn_id.clone(),
                    time: record_time,
                    sequence: change_sequence,
                    path: relative.clone(),
                    operation: operation.clone(),
                    success,
                });
                let activity_sequence = next_counter(&mut sequence, "activity");
                let mut activity =
                    Activity::new("file_edit", turn_id.clone(), record_time, activity_sequence);
                activity.path = Some(relative);
                activity.operation = operation;
                activity.success = Some(success);
                activities.push(activity);
            }
        }
    }

    let hotspot_core_scan = duration_or_zero(hotspot_core_scan_start);
    let hotspot_core_finalize_start = hotspot_enabled.then(Instant::now);
    let hotspot_finalize_filter_sort_start = hotspot_enabled.then(Instant::now);

    if let Some(pending) = pending_compaction.take() {
        let mut unfinished = obj!(
            "sequence" => Value::Number((pending.sequence as u64).into()),
            "compacted_at" => pending.compacted_at.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
        );
        if let Some(before) = pending.before {
            unfinished.insert("before".to_string(), before);
        }
        context_compactions.push(Value::Object(unfinished));
    }

    let effective_turn_set: HashSet<String> = turn_stack.iter().cloned().collect();
    let mut effective_messages: Vec<RawMessage> = messages
        .into_iter()
        .filter(|message| {
            test_turn_effective(
                message.turn_id.as_deref(),
                has_turn_events,
                &effective_turn_set,
            ) && subagent_message_start_line
                .is_none_or(|start_line| message.source_line >= start_line)
        })
        .collect();
    let activity_only_subagent =
        thread_source == "subagent" && subagent_boundaries.task_start_line.is_some();
    if effective_messages.is_empty() && !activity_only_subagent {
        if thread_source == "guardian_review" || is_guardian_review_source(&source_value) {
            return Ok(None);
        }
        bail!("No exportable messages found in {}", path.display());
    }
    effective_messages.sort_by_key(|item| (item.time.unwrap_or(MAX_TS), item.sequence));
    let effective_plan_items: Vec<PlanItem> = plan_items
        .into_iter()
        .filter(|item| {
            test_turn_effective(Some(&item.turn_id), has_turn_events, &effective_turn_set)
        })
        .collect();
    let effective_goals: Vec<GoalInfo> = goals
        .into_iter()
        .filter(|goal| goal_is_effective(goal, has_turn_events, &effective_turn_set))
        .collect();
    let hotspot_finalize_filter_sort = duration_or_zero(hotspot_finalize_filter_sort_start);
    let hotspot_finalize_messages_start = hotspot_enabled.then(Instant::now);
    let message_outputs = build_output_messages(&effective_messages, &effective_plan_items);
    let activity_only_update_time = subagent_boundaries
        .task_start_line
        .and_then(|start_line| latest_record_time_from_line(&records, start_line))
        .or(create_time);
    let last_message_time = message_outputs
        .last_message_time
        .or_else(|| {
            activity_only_subagent
                .then_some(activity_only_update_time)
                .flatten()
        })
        .ok_or_else(|| {
            anyhow!(
                "No valid message/activity timestamp found in {}",
                path.display()
            )
        })?;
    let hotspot_finalize_messages = duration_or_zero(hotspot_finalize_messages_start);
    let hotspot_finalize_turns_goals_start = hotspot_enabled.then(Instant::now);

    let mut output_turns = Vec::<Value>::new();
    for turn_id in &turn_stack {
        let Some(info) = turn_info.get(turn_id) else {
            continue;
        };
        let mut output = obj!(
            "turn_id" => Value::String(turn_id.clone()),
            "status" => Value::String(info.status.clone()),
            "start_time" => info.start_time.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
        );
        if let Some(end_time) = info.end_time {
            output.insert("end_time".to_string(), Value::String(utc_iso(end_time)));
        }
        if let Some(reason) = info
            .abort_reason
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            output.insert(
                "abort_reason".to_string(),
                Value::String(reason.to_string()),
            );
        }
        if let Some(mode) = info
            .mode
            .as_deref()
            .filter(|value| !value.trim().is_empty() && *value != "default")
        {
            output.insert("mode".to_string(), Value::String(mode.to_string()));
        }
        if matches!(info.status.as_str(), "aborted" | "failed") {
            let duration = info
                .duration_ms
                .or_else(|| match (info.start_time, info.end_time) {
                    (Some(start), Some(end)) => Some(((end - start).max(0) + 500) / 1_000),
                    _ => None,
                });
            if let Some(duration) = duration {
                output.insert("duration_ms".to_string(), Value::Number(duration.into()));
            }
        }
        if info.status == "failed" {
            if let Some(first_token) = info.time_to_first_token_ms {
                output.insert(
                    "time_to_first_token_ms".to_string(),
                    Value::Number(first_token.into()),
                );
            }
        }
        if let Some(termination) = &info.termination {
            output.insert("termination".to_string(), termination.clone());
        }
        if let Some(conflict) = &info.timestamp_conflict {
            output.insert(
                "data_quality".to_string(),
                Value::Object(obj!("timestamp_conflict" => conflict.clone())),
            );
        }
        output_turns.push(Value::Object(output));
    }
    let session_state = if output_turns.iter().any(|turn| {
        turn.as_object()
            .and_then(|turn| value_str(turn.get("status")))
            == Some("in_progress")
    }) {
        "active_at_export"
    } else {
        "idle"
    };

    let mut sorted_goals = effective_goals;
    sorted_goals.sort_by_key(|goal| goal.sequence);
    let mut output_goals = Vec::<Value>::new();
    for (index, goal) in sorted_goals.into_iter().enumerate() {
        let mut output = obj!(
            "id" => Value::String(format!("goal{:02}", index + 1)),
            "thread_id" => Value::String(goal.thread_id.clone()),
        );
        if let Some(turn_id) = goal
            .created_turn_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            output.insert(
                "created_turn_id".to_string(),
                Value::String(turn_id.to_string()),
            );
        }
        if let Some(objective) = goal
            .objective
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            output.insert(
                "objective".to_string(),
                Value::String(objective.to_string()),
            );
        }
        let reported = goal
            .reported_status
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        output.insert(
            "reported_status".to_string(),
            Value::String(reported.clone()),
        );
        output.insert(
            "status_source".to_string(),
            Value::String(goal.status_source.clone()),
        );
        let last_info = goal
            .last_turn_id
            .as_deref()
            .and_then(|id| turn_info.get(id));
        let execution_state = if goal.awaiting_task_start {
            "idle"
        } else if let Some(last_info) = last_info {
            match last_info.status.as_str() {
                "in_progress" => "running",
                "completed" | "rolled_back" => "idle",
                "failed" | "aborted" => "interrupted",
                _ => "unknown",
            }
        } else if matches!(reported.as_str(), "completed" | "cancelled" | "failed") {
            "idle"
        } else {
            "unknown"
        };
        output.insert(
            "execution_state".to_string(),
            Value::String(execution_state.to_string()),
        );
        if let Some(last_info) = last_info {
            output.insert(
                "last_turn_id".to_string(),
                Value::String(last_info.turn_id.clone()),
            );
            output.insert(
                "last_turn_status".to_string(),
                Value::String(last_info.status.clone()),
            );
            if last_info.status == "completed" {
                if let Some(end_time) = last_info.end_time {
                    output.insert(
                        "last_task_completed_at".to_string(),
                        Value::String(utc_iso(end_time)),
                    );
                }
            }
        } else if let Some(last_turn_id) = goal
            .last_turn_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            output.insert(
                "last_turn_id".to_string(),
                Value::String(last_turn_id.to_string()),
            );
        }
        if let Some(created_at) = goal.created_at {
            output.insert("created_at".to_string(), Value::String(utc_iso(created_at)));
        }
        if let Some(updated_at) = goal.updated_at {
            output.insert("updated_at".to_string(), Value::String(utc_iso(updated_at)));
        }
        let mut interruptions = goal.interruptions;
        interruptions.sort_by_key(|item| item.sequence);
        if !interruptions.is_empty() {
            let values = interruptions
                .into_iter()
                .map(|item| {
                    let mut output = obj!(
                        "turn_id" => Value::String(item.turn_id),
                        "interrupted_at" => item.interrupted_at.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
                    );
                    if let Some(termination) = item.termination.as_object() {
                        for key in ["type", "code", "limit_reached_at", "retry_at"] {
                            if let Some(value) = termination.get(key) {
                                output.insert(key.to_string(), value.clone());
                            }
                        }
                    }
                    if let Some(resumed_at) = item.resumed_at {
                        output.insert("resumed_at".to_string(), Value::String(utc_iso(resumed_at)));
                    }
                    Value::Object(output)
                })
                .collect();
            output.insert("interruptions".to_string(), Value::Array(values));
        }
        output_goals.push(Value::Object(output));
    }

    let mut output_rollbacks = Vec::new();
    rollbacks.sort_by_key(|item| item.sequence);
    for rollback in rollbacks {
        let mut output = obj!(
            "sequence" => Value::Number((rollback.sequence as u64).into()),
            "rolled_back_at" => rollback.time.map(utc_iso).map(Value::String).unwrap_or(Value::Null),
            "num_turns" => Value::Number((rollback.num_turns as u64).into()),
        );
        if !rollback.removed_turn_ids.is_empty() {
            output.insert(
                "removed_turn_ids".to_string(),
                Value::Array(
                    rollback
                        .removed_turn_ids
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        output_rollbacks.push(Value::Object(output));
    }
    let hotspot_finalize_turns_goals = duration_or_zero(hotspot_finalize_turns_goals_start);
    let hotspot_finalize_changes_start = hotspot_enabled.then(Instant::now);

    let effective_changes: Vec<ChangeInfo> = changes
        .into_iter()
        .filter(|item| {
            test_turn_effective(
                item.turn_id.as_deref(),
                has_turn_events,
                &effective_turn_set,
            )
        })
        .collect();
    let effective_activities: Vec<Activity> = activities
        .into_iter()
        .filter(|item| {
            test_turn_effective(
                item.turn_id.as_deref(),
                has_turn_events,
                &effective_turn_set,
            )
        })
        .collect();
    let output_changes = changes_for_output(&effective_changes, ACTIVITY_DETAIL);
    let hotspot_finalize_changes = duration_or_zero(hotspot_finalize_changes_start);
    let hotspot_finalize_activities_start = hotspot_enabled.then(Instant::now);
    let output_activities = activities_for_output(&effective_activities, &cwd, ACTIVITY_DETAIL);
    let hotspot_finalize_activities = duration_or_zero(hotspot_finalize_activities_start);
    let hotspot_finalize_assembly_start = hotspot_enabled.then(Instant::now);

    let guardian_review = is_guardian_review_source(&source_value);
    let mut codex_info = Obj::new();
    if let Some(value) = originator {
        codex_info.insert("originator".to_string(), Value::String(value));
    }
    if let Some(value) = source_value
        .as_str()
        .filter(|value| !value.trim().is_empty())
    {
        codex_info.insert("source".to_string(), Value::String(value.to_string()));
    }
    if let Some(value) = cli_version {
        codex_info.insert("cli_version".to_string(), Value::String(value));
    }
    if let Some(value) = model_provider {
        codex_info.insert("model_provider".to_string(), Value::String(value));
    }
    codex_info.insert(
        "models".to_string(),
        Value::Array(models.into_iter().map(Value::String).collect()),
    );

    let mut workspace = obj!("cwd" => Value::String(cwd.clone()));
    if let Some(git) = git {
        let mut repository = Obj::new();
        for (output_key, source_key) in [
            ("url", "repository_url"),
            ("branch", "branch"),
            ("initial_commit", "commit_hash"),
        ] {
            if let Some(value) = value_str(git.get(source_key)) {
                repository.insert(output_key.to_string(), Value::String(value.to_string()));
            }
        }
        if !repository.is_empty() {
            workspace.insert("repository".to_string(), Value::Object(repository));
        }
    }

    let latest_snapshot = match (context_window_tokens, latest_context_used_tokens) {
        (Some(window), Some(used)) => context_snapshot(window, used),
        _ => None,
    };
    let context = if latest_snapshot.is_some() || !context_compactions.is_empty() {
        let mut output = latest_snapshot
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        if !context_compactions.is_empty() {
            output.insert("compactions".to_string(), Value::Array(context_compactions));
        }
        Some(Value::Object(output))
    } else {
        None
    };

    let mut session = Session {
        session_id,
        root_session_id,
        thread_source,
        guardian_review,
        parent_thread_id,
        forked_from_id,
        agent_nickname,
        agent_path,
        agent_depth,
        agent_role,
        multi_agent_version,
        create_time,
        last_message_time,
        messages: message_outputs.messages,
        raw_messages: effective_messages,
        plans: message_outputs.plans,
        plan_updates: Vec::new(),
        goals: output_goals,
        goal_updates: Vec::new(),
        user_input_requests,
        agent_events: Vec::new(),
        delegation: None,
        reviews: message_outputs.reviews,
        memory_citations: message_outputs.memory_citations,
        writing_blocks: message_outputs.writing_blocks,
        ui_actions: message_outputs.ui_actions,
        ui_parse_warnings: message_outputs.ui_parse_warnings,
        rollbacks: output_rollbacks,
        turns: output_turns,
        codex: Value::Object(codex_info),
        workspace: Value::Object(workspace),
        context,
        usage: None,
        activity_detail: ACTIVITY_DETAIL.to_ascii_lowercase(),
        session_state: session_state.to_string(),
        changes: output_changes,
        activities: output_activities,
        execution_profile: None,
        data_quality: source_data_quality_from_path(path)?.to_value(),
        source_path: path.to_path_buf(),
        subagent_sessions: Vec::new(),
        internal_session_ids: Vec::new(),
    };
    let hotspot_finalize_assembly = duration_or_zero(hotspot_finalize_assembly_start);
    let hotspot_core_finalize = duration_or_zero(hotspot_core_finalize_start);
    let hotspot_enrichment_start = hotspot_enabled.then(Instant::now);
    enrich_session_from_records(&records, &mut session, attachment_root)?;
    let hotspot_enrichment = duration_or_zero(hotspot_enrichment_start);
    if hotspot_enabled {
        let profile = SessionHotspotProfile {
            session_id: session.session_id.clone(),
            path: path.to_path_buf(),
            input_bytes: hotspot_input_bytes,
            retained_records: records.len(),
            ingestion: hotspot_ingestion,
            metadata: hotspot_metadata,
            core_scan: hotspot_core_scan,
            core_finalize: hotspot_core_finalize,
            finalize_filter_sort: hotspot_finalize_filter_sort,
            finalize_messages: hotspot_finalize_messages,
            finalize_turns_goals: hotspot_finalize_turns_goals,
            finalize_changes: hotspot_finalize_changes,
            finalize_activities: hotspot_finalize_activities,
            finalize_assembly: hotspot_finalize_assembly,
            enrichment: hotspot_enrichment,
            total: duration_or_zero(hotspot_total_start),
        };
        hotspot_profiles()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(profile);
    }
    Ok(Some(session))
}

// ---------------------------------------------------------------------------
// Second-pass semantic enrichment (Goal/plan/multi-agent/attachments)
// ---------------------------------------------------------------------------

const AGENT_FUNCTION_NAMES: &[&str] = &[
    "spawn_agent",
    "followup_task",
    "send_message",
    "wait_agent",
    "list_agents",
    "interrupt_agent",
];

fn agent_function_name(name: &str) -> bool {
    AGENT_FUNCTION_NAMES.contains(&name)
}

fn sequence_values(value: Option<&Value>) -> &[Value] {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn value_object_with_sequence(sequence: usize, source: Option<&str>) -> Obj {
    let mut output = Obj::new();
    output.insert(
        "sequence".to_string(),
        Value::Number((sequence as u64).into()),
    );
    if let Some(source) = source {
        output.insert("source".to_string(), Value::String(source.to_string()));
    }
    output
}

fn insert_optional_string(output: &mut Obj, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|text| !text.trim().is_empty()) {
        output.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn filter_values_by_turn(values: &mut Vec<Value>, local_set: &HashSet<String>) {
    values.retain(|value| {
        let Some(map) = value.as_object() else {
            return true;
        };
        match value_str(map.get("turn_id")) {
            None => true,
            Some(turn_id) => local_set.contains(turn_id),
        }
    });
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SubagentLocalBoundaries {
    task_start_line: Option<usize>,
    message_start_line: Option<usize>,
}

fn is_exportable_assistant_record(record: &Value) -> bool {
    let Some(record_map) = record.as_object() else {
        return false;
    };
    let Some(payload) = record_map.get("payload").and_then(Value::as_object) else {
        return false;
    };
    record_map.get("type").and_then(Value::as_str) == Some("response_item")
        && value_str(payload.get("type")) == Some("message")
        && value_str(payload.get("role")) == Some("assistant")
        && matches!(
            value_str(payload.get("phase")),
            Some("commentary" | "final_answer")
        )
        && get_message_text(payload.get("content")).is_some_and(|text| !text.trim().is_empty())
}

fn subagent_local_boundaries(records: &Records) -> SubagentLocalBoundaries {
    let mut active_task_start = None;
    let mut delegation_seen = false;
    let mut containing_task_start = None;
    let mut last_line = 0usize;
    for (line_number, record) in records {
        last_line = last_line.max(*line_number);
        let Some(record_map) = record.as_object() else {
            continue;
        };
        let Some(payload) = record_map.get("payload").and_then(Value::as_object) else {
            continue;
        };
        let record_type = value_str(record_map.get("type"));
        let payload_type = value_str(payload.get("type"));
        if record_type == Some("event_msg") && payload_type == Some("task_started") {
            if delegation_seen {
                return SubagentLocalBoundaries {
                    task_start_line: Some(*line_number),
                    message_start_line: Some(*line_number),
                };
            }
            active_task_start = Some(*line_number);
            continue;
        }
        if record_type == Some("response_item")
            && payload_type == Some("message")
            && value_str(payload.get("role")) == Some("developer")
        {
            let developer_text = get_message_text(payload.get("content")).unwrap_or_default();
            if developer_text.contains("You are an agent in a team of agents") {
                delegation_seen = true;
                containing_task_start = active_task_start;
                continue;
            }
        }
        if delegation_seen
            && containing_task_start.is_some()
            && is_exportable_assistant_record(record)
        {
            return SubagentLocalBoundaries {
                task_start_line: containing_task_start,
                message_start_line: Some(*line_number),
            };
        }
        if record_type == Some("event_msg")
            && matches!(payload_type, Some("task_complete" | "turn_aborted"))
        {
            active_task_start = None;
        }
    }
    if delegation_seen && containing_task_start.is_some() {
        return SubagentLocalBoundaries {
            task_start_line: containing_task_start,
            message_start_line: Some(last_line.saturating_add(1)),
        };
    }
    SubagentLocalBoundaries::default()
}

fn subagent_local_start_line(records: &Records) -> Option<usize> {
    subagent_local_boundaries(records).task_start_line
}

fn latest_record_time_from_line(records: &Records, start_line: usize) -> Option<Timestamp> {
    records
        .iter()
        .filter(|(line_number, _)| *line_number >= start_line)
        .filter_map(|(_, record)| {
            record
                .as_object()
                .and_then(|map| parse_datetime_value(map.get("timestamp")))
        })
        .max()
}

fn enrich_session_from_records(
    records: &Records,
    session: &mut Session,
    attachment_root: &Path,
) -> Result<()> {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(35, 0);

    let session_id = session.session_id.clone();
    let is_subagent = session.thread_source == "subagent";
    let create_time = session.create_time;
    let mut local_turn_ids: Vec<String> = Vec::new();
    let mut turn_profiles: HashMap<String, Obj> = HashMap::new();
    let mut plan_updates: Vec<Value> = Vec::new();
    let mut goal_updates: Vec<Value> = Vec::new();
    let mut pending_goal_update_indexes: Vec<usize> = Vec::new();
    let mut pending_goal_calls: HashMap<String, Obj> = HashMap::new();
    let mut goal_contexts_by_turn: HashMap<String, Vec<String>> = HashMap::new();
    let mut agent_events: Vec<Value> = Vec::new();
    let mut pending_agent_calls: HashMap<String, usize> = HashMap::new();
    let mut response_user_candidates: HashMap<(String, String), VecDeque<Value>> = HashMap::new();
    let mut structured_user_items: Vec<Value> = Vec::new();
    let mut structured_command_activities: Vec<Value> = Vec::new();
    let mut structured_tool_activities: Vec<Value> = Vec::new();
    let mut structured_web_activities: Vec<Value> = Vec::new();
    let mut structured_image_activities: Vec<Value> = Vec::new();
    let mut async_deliveries: Vec<Value> = Vec::new();
    let mut structured_changes: Vec<Value> = Vec::new();
    let mut latest_thread_usage: Option<Value> = None;
    let mut current_turn_id: Option<String> = None;
    let mut plan_sequence = 0usize;
    let mut goal_sequence = 0usize;
    let mut agent_sequence = 0usize;
    let actor_path = session
        .agent_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("/root")
        .to_string();
    let registry = collect_attachment_contents(records, attachment_root);

    let local_start_line = if is_subagent {
        subagent_local_start_line(records)
    } else {
        None
    };

    for (line_number, record) in records {
        let Some(record_map) = record.as_object() else {
            continue;
        };
        let Some(payload) = record_map.get("payload").and_then(Value::as_object) else {
            continue;
        };
        let record_type = value_str(record_map.get("type"));
        let payload_type = value_str(payload.get("type"));
        let record_time = parse_datetime_value(record_map.get("timestamp"));
        let local = !is_subagent
            || local_start_line.is_some_and(|start| *line_number >= start)
            || (local_start_line.is_none() && record_time_is_local(record_map, create_time, true));

        if record_type == Some("event_msg") && payload_type == Some("task_started") && local {
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                current_turn_id = Some(turn_id.to_string());
                if !local_turn_ids.iter().any(|value| value == turn_id) {
                    local_turn_ids.push(turn_id.to_string());
                }
                for index in pending_goal_update_indexes.drain(..) {
                    if let Some(update) = goal_updates.get_mut(index).and_then(Value::as_object_mut)
                    {
                        update.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
                    }
                }
            }
            continue;
        }
        if record_type == Some("event_msg")
            && matches!(payload_type, Some("task_complete" | "turn_aborted"))
            && local
        {
            // Use core's terminal-only local identity without rewriting IDs.
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                if !local_turn_ids.iter().any(|value| value == turn_id) {
                    local_turn_ids.push(turn_id.to_string());
                }
            }
            if value_str(payload.get("turn_id")) == current_turn_id.as_deref() {
                current_turn_id = None;
            }
        }
        if !local {
            continue;
        }

        if record_type == Some("token_usage_record") {
            if let Some(usage) = thread_usage_summary(payload, record_time) {
                latest_thread_usage = Some(usage);
            }
            continue;
        }

        if record_type == Some("event_msg") && payload_type == Some("item_completed") {
            if let Some(item) = payload.get("item").and_then(Value::as_object) {
                let item_turn_id = value_str(payload.get("turn_id")).or(current_turn_id.as_deref());
                match value_str(item.get("type")) {
                    Some("UserMessage") => {
                        if let Some(output) =
                            structured_user_item_output(item, item_turn_id, record_time)
                        {
                            structured_user_items.push(output);
                        }
                        continue;
                    }
                    Some("CommandExecution") => {
                        structured_command_activities.push(typed_command_execution_output(
                            item,
                            item_turn_id,
                            record_time,
                            payload,
                        ));
                        continue;
                    }
                    Some("WebSearch" | "Extension") => {
                        if let Some(activity) = typed_web_activity(item, item_turn_id, record_time)
                        {
                            structured_web_activities.push(activity);
                        }
                        continue;
                    }
                    Some("ImageView") => {
                        if let Some(activity) =
                            typed_image_view_activity(item, item_turn_id, record_time)
                        {
                            structured_image_activities.push(activity);
                        }
                        continue;
                    }
                    Some("McpToolCall" | "DynamicToolCall") => {
                        structured_tool_activities.push(typed_tool_activity(
                            item,
                            item_turn_id,
                            record_time,
                            payload,
                        ));
                        continue;
                    }
                    Some("AgentMessage") => {
                        if let Some(delivery) = async_delivery(item, item_turn_id, record_time) {
                            async_deliveries.push(delivery);
                        }
                        continue;
                    }
                    Some("FileChange") => {
                        structured_changes.extend(typed_file_change_outputs(
                            item,
                            item_turn_id,
                            record_time,
                        ));
                        continue;
                    }
                    _ => {}
                }
            }
        }

        if record_type == Some("turn_context") {
            if let Some(turn_id) = value_str(payload.get("turn_id")) {
                let profile = profile_from_turn_context(payload);
                if !profile.is_empty() {
                    turn_profiles.insert(turn_id.to_string(), profile);
                }
            }
            continue;
        }

        if record_type == Some("event_msg") && payload_type == Some("thread_goal_updated") {
            let Some(goal_payload) = payload.get("goal").and_then(Value::as_object) else {
                continue;
            };
            let thread_id = value_str(payload.get("threadId"))
                .or_else(|| value_str(goal_payload.get("threadId")));
            if thread_id != Some(session_id.as_str()) {
                continue;
            }
            goal_sequence += 1;
            let mut update = value_object_with_sequence(goal_sequence, Some("thread_goal_updated"));
            if let Some(record_time) = record_time {
                update.insert(
                    "create_time".to_string(),
                    Value::String(utc_iso(record_time)),
                );
            }
            if let Some(objective) = nonblank_raw_str(goal_payload.get("objective")) {
                let (restored, attachments, restored_any) =
                    replace_attachment_transport(objective, &registry);
                update.insert("objective".to_string(), Value::String(restored));
                if !attachments.is_empty() {
                    update.insert("attachments".to_string(), Value::Array(attachments));
                }
                if restored_any {
                    update.insert(
                        "objective_source".to_string(),
                        Value::String("codex_attachment".to_string()),
                    );
                    update.insert(
                        "transport_objective".to_string(),
                        Value::String(objective.to_string()),
                    );
                }
            }
            if let Some(status) = value_str(goal_payload.get("status")) {
                update.insert(
                    "reported_status".to_string(),
                    Value::String(status.to_ascii_lowercase()),
                );
            }
            let created_at = parse_unix_seconds(goal_payload.get("createdAt"));
            let updated_at = parse_unix_seconds(goal_payload.get("updatedAt")).or(record_time);
            if let Some(created_at) = created_at {
                update.insert("created_at".to_string(), Value::String(utc_iso(created_at)));
            }
            if let Some(updated_at) = updated_at {
                update.insert("updated_at".to_string(), Value::String(utc_iso(updated_at)));
            }
            for (output_key, source_key) in [
                ("tokens_used", "tokensUsed"),
                ("token_budget", "tokenBudget"),
                ("time_used_seconds", "timeUsedSeconds"),
            ] {
                if let Some(value) = int_value(goal_payload.get(source_key), Some(0)) {
                    update.insert(output_key.to_string(), Value::Number(value.into()));
                }
            }
            if let Some(turn_id) = current_turn_id.as_deref() {
                update.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
            }
            let update_index = goal_updates.len();
            goal_updates.push(Value::Object(update));
            if current_turn_id.is_none() {
                pending_goal_update_indexes.push(update_index);
            }
            continue;
        }

        if record_type == Some("response_item")
            && payload_type == Some("message")
            && value_str(payload.get("role")) == Some("user")
        {
            let content = get_message_text(payload.get("content"));
            let turn_id = call_turn_id(payload, current_turn_id.as_deref());
            if let (Some(turn_id), Some(content)) = (turn_id.as_deref(), content.as_deref()) {
                if !content.trim().is_empty() {
                    response_user_candidates
                        .entry((turn_id.to_string(), text_key(Some(content))))
                        .or_default()
                        .push_back(user_candidate_output(Some(turn_id), record_time, content));
                }
            }
            let objective = goal_context_objective(content.as_deref());
            if let (Some(turn_id), Some(objective)) = (turn_id, objective) {
                goal_contexts_by_turn
                    .entry(turn_id)
                    .or_default()
                    .push(objective);
            }
            continue;
        }

        if record_type == Some("event_msg") && payload_type == Some("sub_agent_activity") {
            agent_sequence += 1;
            let mut event = value_object_with_sequence(agent_sequence, None);
            event.insert(
                "type".to_string(),
                Value::String("agent_activity".to_string()),
            );
            let occurred_seconds = float_value(payload.get("occurred_at_ms"), None)
                .map(|value| (value as i64).div_euclid(1000));
            let occurred = occurred_seconds
                .and_then(|seconds| seconds.checked_mul(1_000_000))
                .or(record_time);
            if let Some(occurred) = occurred {
                event.insert("create_time".to_string(), Value::String(utc_iso(occurred)));
            }
            event.insert("actor".to_string(), Value::String(actor_path.clone()));
            for (output_key, source_key) in [
                ("target_thread_id", "agent_thread_id"),
                ("target", "agent_path"),
                ("status", "kind"),
            ] {
                if let Some(value) = value_str(payload.get(source_key)) {
                    event.insert(output_key.to_string(), Value::String(value.to_string()));
                }
            }
            agent_events.push(Value::Object(event));
            continue;
        }

        if record_type == Some("response_item")
            && matches!(payload_type, Some("function_call" | "custom_tool_call"))
        {
            let name = value_str(payload.get("name")).unwrap_or_default();
            let arguments = json_object(payload.get("arguments"));
            let turn_id = call_turn_id(payload, current_turn_id.as_deref());
            if matches!(name, "create_goal" | "update_goal" | "get_goal") {
                if let Some(call_id) = value_str(payload.get("call_id")) {
                    let mut pending_call = Obj::new();
                    pending_call.insert("source".to_string(), Value::String(name.to_string()));
                    insert_optional_string(&mut pending_call, "turn_id", turn_id.as_deref());
                    pending_goal_calls.insert(call_id.to_string(), pending_call);
                }
                continue;
            }
            if name == "update_plan" {
                plan_sequence += 1;
                let mut update = value_object_with_sequence(plan_sequence, None);
                insert_optional_string(&mut update, "turn_id", turn_id.as_deref());
                if let Some(record_time) = record_time {
                    update.insert(
                        "create_time".to_string(),
                        Value::String(utc_iso(record_time)),
                    );
                }
                if let Some(explanation) = value_str(arguments.get("explanation")) {
                    update.insert(
                        "explanation".to_string(),
                        Value::String(explanation.to_string()),
                    );
                }
                let mut steps = Vec::new();
                for plan_item in sequence_values(arguments.get("plan")) {
                    let Some(plan_item) = plan_item.as_object() else {
                        continue;
                    };
                    let Some(step_text) = value_str(plan_item.get("step")) else {
                        continue;
                    };
                    let mut step = obj!("text" => Value::String(step_text.to_string()));
                    if let Some(status) = value_str(plan_item.get("status")) {
                        step.insert("status".to_string(), Value::String(status.to_string()));
                    }
                    steps.push(Value::Object(step));
                }
                if !steps.is_empty() {
                    update.insert("steps".to_string(), Value::Array(steps));
                }
                plan_updates.push(Value::Object(update));
                continue;
            }
            if name == "exec" {
                if let Some(tool_input) = raw_str(payload.get("input")) {
                    if let Some(goal_tool_name) = exec_goal_tool_name(Some(tool_input)) {
                        if let Some(call_id) = value_str(payload.get("call_id")) {
                            let mut pending_call = Obj::new();
                            pending_call.insert(
                                "source".to_string(),
                                Value::String(format!("exec:{goal_tool_name}")),
                            );
                            insert_optional_string(
                                &mut pending_call,
                                "turn_id",
                                turn_id.as_deref(),
                            );
                            pending_goal_calls.insert(call_id.to_string(), pending_call);
                        }
                    }
                    for parsed in javascript_plan_updates(tool_input) {
                        plan_sequence += 1;
                        let mut update = value_object_with_sequence(plan_sequence, None);
                        insert_optional_string(&mut update, "turn_id", turn_id.as_deref());
                        if let Some(record_time) = record_time {
                            update.insert(
                                "create_time".to_string(),
                                Value::String(utc_iso(record_time)),
                            );
                        }
                        if let Some(explanation) = parsed.get("explanation") {
                            update.insert("explanation".to_string(), explanation.clone());
                        }
                        if let Some(steps) = parsed.get("steps") {
                            update.insert("steps".to_string(), steps.clone());
                        }
                        plan_updates.push(Value::Object(update));
                    }
                }
                continue;
            }
            if agent_function_name(name) {
                agent_sequence += 1;
                let event_type = match name {
                    "spawn_agent" => "spawn",
                    "followup_task" => "followup_task",
                    "send_message" => "message_sent",
                    "wait_agent" => "wait",
                    "list_agents" => "list",
                    "interrupt_agent" => "interrupt",
                    _ => "agent_call",
                };
                let mut event = value_object_with_sequence(agent_sequence, None);
                event.insert("type".to_string(), Value::String(event_type.to_string()));
                event.insert("actor".to_string(), Value::String(actor_path.clone()));
                insert_optional_string(&mut event, "turn_id", turn_id.as_deref());
                if let Some(record_time) = record_time {
                    event.insert(
                        "create_time".to_string(),
                        Value::String(utc_iso(record_time)),
                    );
                }
                for (output_key, source_key) in [
                    ("target", "target"),
                    ("task_name", "task_name"),
                    ("fork_turns", "fork_turns"),
                    ("timeout_ms", "timeout_ms"),
                ] {
                    if let Some(value) = arguments.get(source_key) {
                        let valid = value.as_str().is_some_and(|text| !text.trim().is_empty())
                            || value.as_i64().is_some()
                            || value.as_u64().is_some();
                        if valid {
                            event.insert(output_key.to_string(), value.clone());
                        }
                    }
                }
                if let Some(message) = raw_str(arguments.get("message")) {
                    event.insert(
                        "payload_encrypted".to_string(),
                        Value::Bool(message.starts_with("gAAAA")),
                    );
                }
                let event_index = agent_events.len();
                agent_events.push(Value::Object(event));
                if let Some(call_id) = value_str(payload.get("call_id")) {
                    pending_agent_calls.insert(call_id.to_string(), event_index);
                }
                continue;
            }
        }

        if record_type == Some("response_item")
            && matches!(
                payload_type,
                Some("function_call_output" | "custom_tool_call_output")
            )
        {
            let call_id = value_str(payload.get("call_id")).map(str::to_string);
            if let Some(call_id) = call_id.as_deref() {
                if let Some(pending_call) = pending_goal_calls.remove(call_id) {
                    let observation = matches!(
                        value_str(pending_call.get("source")),
                        Some("get_goal" | "exec:get_goal")
                    );
                    let result = if observation {
                        extract_goal_observation_result(payload.get("output")).unwrap_or_default()
                    } else {
                        extract_goal_result(payload.get("output")).unwrap_or_else(|| {
                            let output_text = raw_str(payload.get("output"))
                                .map(str::to_string)
                                .or_else(|| get_message_text(payload.get("output")));
                            let output_value = output_text.map(Value::String);
                            json_object(output_value.as_ref())
                        })
                    };
                    if let Some(goal_payload) = result.get("goal").and_then(Value::as_object) {
                        if value_str(goal_payload.get("threadId")) == Some(session_id.as_str()) {
                            goal_sequence += 1;
                            let source =
                                value_str(pending_call.get("source")).unwrap_or("goal_tool");
                            let mut update =
                                value_object_with_sequence(goal_sequence, Some(source));
                            if observation {
                                update.insert("observation".to_string(), Value::Bool(true));
                                update.insert(
                                    "source_call_id".to_string(),
                                    Value::String(call_id.to_string()),
                                );
                                if let Some(time) = record_time {
                                    update.insert(
                                        "observed_at".to_string(),
                                        Value::String(utc_iso(time)),
                                    );
                                }
                            }
                            if let Some(record_time) = record_time {
                                update.insert(
                                    "create_time".to_string(),
                                    Value::String(utc_iso(record_time)),
                                );
                            }
                            insert_optional_string(
                                &mut update,
                                "turn_id",
                                value_str(pending_call.get("turn_id")),
                            );
                            if let Some(objective) = nonblank_raw_str(goal_payload.get("objective"))
                            {
                                let (restored, attachments, restored_any) =
                                    replace_attachment_transport(objective, &registry);
                                update.insert("objective".to_string(), Value::String(restored));
                                if !attachments.is_empty() {
                                    update.insert(
                                        "attachments".to_string(),
                                        Value::Array(attachments),
                                    );
                                }
                                if restored_any {
                                    update.insert(
                                        "objective_source".to_string(),
                                        Value::String("codex_attachment".to_string()),
                                    );
                                    update.insert(
                                        "transport_objective".to_string(),
                                        Value::String(objective.to_string()),
                                    );
                                }
                            }
                            if let Some(status) = value_str(goal_payload.get("status")) {
                                update.insert(
                                    "reported_status".to_string(),
                                    Value::String(status.to_ascii_lowercase()),
                                );
                            }
                            if let Some(created_at) =
                                parse_unix_seconds(goal_payload.get("createdAt"))
                            {
                                update.insert(
                                    "created_at".to_string(),
                                    Value::String(utc_iso(created_at)),
                                );
                            }
                            if let Some(updated_at) =
                                parse_unix_seconds(goal_payload.get("updatedAt")).or(record_time)
                            {
                                update.insert(
                                    "updated_at".to_string(),
                                    Value::String(utc_iso(updated_at)),
                                );
                            }
                            for (output_key, source_key) in [
                                ("tokens_used", "tokensUsed"),
                                ("token_budget", "tokenBudget"),
                                ("time_used_seconds", "timeUsedSeconds"),
                            ] {
                                if let Some(value) =
                                    int_value(goal_payload.get(source_key), Some(0))
                                {
                                    update.insert(
                                        output_key.to_string(),
                                        Value::Number(value.into()),
                                    );
                                }
                            }
                            if let Some(remaining) =
                                int_value(result.get("remainingTokens"), Some(0))
                            {
                                update.insert(
                                    "remaining_tokens".to_string(),
                                    Value::Number(remaining.into()),
                                );
                            }
                            goal_updates.push(Value::Object(update));
                        }
                    }
                    continue;
                }
            }

            let Some(call_id) = call_id else { continue };
            let Some(event_index) = pending_agent_calls.remove(&call_id) else {
                continue;
            };
            let output_text = raw_str(payload.get("output"))
                .map(str::to_string)
                .or_else(|| get_message_text(payload.get("output")));
            let output_value = output_text.map(Value::String);
            let result = json_object(output_value.as_ref());
            let Some(event) = agent_events
                .get_mut(event_index)
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            if let Some(task_path) = value_str(result.get("task_name")) {
                event.insert("target".to_string(), Value::String(task_path.to_string()));
            }
            if let Some(timed_out) = bool_value(result.get("timed_out")) {
                event.insert("timed_out".to_string(), Value::Bool(timed_out));
            }
            let mut statuses = Vec::new();
            for agent in sequence_values(result.get("agents")) {
                let Some(agent) = agent.as_object() else {
                    continue;
                };
                if let (Some(name), Some(status)) = (
                    value_str(agent.get("agent_name")),
                    value_str(agent.get("agent_status")),
                ) {
                    statuses.push(Value::Object(obj!(
                        "agent" => Value::String(name.to_string()),
                        "status" => Value::String(status.to_string()),
                    )));
                }
            }
            if !statuses.is_empty() {
                event.insert("agents".to_string(), Value::Array(statuses));
            }
            if let Some(record_time) = record_time {
                event.insert(
                    "completed_at".to_string(),
                    Value::String(utc_iso(record_time)),
                );
            }
            continue;
        }

        if record_type == Some("response_item") && payload_type == Some("agent_message") {
            agent_sequence += 1;
            let mut event = value_object_with_sequence(agent_sequence, None);
            event.insert(
                "type".to_string(),
                Value::String("message_received".to_string()),
            );
            if let Some(record_time) = record_time {
                event.insert(
                    "create_time".to_string(),
                    Value::String(utc_iso(record_time)),
                );
            }
            insert_optional_string(&mut event, "actor", value_str(payload.get("author")));
            insert_optional_string(&mut event, "target", value_str(payload.get("recipient")));
            let content = get_message_text(payload.get("content")).unwrap_or_default();
            if let Some(captures) = agent_message_type_regex().captures(&content) {
                event.insert(
                    "message_type".to_string(),
                    Value::String(captures[1].trim().to_string()),
                );
            }
            if let Some(captures) = agent_task_name_regex().captures(&content) {
                event.insert(
                    "task_name".to_string(),
                    Value::String(captures[1].trim().to_string()),
                );
            }
            event.insert(
                "payload_encrypted".to_string(),
                Value::Bool(content.contains("gAAAA") || content.contains("Message Type:")),
            );
            agent_events.push(Value::Object(event));
        }
    }

    if !pending_goal_update_indexes.is_empty() {
        if let Some(last_local_turn_id) = local_turn_ids.last() {
            for index in pending_goal_update_indexes.drain(..) {
                if let Some(update) = goal_updates.get_mut(index).and_then(Value::as_object_mut) {
                    update.insert(
                        "turn_id".to_string(),
                        Value::String(last_local_turn_id.clone()),
                    );
                }
            }
        }
    }

    let effective_ids: HashSet<String> = session
        .turns
        .iter()
        .filter_map(|turn| {
            turn.get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let has_effective_lifecycle = !session.turns.is_empty() || !session.rollbacks.is_empty();
    let async_deliveries = async_deliveries
        .into_iter()
        .filter(|delivery| {
            test_turn_effective(
                delivery.get("turn_id").and_then(Value::as_str),
                has_effective_lifecycle,
                &effective_ids,
            )
        })
        .collect();
    attach_async_deliveries(&mut session.user_input_requests, async_deliveries);
    if is_subagent {
        let local_set: HashSet<String> = local_turn_ids.into_iter().collect();
        session.turns.retain(|turn| {
            turn.as_object()
                .and_then(|map| value_str(map.get("turn_id")))
                .is_some_and(|turn_id| local_set.contains(turn_id))
        });
        session.messages.retain(|message| {
            message
                .as_object()
                .and_then(|map| value_str(map.get("turn_id")))
                .is_some_and(|turn_id| local_set.contains(turn_id))
        });
        session.raw_messages.retain(|message| {
            message
                .turn_id
                .as_ref()
                .is_some_and(|turn_id| local_set.contains(turn_id))
        });
        filter_values_by_turn(&mut session.plans, &local_set);
        filter_values_by_turn(&mut session.reviews, &local_set);
        filter_values_by_turn(&mut session.memory_citations, &local_set);
        filter_values_by_turn(&mut session.writing_blocks, &local_set);
        filter_values_by_turn(&mut session.ui_actions, &local_set);
        filter_values_by_turn(&mut session.ui_parse_warnings, &local_set);
        filter_values_by_turn(&mut session.user_input_requests, &local_set);
        filter_values_by_turn(&mut session.activities, &local_set);
        filter_values_by_turn(&mut session.changes, &local_set);
        session.goals.retain(|goal| {
            goal.as_object()
                .and_then(|map| value_str(map.get("thread_id")))
                == Some(session_id.as_str())
        });
    }

    if !structured_command_activities.is_empty() {
        session.activities.retain(|activity| {
            let Some(map) = activity.as_object() else {
                return true;
            };
            if value_str(map.get("type")) != Some("command") {
                return true;
            }
            map.get("aggregate_result")
                .and_then(Value::as_object)
                .and_then(|aggregate| value_str(aggregate.get("status")))
                == Some("failed")
        });
        session.activities.extend(structured_command_activities);
        session.activities.sort_by(|left, right| {
            let left_time = left
                .as_object()
                .and_then(|map| value_str(map.get("create_time")))
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            let right_time = right
                .as_object()
                .and_then(|map| value_str(map.get("create_time")))
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            left_time.cmp(right_time)
        });
    }
    if !structured_tool_activities.is_empty() {
        session
            .activities
            .extend(structured_tool_activities.into_iter().filter(|activity| {
                test_turn_effective(
                    activity.get("turn_id").and_then(Value::as_str),
                    has_effective_lifecycle,
                    &effective_ids,
                )
            }));
        session.activities.sort_by(|left, right| {
            let a = left
                .get("create_time")
                .and_then(Value::as_str)
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            let b = right
                .get("create_time")
                .and_then(Value::as_str)
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            a.cmp(b)
        });
    }
    if !structured_web_activities.is_empty() || !structured_image_activities.is_empty() {
        session.activities.extend(
            structured_web_activities
                .into_iter()
                .chain(structured_image_activities)
                .filter(|activity| {
                    test_turn_effective(
                        activity.get("turn_id").and_then(Value::as_str),
                        has_effective_lifecycle,
                        &effective_ids,
                    )
                }),
        );
        session.activities.sort_by(|left, right| {
            let a = left
                .get("create_time")
                .and_then(Value::as_str)
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            let b = right
                .get("create_time")
                .and_then(Value::as_str)
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            a.cmp(b)
        });
    }
    if !structured_changes.is_empty() {
        session.changes = structured_changes;
        session.changes.sort_by(|left, right| {
            let left_time = left
                .as_object()
                .and_then(|map| value_str(map.get("create_time")))
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            let right_time = right
                .as_object()
                .and_then(|map| value_str(map.get("create_time")))
                .unwrap_or("9999-12-31T23:59:59.999+00:00");
            left_time.cmp(right_time)
        });
    }
    if let Some(usage) = latest_thread_usage {
        session.usage = Some(usage);
    }

    let structured_user_messages =
        merge_verified_structured_users(&mut response_user_candidates, &structured_user_items);
    let mut source_messages: Vec<Value> = session
        .messages
        .iter()
        .filter_map(|message| message.as_object().cloned().map(Value::Object))
        .collect();
    source_messages.extend(structured_user_messages);
    let mut represented_goal_keys = HashSet::new();
    for message in &source_messages {
        let Some(map) = message.as_object() else {
            continue;
        };
        if value_str(map.get("role")) != Some("user") {
            continue;
        }
        if let Some(key) = goal_text_key(raw_str(map.get("content"))) {
            represented_goal_keys.insert(key);
        }
    }

    for update_value in &goal_updates {
        let Some(update) = update_value.as_object() else {
            continue;
        };
        let objective_value =
            raw_str(update.get("transport_objective")).or_else(|| raw_str(update.get("objective")));
        let objective_key = goal_text_key(objective_value);
        let turn_id = value_str(update.get("turn_id"));
        let eligible = objective_key
            .as_ref()
            .is_some_and(|key| !represented_goal_keys.contains(key))
            && value_str(update.get("source")) == Some("thread_goal_updated")
            && value_str(update.get("reported_status")) == Some("active")
            && turn_id.is_some()
            && goal_update_is_fresh_input(update);
        if !eligible {
            continue;
        }
        let objective_key = objective_key.expect("checked");
        let turn_id = turn_id.expect("checked");
        let context_matches = goal_contexts_by_turn.get(turn_id).is_some_and(|contexts| {
            contexts
                .iter()
                .any(|objective| goal_text_key(Some(objective)) == Some(objective_key.clone()))
        });
        let Some(content_value) = nonblank_raw_str(update.get("objective")) else {
            continue;
        };
        if !context_matches {
            continue;
        }
        let mut synthetic = obj!(
            "id" => Value::String(format!("goal-update-{}", int_value(update.get("sequence"), None).unwrap_or_default())),
            "turn_id" => Value::String(turn_id.to_string()),
            "role" => Value::String("user".to_string()),
            "create_time" => update.get("create_time").cloned().unwrap_or(Value::Null),
            "content" => Value::String(content_value.to_string()),
            "content_source" => Value::String(value_str(update.get("objective_source")).unwrap_or("thread_goal_updated").to_string()),
            "message_mode" => Value::String("goal".to_string()),
        );
        if let Some(attachments) = update.get("attachments").and_then(Value::as_array) {
            let values: Vec<Value> = attachments
                .iter()
                .filter_map(|item| item.as_object().cloned().map(Value::Object))
                .collect();
            if !values.is_empty() {
                synthetic.insert("attachments".to_string(), Value::Array(values));
            }
        }
        if let Some(transport) = raw_str(update.get("transport_objective")) {
            synthetic.insert(
                "transport_content".to_string(),
                Value::String(transport.to_string()),
            );
        }
        synthetic.insert(
            "message_origin".to_string(),
            Value::String(
                value_str(update.get("source"))
                    .unwrap_or("thread_goal_updated")
                    .to_string(),
            ),
        );
        source_messages.push(Value::Object(synthetic));
        represented_goal_keys.insert(objective_key);
    }

    let mut indexed_messages: Vec<(usize, Value)> =
        source_messages.into_iter().enumerate().collect();
    indexed_messages.sort_by(|left, right| {
        let left_time = left
            .1
            .as_object()
            .and_then(|map| value_str(map.get("create_time")))
            .unwrap_or("9999-12-31T23:59:59.999+00:00");
        let right_time = right
            .1
            .as_object()
            .and_then(|map| value_str(map.get("create_time")))
            .unwrap_or("9999-12-31T23:59:59.999+00:00");
        left_time.cmp(right_time).then_with(|| left.0.cmp(&right.0))
    });
    let sorted_messages: Vec<Value> = indexed_messages
        .into_iter()
        .map(|(_, value)| value)
        .collect();
    let (messages, id_map) = renumber_messages(&sorted_messages);
    let mut attachment_message_by_path: HashMap<String, String> = HashMap::new();
    let mut goal_message_by_key: HashMap<String, String> = HashMap::new();
    let mut restored_messages = Vec::with_capacity(messages.len());
    for message in messages {
        let Some(message_map) = message.as_object() else {
            restored_messages.push(message);
            continue;
        };
        let Some(content) = raw_str(message_map.get("content")) else {
            restored_messages.push(message);
            continue;
        };
        let existing_attachments = message_map.get("attachments").and_then(Value::as_array);
        let updated = if existing_attachments.is_some()
            || !message_attachment_transport_is_restorable(message_map)
        {
            message_map.clone()
        } else {
            let (restored, attachments, restored_any) =
                replace_attachment_transport(content, &registry);
            if attachments.is_empty() {
                message_map.clone()
            } else {
                let mut output = Obj::new();
                for (key, value) in message_map {
                    if key == "content" {
                        output.insert("content".to_string(), Value::String(restored.clone()));
                        output.insert(
                            "content_source".to_string(),
                            Value::String(
                                if restored_any {
                                    "codex_attachment"
                                } else {
                                    "attachment_reference"
                                }
                                .to_string(),
                            ),
                        );
                        let goal_mode = content.to_ascii_lowercase().contains("/goal")
                            || attachments.iter().any(|item| {
                                item.as_object()
                                    .and_then(|map| value_str(map.get("filename")))
                                    == Some("goal-objective.md")
                            });
                        output.insert(
                            "message_mode".to_string(),
                            Value::String(if goal_mode { "goal" } else { "standard" }.to_string()),
                        );
                        output.insert("attachments".to_string(), Value::Array(attachments.clone()));
                        output.insert(
                            "transport_content".to_string(),
                            Value::String(content.to_string()),
                        );
                    } else {
                        output.insert(key.clone(), value.clone());
                    }
                }
                output
            }
        };
        let message_id = value_str(updated.get("id")).unwrap_or_default().to_string();
        if value_str(updated.get("role")) == Some("user") {
            for reference_text in [raw_str(updated.get("transport_content")), Some(content)] {
                let Some(reference_text) = reference_text else {
                    continue;
                };
                for reference in attachment_references(reference_text) {
                    attachment_message_by_path
                        .insert(reference.normalized_path, message_id.clone());
                }
            }
        }
        if value_str(updated.get("role")) == Some("user")
            && value_str(updated.get("message_mode")) == Some("goal")
        {
            for key_text in [
                raw_str(updated.get("transport_content")),
                raw_str(updated.get("content")),
            ] {
                if let Some(key) = goal_text_key(key_text) {
                    goal_message_by_key
                        .entry(key)
                        .or_insert_with(|| message_id.clone());
                }
            }
        }
        restored_messages.push(Value::Object(updated));
    }
    session.messages = restored_messages;

    session.plans = session
        .plans
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();
    session.reviews = session
        .reviews
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();
    session.memory_citations = session
        .memory_citations
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();
    session.writing_blocks = session
        .writing_blocks
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();
    session.ui_actions = session
        .ui_actions
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();
    session.ui_parse_warnings = session
        .ui_parse_warnings
        .iter()
        .map(|value| replace_message_ids(value, &id_map))
        .collect();

    for update_value in &mut goal_updates {
        let Some(update) = update_value.as_object_mut() else {
            continue;
        };
        let objective_owned = raw_str(update.get("transport_objective"))
            .or_else(|| raw_str(update.get("objective")))
            .map(str::to_string);
        let Some(objective) = objective_owned else {
            continue;
        };
        let mut matched_message_id = None;
        for reference in attachment_references(&objective) {
            if let Some(message_id) = attachment_message_by_path.get(&reference.normalized_path) {
                matched_message_id = Some(message_id.clone());
                break;
            }
        }
        if matched_message_id.is_none() {
            if let Some(key) = goal_text_key(Some(&objective)) {
                matched_message_id = goal_message_by_key.get(&key).cloned();
            }
        }
        if let Some(message_id) = matched_message_id {
            update.remove("objective");
            update.insert(
                "objective_message_id".to_string(),
                Value::String(message_id),
            );
        }
    }

    let mut local_goals = Vec::new();
    for goal in &session.goals {
        let Some(goal_map) = goal.as_object() else {
            continue;
        };
        let mut item = goal_map.clone();
        let objective_owned = raw_str(item.get("objective")).map(str::to_string);
        if let Some(objective) = objective_owned {
            let references = attachment_references(&objective);
            let (restored, attachments, restored_any) =
                replace_attachment_transport(&objective, &registry);
            let mut matched_message_id = None;
            for reference in references {
                if let Some(message_id) = attachment_message_by_path.get(&reference.normalized_path)
                {
                    matched_message_id = Some(message_id.clone());
                    break;
                }
            }
            if matched_message_id.is_none() {
                if let Some(key) = goal_text_key(Some(&objective)) {
                    matched_message_id = goal_message_by_key.get(&key).cloned();
                }
            }
            if let Some(message_id) = matched_message_id {
                item.remove("objective");
                item.insert(
                    "objective_message_id".to_string(),
                    Value::String(message_id),
                );
            } else {
                item.insert("objective".to_string(), Value::String(restored));
                if !attachments.is_empty() {
                    item.insert("attachments".to_string(), Value::Array(attachments));
                }
                if restored_any {
                    item.insert(
                        "objective_source".to_string(),
                        Value::String("codex_attachment".to_string()),
                    );
                    item.insert("transport_objective".to_string(), Value::String(objective));
                }
            }
        }
        local_goals.push(Value::Object(item));
    }

    let projected_goal_updates = projectable_goal_updates(&goal_updates);
    if let Some(latest_update_value) = projected_goal_updates.last() {
        let latest_update = latest_update_value.as_object().expect("goal update object");
        let latest_created_at = raw_str(latest_update.get("created_at")).map(str::to_string);
        let generation_updates: Vec<&Obj> = if let Some(created_at) = latest_created_at.as_deref() {
            projected_goal_updates
                .iter()
                .filter_map(Value::as_object)
                .filter(|update| raw_str(update.get("created_at")) == Some(created_at))
                .collect()
        } else {
            vec![latest_update]
        };
        let empty_creation = Obj::new();
        let first_generation_update = generation_updates
            .iter()
            .copied()
            .find(|update| update.get("observation").and_then(Value::as_bool) != Some(true))
            .unwrap_or(&empty_creation);
        let mut current_goal = local_goals
            .iter()
            .filter_map(Value::as_object)
            .find(|goal| {
                value_str(goal.get("thread_id")) == Some(session_id.as_str())
                    && latest_created_at.as_deref().is_none_or(|created_at| {
                        raw_str(goal.get("created_at")) == Some(created_at)
                    })
            })
            .cloned()
            .unwrap_or_default();
        current_goal.insert("id".to_string(), Value::String("goal01".to_string()));
        current_goal.insert("thread_id".to_string(), Value::String(session_id.clone()));
        if let Some(created_turn_id) = value_str(first_generation_update.get("turn_id")) {
            current_goal.insert(
                "created_turn_id".to_string(),
                Value::String(created_turn_id.to_string()),
            );
        } else {
            current_goal.remove("created_turn_id");
        }
        let objective_update = generation_updates
            .iter()
            .rev()
            .find(|candidate| {
                candidate.contains_key("objective_message_id")
                    || candidate.contains_key("objective")
                    || candidate.contains_key("attachments")
            })
            .copied();
        for key in [
            "objective",
            "objective_message_id",
            "attachments",
            "objective_source",
            "transport_objective",
        ] {
            current_goal.remove(key);
        }
        if let Some(objective_update) = objective_update {
            if let Some(message_id) = value_str(objective_update.get("objective_message_id")) {
                current_goal.insert(
                    "objective_message_id".to_string(),
                    Value::String(message_id.to_string()),
                );
            } else {
                for key in [
                    "objective",
                    "attachments",
                    "objective_source",
                    "transport_objective",
                ] {
                    if let Some(value) = objective_update.get(key) {
                        current_goal.insert(key.to_string(), value.clone());
                    }
                }
            }
        }
        let reported = value_str(latest_update.get("reported_status"))
            .unwrap_or("unknown")
            .to_string();
        current_goal.insert(
            "reported_status".to_string(),
            Value::String(reported.clone()),
        );
        current_goal.insert(
            "status_source".to_string(),
            Value::String(
                value_str(latest_update.get("source"))
                    .unwrap_or("thread_goal_updated")
                    .to_string(),
            ),
        );
        if let Some(created_at) = latest_created_at {
            current_goal.insert("created_at".to_string(), Value::String(created_at));
        } else {
            current_goal.remove("created_at");
        }
        if let Some(updated_at) = raw_str(latest_update.get("updated_at")) {
            current_goal.insert(
                "updated_at".to_string(),
                Value::String(updated_at.to_string()),
            );
        } else {
            current_goal.remove("updated_at");
        }
        for key in [
            "tokens_used",
            "token_budget",
            "time_used_seconds",
            "remaining_tokens",
        ] {
            if let Some(value) = latest_update.get(key) {
                current_goal.insert(key.to_string(), value.clone());
            } else {
                current_goal.remove(key);
            }
        }
        let session_turns: Vec<&Obj> = session
            .turns
            .iter()
            .filter_map(Value::as_object)
            .filter(|turn| value_str(turn.get("turn_id")).is_some())
            .collect();
        let last_turn_id = value_str(latest_update.get("turn_id"))
            .map(str::to_string)
            .or_else(|| {
                session_turns
                    .last()
                    .and_then(|turn| value_str(turn.get("turn_id")))
                    .map(str::to_string)
            });
        let turn_by_id: HashMap<String, &Obj> = session_turns
            .iter()
            .filter_map(|turn| {
                value_str(turn.get("turn_id")).map(|turn_id| (turn_id.to_string(), *turn))
            })
            .collect();
        let last_turn = last_turn_id
            .as_ref()
            .and_then(|turn_id| turn_by_id.get(turn_id).copied());
        if let Some(last_turn_id) = last_turn_id.as_deref() {
            current_goal.insert(
                "last_turn_id".to_string(),
                Value::String(last_turn_id.to_string()),
            );
        } else {
            current_goal.remove("last_turn_id");
        }
        if let Some(last_turn) = last_turn {
            let last_status = value_str(last_turn.get("status"));
            if let Some(last_status) = last_status {
                current_goal.insert(
                    "last_turn_status".to_string(),
                    Value::String(last_status.to_string()),
                );
            } else {
                current_goal.remove("last_turn_status");
            }
            if last_status == Some("completed") {
                if let Some(end_time) = raw_str(last_turn.get("end_time")) {
                    current_goal.insert(
                        "last_task_completed_at".to_string(),
                        Value::String(end_time.to_string()),
                    );
                }
            } else {
                current_goal.remove("last_task_completed_at");
            }
            let execution_state = match last_status {
                Some("in_progress") => "running",
                Some("completed") => "idle",
                Some("failed" | "aborted") => "interrupted",
                Some("rolled_back") => "idle",
                _ => "unknown",
            };
            current_goal.insert(
                "execution_state".to_string(),
                Value::String(execution_state.to_string()),
            );
        } else {
            current_goal.remove("last_turn_status");
            current_goal.remove("last_task_completed_at");
            let terminal = matches!(
                reported.as_str(),
                "complete" | "completed" | "cancelled" | "failed" | "paused" | "blocked"
            );
            current_goal.insert(
                "execution_state".to_string(),
                Value::String(if terminal { "idle" } else { "unknown" }.to_string()),
            );
        }
        local_goals = vec![Value::Object(current_goal)];
    }
    session.goals = local_goals;

    for turn in &mut session.turns {
        let Some(turn_map) = turn.as_object_mut() else {
            continue;
        };
        let Some(turn_id) = value_str(turn_map.get("turn_id")).map(str::to_string) else {
            continue;
        };
        if let Some(profile) = turn_profiles.get(&turn_id) {
            turn_map.insert(
                "execution_profile".to_string(),
                Value::Object(profile.clone()),
            );
        }
    }
    session.execution_profile = execution_profile_summary(&session.turns);
    session.plan_updates = plan_updates;
    session.goal_updates = goal_updates;
    session.agent_events = agent_events;

    if is_subagent {
        let mut delegation = Obj::new();
        insert_optional_string(
            &mut delegation,
            "parent_thread_id",
            session.parent_thread_id.as_deref(),
        );
        insert_optional_string(
            &mut delegation,
            "forked_from_id",
            session.forked_from_id.as_deref(),
        );
        insert_optional_string(
            &mut delegation,
            "agent_name",
            session.agent_nickname.as_deref(),
        );
        insert_optional_string(&mut delegation, "path", session.agent_path.as_deref());
        insert_optional_string(&mut delegation, "role", session.agent_role.as_deref());
        if let Some(path) = session
            .agent_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
        {
            let normalized = path.trim_end_matches(&['/', '\\'][..]).replace('\\', "/");
            if let Some(task_name) = normalized
                .rsplit('/')
                .next()
                .filter(|value| !value.is_empty())
            {
                delegation.insert(
                    "task_name".to_string(),
                    Value::String(task_name.to_string()),
                );
            }
        }
        if let Some(create_time) = create_time {
            delegation.insert(
                "assigned_at".to_string(),
                Value::String(utc_iso(create_time)),
            );
        }
        delegation.insert("payload_encrypted".to_string(), Value::Bool(true));
        session.delegation = Some(Value::Object(delegation));
    }

    session.activities = session
        .activities
        .iter()
        .map(clean_dynamic_command_value)
        .collect();
    session.session_state = if session.turns.iter().any(|turn| {
        turn.as_object()
            .and_then(|map| value_str(map.get("status")))
            == Some("in_progress")
    }) {
        "active_at_export".to_string()
    } else {
        "idle".to_string()
    };

    if let Some(latest) = latest_session_payload_timestamp(&session) {
        session.last_message_time = session.last_message_time.max(latest);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Root/subagent aggregation, handoff construction and output
// ---------------------------------------------------------------------------

fn append_session_payload(output: &mut Obj, session: &Session) {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(31, 0);

    if let Some(value) = session.execution_profile.as_ref() {
        output.insert("execution_profile".to_string(), value.clone());
    }
    if let Some(value) = session.usage.as_ref() {
        output.insert("usage".to_string(), value.clone());
    }
    if let Some(value) = session.context.as_ref() {
        output.insert("context".to_string(), value.clone());
    }
    if let Some(value) = session.data_quality.as_ref() {
        output.insert("data_quality".to_string(), value.clone());
    }
    if !session.turns.is_empty() {
        output.insert("turns".to_string(), Value::Array(session.turns.clone()));
    }
    output.insert(
        "messages".to_string(),
        Value::Array(session.messages.clone()),
    );
    for (key, values) in [
        ("plans", &session.plans),
        ("plan_updates", &session.plan_updates),
        ("goals", &session.goals),
        ("goal_updates", &session.goal_updates),
        ("user_input_requests", &session.user_input_requests),
        ("agent_events", &session.agent_events),
        ("reviews", &session.reviews),
        ("memory_citations", &session.memory_citations),
        ("writing_blocks", &session.writing_blocks),
        ("ui_actions", &session.ui_actions),
        ("ui_parse_warnings", &session.ui_parse_warnings),
        ("rollbacks", &session.rollbacks),
        ("activities", &session.activities),
        ("changes", &session.changes),
    ] {
        if !values.is_empty() {
            output.insert(key.to_string(), Value::Array(values.clone()));
        }
    }
    if let Some(value) = session.delegation.as_ref() {
        output.insert("delegation".to_string(), value.clone());
    }
}

// Production candidate path: move payload arrays and nested JSON Values to the
// final handoff instead of deeply cloning them. Metadata required by export
// bookkeeping stays in Session, including session IDs and subagent IDs.
fn append_session_payload_move(output: &mut Obj, session: &mut Session) {
    if let Some(value) = session.execution_profile.take() {
        output.insert("execution_profile".to_string(), value);
    }
    if let Some(value) = session.usage.take() {
        output.insert("usage".to_string(), value);
    }
    if let Some(value) = session.context.take() {
        output.insert("context".to_string(), value);
    }
    if let Some(value) = session.data_quality.take() {
        output.insert("data_quality".to_string(), value);
    }
    if !session.turns.is_empty() {
        output.insert(
            "turns".to_string(),
            Value::Array(std::mem::take(&mut session.turns)),
        );
    }
    output.insert(
        "messages".to_string(),
        Value::Array(std::mem::take(&mut session.messages)),
    );
    for (key, values) in [
        ("plans", &mut session.plans),
        ("plan_updates", &mut session.plan_updates),
        ("goals", &mut session.goals),
        ("goal_updates", &mut session.goal_updates),
        ("user_input_requests", &mut session.user_input_requests),
        ("agent_events", &mut session.agent_events),
        ("reviews", &mut session.reviews),
        ("memory_citations", &mut session.memory_citations),
        ("writing_blocks", &mut session.writing_blocks),
        ("ui_actions", &mut session.ui_actions),
        ("ui_parse_warnings", &mut session.ui_parse_warnings),
        ("rollbacks", &mut session.rollbacks),
        ("activities", &mut session.activities),
        ("changes", &mut session.changes),
    ] {
        if !values.is_empty() {
            output.insert(key.to_string(), Value::Array(std::mem::take(values)));
        }
    }
    if let Some(value) = session.delegation.take() {
        output.insert("delegation".to_string(), value);
    }
}

fn build_subagent_output(session: &Session) -> Result<Value> {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(30, 0);

    let mut output = obj!(
        "session_id" => Value::String(session.session_id.clone()),
        "root_session_id" => Value::String(session.root_session_id.clone()),
        "thread_source" => Value::String("subagent".to_string()),
    );
    insert_optional_string(
        &mut output,
        "parent_thread_id",
        session.parent_thread_id.as_deref(),
    );
    insert_optional_string(
        &mut output,
        "forked_from_id",
        session.forked_from_id.as_deref(),
    );
    insert_optional_string(&mut output, "name", session.agent_nickname.as_deref());
    insert_optional_string(&mut output, "path", session.agent_path.as_deref());
    insert_optional_string(&mut output, "role", session.agent_role.as_deref());
    insert_optional_string(
        &mut output,
        "multi_agent_version",
        session.multi_agent_version.as_deref(),
    );
    if let Some(path) = session
        .agent_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
    {
        let normalized = path.trim_end_matches(&['/', '\\'][..]).replace('\\', "/");
        if let Some(task_name) = normalized
            .rsplit('/')
            .next()
            .filter(|value| !value.is_empty())
        {
            output.insert(
                "task_name".to_string(),
                Value::String(task_name.to_string()),
            );
        }
    }
    if let Some(depth) = session.agent_depth {
        output.insert("depth".to_string(), Value::Number(depth.into()));
    }
    output.insert(
        "create_time".to_string(),
        session
            .create_time
            .map(|value| Value::String(utc_iso(value)))
            .unwrap_or(Value::Null),
    );
    output.insert(
        "update_time".to_string(),
        Value::String(utc_iso(session.last_message_time)),
    );
    if !session.codex.is_null() {
        output.insert("codex".to_string(), session.codex.clone());
    }
    if !session.workspace.is_null() {
        output.insert("workspace".to_string(), session.workspace.clone());
    }
    output.insert(
        "session_state".to_string(),
        Value::String(session.session_state.clone()),
    );
    output.insert(
        "activity_detail".to_string(),
        Value::String(session.activity_detail.clone()),
    );
    append_session_payload(&mut output, session);
    Ok(Value::Object(output))
}

fn build_subagent_output_move(session: &mut Session) -> Result<Value> {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(30, 0);

    let mut output = obj!(
        "session_id" => Value::String(session.session_id.clone()),
        "root_session_id" => Value::String(session.root_session_id.clone()),
        "thread_source" => Value::String("subagent".to_string()),
    );
    insert_optional_string(
        &mut output,
        "parent_thread_id",
        session.parent_thread_id.as_deref(),
    );
    insert_optional_string(
        &mut output,
        "forked_from_id",
        session.forked_from_id.as_deref(),
    );
    insert_optional_string(&mut output, "name", session.agent_nickname.as_deref());
    insert_optional_string(&mut output, "path", session.agent_path.as_deref());
    insert_optional_string(&mut output, "role", session.agent_role.as_deref());
    insert_optional_string(
        &mut output,
        "multi_agent_version",
        session.multi_agent_version.as_deref(),
    );
    if let Some(path) = session
        .agent_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
    {
        let normalized = path.trim_end_matches(&['/', '\\'][..]).replace('\\', "/");
        if let Some(task_name) = normalized
            .rsplit('/')
            .next()
            .filter(|value| !value.is_empty())
        {
            output.insert(
                "task_name".to_string(),
                Value::String(task_name.to_string()),
            );
        }
    }
    if let Some(depth) = session.agent_depth {
        output.insert("depth".to_string(), Value::Number(depth.into()));
    }
    output.insert(
        "create_time".to_string(),
        session
            .create_time
            .map(|value| Value::String(utc_iso(value)))
            .unwrap_or(Value::Null),
    );
    output.insert(
        "update_time".to_string(),
        Value::String(utc_iso(session.last_message_time)),
    );
    if !session.codex.is_null() {
        output.insert("codex".to_string(), std::mem::take(&mut session.codex));
    }
    if !session.workspace.is_null() {
        output.insert(
            "workspace".to_string(),
            std::mem::take(&mut session.workspace),
        );
    }
    output.insert(
        "session_state".to_string(),
        Value::String(session.session_state.clone()),
    );
    output.insert(
        "activity_detail".to_string(),
        Value::String(session.activity_detail.clone()),
    );
    append_session_payload_move(&mut output, session);
    Ok(Value::Object(output))
}

fn is_subagent_session(session: &Session) -> bool {
    session.thread_source == "subagent"
        && session
            .parent_thread_id
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
}

fn guardian_root_id(
    session_id: &str,
    session: &Session,
    all_sessions: &HashMap<String, Session>,
    root_ids: &HashSet<String>,
) -> Option<String> {
    let root_id = session.root_session_id.trim();
    if !root_id.is_empty() && root_id != session_id && root_ids.contains(root_id) {
        return Some(root_id.to_string());
    }

    let mut parent_id = session.parent_thread_id.clone().unwrap_or_default();
    let mut visited = HashSet::from([session_id.to_string()]);
    while !parent_id.is_empty() && !visited.contains(&parent_id) {
        if root_ids.contains(&parent_id) {
            return Some(parent_id);
        }
        visited.insert(parent_id.clone());
        let parent = all_sessions.get(&parent_id)?;
        parent_id = parent.parent_thread_id.clone().unwrap_or_default();
    }
    None
}

fn integrate_subagent_sessions(
    sessions_by_id: HashMap<String, Session>,
) -> (HashMap<String, Session>, usize) {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(25, 0);

    // Resolve parents while all sessions remain accessible by reference.
    // Previously root and child Sessions were deeply cloned before clearing
    // the original map; this two-phase plan moves each Session exactly once.
    let root_ids: HashSet<String> = sessions_by_id
        .iter()
        .filter(|(_, session)| !is_subagent_session(session) && !session.guardian_review)
        .map(|(id, _)| id.clone())
        .collect();

    let mut child_roots = HashMap::<String, String>::new();
    let mut guardian_updates = Vec::<(String, String, Timestamp, bool, bool)>::new();
    for (session_id, session) in &sessions_by_id {
        if session.guardian_review {
            if let Some(root_id) = guardian_root_id(session_id, session, &sessions_by_id, &root_ids)
            {
                guardian_updates.push((
                    root_id,
                    session_id.clone(),
                    session.last_message_time,
                    session.session_state == "active_at_export",
                    is_subagent_session(session),
                ));
            }
            continue;
        }
        if !is_subagent_session(session) {
            continue;
        }
        let mut root_id = session.root_session_id.trim().to_string();
        if root_id.is_empty() || root_id == *session_id || !root_ids.contains(&root_id) {
            let mut parent_id = session.parent_thread_id.clone().unwrap_or_default();
            let mut visited = HashSet::from([session_id.clone()]);
            while !parent_id.is_empty() && !visited.contains(&parent_id) {
                visited.insert(parent_id.clone());
                let Some(parent) = sessions_by_id.get(&parent_id) else {
                    break;
                };
                if !is_subagent_session(parent) && !parent.guardian_review {
                    root_id = parent_id.clone();
                    break;
                }
                parent_id = parent.parent_thread_id.clone().unwrap_or_default();
            }
        }
        if root_ids.contains(&root_id) {
            child_roots.insert(session_id.clone(), root_id);
        }
    }

    let mut roots = HashMap::<String, Session>::with_capacity(root_ids.len());
    let mut grouped = HashMap::<String, Vec<Session>>::new();
    let mut unattached = HashMap::<String, Session>::new();
    for (session_id, session) in sessions_by_id {
        if session.guardian_review {
            // Guardian payload is intentionally not exported.
            continue;
        }
        if !is_subagent_session(&session) {
            #[cfg(feature = "integrated-study")]
            let _study_scope = integrated_study::scope(26, 0);
            roots.insert(session_id, session);
        } else if let Some(root_id) = child_roots.remove(&session_id) {
            #[cfg(feature = "integrated-study")]
            let _study_scope = integrated_study::scope(27, 0);
            grouped.entry(root_id).or_default().push(session);
        } else {
            #[cfg(feature = "integrated-study")]
            let _study_scope = integrated_study::scope(27, 0);
            unattached.insert(session_id, session);
        }
    }

    // Preserve r9's legacy Guardian timestamp/state behavior. The list is
    // collected in the same hash-map iteration order as the former clone path.
    for (root_id, guardian_id, latest, active, legacy_subagent) in guardian_updates {
        if let Some(root) = roots.get_mut(&root_id) {
            root.internal_session_ids.push(guardian_id);
            if legacy_subagent {
                root.last_message_time = root.last_message_time.max(latest);
                if active {
                    root.session_state = "active_at_export".to_string();
                }
            }
        }
    }

    let mut integrated_count = 0usize;
    for (root_id, mut children) in grouped {
        children.sort_by(|left, right| {
            left.create_time
                .unwrap_or(MAX_TS)
                .cmp(&right.create_time.unwrap_or(MAX_TS))
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        if let Some(root) = roots.get_mut(&root_id) {
            if let Some(last_time) = children.iter().map(|child| child.last_message_time).max() {
                root.last_message_time = root.last_message_time.max(last_time);
            }
            if children
                .iter()
                .any(|child| child.session_state == "active_at_export")
            {
                root.session_state = "active_at_export".to_string();
            }
            integrated_count += children.len();
            root.subagent_sessions = children;
        }
    }
    roots.extend(unattached);
    (roots, integrated_count)
}

fn output_numbering_width(total_handoffs: usize) -> usize {
    total_handoffs.max(1).to_string().len()
}

fn conversation_ordinals(sessions: &[Session]) -> HashMap<String, usize> {
    let mut ordered: Vec<&Session> = sessions.iter().collect();
    ordered.sort_by(|left, right| {
        left.create_time
            .unwrap_or(MAX_TS)
            .cmp(&right.create_time.unwrap_or(MAX_TS))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, session)| (session.session_id.clone(), index + 1))
        .collect()
}

fn numbered_handoff_filename(base_filename: &str, ordinal: usize, total_handoffs: usize) -> String {
    let width = output_numbering_width(total_handoffs);
    format!("{:0width$}. {}", ordinal, base_filename, width = width)
}

fn build_handoff(
    session: &Session,
    title_index: &HashMap<String, String>,
) -> Result<(String, Value)> {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(29, 0);

    let title = title_index
        .get(&session.session_id)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| fallback_title(&session.raw_messages, &session.session_id));
    let safe_title = sanitize_filename_part(&title, &session.session_id);
    let filename = format!(
        "{}-{}.handoff.json",
        safe_title,
        filename_timestamp_gmt8(session.last_message_time)
    );
    let mut handoff = obj!(
        "title" => Value::String(title),
        "create_time" => session.create_time.map(|value| Value::String(utc_iso(value))).unwrap_or(Value::Null),
        "update_time" => Value::String(utc_iso(session.last_message_time)),
        "conversation_id" => Value::String(session.session_id.clone()),
        "exporter_version" => Value::String(VERSION.to_string()),
        "codex" => session.codex.clone(),
        "workspace" => session.workspace.clone(),
        "session_state" => Value::String(session.session_state.clone()),
        "activity_detail" => Value::String(session.activity_detail.clone()),
    );
    append_session_payload(&mut handoff, session);
    if !session.subagent_sessions.is_empty() {
        let mut subagents = Vec::new();
        for child in &session.subagent_sessions {
            subagents.push(build_subagent_output(child)?);
        }
        if !subagents.is_empty() {
            handoff.insert("subagents".to_string(), Value::Array(subagents));
        }
    }
    Ok((filename, Value::Object(handoff)))
}

// The normal parallel production path owns its data; legacy export uses build_handoff().
fn build_handoff_move(
    session: &mut Session,
    title_index: &HashMap<String, String>,
) -> Result<(String, Value)> {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(29, 0);

    let title = title_index
        .get(&session.session_id)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| fallback_title(&session.raw_messages, &session.session_id));
    let safe_title = sanitize_filename_part(&title, &session.session_id);
    let filename = format!(
        "{}-{}.handoff.json",
        safe_title,
        filename_timestamp_gmt8(session.last_message_time)
    );
    let mut handoff = obj!(
        "title" => Value::String(title),
        "create_time" => session.create_time.map(|value| Value::String(utc_iso(value))).unwrap_or(Value::Null),
        "update_time" => Value::String(utc_iso(session.last_message_time)),
        "conversation_id" => Value::String(session.session_id.clone()),
        "exporter_version" => Value::String(VERSION.to_string()),
        "codex" => std::mem::take(&mut session.codex),
        "workspace" => std::mem::take(&mut session.workspace),
        "session_state" => Value::String(session.session_state.clone()),
        "activity_detail" => Value::String(session.activity_detail.clone()),
    );
    append_session_payload_move(&mut handoff, session);
    if !session.subagent_sessions.is_empty() {
        let mut subagents = Vec::new();
        for child in &mut session.subagent_sessions {
            subagents.push(build_subagent_output_move(child)?);
        }
        if !subagents.is_empty() {
            handoff.insert("subagents".to_string(), Value::Array(subagents));
        }
    }
    Ok((filename, Value::Object(handoff)))
}

struct CrlfVecWriter {
    bytes: Vec<u8>,
    fast_scan: bool,
}

impl CrlfVecWriter {
    fn new(fast_scan: bool) -> Self {
        let bytes = if fast_scan {
            // Most handoffs are well above a few KiB. A modest reserve avoids the first
            // reallocations without retaining excessive unused capacity across the corpus.
            Vec::with_capacity(64 * 1024)
        } else {
            Vec::new()
        };
        Self { bytes, fast_scan }
    }
}

impl Write for CrlfVecWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut start = 0usize;
        if self.fast_scan {
            for index in memchr_iter(b'\n', buf) {
                self.bytes.extend_from_slice(&buf[start..index]);
                self.bytes.extend_from_slice(b"\r\n");
                start = index + 1;
            }
        } else {
            for (index, byte) in buf.iter().enumerate() {
                if *byte == b'\n' {
                    self.bytes.extend_from_slice(&buf[start..index]);
                    self.bytes.extend_from_slice(b"\r\n");
                    start = index + 1;
                }
            }
        }
        self.bytes.extend_from_slice(&buf[start..]);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_handoff_legacy(handoff: &Value, serializer: SerializerMode) -> Result<Vec<u8>> {
    let mut bytes = match serializer {
        SerializerMode::Orjson2 => serde_json::to_vec_pretty(handoff)?,
        SerializerMode::Compact => serde_json::to_vec(handoff)?,
    };
    if serializer == SerializerMode::Orjson2 {
        let mut crlf = Vec::with_capacity(
            bytes.len() + bytes.iter().filter(|byte| **byte == b'\n').count() + 2,
        );
        for byte in bytes {
            if byte == b'\n' {
                crlf.extend_from_slice(b"\r\n");
            } else {
                crlf.push(byte);
            }
        }
        crlf.extend_from_slice(b"\r\n");
        bytes = crlf;
    } else {
        bytes.extend_from_slice(b"\r\n");
    }
    Ok(bytes)
}

fn serialize_handoff_with_fast_crlf(
    handoff: &Value,
    serializer: SerializerMode,
    fast_crlf: bool,
) -> Result<Vec<u8>> {
    if LEGACY_OUTPUT_IO_ENABLED.load(AtomicOrdering::Relaxed) {
        return serialize_handoff_legacy(handoff, serializer);
    }
    if serializer == SerializerMode::Compact {
        let mut bytes = serde_json::to_vec(handoff)?;
        bytes.push(b'\n');
        return Ok(bytes);
    }
    let mut writer = CrlfVecWriter::new(fast_crlf);
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"  ");
    {
        let mut json = serde_json::Serializer::with_formatter(&mut writer, formatter);
        handoff.serialize(&mut json)?;
    }
    writer.bytes.extend_from_slice(b"\r\n");
    Ok(writer.bytes)
}

fn serialize_handoff(handoff: &Value, serializer: SerializerMode) -> Result<Vec<u8>> {
    #[cfg(feature = "integrated-study")]
    let _study_scope = integrated_study::scope(33, 0);

    let fast_crlf = FAST_CRLF_SERIALIZER_ENABLED.load(AtomicOrdering::Relaxed);
    if serializer == SerializerMode::Compact
        || LEGACY_OUTPUT_IO_ENABLED.load(AtomicOrdering::Relaxed)
        || LEGACY_CRLF_WRITER_ENABLED.load(AtomicOrdering::Relaxed)
        || fast_crlf
    {
        // Explicit historical diagnostics retain their original behavior.
        return serialize_handoff_with_fast_crlf(handoff, serializer, fast_crlf);
    }
    Ok(crlf_formatter::to_vec_pretty(handoff)?)
}

fn read_existing_conversation_id_legacy(path: &Path) -> Result<Option<String>> {
    let bytes = fs::read(path)
        .with_context(|| format!("unable to read existing handoff {}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid existing handoff JSON {}", path.display()))?;
    Ok(value
        .as_object()
        .and_then(|map| value_str(map.get("conversation_id")))
        .map(str::to_string))
}

fn conversation_id_from_prefix(bytes: &[u8]) -> Option<String> {
    const MARKER: &[u8] = b"\"conversation_id\"";
    let marker_index = bytes
        .windows(MARKER.len())
        .position(|window| window == MARKER)?;
    let mut index = marker_index + MARKER.len();
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index) != Some(&b':') {
        return None;
    }
    index += 1;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index) != Some(&b'"') {
        return None;
    }
    index += 1;
    let start = index;
    while let Some(byte) = bytes.get(index) {
        if *byte == b'"' {
            let raw = std::str::from_utf8(&bytes[start..index]).ok()?;
            if raw.len() >= 8
                && raw
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            {
                return Some(raw.to_string());
            }
            return None;
        }
        if *byte == b'\\' || *byte < 0x20 {
            return None;
        }
        index += 1;
    }
    None
}

fn read_existing_conversation_id(path: &Path) -> Result<Option<String>> {
    if LEGACY_OUTPUT_IO_ENABLED.load(AtomicOrdering::Relaxed) {
        return read_existing_conversation_id_legacy(path);
    }
    let mut file = File::open(path)
        .with_context(|| format!("unable to read existing handoff {}", path.display()))?;
    let mut prefix = Vec::with_capacity(4096);
    Read::by_ref(&mut file)
        .take(16 * 1024)
        .read_to_end(&mut prefix)?;
    if let Some(conversation_id) = conversation_id_from_prefix(&prefix) {
        return Ok(Some(conversation_id));
    }
    read_existing_conversation_id_legacy(path)
}

fn existing_conversation_id_for_path<'a>(
    existing_exports: &'a HashMap<String, Vec<PathBuf>>,
    path: &Path,
) -> Option<&'a str> {
    existing_exports
        .iter()
        .find_map(|(conversation_id, paths)| {
            paths
                .iter()
                .any(|candidate| candidate == path)
                .then_some(conversation_id.as_str())
        })
}

fn read_existing_exports(directory: &Path) -> HashMap<String, Vec<PathBuf>> {
    let mut result: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file()
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".handoff.json"))
        {
            continue;
        }
        match read_existing_conversation_id(&path) {
            Ok(Some(conversation_id)) => result.entry(conversation_id).or_default().push(path),
            Ok(None) => {}
            Err(error) => eprintln!(
                "WARNING: Skipping unreadable existing handoff file: {}\n{error:#}",
                path.display()
            ),
        }
    }
    result
}

fn existing_handoff_paths(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let is_file = entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false);
        if !is_file {
            continue;
        }
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".handoff.json"))
        {
            paths.push(path);
        }
    }
    paths
}

fn read_existing_exports_parallel(
    directory: &Path,
    pool: &rayon::ThreadPool,
) -> HashMap<String, Vec<PathBuf>> {
    let paths = existing_handoff_paths(directory);
    let indexed: Vec<(PathBuf, Result<Option<String>>)> = pool.install(|| {
        paths
            .par_iter()
            .map(|path| (path.clone(), read_existing_conversation_id(path)))
            .collect()
    });
    let mut result: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (path, indexed_id) in indexed {
        match indexed_id {
            Ok(Some(conversation_id)) => result.entry(conversation_id).or_default().push(path),
            Ok(None) => {}
            Err(error) => eprintln!(
                "WARNING: Skipping unreadable existing handoff file: {}\n{error:#}",
                path.display()
            ),
        }
    }
    result
}

fn reverse_existing_exports(
    existing_exports: &HashMap<String, Vec<PathBuf>>,
) -> HashMap<PathBuf, String> {
    let mut result = HashMap::new();
    for (conversation_id, paths) in existing_exports {
        for path in paths {
            result.insert(path.clone(), conversation_id.clone());
        }
    }
    result
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("unable to remove old output {}", path.display()))
        }
    }
}

fn timestamp_to_system_time(timestamp: Timestamp) -> std::time::SystemTime {
    let seconds = timestamp.div_euclid(1_000_000);
    let micros = timestamp.rem_euclid(1_000_000) as u64;
    if seconds >= 0 {
        std::time::UNIX_EPOCH + Duration::from_secs(seconds as u64) + Duration::from_micros(micros)
    } else {
        std::time::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
            + Duration::from_micros(micros)
    }
}

fn set_export_handle_timestamps(
    file: &File,
    path: &Path,
    create_time: Timestamp,
    update_time: Timestamp,
) -> Result<()> {
    let mut times = fs::FileTimes::new()
        .set_accessed(timestamp_to_system_time(update_time))
        .set_modified(timestamp_to_system_time(update_time));
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTimesExt;
        times = times.set_created(timestamp_to_system_time(create_time));
    }
    file.set_times(times)
        .with_context(|| format!("unable to set timestamps {}", path.display()))?;
    Ok(())
}

fn set_export_file_timestamps(
    path: &Path,
    create_time: Timestamp,
    update_time: Timestamp,
) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("unable to open output timestamps {}", path.display()))?;
    set_export_handle_timestamps(&file, path, create_time, update_time)
}

fn write_export_temp(
    temp_path: &Path,
    data: &[u8],
    create_time: Timestamp,
    update_time: Timestamp,
) -> Result<()> {
    if LEGACY_OUTPUT_IO_ENABLED.load(AtomicOrdering::Relaxed) {
        fs::write(temp_path, data)
            .with_context(|| format!("unable to write {}", temp_path.display()))?;
        return Ok(());
    }
    let mut file = File::create(temp_path)
        .with_context(|| format!("unable to create {}", temp_path.display()))?;
    file.write_all(data)
        .with_context(|| format!("unable to write {}", temp_path.display()))?;
    set_export_handle_timestamps(&file, temp_path, create_time, update_time)?;
    Ok(())
}

fn write_handoff_temp(
    temp_path: &Path,
    handoff: &Value,
    serializer: SerializerMode,
    create_time: Timestamp,
    update_time: Timestamp,
) -> Result<u64> {
    // v0.6.6 showed that direct serde streaming was slower on this fixed Windows host.
    // Keep both pretty and compact modes as contiguous-buffer serialization followed by
    // one write_all call; compact remains an A/B diagnostic, not the production default.
    let data = serialize_handoff(handoff, serializer)?;
    write_export_temp(temp_path, &data, create_time, update_time)?;
    Ok(data.len() as u64)
}

static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn export_codex_session(
    session: &Session,
    title_index: &HashMap<String, String>,
    existing_exports: &mut HashMap<String, Vec<PathBuf>>,
    output_directory: &Path,
    serializer: SerializerMode,
    ordinal: usize,
    total_handoffs: usize,
) -> Result<PathBuf> {
    let (base_filename, handoff) = build_handoff(session, title_index)?;
    let filename = numbered_handoff_filename(&base_filename, ordinal, total_handoffs);
    let final_path = output_directory.join(filename);
    if final_path.is_file() {
        let current = if LEGACY_OUTPUT_IO_ENABLED.load(AtomicOrdering::Relaxed) {
            read_existing_conversation_id(&final_path).ok().flatten()
        } else {
            existing_conversation_id_for_path(existing_exports, &final_path).map(str::to_string)
        };
        if current.as_deref() != Some(session.session_id.as_str()) {
            bail!(
                "Output filename collision with another file: {}",
                final_path.display()
            );
        }
    }
    let create_time = session.create_time.ok_or_else(|| {
        anyhow!(
            "Conversation create_time is missing for {}",
            session.session_id
        )
    })?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_path = output_directory.join(format!(
        ".{}.{}.{}.tmp",
        session.session_id,
        std::process::id(),
        sequence
    ));
    let _data_len = write_handoff_temp(
        &temp_path,
        &handoff,
        serializer,
        create_time,
        session.last_message_time,
    )?;
    let result = (|| -> Result<PathBuf> {
        let mut obsolete_ids = vec![session.session_id.clone()];
        obsolete_ids.extend(
            session
                .subagent_sessions
                .iter()
                .map(|child| child.session_id.clone()),
        );
        obsolete_ids.extend(session.internal_session_ids.iter().cloned());
        for obsolete_id in &obsolete_ids {
            if let Some(paths) = existing_exports.get(obsolete_id) {
                for old_path in paths {
                    if old_path.is_file() {
                        fs::remove_file(old_path).with_context(|| {
                            format!("unable to remove old output {}", old_path.display())
                        })?;
                    }
                }
            }
            if obsolete_id != &session.session_id {
                existing_exports.remove(obsolete_id);
            }
        }
        if final_path.exists() {
            fs::remove_file(&final_path)
                .with_context(|| format!("unable to replace {}", final_path.display()))?;
        }
        fs::rename(&temp_path, &final_path).with_context(|| {
            format!(
                "unable to move {} to {}",
                temp_path.display(),
                final_path.display()
            )
        })?;
        if LEGACY_OUTPUT_IO_ENABLED.load(AtomicOrdering::Relaxed) {
            set_export_file_timestamps(&final_path, create_time, session.last_message_time)?;
        }
        existing_exports.insert(session.session_id.clone(), vec![final_path.clone()]);
        Ok(final_path)
    })();
    let _ = fs::remove_file(&temp_path);
    result
}

#[derive(Default, Clone, Debug)]
struct ExportPhaseMetrics {
    prepare: Duration,
    cleanup: Duration,
    finalize: Duration,
}

#[derive(Debug)]
struct PreparedExport {
    temp_path: PathBuf,
    final_path: PathBuf,
    data_len: u64,
    create_time: Timestamp,
    update_time: Timestamp,
}

type BuiltCandidate = (String, Value, Option<Vec<u8>>, Option<PreparedExport>);

#[derive(Debug)]
struct StagingDirectory {
    path: PathBuf,
}

impl StagingDirectory {
    fn create(output_directory: &Path) -> Result<Self> {
        let parent = output_directory.parent().ok_or_else(|| {
            anyhow!(
                "Output directory has no parent for same-volume staging: {}",
                output_directory.display()
            )
        })?;
        for _ in 0..32 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
            let path = parent.join(format!(
                ".Codex-Handoff-Stage-{}.{}",
                std::process::id(),
                sequence
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("unable to create staging directory {}", path.display())
                    });
                }
            }
        }
        bail!(
            "Unable to allocate a unique staging directory next to {}",
            output_directory.display()
        )
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn next_export_temp_path(temp_directory: &Path, session_id: &str) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
    temp_directory.join(format!(
        ".{}.{}.{}.tmp",
        session_id,
        std::process::id(),
        sequence
    ))
}

fn prepare_candidate_export(
    session: &Session,
    filename: &str,
    handoff: &Value,
    pre_serialized: Option<&[u8]>,
    temp_directory: &Path,
    output_directory: &Path,
    serializer: SerializerMode,
) -> Result<PreparedExport> {
    let create_time = session.create_time.ok_or_else(|| {
        anyhow!(
            "Conversation create_time is missing for {}",
            session.session_id
        )
    })?;
    let temp_path = next_export_temp_path(temp_directory, &session.session_id);
    let data_len = if let Some(data) = pre_serialized {
        match write_export_temp(&temp_path, data, create_time, session.last_message_time) {
            Ok(()) => data.len() as u64,
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                return Err(error);
            }
        }
    } else {
        match write_handoff_temp(
            &temp_path,
            handoff,
            serializer,
            create_time,
            session.last_message_time,
        ) {
            Ok(data_len) => data_len,
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                return Err(error);
            }
        }
    };
    Ok(PreparedExport {
        temp_path,
        final_path: output_directory.join(filename),
        data_len,
        create_time,
        update_time: session.last_message_time,
    })
}

fn cleanup_prepared_temp_files(prepared: &[PreparedExport]) {
    for item in prepared {
        let _ = fs::remove_file(&item.temp_path);
    }
}

fn cleanup_optional_prepared(prepared: &Option<Vec<PreparedExport>>) {
    if let Some(prepared) = prepared.as_ref() {
        cleanup_prepared_temp_files(prepared);
    }
}

fn export_prebuilt_candidates_parallel(
    sessions: &[Session],
    candidates: &[(String, Value)],
    pre_serialized: Option<&[Vec<u8>]>,
    pre_prepared: Option<Vec<PreparedExport>>,
    existing_exports: &HashMap<String, Vec<PathBuf>>,
    output_directory: &Path,
    temp_directory: &Path,
    serializer: SerializerMode,
    write_lanes: usize,
    cleanup_lanes: usize,
    finalize_lanes: usize,
    defer_planned_final_delete: bool,
    replace_existing_final: bool,
    pool: &rayon::ThreadPool,
) -> Result<(u64, ExportPhaseMetrics)> {
    if sessions.len() != candidates.len() {
        cleanup_optional_prepared(&pre_prepared);
        bail!(
            "Internal export plan mismatch: {} session(s) but {} candidate handoff(s)",
            sessions.len(),
            candidates.len()
        );
    }

    if let Some(serialized) = pre_serialized {
        if serialized.len() != sessions.len() {
            cleanup_optional_prepared(&pre_prepared);
            bail!(
                "Internal pre-serialization plan mismatch: {} byte buffer(s) but {} session(s)",
                serialized.len(),
                sessions.len()
            );
        }
    }
    if let Some(prepared) = pre_prepared.as_ref() {
        if prepared.len() != sessions.len() {
            cleanup_prepared_temp_files(prepared);
            bail!(
                "Internal pipelined prepare mismatch: {} prepared temp(s) but {} session(s)",
                prepared.len(),
                sessions.len()
            );
        }
    }

    let legacy_bookkeeping = LEGACY_EXPORT_BOOKKEEPING_ENABLED.load(AtomicOrdering::Relaxed);
    let existing_by_path =
        (!legacy_bookkeeping).then(|| reverse_existing_exports(existing_exports));

    // Preserve the legacy collision rule before removing any existing output.
    let mut planned_paths = HashMap::<PathBuf, String>::new();
    for (session, (filename, _)) in sessions.iter().zip(candidates.iter()) {
        let final_path = output_directory.join(filename);
        if let Some(other_session_id) =
            planned_paths.insert(final_path.clone(), session.session_id.clone())
        {
            if other_session_id != session.session_id {
                cleanup_optional_prepared(&pre_prepared);
                bail!(
                    "Output filename collision between conversations {} and {}: {}",
                    other_session_id,
                    session.session_id,
                    final_path.display()
                );
            }
        }
        if legacy_bookkeeping {
            if final_path.is_file() {
                let current = if LEGACY_OUTPUT_IO_ENABLED.load(AtomicOrdering::Relaxed) {
                    read_existing_conversation_id(&final_path).ok().flatten()
                } else {
                    existing_conversation_id_for_path(existing_exports, &final_path)
                        .map(str::to_string)
                };
                if current.as_deref() != Some(session.session_id.as_str()) {
                    cleanup_optional_prepared(&pre_prepared);
                    bail!(
                        "Output filename collision with another file: {}",
                        final_path.display()
                    );
                }
            }
        } else if let Some(existing_by_path) = existing_by_path.as_ref() {
            if let Some(current) = existing_by_path.get(&final_path) {
                if current != &session.session_id {
                    cleanup_optional_prepared(&pre_prepared);
                    bail!(
                        "Output filename collision with another file: {}",
                        final_path.display()
                    );
                }
            } else if final_path.exists() {
                // Preserve collision safety for an unreadable/unindexed file. This metadata
                // probe is only needed for the exceptional path, not for normal rewrites.
                cleanup_optional_prepared(&pre_prepared);
                bail!(
                    "Output filename collision with unindexed file: {}",
                    final_path.display()
                );
            }
        }
    }

    // Reuse the already-built candidate handoff instead of rebuilding every root/subagent tree.
    // Baseline mode performs serialization/temp writes here. Pipeline mode already prepared
    // every temp file during the candidate-build pass and skips this second Rayon traversal.
    let (prepared, prepare_duration) = if let Some(prepared) = pre_prepared {
        (prepared, Duration::ZERO)
    } else {
        let prepare_one = |index: usize| -> Result<PreparedExport> {
            let session = &sessions[index];
            let (filename, handoff) = &candidates[index];
            let serialized = pre_serialized.map(|all| all[index].as_slice());
            prepare_candidate_export(
                session,
                filename,
                handoff,
                serialized,
                temp_directory,
                output_directory,
                serializer,
            )
        };
        let prepare_start = Instant::now();
        let prepared_results: Vec<Result<PreparedExport>> = if write_lanes == 0 {
            pool.install(|| {
                (0..sessions.len())
                    .into_par_iter()
                    .map(prepare_one)
                    .collect()
            })
        } else {
            let ranges = export_lane_ranges(sessions.len(), write_lanes);
            let grouped: Vec<Vec<Result<PreparedExport>>> = pool.install(|| {
                ranges
                    .par_iter()
                    .map(|range| range.clone().map(&prepare_one).collect())
                    .collect()
            });
            grouped.into_iter().flatten().collect()
        };

        let mut prepared = Vec::with_capacity(prepared_results.len());
        let mut first_error = None;
        for result in prepared_results {
            match result {
                Ok(item) => prepared.push(item),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            cleanup_prepared_temp_files(&prepared);
            return Err(error);
        }
        (prepared, prepare_start.elapsed())
    };
    let mut phases = ExportPhaseMetrics {
        prepare: prepare_duration,
        ..ExportPhaseMetrics::default()
    };

    let cleanup_start = Instant::now();
    // Compute the same obsolete root/subagent output set as the legacy exporter.
    let mut obsolete_paths = HashSet::<PathBuf>::new();
    for session in sessions {
        let mut obsolete_ids = vec![session.session_id.as_str()];
        obsolete_ids.extend(
            session
                .subagent_sessions
                .iter()
                .map(|child| child.session_id.as_str()),
        );
        obsolete_ids.extend(session.internal_session_ids.iter().map(String::as_str));
        for obsolete_id in obsolete_ids {
            if let Some(paths) = existing_exports.get(obsolete_id) {
                obsolete_paths.extend(paths.iter().cloned());
            }
        }
    }

    if replace_existing_final || defer_planned_final_delete {
        // Planned final paths are handled by the finalization strategy. Keep cleanup
        // focused on genuinely obsolete filenames/subagent outputs so its concurrency
        // can be tuned independently from final rename concurrency.
        obsolete_paths.retain(|path| !planned_paths.contains_key(path));
    }
    let obsolete_paths: Vec<PathBuf> = obsolete_paths.into_iter().collect();
    let remove_one = |index: usize| -> Result<()> {
        let old_path = &obsolete_paths[index];
        if legacy_bookkeeping {
            if old_path.is_file() {
                fs::remove_file(old_path).with_context(|| {
                    format!("unable to remove old output {}", old_path.display())
                })?;
            }
            Ok(())
        } else {
            remove_file_if_present(old_path)
        }
    };
    let remove_results: Vec<Result<()>> = if cleanup_lanes == 0 {
        pool.install(|| {
            (0..obsolete_paths.len())
                .into_par_iter()
                .map(remove_one)
                .collect()
        })
    } else {
        let ranges = export_lane_ranges(obsolete_paths.len(), cleanup_lanes);
        let grouped: Vec<Vec<Result<()>>> = pool.install(|| {
            ranges
                .par_iter()
                .map(|range| range.clone().map(&remove_one).collect())
                .collect()
        });
        grouped.into_iter().flatten().collect()
    };
    if let Some(error) = remove_results.into_iter().find_map(|result| result.err()) {
        cleanup_prepared_temp_files(&prepared);
        return Err(error);
    }
    phases.cleanup = cleanup_start.elapsed();

    progress::stage("寫入 Handoff", prepared.len());
    let finalize_start = Instant::now();
    // Final paths are unique after preflight, so rename + timestamp updates can run independently.
    let finalize_one = |index: usize| -> Result<()> {
        let item = &prepared[index];
        if defer_planned_final_delete && !replace_existing_final {
            // All temp writes and collision checks have already succeeded. Localize the
            // planned-final delete next to its rename instead of deleting every current
            // output in a separate global cleanup barrier.
            remove_file_if_present(&item.final_path)
                .with_context(|| format!("unable to replace {}", item.final_path.display()))?;
        } else if legacy_bookkeeping && !replace_existing_final && item.final_path.exists() {
            fs::remove_file(&item.final_path)
                .with_context(|| format!("unable to replace {}", item.final_path.display()))?;
        }
        fs::rename(&item.temp_path, &item.final_path).with_context(|| {
            format!(
                "unable to move {} to {}",
                item.temp_path.display(),
                item.final_path.display()
            )
        })?;
        if LEGACY_OUTPUT_IO_ENABLED.load(AtomicOrdering::Relaxed) {
            set_export_file_timestamps(&item.final_path, item.create_time, item.update_time)?;
        }
        progress::wrote();
        Ok(())
    };
    let finalize_results: Vec<Result<()>> = if finalize_lanes == 0 {
        pool.install(|| {
            (0..prepared.len())
                .into_par_iter()
                .map(finalize_one)
                .collect()
        })
    } else {
        let ranges = export_lane_ranges(prepared.len(), finalize_lanes);
        let grouped: Vec<Vec<Result<()>>> = pool.install(|| {
            ranges
                .par_iter()
                .map(|range| range.clone().map(&finalize_one).collect())
                .collect()
        });
        grouped.into_iter().flatten().collect()
    };
    if let Some(error) = finalize_results.into_iter().find_map(|result| result.err()) {
        cleanup_prepared_temp_files(&prepared);
        return Err(error);
    }
    phases.finalize = finalize_start.elapsed();

    Ok((prepared.iter().map(|item| item.data_len).sum(), phases))
}

fn is_zero_byte_rollout(path: &Path) -> bool {
    !rollout_codec::is_compressed_rollout(path)
        && fs::metadata(path).is_ok_and(|metadata| metadata.len() == 0)
}

fn discover_rollout_entries(root: &Path) -> Result<Vec<(PathBuf, u64)>> {
    use std::collections::BTreeMap;
    let mut by_logical_path = BTreeMap::<PathBuf, (PathBuf, u64)>::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry =
            entry.with_context(|| format!("cannot scan sessions directory {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("rollout-")
            || !(name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
        {
            continue;
        }
        let size = fs::metadata(&path)
            .with_context(|| format!("cannot stat rollout {}", path.display()))?
            .len();
        let compressed = rollout_codec::is_compressed_rollout(&path);
        let logical = if compressed {
            path.with_extension("")
        } else {
            path.clone()
        };
        match by_logical_path.entry(logical) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert((path, size));
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                // Prefer a nonempty .jsonl, as Codex does. A zero-byte .jsonl
                // can be a transient placeholder during decompression: when a
                // usable .zst is present, never let the placeholder hide it.
                let existing_compressed = rollout_codec::is_compressed_rollout(&o.get().0);
                if (!compressed && size > 0)
                    || (!existing_compressed && o.get().1 == 0 && compressed)
                {
                    o.insert((path, size));
                }
            }
        }
    }
    Ok(by_logical_path.into_values().collect())
}

fn discover_rollouts(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(discover_rollout_entries(root)?
        .into_iter()
        .map(|(path, _)| path)
        .collect())
}

fn discover_rollouts_with_sizes(root: &Path) -> Result<Vec<(PathBuf, u64)>> {
    let mut files = discover_rollout_entries(root)?;
    files.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    Ok(files)
}

fn json_semantic_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_semantic_equal(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, value)| {
                    b.get(key)
                        .is_some_and(|other| json_semantic_equal(value, other))
                })
        }
        _ => false,
    }
}

fn first_json_difference(left: &Value, right: &Value, path: &str) -> Option<String> {
    match (left, right) {
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Some(format!("{path}: array length {} != {}", a.len(), b.len()));
            }
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                if let Some(diff) = first_json_difference(x, y, &format!("{path}[{index}]")) {
                    return Some(diff);
                }
            }
            None
        }
        (Value::Object(a), Value::Object(b)) => {
            let a_keys: HashSet<&String> = a.keys().collect();
            let b_keys: HashSet<&String> = b.keys().collect();
            if a_keys != b_keys {
                let missing: Vec<&String> = a_keys.difference(&b_keys).copied().collect();
                let extra: Vec<&String> = b_keys.difference(&a_keys).copied().collect();
                return Some(format!("{path}: missing={missing:?} extra={extra:?}"));
            }
            for (key, value) in a {
                let other = b.get(key).expect("key sets already compared");
                if let Some(diff) = first_json_difference(value, other, &format!("{path}.{key}")) {
                    return Some(diff);
                }
            }
            None
        }
        _ if json_semantic_equal(left, right) => None,
        (Value::String(a), Value::String(b)) => {
            let left_chars: Vec<char> = a.chars().collect();
            let right_chars: Vec<char> = b.chars().collect();
            let shared = left_chars.len().min(right_chars.len());
            let index = (0..shared)
                .find(|&index| left_chars[index] != right_chars[index])
                .unwrap_or(shared);
            let line = left_chars[..index.min(left_chars.len())]
                .iter()
                .filter(|&&ch| ch == '\n')
                .count()
                + 1;
            let context_start = index.saturating_sub(80);
            let left_end = (index + 120).min(left_chars.len());
            let right_end = (index + 120).min(right_chars.len());
            let left_context: String = left_chars[context_start.min(left_chars.len())..left_end]
                .iter()
                .collect();
            let right_context: String = right_chars
                [context_start.min(right_chars.len())..right_end]
                .iter()
                .collect();
            Some(format!(
                "{path}: string mismatch at char {index}, line {line}; left_chars={} right_chars={}; left_context={left_context:?}; right_context={right_context:?}",
                left_chars.len(),
                right_chars.len(),
            ))
        }
        _ => Some(format!("{path}: left={left} right={right}")),
    }
}

fn load_reference_handoffs(directory: &Path) -> Result<HashMap<String, (PathBuf, Value)>> {
    let mut output = HashMap::new();
    for entry in fs::read_dir(directory)
        .with_context(|| format!("unable to read reference {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file()
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".handoff.json"))
        {
            continue;
        }
        let value: Value = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid reference JSON {}", path.display()))?;
        let conversation_id = value
            .as_object()
            .and_then(|map| value_str(map.get("conversation_id")))
            .ok_or_else(|| anyhow!("missing conversation_id in {}", path.display()))?;
        output.insert(conversation_id.to_string(), (path, value));
    }
    Ok(output)
}

fn compare_candidates_to_reference(
    candidates: &[(String, Value)],
    reference_directory: &Path,
) -> Result<()> {
    let references = load_reference_handoffs(reference_directory)?;
    let candidate_ids: HashSet<String> = candidates
        .iter()
        .filter_map(|(_, value)| {
            value
                .as_object()
                .and_then(|map| value_str(map.get("conversation_id")))
                .map(str::to_string)
        })
        .collect();
    let reference_ids: HashSet<String> = references.keys().cloned().collect();
    if candidate_ids != reference_ids {
        let missing: Vec<_> = reference_ids.difference(&candidate_ids).cloned().collect();
        let extra: Vec<_> = candidate_ids.difference(&reference_ids).cloned().collect();
        bail!("conversation set mismatch: missing={missing:?} extra={extra:?}");
    }
    let mut filename_mismatches = 0usize;
    let mut semantic_mismatches = 0usize;
    for (filename, candidate) in candidates {
        let conversation_id = candidate
            .as_object()
            .and_then(|map| value_str(map.get("conversation_id")))
            .unwrap_or_default();
        let (reference_path, reference) = &references[conversation_id];
        let reference_filename = reference_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if reference_filename != filename {
            filename_mismatches += 1;
            eprintln!(
                "[DIFF] {conversation_id}: filename Rust={filename} Python={reference_filename}"
            );
        }
        let candidate_exporter_version = candidate
            .as_object()
            .and_then(|map| value_str(map.get("exporter_version")));
        if candidate_exporter_version != Some(VERSION) {
            semantic_mismatches += 1;
            eprintln!(
                "[DIFF] {conversation_id}: $.exporter_version: Rust={candidate_exporter_version:?} expected={VERSION:?}"
            );
            continue;
        }

        // Python and Rust intentionally report their own generator provenance.
        // Cross-language correctness compares every other domain field exactly.
        let mut candidate_semantic = candidate.clone();
        let mut reference_semantic = reference.clone();
        if let Some(map) = candidate_semantic.as_object_mut() {
            map.remove("exporter_version");
        }
        if let Some(map) = reference_semantic.as_object_mut() {
            map.remove("exporter_version");
        }
        if !json_semantic_equal(&candidate_semantic, &reference_semantic) {
            semantic_mismatches += 1;
            let detail = first_json_difference(&candidate_semantic, &reference_semantic, "$")
                .unwrap_or_else(|| "unknown difference".to_string());
            eprintln!("[DIFF] {conversation_id}: {detail}");
        }
    }
    println!("Reference conversations: {}", references.len());
    println!("Candidate conversations: {}", candidates.len());
    println!("Filename mismatches: {filename_mismatches}");
    println!("Semantic mismatches: {semantic_mismatches}");
    if filename_mismatches > 0 || semantic_mismatches > 0 {
        bail!("Rust candidate does not yet match the Python reference");
    }
    println!("Rust candidate matches the Python reference: yes");
    Ok(())
}

#[derive(Default, Clone, Debug, Serialize)]
struct RunMetrics {
    physical_rollouts: usize,
    continuations_merged: usize,
    total: Duration,
    title_index: Duration,
    project_membership: Duration,
    result_collection: Duration,
    integration_only: Duration,
    build_only: Duration,
    session_teardown: Duration,
    thread_pool_teardown: Duration,
    discovery: Duration,
    scan: Duration,
    integrate_build: Duration,
    pipeline_build_write: Duration,
    export: Duration,
    export_existing_index: Duration,
    export_prepare: Duration,
    export_cleanup: Duration,
    export_finalize: Duration,
    export_shell_prewarm: Duration,
    export_shell_notify: Duration,
    files_scanned: usize,
    matched_sessions: usize,
    root_conversations: usize,
    integrated_subagents: usize,
    scan_errors: usize,
    export_errors: usize,
    outputs_rewritten: usize,
    shell_prewarm_notified: bool,
    shell_refresh_notified: bool,
    input_bytes: u64,
    output_bytes: u64,
}

#[cfg(windows)]
fn shell_notify_path_wide(path: &Path) -> Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        bail!(
            "Output directory contains an embedded NUL and cannot be sent to Windows Shell: {}",
            path.display()
        );
    }
    wide.push(0);
    Ok(wide)
}

#[cfg(windows)]
fn notify_shell_output_directory_changed(path: &Path, flush_nowait: bool) -> Result<()> {
    let wide = shell_notify_path_wide(path)?;
    let flags = SHCNF_PATHW | if flush_nowait { SHCNF_FLUSHNOWAIT } else { 0 };
    // SAFETY: `wide` is a live, NUL-terminated UTF-16 buffer for the duration of the call.
    // SHCNE_UPDATEDIR + SHCNF_PATHW reads dwItem1 as a path and ignores dwItem2.
    // SHCNF_FLUSHNOWAIT, when selected, starts delivery and returns once delivery has begun.
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEDIR as i32,
            flags,
            wide.as_ptr().cast(),
            std::ptr::null(),
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn notify_shell_output_directory_changed(_path: &Path, _flush_nowait: bool) -> Result<()> {
    Ok(())
}

fn use_shell_notify_prewarm(args: &Args) -> bool {
    DEFAULT_SHELL_NOTIFY_PREWARM || args.shell_notify_prewarm
}

fn project_state_path(args: &Args) -> PathBuf {
    args.project_state.clone().unwrap_or_else(|| {
        args.sessions_root
            .parent()
            .unwrap_or(Path::new("."))
            .join(".codex-global-state.json")
    })
}

fn ensure_existing_handoffs_belong_to_scope(
    output: &Path,
    scope: &project_membership::ProjectScope,
) -> Result<()> {
    destination::validate_existing(output, &scope.selected_ids, scope.root_ids.len() == 1)
}

fn build_all_candidates(
    args: &Args,
    write_outputs: bool,
) -> Result<(Vec<(String, Value)>, RunMetrics)> {
    let total_start = Instant::now();
    progress::stage("檢查來源", 0);
    let target_normalized = normalize_windows_path(&args.target_cwd);
    let attachment_disk_root = if args.product_mode {
        args.attachment_root.clone()
    } else {
        std::env::var_os(ATTACHMENT_MIRROR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| args.attachment_root.clone())
    };
    let title_start = Instant::now();
    let title_index = read_title_index(&args.session_index)?;
    let title_index_time = title_start.elapsed();
    let discovery_start = Instant::now();
    let (files, input_bytes) = if args.legacy_discovery {
        let mut files = if let Some(single) = args.single_session.as_ref() {
            vec![single.clone()]
        } else {
            discover_rollouts(&args.sessions_root)?
        };
        files.sort_by_key(|path| {
            std::cmp::Reverse(
                fs::metadata(path)
                    .map(|metadata| metadata.len())
                    .unwrap_or_default(),
            )
        });
        let input_bytes = files
            .iter()
            .filter_map(|path| fs::metadata(path).ok().map(|metadata| metadata.len()))
            .sum();
        (files, input_bytes)
    } else {
        let discovered = if let Some(single) = args.single_session.as_ref() {
            vec![(
                single.clone(),
                fs::metadata(single)
                    .map(|metadata| metadata.len())
                    .unwrap_or_default(),
            )]
        } else {
            discover_rollouts_with_sizes(&args.sessions_root)?
        };
        let input_bytes = discovered.iter().map(|(_, size)| *size).sum();
        let files = discovered.into_iter().map(|(path, _)| path).collect();
        (files, input_bytes)
    };
    let discovery = discovery_start.elapsed();

    // One immutable project-membership snapshot per export. Fail before any
    // output write/delete if the Codex project registry is unavailable or
    // inconsistent. Historical test fixtures must opt into legacy mode.
    let membership_start = Instant::now();
    let input_guard = if args.product_mode {
        Some(source_guard::InputGuard::capture(args, &files)?)
    } else {
        None
    };
    if write_outputs {
        destination::validate_path(&args.output_directory, args)?;
    }
    let project_scope = if args.legacy_cwd_only {
        println!("Project membership: explicit --legacy-cwd-only compatibility mode");
        None
    } else {
        let path = project_state_path(args);
        let state = project_membership::read_stable_state(&path)?;
        if let Some(expected) = &args.expected_state {
            if *expected != sidebar_targets::state_fingerprint(&state)? {
                bail!("Codex 側邊欄狀態已變更，尚未寫入；請重新選擇目標");
            }
        }
        let scope = if let Some(id) = args.thread_id.as_deref() {
            project_membership::resolve_independent(&files, &state, id)?
        } else {
            project_membership::resolve_state(
                &files,
                &state,
                &target_normalized,
                args.project_id.as_deref(),
            )?
        };
        if args.product_mode && scope.root_ids.is_empty() {
            bail!("此專案目前沒有可匯出的主對話；沒有建立或清理任何 Handoff");
        }
        if args.product_mode {
            if let Some(id) = args.thread_id.as_deref() {
                let home = args
                    .sessions_root
                    .parent()
                    .ok_or_else(|| anyhow!("無效的 Codex Session 路徑"))?;
                if let Some(row) = sqlite_threads::lookup_one(home, id)? {
                    if row.archived {
                        bail!("此獨立對話已封存，未進行匯出：{id}");
                    }
                    if !row.rollout_path.is_empty() {
                        let hint = sidebar_targets::basename(&row.rollout_path);
                        let matches = scope.rollout_groups.get(id).is_some_and(|paths| {
                            paths
                                .iter()
                                .any(|p| sidebar_targets::basename(&p.to_string_lossy()) == hint)
                        });
                        if !matches {
                            bail!("獨立對話的 Thread 索引與來源檔案位置不一致，未進行匯出：{id}");
                        }
                    }
                }
            }
        }
        if write_outputs {
            ensure_existing_handoffs_belong_to_scope(&args.output_directory, &scope)?;
        }
        println!(
            "Project membership: {} explicit roots, {} CWD fallback roots, {} children, {} Guardian; {} metadata-free rollout(s)",
            scope.explicit_roots,
            scope.cwd_fallback_roots,
            scope.children,
            scope.guardians,
            scope.metadata_missing
        );
        println!(
            "Selected project: {} root(s), {} total selected session(s)",
            scope.root_ids.len(),
            scope.selected_ids.len()
        );
        if scope.unattached_guardians_skipped > 0 {
            println!(
                "Unattached Guardian sessions excluded (parent not in active snapshot): {}",
                scope.unattached_guardians_skipped
            );
        }
        Some(scope)
    };

    let project_membership_time = membership_start.elapsed();
    // Materialize only confirmed multi-file continuations as isolated,
    // ephemeral logical rollouts. This occurs before any output write/delete.
    // No Codex source rollout is renamed, removed or edited.
    let groups = if let Some(scope) = project_scope.as_ref() {
        scope.rollout_groups.clone()
    } else {
        project_membership::rollout_file_groups(&files)?
    };
    let prepared_rollouts = rollout_continuations::prepare(
        &files,
        &groups,
        project_scope.as_ref().map(|scope| &scope.selected_ids),
        project_scope.as_ref().map(|scope| &scope.continuation_ids),
    )?;
    let physical_rollout_count = files.len();
    let files = &prepared_rollouts.files;
    if prepared_rollouts.merged_count > 0 {
        println!(
            "Verified history_base continuation chain(s): {} logical Session(s)",
            prepared_rollouts.merged_count
        );
        println!(
            "Physical rollout files collapsed into logical continuations: {}",
            physical_rollout_count.saturating_sub(prepared_rollouts.files.len())
        );
    }
    if write_outputs {
        fs::create_dir_all(&args.output_directory)?;
    }
    let mut shell_prewarm_handle = if write_outputs && use_shell_notify_prewarm(args) {
        let output_directory = args.output_directory.clone();
        Some(std::thread::spawn(move || {
            let started = Instant::now();
            let result = notify_shell_output_directory_changed(&output_directory, false);
            (started.elapsed(), result)
        }))
    } else {
        None
    };
    progress::stage("解析對話紀錄", files.len());
    let scan_start = Instant::now();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(args.workers)
        .thread_name(|index| format!("codex-rust-{index}"))
        .build()?;
    let results: Vec<(PathBuf, Result<Option<Session>>)> = pool.install(|| {
        files
            .par_iter()
            .map(|path| {
                #[cfg(feature = "integrated-study")]
                let study_task = integrated_study::task(path, "scan");
                let use_fused_reducer = use_fused_reducer(args);
                // Codex can create a rollout file before its first JSONL record is
                // written. A truly zero-byte file has no session semantics yet, so
                // treat it as a transient placeholder rather than a parse failure.
                // Non-empty malformed JSONL still follows the normal error path.
                let result = if is_zero_byte_rollout(path) {
                    Ok(None)
                } else if use_fused_reducer {
                    convert_codex_session_file_streaming_scoped(
                        path,
                        &target_normalized,
                        &attachment_disk_root,
                        project_scope.as_ref().map(|scope| &scope.selected_ids),
                    )
                } else if use_legacy_ingestion(args) {
                    convert_codex_session_file_with_ingestion_scoped(
                        path,
                        &target_normalized,
                        &attachment_disk_root,
                        true,
                        project_scope.as_ref().map(|scope| &scope.selected_ids),
                    )
                } else {
                    convert_codex_session_file_with_ingestion_scoped(
                        path,
                        &target_normalized,
                        &attachment_disk_root,
                        false,
                        project_scope.as_ref().map(|scope| &scope.selected_ids),
                    )
                };
                #[cfg(feature = "integrated-study")]
                if let Some(task) = study_task {
                    task.finish(result.as_ref().ok().map(|v| v.is_some()));
                }
                progress::advance();
                (path.clone(), result)
            })
            .collect()
    });
    let scan = scan_start.elapsed();
    let result_collection_start = Instant::now();
    let mut sessions_by_id = HashMap::new();
    let mut scan_errors = 0usize;
    for (path, result) in results {
        match result {
            Ok(Some(session)) => {
                if sessions_by_id.contains_key(&session.session_id) {
                    scan_errors += 1;
                    eprintln!(
                        "\n[ERROR] Duplicate parsed logical Session ID {} from {}; refusing silent overwrite",
                        session.session_id,
                        path.display()
                    );
                } else {
                    sessions_by_id.insert(session.session_id.clone(), session);
                }
            }
            Ok(None) => {}
            Err(error) => {
                scan_errors += 1;
                eprintln!(
                    "\n[ERROR] Stage: Session scan/parse\n[ERROR] Source: {}\n[ERROR] Message: {error:#}",
                    path.display()
                );
            }
        }
    }
    let result_collection_time = result_collection_start.elapsed();
    if scan_errors > 0 {
        bail!("{scan_errors} rollout session(s) failed to parse");
    }

    let (write_lanes, cleanup_lanes, finalize_lanes) = resolved_export_lanes(args);
    let staging_directory = if write_outputs && args.isolated_staging {
        Some(StagingDirectory::create(&args.output_directory)?)
    } else {
        None
    };
    let temp_directory = staging_directory
        .as_ref()
        .map(StagingDirectory::path)
        .unwrap_or(args.output_directory.as_path());

    progress::stage("整合續接與子代理", 0);
    let build_start = Instant::now();
    let matched_sessions = sessions_by_id.len();
    #[cfg(feature = "integrated-study")]
    let study_integration_task =
        integrated_study::task(Path::new("<root-subagent-integration>"), "integration");
    let (grouped, integrated_subagents) = integrate_subagent_sessions(sessions_by_id);
    if let Some(scope) = &project_scope {
        let actual: HashSet<String> = grouped.keys().cloned().collect();
        if actual != scope.root_ids {
            let missing: Vec<&String> = scope.root_ids.difference(&actual).take(10).collect();
            let excess: Vec<&String> = actual.difference(&scope.root_ids).take(10).collect();
            bail!(
                "Project output root mismatch before writing: expected={}, actual={}, missing={missing:?}, unexpected={excess:?}",
                scope.root_ids.len(),
                actual.len()
            );
        }
        if integrated_subagents != scope.children {
            bail!(
                "Project child integration mismatch before writing: selected={}, integrated={}",
                scope.children,
                integrated_subagents
            );
        }
    }
    #[cfg(feature = "integrated-study")]
    if let Some(task) = study_integration_task {
        task.finish(None);
    }
    let mut sessions: Vec<Session> = grouped.into_values().collect();
    sessions.sort_by_key(|session| session.last_message_time);
    let ordinals = conversation_ordinals(&sessions);
    let total_handoffs = sessions.len();
    let pipeline_enabled = args.pipeline_build_write && write_outputs && !args.legacy_export;
    let integration_only = build_start.elapsed();
    if write_outputs && project_scope.is_none() {
        let mut allowed = HashSet::new();
        for session in &sessions {
            allowed.insert(session.session_id.clone());
            allowed.extend(
                session
                    .subagent_sessions
                    .iter()
                    .map(|s| s.session_id.clone()),
            );
            allowed.extend(session.internal_session_ids.iter().cloned());
        }
        destination::validate_existing(&args.output_directory, &allowed, sessions.len() == 1)?;
    }
    progress::stage("建立 Handoff", total_handoffs);
    let candidate_build_start = Instant::now();
    let build_candidate = |session: &mut Session| -> Result<BuiltCandidate> {
        #[cfg(feature = "integrated-study")]
        let study_task = integrated_study::task(&session.source_path, "build");
        let (base_filename, handoff) = if args.legacy_export {
            build_handoff(session, &title_index)?
        } else {
            build_handoff_move(session, &title_index)?
        };
        #[cfg(feature = "integrated-study")]
        if let Some(task) = study_task {
            task.finish(Some(true));
        }
        let ordinal = *ordinals
            .get(&session.session_id)
            .ok_or_else(|| anyhow!("Missing output ordinal for {}", session.session_id))?;
        let filename = if args.thread_id.is_some() {
            base_filename
        } else {
            numbered_handoff_filename(&base_filename, ordinal, total_handoffs)
        };
        let serialized = if args.pre_serialize_candidates {
            Some(serialize_handoff(&handoff, args.serializer)?)
        } else {
            None
        };
        let prepared = if pipeline_enabled {
            Some(prepare_candidate_export(
                session,
                &filename,
                &handoff,
                serialized.as_deref(),
                temp_directory,
                &args.output_directory,
                args.serializer,
            )?)
        } else {
            None
        };
        progress::advance();
        Ok((filename, handoff, serialized, prepared))
    };
    let candidate_results: Vec<Result<BuiltCandidate>> = if args.legacy_handoff_build {
        sessions.iter_mut().map(build_candidate).collect()
    } else if pipeline_enabled && write_lanes != 0 {
        let mut groups = Vec::new();
        let ranges = export_lane_ranges(sessions.len(), write_lanes);
        let mut remaining = sessions.as_mut_slice();
        let mut previous_end = 0;
        for range in ranges {
            let size = range.end - range.start;
            debug_assert_eq!(range.start, previous_end);
            let (chunk, rest) = remaining.split_at_mut(size);
            groups.push(chunk);
            remaining = rest;
            previous_end = range.end;
        }
        let grouped: Vec<Vec<Result<BuiltCandidate>>> = pool.install(|| {
            groups
                .into_par_iter()
                .map(|group| group.iter_mut().map(build_candidate).collect())
                .collect()
        });
        grouped.into_iter().flatten().collect()
    } else {
        pool.install(|| sessions.par_iter_mut().map(build_candidate).collect())
    };
    let mut candidates = Vec::with_capacity(candidate_results.len());
    let mut serialized_candidates = if args.pre_serialize_candidates {
        Vec::with_capacity(candidate_results.len())
    } else {
        Vec::new()
    };
    let mut pipeline_prepared = if pipeline_enabled {
        Vec::with_capacity(candidate_results.len())
    } else {
        Vec::new()
    };
    let mut build_error = None;
    for result in candidate_results {
        match result {
            Ok((filename, handoff, serialized, prepared)) => {
                candidates.push((filename, handoff));
                if let Some(data) = serialized {
                    serialized_candidates.push(data);
                }
                if let Some(prepared) = prepared {
                    pipeline_prepared.push(prepared);
                }
            }
            Err(error) if build_error.is_none() => build_error = Some(error),
            Err(_) => {}
        }
    }
    if let Some(error) = build_error {
        cleanup_prepared_temp_files(&pipeline_prepared);
        return Err(error);
    }
    if write_outputs {
        // A shared directory is permitted for a single handoff, but no
        // candidate may collide with a different conversation's final file.
        // Check before stale cleanup in both modern and legacy output paths.
        if let Err(error) = destination::validate_planned(&args.output_directory, &candidates) {
            cleanup_prepared_temp_files(&pipeline_prepared);
            return Err(error);
        }
    }
    let pre_serialized = args
        .pre_serialize_candidates
        .then_some(serialized_candidates.as_slice());
    let pre_prepared = pipeline_enabled.then_some(pipeline_prepared);
    let build_only = candidate_build_start.elapsed();
    let integrate_build = build_start.elapsed();
    let pipeline_build_write = if pipeline_enabled {
        integrate_build
    } else {
        Duration::ZERO
    };

    if let Some(guard) = input_guard {
        guard.verify(args)?;
    }
    if write_outputs {
        destination::validate_path(&args.output_directory, args)?;
        if let Some(scope) = &project_scope {
            ensure_existing_handoffs_belong_to_scope(&args.output_directory, scope)?;
        }
    }
    progress::stage("準備寫入檔案", 0);
    let export_start = Instant::now();
    let mut output_bytes = 0u64;
    let mut export_errors = 0usize;
    let mut outputs_rewritten = 0usize;
    let mut shell_prewarm_notified = false;
    let mut shell_refresh_notified = false;
    let mut export_existing_index = Duration::ZERO;
    let mut export_phases = ExportPhaseMetrics::default();
    let mut export_shell_prewarm = Duration::ZERO;
    let mut export_shell_notify = Duration::ZERO;
    if write_outputs {
        let existing_index_start = Instant::now();
        let existing = if args.legacy_export_bookkeeping {
            read_existing_exports(&args.output_directory)
        } else {
            read_existing_exports_parallel(&args.output_directory, &pool)
        };
        export_existing_index = existing_index_start.elapsed();
        if args.legacy_export {
            let mut existing = existing;
            for session in &sessions {
                let ordinal = *ordinals
                    .get(&session.session_id)
                    .ok_or_else(|| anyhow!("Missing output ordinal for {}", session.session_id))?;
                match export_codex_session(
                    session,
                    &title_index,
                    &mut existing,
                    &args.output_directory,
                    args.serializer,
                    ordinal,
                    total_handoffs,
                ) {
                    Ok(path) => {
                        output_bytes += fs::metadata(&path)
                            .map(|metadata| metadata.len())
                            .unwrap_or_default();
                        outputs_rewritten += 1;
                        if args.verbose {
                            println!("[EXPORT] {}", path.display());
                        }
                    }
                    Err(error) => {
                        export_errors += 1;
                        eprintln!(
                            "\n[ERROR] Stage: Session export\n[ERROR] Source: {}\n[ERROR] Session: {}\n[ERROR] Message: {error:#}",
                            session.source_path.display(),
                            session.session_id
                        );
                    }
                }
            }
        } else {
            match export_prebuilt_candidates_parallel(
                &sessions,
                &candidates,
                pre_serialized,
                pre_prepared,
                &existing,
                &args.output_directory,
                temp_directory,
                args.serializer,
                write_lanes,
                cleanup_lanes,
                finalize_lanes,
                args.defer_planned_final_delete,
                args.replace_existing_final,
                &pool,
            ) {
                Ok((bytes, phases)) => {
                    output_bytes = bytes;
                    export_phases = phases;
                    outputs_rewritten = sessions.len();
                    if args.verbose {
                        for (filename, _) in &candidates {
                            println!(
                                "[EXPORT] {}",
                                args.output_directory.join(filename).display()
                            );
                        }
                    }
                }
                Err(error) => {
                    export_errors = 1;
                    eprintln!(
                        "\n[ERROR] Stage: Parallel prebuilt export\n[ERROR] Message: {error:#}"
                    );
                }
            }
        }
        if export_errors == 0 && outputs_rewritten != sessions.len() {
            bail!(
                "Full-rewrite invariant violated: rewrote {} of {} root handoff(s)",
                outputs_rewritten,
                sessions.len()
            );
        }
        if export_errors == 0 {
            if let Some(handle) = shell_prewarm_handle.take() {
                let (duration, result) = handle
                    .join()
                    .map_err(|_| anyhow!("Explorer prewarm notification thread panicked"))?;
                result?;
                export_shell_prewarm = duration;
                shell_prewarm_notified = true;
            }
            let shell_notify_start = Instant::now();
            notify_shell_output_directory_changed(
                &args.output_directory,
                args.shell_notify_flush_nowait,
            )?;
            export_shell_notify = shell_notify_start.elapsed();
            shell_refresh_notified = true;
        }
    }
    let export = export_start.elapsed();
    let root_conversations = sessions.len();
    #[cfg(feature = "integrated-study")]
    if integrated_study::enabled() {
        pool.broadcast(|_| integrated_study::drain_thread());
    }
    // r7 measured the candidate wall before destroying the original Session
    // graphs and Rayon pool. Account for both teardown steps explicitly;
    // the real external process clock has always included them.
    let teardown_start = Instant::now();
    drop(sessions);
    let session_teardown = teardown_start.elapsed();
    let pool_drop_start = Instant::now();
    drop(pool);
    let thread_pool_teardown = pool_drop_start.elapsed();
    let total = total_start.elapsed();
    Ok((
        candidates,
        RunMetrics {
            physical_rollouts: physical_rollout_count,
            continuations_merged: prepared_rollouts.merged_count,
            total,
            title_index: title_index_time,
            project_membership: project_membership_time,
            result_collection: result_collection_time,
            integration_only,
            build_only,
            session_teardown,
            thread_pool_teardown,
            discovery,
            scan,
            integrate_build,
            pipeline_build_write,
            export,
            export_existing_index,
            export_prepare: export_phases.prepare,
            export_cleanup: export_phases.cleanup,
            export_finalize: export_phases.finalize,
            export_shell_prewarm,
            export_shell_notify,
            files_scanned: files.len(),
            matched_sessions,
            root_conversations,
            integrated_subagents,
            scan_errors,
            export_errors,
            outputs_rewritten,
            shell_prewarm_notified,
            shell_refresh_notified,
            input_bytes,
            output_bytes,
        },
    ))
}

fn print_metrics(metrics: &RunMetrics) {
    println!();
    println!("Session files scanned: {}", metrics.files_scanned);
    println!("Matched sessions: {}", metrics.matched_sessions);
    println!("Matched root conversations: {}", metrics.root_conversations);
    println!(
        "Integrated subagent sessions: {}",
        metrics.integrated_subagents
    );
    println!("Scan errors: {}", metrics.scan_errors);
    println!("Export errors: {}", metrics.export_errors);
    if metrics.outputs_rewritten > 0 || metrics.output_bytes > 0 {
        println!(
            "Root handoffs rewritten: {}/{}",
            metrics.outputs_rewritten, metrics.root_conversations
        );
    }
    if metrics.shell_prewarm_notified {
        println!("Explorer prewarm notification: sent");
    }
    if metrics.shell_refresh_notified {
        println!("Explorer refresh notification: sent");
    }
    println!(
        "Logical input size: {:.3} MiB",
        metrics.input_bytes as f64 / 1_048_576.0
    );
    if metrics.output_bytes > 0 {
        println!(
            "Output size written: {:.3} MiB",
            metrics.output_bytes as f64 / 1_048_576.0
        );
    }
    println!("Discovery time: {:.6} s", metrics.discovery.as_secs_f64());
    println!(
        "Session scan/parse time: {:.6} s",
        metrics.scan.as_secs_f64()
    );
    println!(
        "Integration/handoff build time: {:.6} s",
        metrics.integrate_build.as_secs_f64()
    );
    if metrics.pipeline_build_write > Duration::ZERO {
        println!(
            "Pipelined build/temp-write time: {:.6} s",
            metrics.pipeline_build_write.as_secs_f64()
        );
    }
    println!(
        "Serialization/write time: {:.6} s",
        metrics.export.as_secs_f64()
    );
    if metrics.output_bytes > 0 {
        println!(
            "Existing-output index time: {:.6} s",
            metrics.export_existing_index.as_secs_f64()
        );
        println!(
            "Serialize/temp-write time: {:.6} s",
            metrics.export_prepare.as_secs_f64()
        );
        println!(
            "Stale cleanup time: {:.6} s",
            metrics.export_cleanup.as_secs_f64()
        );
        println!(
            "Finalize/rename time: {:.6} s",
            metrics.export_finalize.as_secs_f64()
        );
        if metrics.shell_prewarm_notified {
            println!(
                "Explorer prewarm time: {:.6} s",
                metrics.export_shell_prewarm.as_secs_f64()
            );
        }
        println!(
            "Explorer notify time: {:.6} s",
            metrics.export_shell_notify.as_secs_f64()
        );
    }
    println!("Total wall time: {:.6} s", metrics.total.as_secs_f64());
    println!(
        "Title index time: {:.6} s",
        metrics.title_index.as_secs_f64()
    );
    println!(
        "Project resolver time: {:.6} s",
        metrics.project_membership.as_secs_f64()
    );
    println!(
        "Result collection time: {:.6} s",
        metrics.result_collection.as_secs_f64()
    );
    println!(
        "Integration bookkeeping time: {:.6} s",
        metrics.integration_only.as_secs_f64()
    );
    println!(
        "Handoff build only time: {:.6} s",
        metrics.build_only.as_secs_f64()
    );
    println!(
        "Original Session teardown time: {:.6} s",
        metrics.session_teardown.as_secs_f64()
    );
    println!(
        "Threadpool teardown time: {:.6} s",
        metrics.thread_pool_teardown.as_secs_f64()
    );
}

fn median_duration(values: &mut [Duration]) -> Duration {
    values.sort();
    values[values.len() / 2]
}

fn sum_profile_duration<F>(profiles: &[SessionHotspotProfile], selector: F) -> Duration
where
    F: Fn(&SessionHotspotProfile) -> Duration,
{
    profiles.iter().fold(Duration::ZERO, |total, profile| {
        total.saturating_add(selector(profile))
    })
}

fn median_profile_duration<F>(profiles: &[SessionHotspotProfile], selector: F) -> Duration
where
    F: Fn(&SessionHotspotProfile) -> Duration,
{
    let mut values: Vec<Duration> = profiles.iter().map(selector).collect();
    if values.is_empty() {
        return Duration::ZERO;
    }
    values.sort();
    values[values.len() / 2]
}

fn p90_profile_duration<F>(profiles: &[SessionHotspotProfile], selector: F) -> Duration
where
    F: Fn(&SessionHotspotProfile) -> Duration,
{
    let mut values: Vec<Duration> = profiles.iter().map(selector).collect();
    if values.is_empty() {
        return Duration::ZERO;
    }
    values.sort();
    let index = ((values.len() - 1) * 9 + 9) / 10;
    values[index.min(values.len() - 1)]
}

fn print_hotspot_phase(
    label: &str,
    total: Duration,
    phase_total: Duration,
    median: Duration,
    p90: Duration,
) {
    let share = if total.is_zero() {
        0.0
    } else {
        phase_total.as_secs_f64() / total.as_secs_f64() * 100.0
    };
    println!(
        "  {label:<22} total={:>9.3} s  share={:>6.2}%  median={:>8.3} ms  p90={:>8.3} ms",
        phase_total.as_secs_f64(),
        share,
        median.as_secs_f64() * 1000.0,
        p90.as_secs_f64() * 1000.0,
    );
}

fn print_hotspot_report(profiles: &[SessionHotspotProfile], metrics: &RunMetrics, top: usize) {
    println!();
    println!("=== Session hotspot profile ===");
    println!("Matched profiled sessions: {}", profiles.len());
    println!(
        "Parallel scan wall time: {:.6} s",
        metrics.scan.as_secs_f64()
    );
    let input_bytes: u64 = profiles.iter().map(|profile| profile.input_bytes).sum();
    let retained_records: usize = profiles
        .iter()
        .map(|profile| profile.retained_records)
        .sum();
    println!(
        "Profiled input size: {:.3} MiB",
        input_bytes as f64 / 1_048_576.0
    );
    println!("Profiled parsed records: {retained_records}");
    println!(
        "Summed session work is CPU-ish work across Rayon threads; phases overlap in wall time."
    );

    let ingestion = sum_profile_duration(profiles, |profile| profile.ingestion);
    let metadata = sum_profile_duration(profiles, |profile| profile.metadata);
    let core_scan = sum_profile_duration(profiles, |profile| profile.core_scan);
    let core_finalize = sum_profile_duration(profiles, |profile| profile.core_finalize);
    let finalize_filter_sort =
        sum_profile_duration(profiles, |profile| profile.finalize_filter_sort);
    let finalize_messages = sum_profile_duration(profiles, |profile| profile.finalize_messages);
    let finalize_turns_goals =
        sum_profile_duration(profiles, |profile| profile.finalize_turns_goals);
    let finalize_changes = sum_profile_duration(profiles, |profile| profile.finalize_changes);
    let finalize_activities = sum_profile_duration(profiles, |profile| profile.finalize_activities);
    let finalize_assembly = sum_profile_duration(profiles, |profile| profile.finalize_assembly);
    let enrichment = sum_profile_duration(profiles, |profile| profile.enrichment);
    let phase_total = ingestion
        .saturating_add(metadata)
        .saturating_add(core_scan)
        .saturating_add(core_finalize)
        .saturating_add(enrichment);

    println!("\nSummed phase work:");
    print_hotspot_phase(
        "JSONL ingestion",
        phase_total,
        ingestion,
        median_profile_duration(profiles, |profile| profile.ingestion),
        p90_profile_duration(profiles, |profile| profile.ingestion),
    );
    print_hotspot_phase(
        "session metadata",
        phase_total,
        metadata,
        median_profile_duration(profiles, |profile| profile.metadata),
        p90_profile_duration(profiles, |profile| profile.metadata),
    );
    print_hotspot_phase(
        "core event scan",
        phase_total,
        core_scan,
        median_profile_duration(profiles, |profile| profile.core_scan),
        p90_profile_duration(profiles, |profile| profile.core_scan),
    );
    print_hotspot_phase(
        "core finalization",
        phase_total,
        core_finalize,
        median_profile_duration(profiles, |profile| profile.core_finalize),
        p90_profile_duration(profiles, |profile| profile.core_finalize),
    );
    println!("\nCore finalization breakdown (share is of core finalization work):");
    if !core_finalize.is_zero()
        && finalize_filter_sort.is_zero()
        && finalize_messages.is_zero()
        && finalize_turns_goals.is_zero()
        && finalize_changes.is_zero()
        && finalize_activities.is_zero()
        && finalize_assembly.is_zero()
    {
        println!(
            "  NOTE: fused-streaming r9 profile records finalization as one coarse phase; sub-breakdown is intentionally zero."
        );
    }
    print_hotspot_phase(
        "effective filter/sort",
        core_finalize,
        finalize_filter_sort,
        median_profile_duration(profiles, |profile| profile.finalize_filter_sort),
        p90_profile_duration(profiles, |profile| profile.finalize_filter_sort),
    );
    print_hotspot_phase(
        "message/UI build",
        core_finalize,
        finalize_messages,
        median_profile_duration(profiles, |profile| profile.finalize_messages),
        p90_profile_duration(profiles, |profile| profile.finalize_messages),
    );
    print_hotspot_phase(
        "turn/goal/rollback",
        core_finalize,
        finalize_turns_goals,
        median_profile_duration(profiles, |profile| profile.finalize_turns_goals),
        p90_profile_duration(profiles, |profile| profile.finalize_turns_goals),
    );
    print_hotspot_phase(
        "changes build",
        core_finalize,
        finalize_changes,
        median_profile_duration(profiles, |profile| profile.finalize_changes),
        p90_profile_duration(profiles, |profile| profile.finalize_changes),
    );
    print_hotspot_phase(
        "activities build",
        core_finalize,
        finalize_activities,
        median_profile_duration(profiles, |profile| profile.finalize_activities),
        p90_profile_duration(profiles, |profile| profile.finalize_activities),
    );
    print_hotspot_phase(
        "session assembly",
        core_finalize,
        finalize_assembly,
        median_profile_duration(profiles, |profile| profile.finalize_assembly),
        p90_profile_duration(profiles, |profile| profile.finalize_assembly),
    );
    print_hotspot_phase(
        "semantic enrichment",
        phase_total,
        enrichment,
        median_profile_duration(profiles, |profile| profile.enrichment),
        p90_profile_duration(profiles, |profile| profile.enrichment),
    );

    let mut ranked = profiles.to_vec();
    ranked.sort_by_key(|profile| std::cmp::Reverse(profile.total));
    let count = top.min(ranked.len());
    println!("\nSlowest {count} profiled session(s):");
    for (index, profile) in ranked.into_iter().take(count).enumerate() {
        println!(
            "  {:>2}. total={:>8.3} ms ingest={:>8.3} core={:>8.3} finalize={:>8.3} [msg={:>7.3} act={:>7.3}] enrich={:>8.3} records={:>6} size={:>7.2} MiB  {}  {}",
            index + 1,
            profile.total.as_secs_f64() * 1000.0,
            profile.ingestion.as_secs_f64() * 1000.0,
            profile.core_scan.as_secs_f64() * 1000.0,
            profile.core_finalize.as_secs_f64() * 1000.0,
            profile.finalize_messages.as_secs_f64() * 1000.0,
            profile.finalize_activities.as_secs_f64() * 1000.0,
            profile.enrichment.as_secs_f64() * 1000.0,
            profile.retained_records,
            profile.input_bytes as f64 / 1_048_576.0,
            profile.session_id,
            profile.path.display(),
        );
    }
}

fn use_legacy_ingestion(args: &Args) -> bool {
    args.legacy_ingestion
}

fn use_direct_canonical_ingestion(args: &Args) -> bool {
    !args.legacy_canonical_ingestion
        && (DEFAULT_DIRECT_CANONICAL_INGESTION || args.direct_canonical_ingestion)
}

fn use_legacy_activity_sort(args: &Args) -> bool {
    args.legacy_activity_sort && !args.lightweight_activity_sort
}

fn run_hotspot_profile(args: &Args) -> Result<()> {
    hotspot_profiles()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    HOTSPOT_ENABLED.store(true, AtomicOrdering::SeqCst);
    let result = build_all_candidates(args, false);
    HOTSPOT_ENABLED.store(false, AtomicOrdering::SeqCst);
    let (_, metrics) = result?;
    let profiles = hotspot_profiles()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    print_metrics(&metrics);
    print_hotspot_report(&profiles, &metrics, args.hotspot_top);
    Ok(())
}

fn configure_runtime(args: &Args) {
    LEGACY_ACTIVITY_SORT_ENABLED.store(use_legacy_activity_sort(args), AtomicOrdering::SeqCst);
    LEGACY_SOURCE_QUALITY_SCAN_ENABLED
        .store(args.legacy_source_quality_scan, AtomicOrdering::SeqCst);
    LEGACY_ACTIVITY_REGEX_ENABLED.store(args.legacy_activity_regex, AtomicOrdering::SeqCst);
    LEGACY_JS_REGEX_ENABLED.store(args.legacy_js_regex, AtomicOrdering::SeqCst);
    LEGACY_RESIDUAL_REGEX_ENABLED.store(args.legacy_residual_regex, AtomicOrdering::SeqCst);
    LEGACY_OUTPUT_IO_ENABLED.store(args.legacy_output_io, AtomicOrdering::SeqCst);
    LEGACY_EXPORT_BOOKKEEPING_ENABLED.store(args.legacy_export_bookkeeping, AtomicOrdering::SeqCst);
    LEGACY_LINE_SPLITTER_ENABLED.store(args.legacy_line_splitter, AtomicOrdering::SeqCst);
    LEGACY_CRLF_WRITER_ENABLED.store(args.legacy_crlf_writer, AtomicOrdering::SeqCst);
    FAST_CRLF_SERIALIZER_ENABLED.store(args.fast_crlf_serializer, AtomicOrdering::SeqCst);
    DIRECT_CANONICAL_INGESTION_ENABLED
        .store(use_direct_canonical_ingestion(args), AtomicOrdering::SeqCst);
}

fn run(args: Args) -> Result<()> {
    if args.list_projects {
        let entries = project_membership::list_projects(&project_state_path(&args))?;
        println!("Codex registered local projects: {}", entries.len());
        for p in entries {
            println!("Project ID: {}", p.id);
            println!("  Name: {}", serde_json::to_string(&p.name)?);
            for alias in p.app_server_ids {
                println!("  App Server ID: {alias}");
            }
            for root in p.root_paths {
                println!("  Root: {}", serde_json::to_string(&root)?);
            }
        }
        return Ok(());
    }
    // No installation-specific or project-specific destination. Existing
    // Handoffs are checked against the selected IDs before any write/cleanup.
    #[cfg(feature = "integrated-study")]
    integrated_study::initialize()?;

    configure_runtime(&args);
    if !args.sessions_root.is_dir() {
        bail!("Sessions root not found: {}", args.sessions_root.display());
    }
    if !args.session_index.is_file() {
        bail!("Session index not found: {}", args.session_index.display());
    }
    configure_attachment_root(&args.attachment_root)?;
    if !(MIN_WORKERS..=MAX_WORKERS).contains(&args.workers) {
        bail!("workers must be in {MIN_WORKERS}..={MAX_WORKERS}");
    }
    for (label, lanes) in [
        ("export-lanes", args.export_lanes),
        ("write-lanes", args.write_lanes),
        ("cleanup-lanes", args.cleanup_lanes),
        ("finalize-lanes", args.finalize_lanes),
    ] {
        if lanes.is_some_and(|value| value > MAX_EXPORT_LANES) {
            bail!("{label} must be in 0..={MAX_EXPORT_LANES}");
        }
    }
    if !args.max_total_seconds.is_finite() || args.max_total_seconds <= 0.0 {
        bail!("--max-total-seconds must be a finite positive number");
    }
    if !(1..=100).contains(&args.hotspot_top) {
        bail!("hotspot-top must be in 1..=100");
    }
    println!("Codex handoff Rust {VERSION}");
    println!("Workers: {}", args.workers);
    println!(
        "Export lanes: {}",
        match args.export_lanes {
            Some(0) => "unbounded (compat override)".to_string(),
            Some(value) => format!("{value} (compat override)"),
            None => "production-split".to_string(),
        }
    );
    let (write_lanes, cleanup_lanes, finalize_lanes) = resolved_export_lanes(&args);
    println!(
        "Temp-write lanes: {}",
        if write_lanes == 0 {
            "unbounded".to_string()
        } else {
            write_lanes.to_string()
        }
    );
    println!(
        "Cleanup lanes: {}",
        if cleanup_lanes == 0 {
            "unbounded".to_string()
        } else {
            cleanup_lanes.to_string()
        }
    );
    println!(
        "Finalize lanes: {}",
        if finalize_lanes == 0 {
            "unbounded".to_string()
        } else {
            finalize_lanes.to_string()
        }
    );
    println!(
        "Deferred planned-final delete: {}",
        if args.defer_planned_final_delete {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "Pre-serialize candidates: {}",
        if args.pre_serialize_candidates {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "Isolated staging: {}",
        if args.isolated_staging { "yes" } else { "no" }
    );
    println!(
        "Build/write pipeline: {}",
        if args.pipeline_build_write {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "Explorer notify prewarm: {}",
        if use_shell_notify_prewarm(&args) {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "Explorer notify delivery: {}",
        if args.shell_notify_flush_nowait {
            "flush-nowait"
        } else {
            "normal"
        }
    );
    println!(
        "JSONL physical-line splitter: {}",
        if args.legacy_line_splitter {
            "scalar-inclusive-lf"
        } else {
            "memchr-inclusive-lf"
        }
    );
    println!(
        "Pretty JSON formatter: {}",
        if args.legacy_output_io || args.legacy_crlf_writer || args.fast_crlf_serializer {
            "historical-crlf-writer"
        } else {
            "structural-crlf"
        }
    );
    println!(
        "CRLF serializer fastpath: {}",
        if args.fast_crlf_serializer {
            "memchr-reserved"
        } else {
            if args.legacy_crlf_writer || args.legacy_output_io {
                "scalar-legacy"
            } else {
                "structural-crlf-default"
            }
        }
    );
    println!(
        "Final replacement: {}",
        if args.replace_existing_final {
            "replace-existing-rename"
        } else {
            "delete-then-rename"
        }
    );
    let use_fused_reducer = use_fused_reducer(&args);
    println!(
        "Reducer: {}",
        if use_fused_reducer {
            "fused-prefix-session-builder"
        } else {
            "legacy-retained-records"
        }
    );
    println!(
        "Ingestion: {}",
        if use_legacy_ingestion(&args) {
            "legacy-full-sonic"
        } else {
            "prefix-routed"
        }
    );
    println!(
        "Canonical fallback: {}",
        if use_direct_canonical_ingestion(&args) {
            "direct-payload-no-envelope"
        } else {
            "legacy-envelope-parse"
        }
    );
    println!(
        "Activity sort: {}",
        if use_legacy_activity_sort(&args) {
            "legacy-clone-heavy"
        } else {
            "lightweight-reference-index"
        }
    );
    println!(
        "Source-quality scan: {}",
        if args.legacy_source_quality_scan {
            "r6-all-lines-lowercase-allocation"
        } else {
            "tool-output-gated-no-allocation"
        }
    );
    println!(
        "Activity regex: {}",
        if args.legacy_activity_regex {
            "compile-per-call"
        } else {
            "cached-once"
        }
    );
    println!(
        "JavaScript parser regex: {}",
        if args.legacy_js_regex {
            "compile-per-call"
        } else {
            "cached-once"
        }
    );
    println!(
        "Residual fixed regex: {}",
        if args.legacy_residual_regex {
            "compile-per-call"
        } else {
            "cached-once"
        }
    );
    println!(
        "Output format: {}",
        match args.serializer {
            SerializerMode::Compact => "compact-utf8-lf-buffered",
            SerializerMode::Orjson2 => "pretty-2space-crlf",
        }
    );
    println!(
        "Output I/O: {}",
        if args.legacy_output_io {
            "legacy-full-parse-copy-posttimestamp"
        } else {
            "fast-index-buffered-pretimestamp"
        }
    );
    println!(
        "Discovery: {}",
        if args.legacy_discovery {
            "legacy-repeated-metadata"
        } else {
            "single-metadata-pass"
        }
    );
    println!(
        "Handoff build: {}",
        if args.legacy_handoff_build {
            "legacy-sequential"
        } else {
            "rayon-parallel"
        }
    );
    println!(
        "Export bookkeeping: {}",
        if args.legacy_export_bookkeeping {
            "legacy-sequential-metadata-probes"
        } else {
            "parallel-index-no-redundant-probes"
        }
    );
    println!(
        "Export path: {}",
        if args.legacy_export {
            "legacy-sequential-double-build"
        } else if args.pipeline_build_write {
            "parallel-build-write-pipeline"
        } else {
            "parallel-prebuilt"
        }
    );
    println!("Sessions root: {}", args.sessions_root.display());
    println!(
        "Attachment reference root: {}",
        args.attachment_root.display()
    );
    if let Some(mirror) = std::env::var_os(ATTACHMENT_MIRROR_ENV) {
        println!(
            "Attachment content mirror: {}",
            PathBuf::from(mirror).display()
        );
    }
    println!("Output directory: {}", args.output_directory.display());
    println!("Full scan/parse/rebuild/rewrite: yes");

    if args.hotspot_profile {
        println!("Hotspot profiler: enabled");
        run_hotspot_profile(&args)?;
        return Ok(());
    }

    if args.benchmark_runs > 0 {
        let mut totals = Vec::with_capacity(args.benchmark_runs);
        let mut scans = Vec::with_capacity(args.benchmark_runs);
        let mut last_candidates = Vec::new();
        for run_index in 1..=args.benchmark_runs {
            let (candidates, metrics) = build_all_candidates(&args, false)?;
            println!(
                "Benchmark {run_index}/{}: total={:.6} s scan={:.6} s roots={} valid-build",
                args.benchmark_runs,
                metrics.total.as_secs_f64(),
                metrics.scan.as_secs_f64(),
                metrics.root_conversations,
            );
            totals.push(metrics.total);
            scans.push(metrics.scan);
            last_candidates = candidates;
        }
        let total_median = median_duration(&mut totals);
        let scan_median = median_duration(&mut scans);
        println!("Median total: {:.6} s", total_median.as_secs_f64());
        println!("Median scan: {:.6} s", scan_median.as_secs_f64());
        if let Some(reference) = args.compare_reference.as_ref() {
            compare_candidates_to_reference(&last_candidates, reference)?;
        }
        return Ok(());
    }

    let (candidates, metrics) = build_all_candidates(&args, !args.no_write)?;
    print_metrics(&metrics);
    if args.profile_phases {
        println!();
        println!("Detailed candidate phases:");
        println!(
            "  Discovery                    {:>10.6} s",
            metrics.discovery.as_secs_f64()
        );
        println!(
            "  Session scan/parse           {:>10.6} s",
            metrics.scan.as_secs_f64()
        );
        println!(
            "  Root/subagent integration    {:>10.6} s",
            metrics.integrate_build.as_secs_f64()
        );
        if metrics.pipeline_build_write > Duration::ZERO {
            println!(
                "  Build/temp-write pipeline     {:>10.6} s",
                metrics.pipeline_build_write.as_secs_f64()
            );
        }
        println!(
            "  Serialization/write          {:>10.6} s",
            metrics.export.as_secs_f64()
        );
        if metrics.output_bytes > 0 {
            println!(
                "    Existing-output index      {:>10.6} s",
                metrics.export_existing_index.as_secs_f64()
            );
            println!(
                "    Serialize/temp write       {:>10.6} s",
                metrics.export_prepare.as_secs_f64()
            );
            println!(
                "    Stale cleanup              {:>10.6} s",
                metrics.export_cleanup.as_secs_f64()
            );
            println!(
                "    Finalize/rename            {:>10.6} s",
                metrics.export_finalize.as_secs_f64()
            );
            if metrics.shell_prewarm_notified {
                println!(
                    "    Explorer prewarm           {:>10.6} s",
                    metrics.export_shell_prewarm.as_secs_f64()
                );
            }
            println!(
                "    Explorer notify            {:>10.6} s",
                metrics.export_shell_notify.as_secs_f64()
            );
        }
    }
    if let Some(reference) = args.compare_reference.as_ref() {
        compare_candidates_to_reference(&candidates, reference)?;
    }
    let candidate_drop_start = Instant::now();
    drop(candidates);
    if args.profile_phases {
        println!(
            "Final Candidate JSON teardown time: {:.6} s",
            candidate_drop_start.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

fn main() {
    let code = cli::entry(std::env::args_os().collect());
    std::process::exit(code);
}

fn legacy_main(args: Args) {
    use std::io::IsTerminal;
    let process_start = Instant::now();
    let should_wait = !args.no_pause && std::io::stdin().is_terminal();
    let check_budget = !args.list_projects
        && !args.no_write
        && args.benchmark_runs == 0
        && !args.hotspot_profile
        && args.compare_reference.is_none();
    let max_seconds = args.max_total_seconds;
    let result = run(args);
    #[cfg(feature = "integrated-study")]
    if integrated_study::enabled() {
        let rows = hotspot_profiles().lock().unwrap_or_else(|e| e.into_inner());
        let value = serde_json::to_value(&*rows).unwrap_or(Value::Null);
        if let Err(error) =
            integrated_study::finish(value, if result.is_ok() { "PASS" } else { "FAIL" })
        {
            eprintln!("[ERROR] study report: {error:#}");
            std::process::exit(2);
        }
    }
    if let Err(error) = result {
        eprintln!("\n[ERROR] {error:#}");
        eprintln!("[ERROR] Export did not finish cleanly; see diagnostics above.");
        if should_wait {
            eprintln!(
                "Press Enter to close this window and keep the error available for copying..."
            );
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        }
        std::process::exit(1);
    }
    let total_seconds = process_start.elapsed().as_secs_f64();
    if check_budget {
        println!("Full-process elapsed: {total_seconds:.6} s (target: <= {max_seconds:.3} s)");
        if total_seconds > max_seconds {
            eprintln!(
                "[PERFORMANCE WARNING] Complete export took {total_seconds:.6} s, above {max_seconds:.3} s. Correctness may still be PASS."
            );
            // A successful export that only exceeds the performance target is
            // a warning, not an interactive error: do not pause or fail the run.
            // Real export errors still use the error branch above (with Pause).
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_windows_paths_case_insensitively() {
        assert_eq!(
            normalize_windows_path(r"d:/Project/Test/"),
            normalize_windows_path(r"D:\Project\Test")
        );
    }

    #[cfg(windows)]
    #[test]
    fn shell_notify_path_encoding_is_nul_terminated() {
        let wide =
            shell_notify_path_wide(Path::new(TEST_OUTPUT_DIRECTORY)).expect("path should encode");
        assert_eq!(wide.last(), Some(&0));
        assert!(!wide[..wide.len() - 1].contains(&0));
    }

    #[test]
    fn parses_javascript_literal_shell_command() {
        let source = r#"tools.shell_command({ command: "git status", workdir: "D:\\repo", timeout_ms: 20000 })"#;
        let (executions, warnings) = javascript_command_data(source);
        assert!(warnings.is_empty());
        assert_eq!(executions.len(), 1);
        assert_eq!(value_str(executions[0].get("command")), Some("git status"));
        assert_eq!(
            int_value(executions[0].get("timeout_ms"), None),
            Some(20_000)
        );
    }

    #[test]
    fn fast_literal_exec_command_matches_legacy_common_shapes() {
        let sources = [
            r#"const r = await tools.exec_command({ cmd: "git status", workdir: "D:\\repo", timeout_ms: 20000 }); return r.output;"#,
            r#"const r = await tools.exec_command({ workdir: "D:\\repo", cmd: "Get-Content -LiteralPath 'README.md' -Raw", timeout_ms: 120000, max_output_chars: 20000 }); return r.output;"#,
            r#"const r = await tools.exec_command({ cmd: "$ErrorActionPreference = 'Stop'\nWrite-Output ok", workdir: "D:\\repo" }); return r.output;"#,
        ];
        for source in sources {
            let fast = fast_literal_exec_command_data(source)
                .expect("literal exec shape should take the r7 fast path");
            let legacy = javascript_exec_command_data_legacy(source);
            assert_eq!(fast, legacy, "fast exec semantics changed for {source}");
        }
    }

    #[test]
    fn fast_literal_exec_command_falls_back_for_dynamic_shapes() {
        for source in [
            r#"const cmd = "git status"; const r = await tools.exec_command({ cmd, workdir: "D:\\repo" });"#,
            r#"const r = await tools.exec_command({ "cmd": "git status", "workdir": "D:\\repo" });"#,
            r#"const r = await tools.exec_command({ cmd: `git status ${branch}`, workdir: "D:\\repo" });"#,
            r#"const a = await tools.exec_command({ cmd: "git status" }); const b = await tools.exec_command({ cmd: "git diff" });"#,
            r#"// tools.exec_command({ cmd: "not executable" })"#,
            r#"const note = 'tools.exec_command({ cmd: "not executable" })';"#,
        ] {
            assert!(
                fast_literal_exec_command_data(source).is_none(),
                "dynamic/ambiguous exec shape must retain the complete legacy parser: {source}"
            );
        }
    }

    #[test]
    fn attachment_paths_support_dynamic_root_mixed_slashes_and_punctuation() {
        let root = r"D:\Custom Codex\attachments";
        let attachment_id = "00000000-0000-4000-8000-000000000001";
        let text = format!(
            "請完整閱讀`D:\\Custom Codex\\attachments/{attachment_id}/pasted-text.txt`，再繼續。"
        );
        let references = attachment_references_for_root(&text, root).unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].attachment_id, attachment_id);
        assert_eq!(references[0].filename, "pasted-text.txt");
    }

    #[test]
    fn only_user_messages_restore_attachment_transport() {
        let user = obj!("role" => Value::String("user".to_string()));
        let assistant = obj!("role" => Value::String("assistant".to_string()));
        assert!(message_attachment_transport_is_restorable(&user));
        assert!(!message_attachment_transport_is_restorable(&assistant));
    }

    #[test]
    fn standalone_attachment_reads_are_accepted_but_composite_commands_are_rejected() {
        let path = r"C:\Users\ExampleUser\.codex\attachments\00000000-0000-4000-8000-000000000001\goal-objective.md";
        let standalone = format!("Get-Content -LiteralPath '{path}' -Raw");
        assert!(simple_attachment_read_reference(&standalone).is_some());
        assert!(
            simple_attachment_read_reference(&format!(
                "(Get-Content -LiteralPath '{path}' -Raw).Length"
            ))
            .is_none()
        );
        assert!(
            simple_attachment_read_reference(&format!(
                "Get-Content -LiteralPath '{path}' -Raw; Write-Output done"
            ))
            .is_none()
        );
    }

    #[test]
    fn tool_output_removes_exactly_one_renderer_newline() {
        assert_eq!(
            remove_one_tool_transport_newline("alpha\r\n\r\n".to_string()),
            "alpha\r\n"
        );
        assert_eq!(
            remove_one_tool_transport_newline("alpha\n\n".to_string()),
            "alpha\n"
        );
        assert_eq!(
            remove_one_tool_transport_newline("alpha".to_string()),
            "alpha"
        );
        assert_eq!(
            extract_successful_command_output(Some(&Value::String(
                "Process exited with code 0\nFinal output:\nalpha\r\n\r\n".to_string(),
            )))
            .as_deref(),
            Some("alpha\r\n")
        );
        assert_eq!(
            extract_successful_command_output(Some(&Value::String(
                "Exit code: 0\nOutput:\nalpha\r\n".to_string(),
            )))
            .as_deref(),
            Some("alpha")
        );
    }

    #[test]
    fn parses_exec_command_and_nested_plan_with_quoted_properties() {
        let command = r"Get-Content -LiteralPath 'C:\Users\ExampleUser\.codex\attachments\00000000-0000-4000-8000-000000000001\goal-objective.md' -Raw";
        let script = format!(
            r#"const r = await tools.exec_command({{"cmd": {}, "yield_time_ms": 10000}}); text(r.output);"#,
            serde_json::to_string(command).unwrap(),
        );
        let (executions, warnings) = javascript_exec_command_data(&script);
        assert!(warnings.is_empty());
        assert_eq!(executions.len(), 1);
        assert_eq!(value_str(executions[0].get("command")), Some(command));
        assert!(attachment_read_reference_from_script(&script).is_some());

        let updates = javascript_plan_updates(
            r#"await tools.update_plan({"explanation":"phase","plan":[{"step":"Inspect","status":"in_progress"},{"step":"Verify","status":"pending"}]});"#,
        );
        assert_eq!(updates.len(), 1);
        let steps = updates[0]
            .get("steps")
            .and_then(Value::as_array)
            .expect("plan steps");
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps[0]
                .as_object()
                .and_then(|item| value_str(item.get("text"))),
            Some("Inspect")
        );
        assert_eq!(
            steps[1]
                .as_object()
                .and_then(|item| value_str(item.get("text"))),
            Some("Verify")
        );
    }

    #[test]
    fn custom_attachment_classifier_gate_rejects_plain_exec_without_attachment_shape() {
        let script = r#"const r = await tools.exec_command({ cmd: "git status --short", workdir: "D:\\repo" }); text(r.output);"#;
        assert!(!custom_attachment_classifier_may_match(script));
        assert!(!custom_shell_attachment_read(script));
    }

    #[test]
    fn custom_attachment_classifier_gate_falls_back_for_template_built_attachment_path() {
        let script = r#"const root = "C:\\Users\\ExampleUser\\.codex\\attachments";
const id = "12345678-1234-1234-1234-123456789abc";
const file = "note.txt";
const r = await tools.exec_command({ cmd: `Get-Content -LiteralPath '${root}\\${id}\\${file}' -Raw` });
text(r.output);"#;
        assert!(custom_attachment_classifier_may_match(script));
        assert!(attachment_read_reference_from_script(script).is_some());
        assert!(custom_shell_attachment_read(script));
    }

    #[test]
    fn exec_command_static_collections_and_dynamic_templates_are_retained() {
        let shorthand_loop = r#"const commands = ["one", "two"];
for (const cmd of commands) {
  await tools.exec_command({cmd, workdir: "D:\\repo"});
}"#;
        let (executions, warnings) = javascript_exec_command_data(shorthand_loop);
        assert!(warnings.is_empty());
        assert_eq!(executions.len(), 2);
        assert_eq!(value_str(executions[0].get("command")), Some("one"));
        assert_eq!(value_str(executions[1].get("command")), Some("two"));

        let object_map = r#"const commands = [
  {cmd: "one", workdir: "D:\\repo"},
  {cmd: "two", workdir: "D:\\repo"}
];
await Promise.all(commands.map(c => tools.exec_command(c)));"#;
        let (executions, warnings) = javascript_exec_command_data(object_map);
        assert!(warnings.is_empty());
        assert_eq!(executions.len(), 2);
        assert_eq!(value_str(executions[0].get("command")), Some("one"));
        assert_eq!(value_str(executions[1].get("command")), Some("two"));

        let dynamic_template = r#"const t = {path: "x"};
await tools.exec_command({cmd: `& '${t.path.replace(/'/g,"''")}' -P -`});"#;
        let (executions, warnings) = javascript_exec_command_data(dynamic_template);
        assert_eq!(executions.len(), 1);
        assert_eq!(
            value_str(executions[0].get("source_kind")),
            Some("dynamic_template")
        );
        assert!(
            value_str(executions[0].get("command_template"))
                .is_some_and(|value| value.contains("${t.path.replace(/'/g,\"''\")}"))
        );
        assert!(!warnings.is_empty());
    }

    #[test]
    fn pasted_text_transport_restores_semantic_content_and_metadata() {
        let attachment_id = "00000000-0000-4000-8000-000000000001";
        let path =
            format!(r"C:\Users\ExampleUser\.codex/attachments/{attachment_id}/pasted-text.txt");
        let original = format!(
            "# Files pasted by the user:\n\n## \"preview\": {path}\n\nPasted text contains the user's request.\n\n## My request:   \r\n  additional instruction\r\n"
        );
        let reference = attachment_references(&original).into_iter().next().unwrap();
        let content = "第一行\r\n第二行\r\n".to_string();
        let mut registry = AttachmentRegistry::new();
        registry.insert(
            reference.normalized_path.clone(),
            AttachmentEntry {
                content: Some(content.clone()),
                metadata: attachment_metadata(
                    &reference,
                    Some(&content),
                    "rollout_tool_output",
                    "restored",
                ),
            },
        );
        let (restored, metadata, changed) = replace_attachment_transport(&original, &registry);
        assert!(changed);
        assert_eq!(
            restored,
            format!("{content}\n\n  additional instruction\r\n")
        );
        assert_eq!(metadata.len(), 1);
        assert_eq!(
            metadata[0]
                .as_object()
                .and_then(|item| value_str(item.get("restoration_status"))),
            Some("restored")
        );
    }

    #[test]
    fn subagent_delegation_separates_task_and_message_boundaries() {
        let records = vec![
            (
                10,
                serde_json::json!({"type":"event_msg","payload":{"type":"task_started"}}),
            ),
            (
                11,
                serde_json::json!({
                    "type":"response_item",
                    "payload":{
                        "type":"message",
                        "role":"developer",
                        "content":[{"type":"input_text","text":"You are an agent in a team of agents"}]
                    }
                }),
            ),
            (
                12,
                serde_json::json!({
                    "type":"response_item",
                    "payload":{
                        "type":"message",
                        "role":"assistant",
                        "phase":"commentary",
                        "content":[{"type":"output_text","text":"local work"}]
                    }
                }),
            ),
            (
                13,
                serde_json::json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            ),
        ];
        assert_eq!(
            subagent_local_boundaries(&records),
            SubagentLocalBoundaries {
                task_start_line: Some(10),
                message_start_line: Some(12),
            }
        );
        let no_active = vec![
            records[1].clone(),
            (
                21,
                serde_json::json!({
                    "type":"event_msg","payload":{"type":"task_started"}
                }),
            ),
        ];
        assert_eq!(
            subagent_local_boundaries(&no_active),
            SubagentLocalBoundaries {
                task_start_line: Some(21),
                message_start_line: Some(21),
            }
        );
    }

    #[test]
    fn activity_only_live_subagent_exports_before_first_assistant_message() {
        let path = std::env::temp_dir().join(format!(
            "codex-rust-live-subagent-{}-{}.jsonl",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let root_id = "00000000-0000-7000-8000-000000000101";
        let child_id = "00000000-0000-7000-8000-000000000102";
        let inherited_turn = "00000000-0000-7000-8000-000000000103";
        let local_turn = "00000000-0000-7000-8000-000000000104";
        let target_cwd = r"D:\workspace";
        let records = vec![
            serde_json::json!({
                "type": "session_meta",
                "timestamp": "2026-08-19T00:00:00Z",
                "payload": {
                    "id": child_id,
                    "session_id": root_id,
                    "thread_source": "subagent",
                    "parent_thread_id": root_id,
                    "agent_nickname": "LiveAgent",
                    "agent_path": "/root/live_task",
                    "timestamp": "2026-08-19T00:00:00Z",
                    "cwd": target_cwd,
                    "originator": "cli",
                    "source": "cli",
                    "cli_version": "test",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-19T00:00:01Z",
                "payload": {"type": "task_started", "turn_id": inherited_turn}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-19T00:00:01.100Z",
                "payload": {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "You are an agent in a team of agents collaborating to complete a task."}]
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-19T00:00:02Z",
                "payload": {"type": "task_started", "turn_id": local_turn}
            }),
            serde_json::json!({
                "type": "turn_context",
                "timestamp": "2026-08-19T00:00:02.100Z",
                "payload": {"turn_id": local_turn, "model": "gpt-test", "effort": "high"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-19T00:00:03Z",
                "payload": {
                    "type": "function_call",
                    "name": "shell_command",
                    "call_id": "call-live",
                    "arguments": "{\"command\":\"git status\"}",
                    "internal_chat_message_metadata_passthrough": {"turn_id": local_turn}
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-19T00:00:03.500Z",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call-live",
                    "output": "Process exited with code 0\nFinal output:\nworking\n"
                }
            }),
            // The writer may have progressed through internal reasoning without
            // emitting another serialized semantic event. Handoff update_time is
            // derived from the latest semantic state that is actually preserved,
            // not from intentionally omitted internal reasoning.
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-19T00:00:04Z",
                "payload": {"type": "reasoning", "summary": []}
            }),
        ];
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(&serde_json::to_vec(&record).unwrap());
            bytes.push(b'\n');
        }
        fs::write(&path, bytes).unwrap();
        let target = normalize_windows_path(target_cwd);
        let legacy = convert_codex_session_file(&path, &target, Path::new("missing-attachments"))
            .unwrap()
            .unwrap();
        let fused =
            convert_codex_session_file_streaming(&path, &target, Path::new("missing-attachments"))
                .unwrap()
                .unwrap();
        let _ = fs::remove_file(&path);

        assert!(legacy.messages.is_empty());
        assert!(legacy.raw_messages.is_empty());
        assert!(fused.messages.is_empty());
        assert!(fused.raw_messages.is_empty());
        assert_eq!(legacy.session_state, "active_at_export");
        assert_eq!(fused.session_state, "active_at_export");
        assert_eq!(
            legacy.last_message_time,
            parse_datetime_text("2026-08-19T00:00:03.500Z").unwrap()
        );
        assert_eq!(fused.last_message_time, legacy.last_message_time);
        assert!(!legacy.turns.is_empty());
        assert!(!legacy.activities.is_empty());

        let titles = HashMap::new();
        let (_, legacy_handoff) = build_handoff(&legacy, &titles).unwrap();
        let (_, fused_handoff) = build_handoff(&fused, &titles).unwrap();
        assert!(json_semantic_equal(&fused_handoff, &legacy_handoff));
        assert_eq!(
            fused_handoff
                .as_object()
                .and_then(|map| map.get("messages"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn source_quality_counts_unicode_replacement_characters() {
        let value = serde_json::json!({"a":"x�y","b":["�",1]});
        assert_eq!(replacement_character_count(&value), 2);
        let quality = SourceDataQuality {
            source_unicode_replacement_chars: 2,
            affected_tool_outputs: 1,
        };
        let output = quality.to_value().unwrap();
        assert_eq!(
            output
                .as_object()
                .and_then(|item| int_value(item.get("source_unicode_replacement_chars"), None)),
            Some(2)
        );
    }

    #[test]
    fn fast_source_quality_candidate_matches_legacy_patterns() {
        for line in [
            br#"{\"output\":\"plain ascii\"}"#.as_slice(),
            br#"{\"output\":\"escaped \\ufffd value\"}"#.as_slice(),
            br#"{\"output\":\"escaped \\uFFFD value\"}"#.as_slice(),
            "{\"output\":\"actual � value\"}".as_bytes(),
        ] {
            assert_eq!(
                line_may_contain_replacement_character_fast(line),
                line_may_contain_replacement_character_legacy(line),
            );
        }
    }

    #[test]
    fn nonblank_raw_str_preserves_exact_whitespace() {
        let value = Value::String("  objective\r\n".to_string());
        assert_eq!(nonblank_raw_str(Some(&value)), Some("  objective\r\n"));
    }

    #[test]
    fn command_execution_text_preserves_exact_whitespace() {
        let execution = obj!(
            "command" => Value::String("echo test\n".to_string()),
        );
        assert_eq!(command_execution_text(&execution), "echo test\n");
    }

    #[test]
    fn proposed_plan_replacement_preserves_plan_item_trailing_newline() {
        let message = RawMessage {
            role: "assistant".to_string(),
            phase: Some("final_answer".to_string()),
            turn_id: Some("turn-1".to_string()),
            content: "before\n\n<proposed_plan>\n# Plan\n\nbody\n</proposed_plan>\n\n---\n\nafter"
                .to_string(),
            time: None,
            sequence: 1,
            source_line: 1,
        };
        let plan = PlanItem {
            turn_id: "turn-1".to_string(),
            source_item_id: Some("plan-source".to_string()),
            text: "# Plan\n\nbody\n".to_string(),
            time: None,
            sequence: 1,
        };
        let outputs = build_output_messages(&[message], &[plan]);
        let content = outputs.messages[0]
            .as_object()
            .and_then(|map| raw_str(map.get("content")))
            .unwrap();
        assert_eq!(content, "before\n\n# Plan\n\nbody\n\n\n---\n\nafter");
    }

    #[test]
    fn serializer_uses_crlf_and_one_trailing_crlf() {
        let value = Value::Object(obj!(
            "a" => Value::Number(1.into()),
            "b" => Value::Array(vec![Value::Bool(true)]),
        ));
        let bytes = serialize_handoff(&value, SerializerMode::Orjson2).unwrap();
        assert!(!bytes.starts_with(&[0xEF, 0xBB, 0xBF]));
        assert!(bytes.ends_with(b"\r\n"));
        assert!(!bytes.ends_with(b"\r\n\r\n"));
        assert!(bytes.windows(2).any(|window| window == b"\r\n"));
    }

    #[test]
    fn semantic_comparison_ignores_object_property_order() {
        let left: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        let right: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        assert!(json_semantic_equal(&left, &right));
    }

    #[test]
    fn ui_attributes_preserve_unicode_values() {
        let (valid, attrs) = parse_ui_attributes(r#"title="繁體中文測試" count=2 enabled=true"#);
        assert!(valid);
        assert_eq!(value_str(attrs.get("title")), Some("繁體中文測試"));
        assert_eq!(int_value(attrs.get("count"), None), Some(2));
        assert_eq!(bool_value(attrs.get("enabled")), Some(true));
    }

    #[test]
    fn selective_ingestion_drops_irrelevant_large_records() {
        let mut state = RolloutIngestionState::default();
        let reasoning = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:00Z",
            "payload": {"type": "reasoning", "summary": ["large internal text"]}
        });
        let bytes = serde_json::to_vec(&reasoning).unwrap();
        assert!(parse_rollout_line(&bytes, &mut state).unwrap().is_none());

        let world_state = serde_json::json!({"type": "world_state", "payload": {"large": "value"}});
        let bytes = serde_json::to_vec(&world_state).unwrap();
        assert!(parse_rollout_line(&bytes, &mut state).unwrap().is_none());
    }

    #[test]
    fn selective_session_meta_removes_unused_instruction_payload() {
        let mut state = RolloutIngestionState::default();
        let record = serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-08-16T00:00:00Z",
            "payload": {
                "id": "session-1",
                "cwd": TEST_TARGET_CWD,
                "timestamp": "2026-08-16T00:00:00Z",
                "base_instructions": "must not survive selective ingestion"
            }
        });
        let bytes = serde_json::to_vec(&record).unwrap();
        let compact = parse_rollout_line(&bytes, &mut state).unwrap().unwrap();
        let payload = compact
            .as_object()
            .and_then(|record| record.get("payload"))
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(value_str(payload.get("id")), Some("session-1"));
        assert_eq!(value_str(payload.get("cwd")), Some(TEST_TARGET_CWD));
        assert!(!payload.contains_key("base_instructions"));
    }

    #[test]
    fn selective_developer_messages_only_keep_subagent_boundary() {
        let mut state = RolloutIngestionState::default();
        let ordinary = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "ordinary injected instructions"}]
            }
        });
        let bytes = serde_json::to_vec(&ordinary).unwrap();
        assert!(parse_rollout_line(&bytes, &mut state).unwrap().is_none());

        let boundary = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "You are an agent in a team of agents"}]
            }
        });
        let bytes = serde_json::to_vec(&boundary).unwrap();
        assert!(parse_rollout_line(&bytes, &mut state).unwrap().is_some());
    }

    #[test]
    fn selective_token_count_preserves_nested_rate_limit_fields() {
        let mut state = RolloutIngestionState::default();
        let record = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-08-16T00:00:00Z",
            "payload": {
                "type": "token_count",
                "info": {
                    "model_context_window": 1000,
                    "last_token_usage": {"total_tokens": 900},
                    "rate_limits": {
                        "limit_id": "primary",
                        "primary": {
                            "used_percent": 100.0,
                            "resets_at": 1234567890,
                            "window_minutes": 10080
                        }
                    }
                }
            }
        });
        let bytes = serde_json::to_vec(&record).unwrap();
        let compact = parse_rollout_line(&bytes, &mut state).unwrap().unwrap();
        let payload = compact
            .as_object()
            .and_then(|record| record.get("payload"))
            .and_then(Value::as_object)
            .unwrap();
        let rate_limits = payload
            .get("info")
            .and_then(Value::as_object)
            .and_then(|info| info.get("rate_limits"))
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(value_str(rate_limits.get("limit_id")), Some("primary"));
        let primary = rate_limits
            .get("primary")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            int_value(primary.get("resets_at"), None),
            Some(1_234_567_890)
        );
        assert_eq!(int_value(primary.get("window_minutes"), None), Some(10_080));
    }

    #[test]
    fn selective_shell_output_keeps_summary_not_bulk_text() {
        let mut state = RolloutIngestionState::default();
        let call = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:00Z",
            "payload": {
                "type": "custom_tool_call",
                "call_id": "call-1",
                "input": "const r = await tools.shell_command({command: 'git status'}); text(r);"
            }
        });
        let call_bytes = serde_json::to_vec(&call).unwrap();
        assert!(
            parse_rollout_line(&call_bytes, &mut state)
                .unwrap()
                .is_some()
        );

        let output = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:01Z",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": "call-1",
                "output": [
                    {"type": "input_text", "text": "Process exited with code 0\nFinal output:\nvery large output"}
                ]
            }
        });
        let output_bytes = serde_json::to_vec(&output).unwrap();
        let compact = parse_rollout_line(&output_bytes, &mut state)
            .unwrap()
            .unwrap();
        let payload = compact
            .as_object()
            .and_then(|record| record.get("payload"))
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            bool_value(payload.get("_compact_shell_summary")),
            Some(true)
        );
        assert_eq!(
            sequence_values(payload.get("_compact_exit_codes"))
                .first()
                .and_then(|value| int_value(Some(value), None)),
            Some(0),
        );
        assert!(!payload.contains_key("output"));
    }

    #[test]
    fn scalar_string_shell_output_preserves_oracle_unknown_mapping() {
        let mut state = RolloutIngestionState::default();
        let call = br#"{"timestamp":"2026-08-16T00:00:00Z","type":"response_item","payload":{"type":"function_call","name":"shell_command","arguments":"{\"command\":\"Get-Content README.md -Raw\",\"workdir\":\"D:\\repo\"}","call_id":"call-string"}}"#;
        assert!(
            parse_rollout_line_prefix(call, &mut state)
                .unwrap()
                .is_some()
        );

        let output = br#"{"timestamp":"2026-08-16T00:00:01Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-string","output":"Exit code: 0\nWall time: 0.1 seconds\nOutput:\nREADME"}}"#;
        let compact = parse_rollout_line_prefix(output, &mut state)
            .unwrap()
            .unwrap();
        let payload = compact
            .as_object()
            .and_then(|record| record.get("payload"))
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            bool_value(payload.get("_compact_shell_summary")),
            Some(true)
        );
        assert!(sequence_values(payload.get("_compact_exit_codes")).is_empty());
        assert_eq!(
            bool_value(payload.get("_compact_output_truncated")),
            Some(false)
        );
        assert_eq!(
            command_output_summary(payload),
            (Vec::new(), false, None, None)
        );

        let noncanonical_output = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:02Z",
            "payload": {
                "type": "function_call_output",
                "call_id": "call-fallback",
                "output": "Process exited with code 7\n[truncated output]"
            }
        });
        let payload_text =
            serde_json::to_string(noncanonical_output.get("payload").unwrap()).unwrap();
        let parsed: ToolOutputSelective<'_> = sonic_rs::from_str(&payload_text).unwrap();
        let text = lazy_to_message_text(parsed.output.as_ref()).unwrap();
        assert_eq!(text, None);
    }

    #[test]
    fn selective_attachment_read_keeps_full_command_output() {
        let mut state = RolloutIngestionState::default();
        let input = r#"const r = await tools.shell_command({
  command: "Get-Content -Raw -LiteralPath 'C:\\Users\\ExampleUser\\.codex\\attachments\\00000000-0000-4000-8000-000000000002\\goal-objective.md'",
  workdir: "D:\\workspace",
  timeout_ms: 10000
});
text(r);"#
            .to_string();
        let call = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:00Z",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "attachment-call",
                "input": input
            }
        });
        let call_bytes = serde_json::to_vec(&call).unwrap();
        assert!(
            parse_rollout_line(&call_bytes, &mut state)
                .unwrap()
                .is_some()
        );

        let output = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:01Z",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": "attachment-call",
                "output": "Process exited with code 0\nFinal output:\nfull objective bytes"
            }
        });
        let output_bytes = serde_json::to_vec(&output).unwrap();
        let compact = parse_rollout_line(&output_bytes, &mut state)
            .unwrap()
            .unwrap();
        let payload = compact
            .as_object()
            .and_then(|record| record.get("payload"))
            .and_then(Value::as_object)
            .unwrap();
        assert!(payload.contains_key("output"));
        assert!(!payload.contains_key("_compact_shell_summary"));
    }

    #[test]
    fn selective_actual_custom_attachment_read_restores_from_rollout_output() {
        let mut state = RolloutIngestionState::default();
        let input = r#"const r = await tools.shell_command({
  command: "Get-Content -Raw -LiteralPath 'C:\\Users\\ExampleUser\\.codex\\attachments\\00000000-0000-4000-8000-000000000002\\goal-objective.md'",
  workdir: "D:\\workspace",
  timeout_ms: 10000
});
text(r);"#;
        let call = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:00Z",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "attachment-custom",
                "input": input
            }
        });
        let output = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:01Z",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": "attachment-custom",
                "output": [
                    {"type": "input_text", "text": "Script completed\nWall time 0.7 seconds\nOutput:\n"},
                    {"type": "input_text", "text": "Exit code: 0\nWall time: 0.6 seconds\nOutput:\nobjective body\r\n"}
                ]
            }
        });
        let call_compact = parse_rollout_line(&serde_json::to_vec(&call).unwrap(), &mut state)
            .unwrap()
            .unwrap();
        let output_compact = parse_rollout_line(&serde_json::to_vec(&output).unwrap(), &mut state)
            .unwrap()
            .unwrap();
        let records = vec![(1, call_compact), (2, output_compact)];
        let registry = collect_attachment_contents(&records, Path::new("missing-attachments"));
        assert_eq!(registry.len(), 1);
        let entry = registry.values().next().unwrap();
        assert_eq!(
            value_str(entry.metadata.get("restoration_source")),
            Some("rollout_tool_output")
        );
        assert_eq!(
            value_str(entry.metadata.get("restoration_status")),
            Some("restored")
        );
        // The command renderer contributes one framing CR/LF after stdout.
        // Exact attachment content removes only that framing line ending.
        assert_eq!(entry.content.as_deref(), Some("objective body"));
    }

    #[test]
    fn selective_function_attachment_read_restores_from_rollout_output() {
        let mut state = RolloutIngestionState::default();
        let arguments = r#"{"command":"Get-Content -Raw -LiteralPath 'C:\\Users\\ExampleUser\\.codex\\attachments\\00000000-0000-4000-8000-000000000003\\goal-objective.md'","workdir":"D:\\workspace"}"#;
        let call = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:00Z",
            "payload": {
                "type": "function_call",
                "name": "shell_command",
                "call_id": "attachment-function",
                "arguments": arguments
            }
        });
        let output = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:01Z",
            "payload": {
                "type": "function_call_output",
                "call_id": "attachment-function",
                "output": "Exit code: 0\nWall time: 0.8 seconds\nOutput:\nfunction objective\r\n"
            }
        });
        let call_compact = parse_rollout_line(&serde_json::to_vec(&call).unwrap(), &mut state)
            .unwrap()
            .unwrap();
        let output_compact = parse_rollout_line(&serde_json::to_vec(&output).unwrap(), &mut state)
            .unwrap()
            .unwrap();
        let records = vec![(1, call_compact), (2, output_compact)];
        let registry = collect_attachment_contents(&records, Path::new("missing-attachments"));
        assert_eq!(registry.len(), 1);
        let entry = registry.values().next().unwrap();
        assert_eq!(
            value_str(entry.metadata.get("restoration_source")),
            Some("rollout_tool_output")
        );
        // Direct function-call rendering also contributes one framing CRLF after
        // stdout; the exact attachment bytes therefore do not include it.
        assert_eq!(entry.content.as_deref(), Some("function objective"));
    }

    #[test]
    fn selective_multi_shell_attachment_exec_keeps_summary_only() {
        let mut state = RolloutIngestionState::default();
        let input = r#"const results = await Promise.all([
  tools.shell_command({command: "Get-Content -Raw -LiteralPath 'C:\\Users\\ExampleUser\\.codex\\attachments\\00000000-0000-4000-8000-000000000002\\goal-objective.md'"}),
  tools.shell_command({command: "Get-Content -Raw -LiteralPath 'AGENTS.md'"})
]); text(results);"#;
        let call = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:00Z",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "multi-shell",
                "input": input
            }
        });
        assert!(
            parse_rollout_line(&serde_json::to_vec(&call).unwrap(), &mut state)
                .unwrap()
                .is_some()
        );
        let output = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-08-16T00:00:01Z",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": "multi-shell",
                "output": "Process exited with code 0\nFinal output:\nvery large combined output"
            }
        });
        let compact = parse_rollout_line(&serde_json::to_vec(&output).unwrap(), &mut state)
            .unwrap()
            .unwrap();
        let payload = compact
            .as_object()
            .and_then(|record| record.get("payload"))
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            bool_value(payload.get("_compact_shell_summary")),
            Some(true)
        );
        assert!(!payload.contains_key("output"));
    }

    #[test]
    fn selective_request_user_input_keeps_call_and_matching_output() {
        let mut state = RolloutIngestionState::default();
        let call = br#"{"timestamp":"2026-08-19T04:48:00.098Z","type":"response_item","payload":{"type":"function_call","name":"request_user_input","arguments":"{\"questions\":[{\"header\":\"H\",\"id\":\"q1\",\"question\":\"Q?\",\"options\":[{\"label\":\"A\",\"description\":\"D\"}]}]}","call_id":"call-f009"}}"#;
        let retained_call = parse_rollout_line(call, &mut state).unwrap().unwrap();
        let call_payload = retained_call
            .as_object()
            .and_then(|record| record.get("payload"))
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            value_str(call_payload.get("name")),
            Some("request_user_input")
        );
        assert!(state.function_output_modes.contains_key("call-f009"));

        let output = br#"{"timestamp":"2026-08-19T04:50:24.366Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-f009","output":"{\"answers\":{\"q1\":{\"answers\":[\"A\"]}}}"}}"#;
        let retained_output = parse_rollout_line(output, &mut state).unwrap().unwrap();
        let output_payload = retained_output
            .as_object()
            .and_then(|record| record.get("payload"))
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(value_str(output_payload.get("call_id")), Some("call-f009"));
        assert!(output_payload.contains_key("output"));
        assert!(!state.function_output_modes.contains_key("call-f009"));
    }

    #[test]
    fn user_input_request_pending_and_unparsed_output_are_preserved() {
        let payload = serde_json::json!({
            "type": "function_call",
            "name": "request_user_input",
            "call_id": "call-pending",
            "arguments": "{\"questions\":[{\"header\":\"H\",\"id\":\"q\",\"question\":\"Q?\",\"options\":[]}]}",
            "internal_chat_message_metadata_passthrough": {"turn_id": "turn-1"}
        });
        let payload = payload.as_object().unwrap();
        let (_, mut request) = build_user_input_request(
            payload,
            parse_datetime_text("2026-08-19T00:00:01.123Z"),
            None,
        )
        .unwrap();
        assert_eq!(
            request
                .as_object()
                .and_then(|map| value_str(map.get("status"))),
            Some("pending")
        );

        let output = serde_json::json!({
            "type": "function_call_output",
            "call_id": "call-pending",
            "output": "not-json"
        });
        complete_user_input_request(
            &mut request,
            output.as_object().unwrap(),
            parse_datetime_text("2026-08-19T00:00:02.456Z"),
        );
        let request = request.as_object().unwrap();
        assert_eq!(value_str(request.get("status")), Some("answer_unparsed"));
        assert_eq!(value_str(request.get("raw_output")), Some("not-json"));
        assert_eq!(
            value_str(request.get("answered_at")),
            Some("2026-08-19T00:00:02.456+00:00")
        );
    }

    #[test]
    fn f009_multiple_requests_preserve_questions_options_and_answers() {
        let path = std::env::temp_dir().join(format!(
            "codex-rust-f009-multi-{}-{}.jsonl",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let session_id = "00000000-0000-7000-8000-000000000021";
        let turn_id = "00000000-0000-7000-8000-000000000022";
        let target_cwd = r"D:\workspace";
        let records = vec![
            serde_json::json!({
                "type": "session_meta",
                "timestamp": "2026-08-19T00:00:00Z",
                "payload": {
                    "id": session_id,
                    "timestamp": "2026-08-19T00:00:00Z",
                    "cwd": target_cwd,
                    "originator": "cli",
                    "source": "cli"
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-19T00:00:01Z",
                "payload": {"type": "task_started", "turn_id": turn_id}
            }),
            // F-009 is a session-level domain, but a real exportable Codex session still
            // contains at least one user/assistant message. Keep the synthetic fixture
            // realistic so this test reaches the request/answer assertions instead of
            // failing the converter's existing "No exportable messages" guard first.
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-19T00:00:01.100Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "choose"}],
                    "internal_chat_message_metadata_passthrough": {"turn_id": turn_id}
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-19T00:00:01.100Z",
                "payload": {"type": "user_message", "message": "choose"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-19T00:00:02.123Z",
                "payload": {
                    "type": "function_call",
                    "name": "request_user_input",
                    "call_id": "call-1",
                    "arguments": "{\"questions\":[{\"header\":\"H1\",\"id\":\"q1\",\"question\":\"Q1?\",\"options\":[{\"label\":\"A1\",\"description\":\"D1\"},{\"label\":\"B1\",\"description\":\"D2\"}]},{\"header\":\"H2\",\"id\":\"q2\",\"question\":\"Q2?\",\"options\":[{\"label\":\"A2\",\"description\":\"D3\"}]},{\"header\":\"H3\",\"id\":\"q3\",\"question\":\"Q3?\",\"options\":[{\"label\":\"A3\",\"description\":\"D4\"}]}]}",
                    "internal_chat_message_metadata_passthrough": {"turn_id": turn_id}
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-19T00:00:03.456Z",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call-1",
                    "output": "{\"answers\":{\"q2\":{\"answers\":[\"A2\"]},\"q1\":{\"answers\":[\"A1\"]},\"q3\":{\"answers\":[\"A3\"]}}}"
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-19T00:00:04.123Z",
                "payload": {
                    "type": "function_call",
                    "name": "request_user_input",
                    "call_id": "call-2",
                    "arguments": "{\"questions\":[{\"header\":\"H4\",\"id\":\"q4\",\"question\":\"Q4?\",\"options\":[{\"label\":\"A4\",\"description\":\"D5\"},{\"label\":\"B4\",\"description\":\"D6\"},{\"label\":\"C4\",\"description\":\"D7\"}]}]}",
                    "internal_chat_message_metadata_passthrough": {"turn_id": turn_id}
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-19T00:00:05.789Z",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call-2",
                    "output": "{\"answers\":{\"q4\":{\"answers\":[\"B4\"]}}}"
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-19T00:00:05.900Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "done"}],
                    "internal_chat_message_metadata_passthrough": {"turn_id": turn_id}
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-19T00:00:06Z",
                "payload": {"type": "task_complete", "turn_id": turn_id}
            }),
        ];
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(&serde_json::to_vec(&record).unwrap());
            bytes.push(b'\n');
        }
        fs::write(&path, bytes).unwrap();
        let target = normalize_windows_path(target_cwd);
        let legacy = convert_codex_session_file(&path, &target, Path::new("missing-attachments"))
            .unwrap()
            .unwrap();
        let fused =
            convert_codex_session_file_streaming(&path, &target, Path::new("missing-attachments"))
                .unwrap()
                .unwrap();
        let _ = fs::remove_file(&path);
        let titles = HashMap::new();
        let (_, legacy_handoff) = build_handoff(&legacy, &titles).unwrap();
        let (_, fused_handoff) = build_handoff(&fused, &titles).unwrap();
        assert!(json_semantic_equal(&fused_handoff, &legacy_handoff));
        let requests = fused_handoff
            .as_object()
            .and_then(|map| map.get("user_input_requests"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0]
                .as_object()
                .and_then(|map| map.get("questions"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(3)
        );
        assert_eq!(
            requests[1]
                .as_object()
                .and_then(|map| map.get("questions"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        let first_options = requests[0]
            .as_object()
            .and_then(|map| map.get("questions"))
            .and_then(Value::as_array)
            .and_then(|questions| questions.first())
            .and_then(Value::as_object)
            .and_then(|question| question.get("options"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(
            first_options
                .iter()
                .filter_map(Value::as_object)
                .filter_map(|option| value_str(option.get("label")))
                .collect::<Vec<_>>(),
            vec!["A1", "B1"]
        );
        let answer_keys = requests[0]
            .as_object()
            .and_then(|map| map.get("answers"))
            .and_then(Value::as_object)
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(answer_keys, vec!["q2", "q1", "q3"]);
        assert_eq!(
            requests[1]
                .as_object()
                .and_then(|map| map.get("answers"))
                .and_then(Value::as_object)
                .and_then(|answers| answers.get("q4"))
                .and_then(Value::as_object)
                .and_then(|answer| answer.get("answers"))
                .and_then(Value::as_array)
                .and_then(|answers| answers.first())
                .and_then(Value::as_str),
            Some("B4")
        );
    }

    #[test]
    fn rollout_stream_reader_ignores_malformed_final_line() {
        let path = std::env::temp_dir().join(format!(
            "codex-rust-selective-tail-{}-{}.jsonl",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        fs::write(
            &path,
            b"{\"type\":\"compacted\",\"timestamp\":\"2026-08-16T00:00:00Z\"}\n{not-json}\n",
        )
        .unwrap();
        let records = read_rollout_jsonl(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn jsonl_reader_ignores_malformed_final_line_even_with_newline() {
        let path = std::env::temp_dir().join(format!(
            "codex-rust-jsonl-tail-{}-{}.jsonl",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        fs::write(&path, b"{\"type\":\"ok\"}\n{not-json}\n").unwrap();
        let records = read_generic_jsonl(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(records.len(), 1);
    }
    #[test]
    fn fused_streaming_builder_matches_legacy_on_minimal_session() {
        let path = std::env::temp_dir().join(format!(
            "codex-rust-fused-parity-{}-{}.jsonl",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let session_id = "00000000-0000-7000-8000-000000000001";
        let turn_id = "00000000-0000-7000-8000-000000000002";
        let target_cwd = r"D:\workspace";
        let metadata = serde_json::json!({"turn_id": turn_id});
        let records = vec![
            serde_json::json!({
                "type": "session_meta",
                "timestamp": "2026-08-16T00:00:00Z",
                "payload": {
                    "id": session_id,
                    "timestamp": "2026-08-16T00:00:00Z",
                    "cwd": target_cwd,
                    "originator": "cli",
                    "source": "cli",
                    "cli_version": "test",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-16T00:00:01Z",
                "payload": {
                    "type": "task_started",
                    "turn_id": turn_id,
                    "model_context_window": 128000
                }
            }),
            serde_json::json!({
                "type": "turn_context",
                "timestamp": "2026-08-16T00:00:01.100Z",
                "payload": {
                    "turn_id": turn_id,
                    "model": "gpt-test",
                    "effort": "high"
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-16T00:00:02Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}],
                    "internal_chat_message_metadata_passthrough": metadata.clone()
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-16T00:00:02Z",
                "payload": {
                    "type": "user_message",
                    "message": "hello"
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-16T00:00:02.200Z",
                "payload": {
                    "type": "function_call",
                    "name": "request_user_input",
                    "call_id": "call-user-input",
                    "arguments": "{\"questions\":[{\"header\":\"Choice\",\"id\":\"choice\",\"question\":\"Pick one\",\"options\":[{\"label\":\"A\",\"description\":\"First\"},{\"label\":\"B\",\"description\":\"Second\"}]}]}",
                    "internal_chat_message_metadata_passthrough": {"turn_id": turn_id}
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-16T00:00:02.400Z",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call-user-input",
                    "output": "{\"answers\":{\"choice\":{\"answers\":[\"A\"]}}}"
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-16T00:00:03Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "world"}],
                    "internal_chat_message_metadata_passthrough": metadata
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-16T00:00:04Z",
                "payload": {
                    "type": "task_complete",
                    "turn_id": turn_id,
                    "started_at": 1786838401,
                    "completed_at": 1786838404
                }
            }),
        ];
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(&serde_json::to_vec(&record).unwrap());
            bytes.push(b'\n');
        }
        fs::write(&path, bytes).unwrap();
        let target = normalize_windows_path(target_cwd);
        let legacy = convert_codex_session_file(&path, &target, Path::new("missing-attachments"))
            .unwrap()
            .unwrap();
        let fused =
            convert_codex_session_file_streaming(&path, &target, Path::new("missing-attachments"))
                .unwrap()
                .unwrap();
        let _ = fs::remove_file(&path);
        let titles = HashMap::new();
        let (legacy_filename, legacy_handoff) = build_handoff(&legacy, &titles).unwrap();
        let (fused_filename, fused_handoff) = build_handoff(&fused, &titles).unwrap();
        assert_eq!(fused_filename, legacy_filename);
        assert!(json_semantic_equal(&fused_handoff, &legacy_handoff));
        let handoff = fused_handoff.as_object().unwrap();
        assert_eq!(value_str(handoff.get("exporter_version")), Some(VERSION));
        let requests = handoff
            .get("user_input_requests")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(requests.len(), 1);
        let request = requests[0].as_object().unwrap();
        assert_eq!(value_str(request.get("call_id")), Some("call-user-input"));
        assert_eq!(value_str(request.get("status")), Some("answered"));
        assert_eq!(value_str(request.get("turn_id")), Some(turn_id));
        assert_eq!(
            request
                .get("answers")
                .and_then(Value::as_object)
                .and_then(|answers| answers.get("choice"))
                .and_then(Value::as_object)
                .and_then(|answer| answer.get("answers"))
                .and_then(Value::as_array)
                .and_then(|answers| answers.first())
                .and_then(Value::as_str),
            Some("A"),
        );
    }

    #[test]
    fn fused_streaming_builder_matches_legacy_for_timestamp_local_subagent_fallback() {
        let path = std::env::temp_dir().join(format!(
            "codex-rust-fused-subagent-{}-{}.jsonl",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let session_id = "00000000-0000-7000-8000-000000000011";
        let root_id = "00000000-0000-7000-8000-000000000010";
        let inherited_turn = "00000000-0000-7000-8000-000000000012";
        let local_turn = "00000000-0000-7000-8000-000000000013";
        let target_cwd = r"D:\workspace";
        let records = vec![
            serde_json::json!({
                "type": "session_meta",
                "timestamp": "2026-08-16T00:00:10Z",
                "payload": {
                    "id": session_id,
                    "session_id": root_id,
                    "thread_source": "subagent",
                    "parent_thread_id": root_id,
                    "timestamp": "2026-08-16T00:00:10Z",
                    "cwd": target_cwd,
                    "originator": "subagent",
                    "source": {"subagent": {"thread_spawn": {"depth": 1, "agent_role": "worker"}}},
                    "agent_path": "/root/worker"
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-16T00:00:01Z",
                "payload": {"type": "task_started", "turn_id": inherited_turn}
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-16T00:00:02Z",
                "payload": {"type": "user_message", "message": "inherited user"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-16T00:00:03Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "inherited answer"}],
                    "internal_chat_message_metadata_passthrough": {"turn_id": inherited_turn}
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-16T00:00:03.100Z",
                "payload": {
                    "type": "function_call",
                    "name": "request_user_input",
                    "call_id": "call-inherited",
                    "arguments": "{\"questions\":[{\"header\":\"Inherited\",\"id\":\"inherited\",\"question\":\"Inherited?\",\"options\":[]}]}",
                    "internal_chat_message_metadata_passthrough": {"turn_id": inherited_turn}
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-16T00:00:03.200Z",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call-inherited",
                    "output": "{\"answers\":{\"inherited\":{\"answers\":[\"skip\"]}}}"
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-16T00:00:04Z",
                "payload": {"type": "task_complete", "turn_id": inherited_turn}
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-16T00:00:11Z",
                "payload": {"type": "task_started", "turn_id": local_turn}
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-16T00:00:12Z",
                "payload": {"type": "user_message", "message": "local user"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-16T00:00:13Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "local answer"}],
                    "internal_chat_message_metadata_passthrough": {"turn_id": local_turn}
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-16T00:00:13.100Z",
                "payload": {
                    "type": "function_call",
                    "name": "request_user_input",
                    "call_id": "call-local",
                    "arguments": "{\"questions\":[{\"header\":\"Local\",\"id\":\"local\",\"question\":\"Local?\",\"options\":[{\"label\":\"keep\",\"description\":\"local choice\"}]}]}",
                    "internal_chat_message_metadata_passthrough": {"turn_id": local_turn}
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-08-16T00:00:13.200Z",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call-local",
                    "output": "{\"answers\":{\"local\":{\"answers\":[\"keep\"]}}}"
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-08-16T00:00:14Z",
                "payload": {"type": "task_complete", "turn_id": local_turn}
            }),
        ];
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(&serde_json::to_vec(&record).unwrap());
            bytes.push(b'\n');
        }
        fs::write(&path, bytes).unwrap();
        let target = normalize_windows_path(target_cwd);
        let legacy = convert_codex_session_file(&path, &target, Path::new("missing-attachments"))
            .unwrap()
            .unwrap();
        let fused =
            convert_codex_session_file_streaming(&path, &target, Path::new("missing-attachments"))
                .unwrap()
                .unwrap();
        let _ = fs::remove_file(&path);
        let titles = HashMap::new();
        let (_, legacy_handoff) = build_handoff(&legacy, &titles).unwrap();
        let (_, fused_handoff) = build_handoff(&fused, &titles).unwrap();
        assert!(json_semantic_equal(&fused_handoff, &legacy_handoff));
        let contents: Vec<&str> = fused_handoff
            .as_object()
            .and_then(|map| map.get("messages"))
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_object)
            .filter_map(|map| raw_str(map.get("content")))
            .collect();
        assert_eq!(contents, vec!["local user", "local answer"]);
        let requests = fused_handoff
            .as_object()
            .and_then(|map| map.get("user_input_requests"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .as_object()
                .and_then(|map| value_str(map.get("call_id"))),
            Some("call-local")
        );
        assert_eq!(
            requests[0]
                .as_object()
                .and_then(|map| value_str(map.get("turn_id"))),
            Some(local_turn)
        );
    }

    #[test]
    fn prefix_route_drops_reasoning_from_small_canonical_prefix() {
        let line = br#"{"timestamp":"2026-08-16T00:00:00Z","type":"response_item","payload":{"type":"reasoning","id":"r1","encrypted_content":"very large payload would follow"}}"#;
        let state = RolloutIngestionState::default();
        let route = prefix_route(line, &state);
        assert_eq!(route.decision, PrefixDecision::Drop);
        assert_eq!(route.timestamp, Some("2026-08-16T00:00:00Z"));
    }

    #[test]
    fn canonical_prefix_parser_accepts_optional_ordinal_between_timestamp_and_type() {
        let line = br#"{"timestamp":"2026-08-16T00:00:00.123Z","ordinal":42,"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#;
        let prefix = canonical_prefix_fields(line).expect("canonical prefix with ordinal");
        assert_eq!(prefix.timestamp, "2026-08-16T00:00:00.123Z");
        assert_eq!(prefix.record_type, "event_msg");
        assert_eq!(prefix.payload_type, Some("task_started"));
    }

    #[test]
    fn canonical_prefix_parser_rejects_non_decimal_ordinal_fast_path() {
        let line = br#"{"timestamp":"2026-08-16T00:00:00.123Z","ordinal":"42","type":"event_msg","payload":{"type":"task_started"}}"#;
        assert!(canonical_prefix_fields(line).is_none());
    }

    #[test]
    fn canonical_prefix_parser_extracts_generated_layout_without_search() {
        let line = br#"{"timestamp":"2026-08-16T00:00:00.123Z","type":"response_item","payload":{"type":"function_call_output","id":"fco_123","call_id":"call_123","output":[{"type":"input_text","text":"ok"}]}}"#;
        let prefix = canonical_prefix_fields(line).unwrap();
        assert_eq!(prefix.timestamp, "2026-08-16T00:00:00.123Z");
        assert_eq!(prefix.record_type, "response_item");
        assert_eq!(prefix.payload_type, Some("function_call_output"));
        let after_payload_type = prefix.after_payload_type.unwrap();
        assert_eq!(
            direct_call_id_after_payload_type(line, after_payload_type),
            Some("call_123")
        );

        let noncanonical = br#"{ "timestamp":"2026-08-16T00:00:00.123Z","type":"response_item","payload":{"type":"reasoning"}}"#;
        assert!(canonical_prefix_fields(noncanonical).is_none());
    }

    #[test]
    fn prefix_raw_shell_summary_matches_decoded_summary() {
        let decoded =
            "Process exited with code 0\nFinal output:\nalpha\nExit code: -2\n[truncated output]";
        let raw = br#"{"timestamp":"2026-08-16T00:00:00Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_1","output":[{"type":"input_text","text":"Process exited with code 0\nFinal output:\nalpha\nExit code: -2\n[truncated output]"}]}}"#;
        assert_eq!(
            raw_shell_output_summary(raw),
            shell_output_summary(Some(decoded))
        );
    }

    #[test]
    fn cached_activity_regex_matches_compile_per_call_classifier() {
        let samples = [
            "pytest -q",
            "python -m pytest tests/test_export.py",
            "git diff --check",
            "git status --short",
            "Get-Content README.md -Raw",
            "rg -n TODO src",
            "Set-Content out.txt x",
            "python script.py",
            "echo hello > out.txt",
            "echo hello",
            "",
        ];
        for command in samples {
            assert_eq!(
                compact_command_category_impl(command, false),
                compact_command_category_impl(command, true),
                "classifier mismatch for {command:?}",
            );
        }
    }

    #[test]
    fn cached_js_parser_regex_matches_compile_per_call_patterns() {
        let samples = [
            r#"const cmd = `git status`; tools.shell_command({ command: cmd, workdir: 'D:\\repo', timeout_ms: 1000 })"#,
            r#"for (const [path, line] of [["a.rs", 1]]) { tools.shell_command({ command: `Get-Content ${path}`, timeout_ms: 2000 }) }"#,
            r#"tools.web__run({ open: [{ ref_id: "turn1" }], click: [{ ref_id: "turn2" }], url: "https://example.com" })"#,
        ];
        let static_patterns: [(&OnceLock<Regex>, &str); 7] = [
            (&JS_TEMPLATE_EXPR_RE, r"\$\{([^{}]+)\}"),
            (&JS_IDENTIFIER_RE, r"^[A-Za-z_$][A-Za-z0-9_$]*$"),
            (
                &JS_STRING_ASSIGNMENT_RE,
                r"(?i)(?:^|[^A-Za-z0-9_$])(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*",
            ),
            (
                &JS_ARRAY_ASSIGNMENT_RE,
                r"(?i)(?:^|[^A-Za-z0-9_$])(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*\[",
            ),
            (
                &JS_FOR_OF_RE,
                r"(?is)for\s*\(\s*const\s+(\[[^\]]+\]|[A-Za-z_$][A-Za-z0-9_$]*)\s+of\s*",
            ),
            (&JS_CLOSE_AND_OPEN_RE, r"^\s*\)\s*\{"),
            (&JS_BINDING_IDENTIFIER_RE, r"[A-Za-z_$][A-Za-z0-9_$]*"),
        ];
        for (cache, pattern) in static_patterns {
            for sample in samples {
                let cached = js_static_regex_for_mode(cache, pattern, false)
                    .as_regex()
                    .find_iter(sample)
                    .map(|m| (m.start(), m.end(), m.as_str().to_string()))
                    .collect::<Vec<_>>();
                let legacy = js_static_regex_for_mode(cache, pattern, true)
                    .as_regex()
                    .find_iter(sample)
                    .map(|m| (m.start(), m.end(), m.as_str().to_string()))
                    .collect::<Vec<_>>();
                assert_eq!(cached, legacy);
            }
        }
        for name in [
            "command",
            "workdir",
            "timeout_ms",
            "ref_id",
            "url",
            "open",
            "click",
        ] {
            for sample in samples {
                let cached = property_regex_for_mode(name, false)
                    .unwrap()
                    .as_regex()
                    .captures_iter(sample)
                    .map(|c| {
                        (
                            c.get(0).unwrap().start(),
                            c.get(0).unwrap().end(),
                            c.get(1).unwrap().as_str().to_string(),
                        )
                    })
                    .collect::<Vec<_>>();
                let legacy = property_regex_for_mode(name, true)
                    .unwrap()
                    .as_regex()
                    .captures_iter(sample)
                    .map(|c| {
                        (
                            c.get(0).unwrap().start(),
                            c.get(0).unwrap().end(),
                            c.get(1).unwrap().as_str().to_string(),
                        )
                    })
                    .collect::<Vec<_>>();
                assert_eq!(cached, legacy);
            }
        }
    }

    #[test]
    fn prefix_and_legacy_ingestion_match_compact_records() {
        let path = std::env::temp_dir().join(format!(
            "codex-rust-prefix-ingestion-{}-{}.jsonl",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let records = vec![
            serde_json::json!({
                "timestamp": "2026-08-16T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": "00000000-0000-7000-8000-000000000101",
                    "timestamp": "2026-08-16T00:00:00Z",
                    "cwd": "D:\\workspace"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-08-16T00:00:01Z",
                "type": "response_item",
                "payload": {
                    "type": "reasoning",
                    "id": "r1",
                    "encrypted_content": "bulk"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-08-16T00:00:02Z",
                "type": "compacted",
                "payload": {"very_large": "discard me"}
            }),
            serde_json::json!({
                "timestamp": "2026-08-16T00:00:03Z",
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "hello"
                }
            }),
        ];
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(&serde_json::to_vec(&record).unwrap());
            bytes.push(b'\n');
        }
        fs::write(&path, bytes).unwrap();
        let prefix = read_rollout_jsonl(&path).unwrap();
        let legacy = read_rollout_jsonl_legacy(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(prefix.len(), legacy.len());
        for ((prefix_line, prefix_value), (legacy_line, legacy_value)) in
            prefix.iter().zip(legacy.iter())
        {
            assert_eq!(prefix_line, legacy_line);
            assert!(json_semantic_equal(prefix_value, legacy_value));
        }
    }

    #[test]
    fn fused_prefix_record_reader_matches_retained_prefix_records() {
        let path = std::env::temp_dir().join(format!(
            "codex-rust-fused-prefix-reader-{}-{}.jsonl",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let records = vec![
            serde_json::json!({
                "timestamp": "2026-08-16T00:00:00Z",
                "type": "session_meta",
                "payload": {"id": "00000000-0000-7000-8000-000000000201", "timestamp": "2026-08-16T00:00:00Z", "cwd": "D:\\workspace"}
            }),
            serde_json::json!({
                "timestamp": "2026-08-16T00:00:01Z",
                "type": "response_item",
                "payload": {"type": "reasoning", "id": "drop-me"}
            }),
            serde_json::json!({
                "timestamp": "2026-08-16T00:00:02Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "hello"}
            }),
        ];
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(&serde_json::to_vec(&record).unwrap());
            bytes.push(b'\n');
        }
        fs::write(&path, bytes).unwrap();
        let retained = read_rollout_jsonl(&path).unwrap();
        let mut fused = Vec::new();
        for_each_rollout_record(&path, |line, record| {
            fused.push((line, record));
            Ok(StreamControl::Continue)
        })
        .unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(retained.len(), fused.len());
        for ((retained_line, retained_value), (fused_line, fused_value)) in
            retained.iter().zip(fused.iter())
        {
            assert_eq!(retained_line, fused_line);
            assert!(json_semantic_equal(retained_value, fused_value));
        }
    }

    #[test]
    fn lightweight_activity_sort_matches_legacy_clone_sort() {
        let workspace = r"D:\\repo";

        let mut inspection = Activity::new("command", Some("turn-a".to_string()), Some(100), 3);
        inspection.executions.push(obj!(
            "command" => Value::String("git status".to_string()),
            "working_directory" => Value::String(workspace.to_string()),
            "exit_code" => Value::Number(0.into()),
        ));
        complete_command_activity(&mut inspection, vec![0], false);

        let mut validation = Activity::new("command", Some("turn-a".to_string()), Some(100), 1);
        validation.executions.push(obj!(
            "command" => Value::String("pytest -q".to_string()),
            "working_directory" => Value::String(workspace.to_string()),
            "timeout_ms" => Value::Number(20_000.into()),
            "exit_code" => Value::Number(0.into()),
        ));
        validation.end_time = Some(110);
        complete_command_activity(&mut validation, vec![0], false);

        let mut web = Activity::new("web_search", Some("turn-b".to_string()), Some(90), 2);
        web.queries.push("rust activity sort".to_string());
        web.references.push("ref-1".to_string());

        let mut generic = Activity::new("custom_tool", Some("turn-b".to_string()), Some(100), 1);
        generic.action = Some("inspect".to_string());

        let activities = vec![inspection, validation, web, generic];
        for detail in ["Compact", "Full"] {
            let optimized = activities_for_output_impl(&activities, workspace, detail, false);
            let legacy = activities_for_output_impl(&activities, workspace, detail, true);
            assert_eq!(
                optimized, legacy,
                "activity sort parity failed for {detail}"
            );
        }
    }

    #[test]
    fn lightweight_activity_sort_matches_legacy_large_unknown_commands() {
        let workspace = r"D:\\repo";
        let mut activities = Vec::new();
        let mut state = 0x9e37_79b9_u64;
        for sequence in 0..2048usize {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let time = Some(1_000_000 + ((state >> 16) % 97) as i64);
            let mut activity = Activity::new(
                "command",
                Some(format!("turn-{}", sequence % 11)),
                time,
                sequence,
            );
            activity.executions.push(obj!(
                "command" => Value::String(format!("echo {:04} {}", sequence, "x".repeat(96))),
                "working_directory" => Value::String(workspace.to_string()),
            ));
            activity.invocation_count = 1;
            activity.result_mapping = "unknown".to_string();
            activities.push(activity);
        }
        for detail in ["Compact", "Full"] {
            let optimized = activities_for_output_impl(&activities, workspace, detail, false);
            let legacy = activities_for_output_impl(&activities, workspace, detail, true);
            assert_eq!(
                optimized, legacy,
                "large activity sort parity failed for {detail}"
            );
        }
    }

    #[test]
    fn direct_crlf_serializer_matches_legacy_bytes() {
        let value = serde_json::json!({
            "title": "Unicode 測試",
            "conversation_id": "00000000-0000-7000-8000-000000000321",
            "nested": {"line": "a\nb", "array": [1, true, null]}
        });
        LEGACY_OUTPUT_IO_ENABLED.store(false, AtomicOrdering::SeqCst);
        let direct = serialize_handoff(&value, SerializerMode::Orjson2).unwrap();
        let legacy = serialize_handoff_legacy(&value, SerializerMode::Orjson2).unwrap();
        assert_eq!(direct, legacy);
    }

    #[test]
    fn fast_crlf_serializer_matches_scalar_bytes() {
        let value = serde_json::json!({
            "title": "Fast CRLF 測試",
            "conversation_id": "00000000-0000-7000-8000-000000000323",
            "nested": {
                "line": "alpha\nbeta",
                "array": [1, 2, 3, {"unicode": "資料"}]
            }
        });
        LEGACY_OUTPUT_IO_ENABLED.store(false, AtomicOrdering::SeqCst);
        let scalar =
            serialize_handoff_with_fast_crlf(&value, SerializerMode::Orjson2, false).unwrap();
        let fast = serialize_handoff_with_fast_crlf(&value, SerializerMode::Orjson2, true).unwrap();
        assert_eq!(fast, scalar);
        assert!(fast.ends_with(b"\r\n"));
    }

    #[test]
    fn independent_export_lane_ranges_cover_all_items() {
        let write = export_lane_ranges(120, 4);
        let finalize = export_lane_ranges(120, 8);
        assert_eq!(write.first().unwrap().start, 0);
        assert_eq!(write.last().unwrap().end, 120);
        assert_eq!(finalize.first().unwrap().start, 0);
        assert_eq!(finalize.last().unwrap().end, 120);
        assert_eq!(write.iter().map(|range| range.len()).sum::<usize>(), 120);
        assert_eq!(finalize.iter().map(|range| range.len()).sum::<usize>(), 120);
    }

    #[test]
    fn compact_stream_writer_preserves_json_semantics() {
        let value = serde_json::json!({
            "title": "Compact 測試",
            "conversation_id": "00000000-0000-7000-8000-000000000322",
            "nested": {"line": "a\nb", "array": [1, true, null]}
        });
        let unique = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "codex-rust-compact-stream-{}-{}.json",
            std::process::id(),
            unique
        ));
        let timestamp = parse_datetime_text("2026-08-16T00:00:00Z").unwrap();
        LEGACY_OUTPUT_IO_ENABLED.store(false, AtomicOrdering::SeqCst);
        let written =
            write_handoff_temp(&path, &value, SerializerMode::Compact, timestamp, timestamp)
                .unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(written, bytes.len() as u64);
        assert!(bytes.ends_with(b"\n"));
        assert!(!bytes.windows(2).any(|window| window == b"\r\n"));
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json_semantic_equal(&parsed, &value));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn single_metadata_discovery_matches_legacy_size_order() {
        let unique = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "codex-rust-discovery-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("rollout-c.jsonl"), b"12345").unwrap();
        fs::write(root.join("nested").join("rollout-a.jsonl"), b"123").unwrap();
        fs::write(root.join("nested").join("rollout-b.jsonl"), b"123").unwrap();
        fs::write(root.join("ignore.txt"), b"ignored").unwrap();

        let mut legacy = discover_rollouts(&root).unwrap();
        legacy.sort_by_key(|path| {
            std::cmp::Reverse(
                fs::metadata(path)
                    .map(|metadata| metadata.len())
                    .unwrap_or_default(),
            )
        });
        let optimized: Vec<PathBuf> = discover_rollouts_with_sizes(&root)
            .unwrap()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        assert_eq!(optimized, legacy);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn compressed_rollout_discovery_is_unique_and_prefer_plain() {
        let unique = TEMP_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "codex-rust-zstd-discovery-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&root).unwrap();
        let plain = root.join("rollout-a.jsonl");
        let compressed = root.join("rollout-a.jsonl.zst");
        fs::write(
            &compressed,
            include_bytes!("../fixtures/sample-rollout.jsonl.zst"),
        )
        .unwrap();
        assert_eq!(discover_rollouts(&root).unwrap(), vec![compressed.clone()]);
        fs::write(&plain, b"x").unwrap();
        assert_eq!(discover_rollouts(&root).unwrap(), vec![plain.clone()]);
        fs::write(&plain, b"").unwrap();
        assert_eq!(discover_rollouts(&root).unwrap(), vec![compressed.clone()]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn compressed_rollout_records_parse_and_reject_bad_trailer() {
        let unique = TEMP_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "codex-rust-zstd-parser-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&root).unwrap();
        let compressed = root.join("rollout-a.jsonl.zst");
        fs::write(
            &compressed,
            include_bytes!("../fixtures/sample-rollout.jsonl.zst"),
        )
        .unwrap();
        let mut n = 0usize;
        for_each_rollout_record(&compressed, |_, value| {
            assert_eq!(
                value.get("type").and_then(Value::as_str),
                Some("session_meta")
            );
            n += 1;
            Ok(StreamControl::Continue)
        })
        .unwrap();
        assert_eq!(n, 1);
        let bad = [
            &include_bytes!("../fixtures/sample-rollout.jsonl.zst")[..],
            b"garbage",
        ]
        .concat();
        fs::write(&compressed, bad).unwrap();
        assert!(for_each_rollout_record(&compressed, |_, _| Ok(StreamControl::Continue)).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn existing_conversation_id_prefix_extracts_canonical_output() {
        let value = serde_json::json!({
            "title": "title with conversation_id text",
            "create_time": "2026-08-16T00:00:00Z",
            "update_time": "2026-08-16T00:00:01Z",
            "conversation_id": "00000000-0000-7000-8000-000000000654",
            "codex": {}
        });
        let bytes = serialize_handoff_legacy(&value, SerializerMode::Orjson2).unwrap();
        assert_eq!(
            conversation_id_from_prefix(&bytes).as_deref(),
            Some("00000000-0000-7000-8000-000000000654")
        );
    }

    #[test]
    fn cached_residual_regex_matches_compile_per_call() {
        static TEST_RE: OnceLock<Regex> = OnceLock::new();
        let pattern = r"(?i)^[0-9a-f]{8}-[0-9a-f-]+$";
        for sample in ["12345678-abcd", "not-a-uuid", "ABCDEF12-3456"] {
            let cached = residual_static_regex_for_mode(&TEST_RE, pattern, false)
                .as_regex()
                .is_match(sample);
            let legacy = residual_static_regex_for_mode(&TEST_RE, pattern, true)
                .as_regex()
                .is_match(sample);
            assert_eq!(cached, legacy);
        }
    }

    #[test]
    fn reverse_existing_exports_maps_paths_to_conversation_ids() {
        let mut existing = HashMap::<String, Vec<PathBuf>>::new();
        existing.insert(
            "conversation-a".to_string(),
            vec![PathBuf::from(r"D:\handoff\a.handoff.json")],
        );
        existing.insert(
            "conversation-b".to_string(),
            vec![PathBuf::from(r"D:\handoff\b.handoff.json")],
        );
        let reversed = reverse_existing_exports(&existing);
        assert_eq!(
            reversed
                .get(&PathBuf::from(r"D:\handoff\a.handoff.json"))
                .map(String::as_str),
            Some("conversation-a")
        );
        assert_eq!(
            reversed
                .get(&PathBuf::from(r"D:\handoff\b.handoff.json"))
                .map(String::as_str),
            Some("conversation-b")
        );
    }

    fn guardian_test_session(
        session_id: &str,
        root_session_id: &str,
        thread_source: &str,
        parent_thread_id: Option<&str>,
        guardian_review: bool,
        last_time: &str,
        session_state: &str,
    ) -> Session {
        let create_time = parse_datetime_text("2026-08-26T00:00:00Z").unwrap();
        Session {
            session_id: session_id.to_string(),
            root_session_id: root_session_id.to_string(),
            thread_source: thread_source.to_string(),
            guardian_review,
            parent_thread_id: parent_thread_id.map(str::to_string),
            forked_from_id: None,
            agent_nickname: None,
            agent_path: None,
            agent_depth: None,
            agent_role: None,
            multi_agent_version: None,
            create_time: Some(create_time),
            last_message_time: parse_datetime_text(last_time).unwrap(),
            messages: Vec::new(),
            raw_messages: Vec::new(),
            plans: Vec::new(),
            plan_updates: Vec::new(),
            goals: Vec::new(),
            goal_updates: Vec::new(),
            user_input_requests: Vec::new(),
            agent_events: Vec::new(),
            delegation: None,
            reviews: Vec::new(),
            memory_citations: Vec::new(),
            writing_blocks: Vec::new(),
            ui_actions: Vec::new(),
            ui_parse_warnings: Vec::new(),
            rollbacks: Vec::new(),
            turns: Vec::new(),
            codex: Value::Object(Obj::new()),
            workspace: Value::Object(Obj::new()),
            context: None,
            usage: None,
            activity_detail: "compact".to_string(),
            session_state: session_state.to_string(),
            changes: Vec::new(),
            activities: Vec::new(),
            execution_profile: None,
            data_quality: None,
            source_path: PathBuf::from(format!("{session_id}.jsonl")),
            subagent_sessions: Vec::new(),
            internal_session_ids: Vec::new(),
        }
    }

    #[test]
    fn owned_handoff_moves_nested_payload_without_changing_schema() {
        let mut session = guardian_test_session(
            "root-move",
            "root-move",
            "user",
            None,
            false,
            "2026-08-26T00:00:00Z",
            "idle",
        );
        session
            .messages
            .push(serde_json::json!({"id":"u01","content":"user"}));
        session.turns.push(serde_json::json!({"id":"turn-one"}));
        session
            .activities
            .push(serde_json::json!({"type":"command","cmd":"echo ok"}));
        session.changes.push(serde_json::json!({"path":"file.rs"}));
        session.plans.push(serde_json::json!({"steps":["a", "b"]}));
        session.delegation = Some(serde_json::json!({"turn":"one"}));
        session.usage = Some(serde_json::json!({"total":123}));
        let mut child = guardian_test_session(
            "child-move",
            "root-move",
            "subagent",
            Some("root-move"),
            false,
            "2026-08-26T00:00:00Z",
            "idle",
        );
        child
            .messages
            .push(serde_json::json!({"id":"a01","content":"child"}));
        child.activities.push(serde_json::json!({"event":"tool"}));
        session.subagent_sessions.push(child);
        let mut titles = HashMap::new();
        titles.insert(session.session_id.clone(), "Unit Test".to_string());
        let (expected_filename, expected) = build_handoff(&session, &titles).unwrap();
        let (actual_filename, actual) = build_handoff_move(&mut session, &titles).unwrap();
        assert_eq!(expected_filename, actual_filename);
        assert_eq!(expected, actual);
        assert!(session.messages.is_empty());
        assert!(session.turns.is_empty());
        assert!(session.activities.is_empty());
        assert!(session.changes.is_empty());
        assert!(session.usage.is_none());
        assert_eq!(session.subagent_sessions[0].session_id, "child-move");
        assert!(session.subagent_sessions[0].messages.is_empty());
        assert!(session.subagent_sessions[0].activities.is_empty());
    }

    #[test]
    fn owned_handoff_preserves_empty_message_array_and_omits_optional_empty_fields() {
        let mut session = guardian_test_session(
            "empty-root",
            "empty-root",
            "user",
            None,
            false,
            "2026-08-26T00:00:00Z",
            "idle",
        );
        let mut names = HashMap::new();
        names.insert("empty-root".to_string(), "Empty".to_string());
        let (_, reference) = build_handoff(&session, &names).unwrap();
        let (_, owned) = build_handoff_move(&mut session, &names).unwrap();
        assert_eq!(reference, owned);
        assert_eq!(owned["messages"], serde_json::json!([]));
        assert!(owned.get("activities").is_none());
    }

    #[test]
    fn session_timestamp_reference_traversal_matches_object_walk() {
        let mut session = guardian_test_session(
            "root-ts",
            "root-ts",
            "user",
            None,
            false,
            "2026-08-26T00:00:00Z",
            "idle",
        );
        session.turns.push(serde_json::json!({
            "updated_at": "2026-08-26T00:03:00Z",
            "retry_at": "2099-01-01T00:00:00Z"
        }));
        session.messages.push(serde_json::json!({
            "create_time": "2026-08-26T00:07:00Z",
            "content": {"unrelated": "2099-01-01T00:00:00Z"}
        }));
        session.activities.push(serde_json::json!({
            "events": [{"completed_at": "2026-08-26T00:05:00Z"}]
        }));
        session.usage = Some(serde_json::json!({"updated_at":"2026-08-26T00:06:00Z"}));
        let original = Value::Object(obj!(
            "turns" => Value::Array(session.turns.clone()),
            "messages" => Value::Array(session.messages.clone()),
            "goals" => Value::Array(session.goals.clone()),
            "goal_updates" => Value::Array(session.goal_updates.clone()),
            "plan_updates" => Value::Array(session.plan_updates.clone()),
            "user_input_requests" => Value::Array(session.user_input_requests.clone()),
            "activities" => Value::Array(session.activities.clone()),
            "changes" => Value::Array(session.changes.clone()),
            "agent_events" => Value::Array(session.agent_events.clone()),
            "usage" => session.usage.clone().unwrap_or(Value::Null),
            "context" => session.context.clone().unwrap_or(Value::Null),
        ));
        assert_eq!(
            latest_session_payload_timestamp(&session),
            max_timestamp_in_value(&original, ""),
        );
        assert_eq!(
            latest_session_payload_timestamp(&session),
            parse_datetime_text("2026-08-26T00:07:00Z"),
        );
    }

    #[test]
    fn guardian_source_marker_is_narrow_and_structured() {
        assert!(is_guardian_review_source(&serde_json::json!({
            "subagent": {"other": "guardian"}
        })));
        assert!(is_guardian_review_source(&serde_json::json!({
            "subagent": {"other": " Guardian "}
        })));
        assert!(!is_guardian_review_source(&serde_json::json!({
            "subagent": {"thread_spawn": {"depth": 1, "agent_role": "worker"}}
        })));
        assert!(!is_guardian_review_source(&Value::String(
            "guardian".to_string()
        )));
    }

    #[test]
    fn guardian_reviews_are_hidden_without_changing_real_subagents() {
        let root_id = "00000000-0000-7000-8000-000000000900";
        let legacy_guardian_id = "synthetic-legacy-guardian";
        let new_guardian_id = "synthetic-new-guardian";
        let worker_id = "synthetic-thread-spawn-worker";

        let root = guardian_test_session(
            root_id,
            root_id,
            "user",
            None,
            false,
            "2026-08-26T00:00:00Z",
            "idle",
        );
        let legacy_guardian = guardian_test_session(
            legacy_guardian_id,
            root_id,
            "subagent",
            Some(root_id),
            true,
            "2026-08-26T00:02:00Z",
            "idle",
        );
        let new_guardian = guardian_test_session(
            new_guardian_id,
            root_id,
            "guardian_review",
            Some(root_id),
            true,
            "2026-08-26T00:03:00Z",
            "active_at_export",
        );
        let worker = guardian_test_session(
            worker_id,
            root_id,
            "subagent",
            Some(root_id),
            false,
            "2026-08-26T00:01:00Z",
            "idle",
        );
        let sessions = HashMap::from([
            (root_id.to_string(), root),
            (legacy_guardian_id.to_string(), legacy_guardian),
            (new_guardian_id.to_string(), new_guardian),
            (worker_id.to_string(), worker),
        ]);

        let (grouped, integrated_count) = integrate_subagent_sessions(sessions);
        assert_eq!(integrated_count, 1);
        assert_eq!(grouped.len(), 1);
        let root = grouped.get(root_id).expect("root should remain exportable");
        assert_eq!(root.subagent_sessions.len(), 1);
        assert_eq!(root.subagent_sessions[0].session_id, worker_id);
        assert_eq!(root.internal_session_ids.len(), 2);
        assert!(
            root.internal_session_ids
                .iter()
                .any(|value| value == legacy_guardian_id)
        );
        assert!(
            root.internal_session_ids
                .iter()
                .any(|value| value == new_guardian_id)
        );
        // r9 already let legacy Guardian subagents advance root update_time; retain
        // that non-content field. The new guardian_review must not newly advance it.
        assert_eq!(
            root.last_message_time,
            parse_datetime_text("2026-08-26T00:02:00Z").unwrap()
        );
        assert_eq!(root.session_state, "idle");
    }

    #[test]
    fn numbered_filename_width_tracks_total_handoff_count() {
        let cases = [
            (0, 1),
            (1, 1),
            (9, 1),
            (10, 2),
            (30, 2),
            (99, 2),
            (100, 3),
            (999, 3),
            (1000, 4),
            (10_000, 5),
        ];
        for (total, width) in cases {
            assert_eq!(output_numbering_width(total), width, "total={total}");
        }
        let base = "Example-20260711143226.handoff.json";
        for (ordinal, total, expected) in [
            (1, 1, "1. Example-20260711143226.handoff.json"),
            (9, 9, "9. Example-20260711143226.handoff.json"),
            (1, 10, "01. Example-20260711143226.handoff.json"),
            (2, 30, "02. Example-20260711143226.handoff.json"),
            (30, 30, "30. Example-20260711143226.handoff.json"),
            (1, 99, "01. Example-20260711143226.handoff.json"),
            (1, 100, "001. Example-20260711143226.handoff.json"),
            (999, 999, "999. Example-20260711143226.handoff.json"),
            (1, 1000, "0001. Example-20260711143226.handoff.json"),
        ] {
            assert_eq!(numbered_handoff_filename(base, ordinal, total), expected);
        }
    }

    #[test]
    fn conversation_ordinals_follow_create_time_not_update_time() {
        let mut later_created = guardian_test_session(
            "conversation-b",
            "conversation-b",
            "user",
            None,
            false,
            "2026-08-26T00:01:00Z",
            "idle",
        );
        later_created.create_time = Some(parse_datetime_text("2026-07-12T00:00:00Z").unwrap());

        let mut earlier_created = guardian_test_session(
            "conversation-a",
            "conversation-a",
            "user",
            None,
            false,
            "2026-08-26T23:59:00Z",
            "idle",
        );
        earlier_created.create_time = Some(parse_datetime_text("2026-07-11T00:00:00Z").unwrap());

        let mut same_time_later_id = guardian_test_session(
            "conversation-c",
            "conversation-c",
            "user",
            None,
            false,
            "2026-08-20T00:00:00Z",
            "idle",
        );
        same_time_later_id.create_time = later_created.create_time.clone();

        let ordinals = conversation_ordinals(&[later_created, same_time_later_id, earlier_created]);
        assert_eq!(ordinals.get("conversation-a"), Some(&1));
        assert_eq!(ordinals.get("conversation-b"), Some(&2));
        assert_eq!(ordinals.get("conversation-c"), Some(&3));
    }

    #[test]
    fn full_rewrite_overwrites_existing_same_conversation_output() {
        let unique = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let output_directory = std::env::temp_dir().join(format!(
            "codex-rust-full-rewrite-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&output_directory).unwrap();

        let session_id = "00000000-0000-7000-8000-000000000777".to_string();
        let timestamp = parse_datetime_text("2026-08-16T00:00:00Z").unwrap();
        let session = Session {
            session_id: session_id.clone(),
            root_session_id: session_id.clone(),
            thread_source: "cli".to_string(),
            guardian_review: false,
            parent_thread_id: None,
            forked_from_id: None,
            agent_nickname: None,
            agent_path: None,
            agent_depth: None,
            agent_role: None,
            multi_agent_version: None,
            create_time: Some(timestamp),
            last_message_time: timestamp,
            messages: Vec::new(),
            raw_messages: Vec::new(),
            plans: Vec::new(),
            plan_updates: Vec::new(),
            goals: Vec::new(),
            goal_updates: Vec::new(),
            user_input_requests: Vec::new(),
            agent_events: Vec::new(),
            delegation: None,
            reviews: Vec::new(),
            memory_citations: Vec::new(),
            writing_blocks: Vec::new(),
            ui_actions: Vec::new(),
            ui_parse_warnings: Vec::new(),
            rollbacks: Vec::new(),
            turns: Vec::new(),
            codex: Value::Object(Obj::new()),
            workspace: Value::Object(Obj::new()),
            context: None,
            usage: None,
            activity_detail: "Compact".to_string(),
            session_state: "completed".to_string(),
            changes: Vec::new(),
            activities: Vec::new(),
            execution_profile: None,
            data_quality: None,
            source_path: output_directory.join("source.jsonl"),
            subagent_sessions: Vec::new(),
            internal_session_ids: Vec::new(),
        };
        let filename = "full-rewrite-test.handoff.json".to_string();
        let fresh = serde_json::json!({
            "conversation_id": session_id,
            "payload": {"state": "fresh"}
        });
        let stale = serde_json::json!({
            "conversation_id": "00000000-0000-7000-8000-000000000777",
            "payload": {"state": "stale"}
        });
        let final_path = output_directory.join(&filename);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        for (defer_planned_final_delete, replace_existing_final) in
            [(false, false), (true, false), (false, true)]
        {
            for use_pre_serialized in [false, true] {
                for use_pipeline_prepared in [false, true] {
                    fs::write(&final_path, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();
                    let existing = read_existing_exports(&output_directory);
                    let sessions = vec![session.clone()];
                    let candidates = vec![(filename.clone(), fresh.clone())];
                    let serialized = if use_pre_serialized {
                        vec![
                            serialize_handoff_with_fast_crlf(&fresh, SerializerMode::Orjson2, true)
                                .unwrap(),
                        ]
                    } else {
                        Vec::new()
                    };
                    let pre_serialized = use_pre_serialized.then_some(serialized.as_slice());
                    let pre_prepared = if use_pipeline_prepared {
                        Some(vec![
                            prepare_candidate_export(
                                &sessions[0],
                                &filename,
                                &fresh,
                                pre_serialized.map(|all| all[0].as_slice()),
                                &output_directory,
                                &output_directory,
                                SerializerMode::Orjson2,
                            )
                            .unwrap(),
                        ])
                    } else {
                        None
                    };
                    let (bytes_written, _phases) = export_prebuilt_candidates_parallel(
                        &sessions,
                        &candidates,
                        pre_serialized,
                        pre_prepared,
                        &existing,
                        &output_directory,
                        &output_directory,
                        SerializerMode::Orjson2,
                        0,
                        0,
                        0,
                        defer_planned_final_delete,
                        replace_existing_final,
                        &pool,
                    )
                    .unwrap();
                    assert!(bytes_written > 0);
                    let rewritten: Value =
                        serde_json::from_slice(&fs::read(&final_path).unwrap()).unwrap();
                    assert!(json_semantic_equal(&rewritten, &fresh));
                    assert_ne!(rewritten, stale);
                }
            }
        }
        // Changing the required number width must also retire the old 3-digit
        // output for the same conversation rather than leaving duplicate files.
        let previous_numbering = output_directory.join("001. full-rewrite-test.handoff.json");
        let current_name = numbered_handoff_filename(&filename, 1, 1);
        let current_numbering = output_directory.join(&current_name);
        fs::write(
            &previous_numbering,
            serde_json::to_vec_pretty(&stale).unwrap(),
        )
        .unwrap();
        let existing = read_existing_exports(&output_directory);
        let (bytes_written, _phases) = export_prebuilt_candidates_parallel(
            &[session],
            &[(current_name, fresh.clone())],
            None,
            None,
            &existing,
            &output_directory,
            &output_directory,
            SerializerMode::Orjson2,
            0,
            0,
            0,
            false,
            false,
            &pool,
        )
        .unwrap();
        assert!(bytes_written > 0);
        assert!(
            !previous_numbering.exists(),
            "obsolete 001. output was not removed"
        );
        let current: Value = serde_json::from_slice(&fs::read(current_numbering).unwrap()).unwrap();
        assert!(json_semantic_equal(&current, &fresh));
        let _ = fs::remove_dir_all(&output_directory);
    }

    #[test]
    fn isolated_staging_directory_is_sibling_and_cleans_up() {
        let unique = TEMP_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "codex-rust-stage-test-{}-{}",
            std::process::id(),
            unique
        ));
        let output_directory = root.join("Codex");
        fs::create_dir_all(&output_directory).unwrap();
        let staging_path;
        {
            let staging = StagingDirectory::create(&output_directory).unwrap();
            staging_path = staging.path().to_path_buf();
            assert_eq!(staging.path().parent(), output_directory.parent());
            assert_ne!(staging.path(), output_directory.as_path());
            fs::write(staging.path().join("probe.tmp"), b"probe").unwrap();
            assert!(staging.path().is_dir());
        }
        assert!(!staging_path.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn export_lane_ranges_are_balanced_and_complete() {
        let ranges = export_lane_ranges(117, 8);
        assert_eq!(ranges.len(), 8);
        assert_eq!(ranges.first().unwrap().start, 0);
        assert_eq!(ranges.last().unwrap().end, 117);
        let lengths: Vec<usize> = ranges.iter().map(|range| range.end - range.start).collect();
        assert!(lengths.iter().all(|len| *len == 14 || *len == 15));
        let flattened: Vec<usize> = ranges.into_iter().flatten().collect();
        assert_eq!(flattened, (0..117).collect::<Vec<_>>());
    }

    #[test]
    fn export_lane_ranges_clamp_to_item_count() {
        assert!(export_lane_ranges(0, 8).is_empty());
        assert_eq!(export_lane_ranges(3, 32), vec![0..1, 1..2, 2..3]);
        assert_eq!(export_lane_ranges(4, 1), vec![0..4]);
    }

    #[test]
    fn cleanup_lane_ranges_can_be_tuned_independently() {
        let cleanup = export_lane_ranges(120, 16);
        let finalize = export_lane_ranges(120, 1);
        assert_eq!(cleanup.len(), 16);
        assert_eq!(finalize.len(), 1);
        assert_eq!(cleanup.iter().map(|range| range.len()).sum::<usize>(), 120);
        assert_eq!(finalize.iter().map(|range| range.len()).sum::<usize>(), 120);
    }

    #[test]
    fn canonical_payload_slice_extracts_final_payload_object() {
        let line = br#"{"timestamp":"2026-08-19T00:00:00.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#;
        let prefix = canonical_prefix_fields(line).expect("canonical prefix");
        assert_eq!(prefix.record_type, "event_msg");
        assert_eq!(prefix.payload_type, Some("task_started"));
        assert_eq!(
            canonical_payload_raw(line, &prefix),
            Some(r#"{"type":"task_started","turn_id":"t1"}"#)
        );

        let with_outer_metadata = br#"{"timestamp":"2026-08-19T00:00:00.000Z","ordinal":19,"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":{"text":"ok"}},"metadata":{"source":"live"}}"#;
        let prefix = canonical_prefix_fields(with_outer_metadata).expect("canonical prefix");
        assert_eq!(prefix.payload_type, Some("custom_tool_call_output"));
        assert!(
            canonical_payload_raw(with_outer_metadata, &prefix)
                .is_some_and(|raw| raw.contains(r#","metadata":"#))
        );

        let mut direct_state = RolloutIngestionState::default();
        direct_state
            .custom_output_modes
            .insert("c1".to_string(), ToolOutputMode::Full);
        let mut legacy_state = RolloutIngestionState::default();
        legacy_state
            .custom_output_modes
            .insert("c1".to_string(), ToolOutputMode::Full);
        let direct =
            parse_rollout_line_prefix_mode(with_outer_metadata, &mut direct_state, true).unwrap();
        let legacy = parse_rollout_line(with_outer_metadata, &mut legacy_state).unwrap();
        assert_eq!(direct, legacy);
        assert_eq!(
            direct_state.custom_output_modes,
            legacy_state.custom_output_modes
        );

        let reordered = br#"{"type":"event_msg","timestamp":"2026-08-19T00:00:00.000Z","payload":{"type":"task_started"}}"#;
        assert!(canonical_prefix_fields(reordered).is_none());
    }

    #[test]
    fn direct_canonical_prefix_mode_matches_baseline_statefully() {
        let lines: &[&[u8]] = &[
            br#"{"timestamp":"2026-08-19T00:00:00.000Z","type":"session_meta","payload":{"id":"019f0000-0000-7000-8000-000000000001","session_id":"019f0000-0000-7000-8000-000000000001","cwd":"D:\\workspace"}}"#,
            br#"{"timestamp":"2026-08-19T00:00:01.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call-custom","input":"const r = await tools.exec_command({cmd:\"echo hi\"}); text(r);"}}"#,
            br#"{"timestamp":"2026-08-19T00:00:02.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-custom","output":[{"type":"output_text","text":"Exit code: 0\\nOutput:\\nhi\\n"}]}}"#,
            br#"{"timestamp":"2026-08-19T00:00:03.000Z","type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":200000}}}"#,
            br#"{"timestamp":"2026-08-19T00:00:04.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
            br#"{"timestamp":"2026-08-19T00:00:05.000Z","type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"call-f009-direct","arguments":"{\"questions\":[{\"header\":\"H\",\"id\":\"q\",\"question\":\"Q?\",\"options\":[]}]}","internal_chat_message_metadata_passthrough":{"turn_id":"turn-direct"}}}"#,
            br#"{"timestamp":"2026-08-19T00:00:06.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-f009-direct","output":"{\"answers\":{\"q\":{\"answers\":[\"A\"]}}}"}}"#,
        ];
        let mut baseline_state = RolloutIngestionState::default();
        let mut direct_state = RolloutIngestionState::default();
        for line in lines {
            let baseline =
                parse_rollout_line_prefix_mode(line, &mut baseline_state, false).unwrap();
            let direct = parse_rollout_line_prefix_mode(line, &mut direct_state, true).unwrap();
            assert_eq!(baseline, direct, "line={}", String::from_utf8_lossy(line));
        }
        assert_eq!(
            baseline_state.custom_output_modes,
            direct_state.custom_output_modes
        );
        assert_eq!(
            baseline_state.function_output_modes,
            direct_state.function_output_modes
        );
    }

    #[test]
    fn direct_canonical_known_fields_match_legacy_envelope_parser() {
        let lines: &[&[u8]] = &[
            br#"{"timestamp":"2026-08-19T00:00:00.000Z","type":"session_meta","payload":{"id":"019f0000-0000-7000-8000-000000000001","session_id":"019f0000-0000-7000-8000-000000000001","cwd":"D:\\workspace","originator":"codex_cli_rs"}}"#,
            br#"{"timestamp":"2026-08-19T00:00:01.000Z","type":"turn_context","payload":{"turn_id":"turn-1","model":"gpt-5","effort":"high","approval_policy":"on-request","approvals_reviewer":"auto_review","sandbox_policy":{"type":"workspace-write","network_access":false}}}"#,
            br#"{"timestamp":"2026-08-19T00:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":200000,"last_token_usage":{"total_tokens":1234}}}}"#,
            br#"{"timestamp":"2026-08-19T00:00:03.000Z","type":"event_msg","payload":{"type":"task_started","model_context_window":200000,"turn_id":"turn-1"}}"#,
            br#"{"timestamp":"2026-08-19T00:00:04.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
            br#"{"timestamp":"2026-08-19T00:00:05.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call-custom","input":"const r = await tools.exec_command({cmd:\"echo hi\"}); text(r);"}}"#,
            br#"{"timestamp":"2026-08-19T00:00:06.000Z","type":"response_item","payload":{"type":"function_call","name":"update_plan","call_id":"call-plan","arguments":"{}"}}"#,
        ];

        let mut legacy_state = RolloutIngestionState::default();
        let mut direct_state = RolloutIngestionState::default();
        for line in lines {
            let legacy = parse_rollout_line(line, &mut legacy_state).unwrap();
            let prefix = canonical_prefix_fields(line).expect("canonical prefix");
            let payload_raw = canonical_payload_raw(line, &prefix).expect("canonical payload");
            let direct = parse_rollout_known_fields(
                prefix.record_type,
                Some(prefix.timestamp),
                Some(payload_raw),
                prefix.payload_type,
                &mut direct_state,
            )
            .unwrap();
            assert_eq!(legacy, direct, "line={}", String::from_utf8_lossy(line));
        }
        assert_eq!(
            legacy_state.custom_output_modes,
            direct_state.custom_output_modes
        );
        assert_eq!(
            legacy_state.function_output_modes,
            direct_state.function_output_modes
        );
        assert_eq!(
            legacy_state.data_quality.source_unicode_replacement_chars,
            direct_state.data_quality.source_unicode_replacement_chars
        );
        assert_eq!(
            legacy_state.data_quality.affected_tool_outputs,
            direct_state.data_quality.affected_tool_outputs
        );
    }

    #[test]
    fn zero_byte_rollout_is_treated_as_transient_placeholder() {
        let path = std::env::temp_dir().join(format!(
            "codex-rust-empty-rollout-{}-{}.jsonl",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        fs::write(&path, b"").unwrap();
        assert!(is_zero_byte_rollout(&path));

        fs::write(&path, b"{}\n").unwrap();
        assert!(!is_zero_byte_rollout(&path));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn selective_item_completed_preserves_structured_command_timing() {
        let mut state = RolloutIngestionState::default();
        let record = parse_rollout_line_prefix_mode(
            br#"{"timestamp":"2026-08-24T00:00:01.000Z","type":"event_msg","payload":{"type":"item_completed","turn_id":"turn-1","started_at_ms":1000,"completed_at_ms":1450,"item":{"type":"CommandExecution","id":"item-1","command":["pwsh","-Command","echo hi"],"status":"completed","exit_code":0}}}"#,
            &mut state,
            true,
        )
        .unwrap()
        .expect("retained item_completed");
        let payload = record
            .get("payload")
            .and_then(Value::as_object)
            .expect("payload");
        assert_eq!(int_value(payload.get("started_at_ms"), None), Some(1000));
        assert_eq!(int_value(payload.get("completed_at_ms"), None), Some(1450));
        let item = payload
            .get("item")
            .and_then(Value::as_object)
            .expect("item");
        let activity = typed_command_execution_output(
            item,
            value_str(payload.get("turn_id")),
            parse_datetime_value(record.get("timestamp")),
            payload,
        );
        let execution = activity
            .get("executions")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .and_then(Value::as_object)
            .expect("execution");
        assert_eq!(int_value(execution.get("started_at_ms"), None), Some(1000));
        assert_eq!(
            int_value(execution.get("completed_at_ms"), None),
            Some(1450)
        );
        assert_eq!(int_value(execution.get("duration_ms"), None), Some(450));
    }

    #[test]
    fn selective_exec_goal_wrapper_preserves_authoritative_output_pair() {
        for (call_type, output_type) in [
            ("custom_tool_call", "custom_tool_call_output"),
            ("function_call", "function_call_output"),
        ] {
            let mut state = RolloutIngestionState::default();
            let call = format!(
                r#"{{"timestamp":"2026-08-24T00:00:00.000Z","type":"response_item","payload":{{"type":"{call_type}","name":"exec","call_id":"call-goal","input":"const r = await tools.update_goal({{threadId: \"thread-1\", status: \"complete\"}}); text(r);"}}}}"#,
            );
            let retained_call = parse_rollout_line_prefix_mode(call.as_bytes(), &mut state, true)
                .unwrap()
                .expect("retained exec goal wrapper");
            let call_payload = retained_call
                .get("payload")
                .and_then(Value::as_object)
                .expect("call payload");
            assert_eq!(value_str(call_payload.get("name")), Some("exec"));
            assert!(
                raw_str(call_payload.get("input"))
                    .is_some_and(|value| value.contains("tools.update_goal"))
            );

            let output = format!(
                r#"{{"timestamp":"2026-08-24T00:00:01.000Z","type":"response_item","payload":{{"type":"{output_type}","call_id":"call-goal","output":"{{\"goal\":{{\"threadId\":\"thread-1\",\"status\":\"complete\"}}}}"}}}}"#,
            );
            let retained_output =
                parse_rollout_line_prefix_mode(output.as_bytes(), &mut state, true)
                    .unwrap()
                    .expect("retained authoritative goal output");
            let output_payload = retained_output
                .get("payload")
                .and_then(Value::as_object)
                .expect("output payload");
            let result = extract_goal_result(output_payload.get("output")).expect("goal result");
            let goal = result
                .get("goal")
                .and_then(Value::as_object)
                .expect("goal payload");
            assert_eq!(value_str(goal.get("status")), Some("complete"));
        }
    }

    #[test]
    fn selective_token_usage_record_preserves_thread_cumulative_usage() {
        let mut state = RolloutIngestionState::default();
        let record = parse_rollout_line_prefix_mode(
            br#"{"timestamp":"2026-09-03T00:00:00.000Z","type":"token_usage_record","payload":{"thread_token_usage":{"input_tokens":100,"cached_input_tokens":20,"cache_write_input_tokens":3,"output_tokens":25,"reasoning_output_tokens":5,"total_tokens":130},"rate_limits":{"ignored":true}}}"#,
            &mut state,
            true,
        )
        .unwrap()
        .expect("retained token_usage_record");
        let payload = record
            .get("payload")
            .and_then(Value::as_object)
            .expect("usage payload");
        assert!(payload.contains_key("thread_token_usage"));
        assert!(!payload.contains_key("rate_limits"));
        let usage = thread_usage_summary(payload, parse_datetime_value(record.get("timestamp")))
            .expect("thread usage summary");
        let summary = usage
            .get("thread_cumulative")
            .and_then(Value::as_object)
            .expect("thread cumulative");
        assert_eq!(value_str(summary.get("scope")), Some("thread_cumulative"));
        assert_eq!(value_str(summary.get("source")), Some("token_usage_record"));
        assert_eq!(int_value(summary.get("total_tokens"), None), Some(130));
        assert_eq!(
            int_value(summary.get("cache_write_input_tokens"), None),
            Some(3)
        );
    }

    #[test]
    fn selective_turn_context_preserves_approval_reviewer_and_sandbox() {
        let compact = compact_turn_context(
            r#"{"turn_id":"turn-1","model":"gpt-5","approval_policy":"on-request","approvals_reviewer":"auto_review","sandbox_policy":{"type":"workspace-write","network_access":false}}"#,
        )
        .unwrap();
        assert_eq!(
            value_str(compact.get("approvals_reviewer")),
            Some("auto_review")
        );
        let profile = profile_from_turn_context(&compact);
        assert_eq!(
            value_str(profile.get("approvals_reviewer")),
            Some("auto_review")
        );
        assert_eq!(
            value_str(profile.get("approval_policy")),
            Some("on-request")
        );
        assert_eq!(
            value_str(profile.get("sandbox_type")),
            Some("workspace-write")
        );
        assert_eq!(bool_value(profile.get("network_access")), Some(false));
    }

    #[test]
    fn universal_cli_selection_maintains_no_arg_default() {
        let d = Args::try_parse_from(["Export-Codex-Handoff"]).unwrap();
        assert!(d.project_id.is_none() && !d.list_projects);
        assert!(d.target_cwd.is_empty());
        assert!(d.output_directory.as_os_str().is_empty());
        let project = Args::try_parse_from([
            "exporter",
            "--project-id",
            "app-server-id",
            "--output-directory",
            "D:\\other",
        ])
        .unwrap();
        assert_eq!(project.project_id.as_deref(), Some("app-server-id"));
        assert!(
            Args::try_parse_from([
                "exporter",
                "--project-id",
                "abc",
                "--target-cwd",
                "D:\\other"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from(["exporter", "--project-id", "abc", "--legacy-cwd-only"]).is_err()
        );
        assert!(
            Args::try_parse_from(["exporter", "--project-id", "abc", "--list-projects"]).is_err()
        );
        assert!(
            Args::try_parse_from(["exporter", "--list-projects"])
                .unwrap()
                .list_projects
        );
    }

    #[test]
    fn empty_guardian_is_skipped_but_empty_real_subagent_is_a_parse_error() {
        let tmp = std::env::temp_dir().join(format!("r13-guardian-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let target = normalize_windows_path("D:\\work");
        for (name, source, ok_skip) in [
            (
                "guardian",
                serde_json::json!({"subagent":{"other":"guardian"}}),
                true,
            ),
            (
                "worker",
                serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}),
                false,
            ),
        ] {
            let path = tmp.join(format!("{name}.jsonl"));
            let meta = serde_json::json!({"type":"session_meta","payload":{
                "id":name,"session_id":"root","cwd":"D:\\work","thread_source":"subagent",
                "parent_thread_id":"root","source":source,"timestamp":"2026-10-15T00:00:00Z"
            }});
            fs::write(&path, format!("{meta}\n")).unwrap();
            let selected = HashSet::from([name.to_string()]);
            let output =
                convert_codex_session_file_streaming_scoped(&path, &target, &tmp, Some(&selected));
            if ok_skip {
                assert!(output.unwrap().is_none());
            } else {
                assert!(output.is_err());
            }
            let legacy = convert_codex_session_file_with_ingestion_scoped(
                &path,
                &target,
                &tmp,
                false,
                Some(&selected),
            );
            if ok_skip {
                assert!(legacy.unwrap().is_none());
            } else {
                assert!(legacy.is_err());
            }
        }
        fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn no_implicit_project_or_destination_in_public_defaults() {
        let args = Args::parse_from(["test"]);
        assert!(args.project_id.is_none());
        assert!(args.thread_id.is_none());
        assert!(args.target_cwd.is_empty());
        assert!(args.output_directory.as_os_str().is_empty());
    }

    #[test]
    fn single_project_allows_foreign_but_rejects_unreadable_handoffs() {
        let path =
            std::env::temp_dir().join(format!("universal-output-guard-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        let scope = project_membership::ProjectScope {
            selected_ids: HashSet::from(["selected-root".to_owned()]),
            root_ids: HashSet::from(["selected-root".to_owned()]),
            explicit_roots: 1,
            cwd_fallback_roots: 0,
            children: 0,
            guardians: 0,
            unattached_guardians_skipped: 0,
            metadata_missing: 0,
            rollout_groups: HashMap::new(),
            continuation_ids: HashSet::new(),
        };
        let file = path.join("Test.handoff.json");
        fs::write(&file, r#"{"conversation_id":"foreign-root"}"#).unwrap();
        assert!(ensure_existing_handoffs_belong_to_scope(&path, &scope).is_ok());
        // Multiple output roots preserve the strict previous policy.
        assert!(destination::validate_existing(&path, &scope.selected_ids, false,).is_err());
        fs::write(&file, r#"{"conversation_id":"selected-root"}"#).unwrap();
        assert!(ensure_existing_handoffs_belong_to_scope(&path, &scope).is_ok());
        fs::write(&file, "not-json-or-handoff").unwrap();
        assert!(ensure_existing_handoffs_belong_to_scope(&path, &scope).is_err());
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn production_defaults_are_frozen() {
        assert_eq!(DEFAULT_WORKERS, 130);
        assert_eq!(MAX_WORKERS, 130);
        assert_eq!(MAX_EXPORT_LANES, 32);
        assert_eq!(DEFAULT_WRITE_LANES, 4);
        assert_eq!(DEFAULT_CLEANUP_LANES, 2);
        assert_eq!(DEFAULT_FINALIZE_LANES, 1);
        assert!(DEFAULT_FUSED_REDUCER);
        assert!(DEFAULT_DIRECT_CANONICAL_INGESTION);
        assert!(DEFAULT_SHELL_NOTIFY_PREWARM);
        let args = Args::try_parse_from(["Export-Codex-Handoff"]).expect("default args");
        assert!(args.export_lanes.is_none());
        assert_eq!(resolved_export_lanes(&args), (4, 2, 1));
        assert!(use_fused_reducer(&args));
        assert!(use_direct_canonical_ingestion(&args));
        assert!(use_shell_notify_prewarm(&args));
        assert!(!use_legacy_activity_sort(&args));
        assert!(!args.legacy_source_quality_scan);
        assert!(!args.direct_canonical_ingestion);
        assert!(!args.legacy_canonical_ingestion);
        assert!(!args.shell_notify_flush_nowait);
        assert!(args.output_directory.as_os_str().is_empty());
    }
}

#[cfg(test)]
mod m01_m05_tests;

#[cfg(test)]
mod e01_e06_tests;

#[cfg(all(test, feature = "study-wide-workers"))]
mod wide_worker_study_tests {
    use super::*;
    #[test]
    fn production_worker_ceiling_is_not_clamped_below_p079() {
        for n in [49, 64, 96, 128, 130] {
            assert_eq!(parse_worker_count(&n.to_string()).unwrap(), n);
        }
        assert!(parse_worker_count("131").is_err());
        assert!(parse_worker_count("0").is_err());
    }
}
