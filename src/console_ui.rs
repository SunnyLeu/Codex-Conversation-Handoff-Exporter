//! Presentation, interaction and complete diagnostic tracing.
//! Normal error-free runs remain memory-only and never create DEBUG files.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

static PRODUCT: AtomicBool = AtomicBool::new(false);
static LOG: OnceLock<Mutex<DiagnosticTrace>> = OnceLock::new();

struct DiagnosticTrace {
    started: Instant,
    run_id: String,
    next_event_id: u64,
    events: Vec<String>,
    had_error: bool,
    file: Option<File>,
    path: Option<PathBuf>,
}
impl DiagnosticTrace {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            started: Instant::now(),
            run_id: format!("v{}-{}-{stamp}", crate::VERSION, std::process::id()),
            next_event_id: 0,
            events: Vec::new(),
            had_error: false,
            file: None,
            path: None,
        }
    }
    fn emit(&mut self, level: &str, stage: &str, details: Value) -> Result<()> {
        self.next_event_id += 1;
        let is_error = matches!(level, "ERROR" | "FATAL");
        self.had_error |= is_error;
        let event = json!({
            "time_utc": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            "elapsed_ms": self.started.elapsed().as_secs_f64() * 1000.0,
            "run_id": self.run_id,
            "event_id": self.next_event_id,
            "level": level,
            "stage": stage,
            "process_id": std::process::id(),
            "thread": format!("{:?}", std::thread::current().id()),
            "details": details,
        });
        let line = event.to_string();
        if let Some(file) = self.file.as_mut() {
            writeln!(file, "{line}").context("無法持續寫入 DEBUG 事件")?;
            if is_error {
                file.flush().context("無法立即保存 ERROR 事件")?;
            }
        } else {
            // Never silently drop early events. A diagnostic file is created
            // only after the first error, not on every successful export.
            self.events.push(line);
            if is_error {
                self.activate()?;
            }
        }
        Ok(())
    }
    fn activate(&mut self) -> Result<PathBuf> {
        if let Some(path) = &self.path {
            return Ok(path.clone());
        }
        let root = std::env::temp_dir().join("CodexHandoff-Diagnostics");
        // Intentionally do not list, expire or delete any historical diagnostics.
        std::fs::create_dir_all(&root)
            .with_context(|| format!("無法建立 DEBUG 資料夾：{}", root.display()))?;
        let path = root.join(format!("{}.debug.jsonl", self.run_id));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("無法建立 DEBUG 檔案：{}", path.display()))?;
        for line in &self.events {
            writeln!(file, "{line}")
                .with_context(|| format!("無法完整保存 DEBUG 事件至 {}", path.display()))?;
        }
        file.flush()
            .with_context(|| format!("無法刷新 DEBUG 檔案：{}", path.display()))?;
        self.events.clear();
        self.path = Some(path.clone());
        self.file = Some(file);
        Ok(path)
    }
    fn flush_existing(&mut self) -> Result<Option<PathBuf>> {
        if !self.had_error {
            return Ok(None);
        }
        let path = self.activate()?;
        if let Some(file) = self.file.as_mut() {
            file.flush()
                .with_context(|| format!("無法刷新 DEBUG 檔案：{}", path.display()))?;
        }
        Ok(Some(path))
    }
}

/// Begin capturing the complete execution path in memory before CLI parsing.
/// Never touches historical diagnostics, nor writes any file on success.
pub(crate) fn capture() {
    PRODUCT.store(true, Ordering::Release);
    let mut trace = LOG
        .get_or_init(|| Mutex::new(DiagnosticTrace::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *trace = DiagnosticTrace::new();
    trace
        .emit(
            "INFO",
            "process.start",
            json!({
                "version": crate::VERSION,
                "process_id": std::process::id(),
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "diagnostic_format": "JSONL-v2",
                "diagnostic_retention": "errors-only; no historical cleanup; no event truncation",
                "content_policy": "no diagnostic redaction",
            }),
        )
        .unwrap_or_else(|e| std::eprintln!("[DEBUG LOGGER ERROR] {e:#}"));
}

pub(crate) fn active() -> bool {
    PRODUCT.load(Ordering::Acquire)
}

pub(crate) fn diagnostic(level: &str, stage: &str, details: Value) {
    if !active() {
        return;
    }
    let result = LOG
        .get_or_init(|| Mutex::new(DiagnosticTrace::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .emit(level, stage, details);
    if let Err(e) = result {
        // A broken diagnostic destination must never be hidden.
        std::eprintln!("[DEBUG LOGGER ERROR] {e:#}");
    }
}

/// Record the original anyhow error display, full Debug representation,
/// available error chain, native I/O codes, and error-site backtrace.
pub(crate) fn diagnostic_error(stage: &str, code: &str, error: &anyhow::Error) {
    let chain: Vec<Value> = error
        .chain()
        .enumerate()
        .map(|(index, cause)| {
            let io = cause.downcast_ref::<std::io::Error>();
            json!({
                "index": index,
                "display": cause.to_string(),
                "debug": format!("{cause:?}"),
                "os_error_code": io.and_then(std::io::Error::raw_os_error),
                "io_error_kind": io.map(|e| format!("{:?}", e.kind())),
            })
        })
        .collect();
    diagnostic(
        "ERROR",
        stage,
        json!({
            "error_code": code,
            "display": error.to_string(),
            "full_chain": format!("{error:#}"),
            "full_debug": format!("{error:?}"),
            "causes": chain,
            "anyhow_backtrace": error.backtrace().to_string(),
            "handler_backtrace": std::backtrace::Backtrace::force_capture().to_string(),
            "backtrace_note": "handler_backtrace is captured by the error handler, not necessarily the original failure site",
        }),
    );
}

pub(crate) fn legacy_line(error: bool, args: std::fmt::Arguments<'_>) {
    if active() {
        let text = format!("{args}");
        let level = if !error {
            "INFO"
        } else if text.contains("[ERROR]") {
            "ERROR"
        } else {
            "WARN"
        };
        diagnostic(level, "conversion.output", json!({"text": text}));
    } else if error {
        std::eprintln!("{args}");
    } else {
        std::println!("{args}");
    }
}

/// Called after a handled error; preserves all prior trace and keeps recording
/// future events, including if the user later completes the export.
pub(crate) fn flush_after_error() -> Result<PathBuf> {
    let mut trace = LOG
        .get_or_init(|| Mutex::new(DiagnosticTrace::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    trace.had_error = true;
    trace
        .flush_existing()?
        .context("錯誤發生但無法取得 DEBUG 日誌路徑")
}

/// Finish a run. Error-free runs never create a DEBUG file. If an error was
/// handled during an otherwise successful run, retain its complete trace.
pub(crate) fn finish_diagnostics(status: &str, error: Option<&str>) -> Result<Option<PathBuf>> {
    diagnostic(
        "INFO",
        "process.finish",
        json!({"status": status, "error": error}),
    );
    let mut trace = LOG
        .get_or_init(|| Mutex::new(DiagnosticTrace::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    trace.flush_existing()
}

/// Preserve the previous failure-only API for callers that must report the
/// exact DEBUG log pathname to the user.
pub(crate) fn save_diagnostic_log(status: &str, error: Option<&str>) -> Result<PathBuf> {
    if let Some(text) = error {
        diagnostic(
            "ERROR",
            "process.error",
            json!({"status": status, "error": text}),
        );
    }
    finish_diagnostics(status, error)?.context("錯誤處理結束但沒有建立 DEBUG 日誌")
}

/// On panic, try to preserve the trace without risking a deadlock if the
/// panicking thread already held the trace lock.
pub(crate) fn capture_panic(info: &std::panic::PanicHookInfo<'_>) {
    let location = info.location().map(|loc| {
        json!({
            "file": loc.file(), "line": loc.line(), "column": loc.column(),
        })
    });
    let payload = info
        .payload()
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            info.payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
        })
        .unwrap_or_else(|| {
            format!(
                "non-string panic payload, type_id={:?}",
                info.payload().type_id()
            )
        });
    let details = json!({
        "location": location, "message": payload,
        "backtrace": std::backtrace::Backtrace::force_capture().to_string(),
    });
    if let Some(trace) = LOG.get() {
        match trace.try_lock() {
            Ok(mut guard) => {
                if let Err(error) = guard.emit("FATAL", "process.panic", details) {
                    std::eprintln!(
                        "[DEBUG LOGGER ERROR] panic trace could not be written: {error:#}"
                    );
                }
                return;
            }
            Err(_) => {}
        }
    }
    std::eprintln!("[DEBUG LOGGER ERROR] panic trace lock unavailable; original panic: {payload}");
}

pub(crate) fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
// Conservative terminal cell widths; no font changes and no alignment based on
// byte length. Combining marks do not consume cells; CJK/fullwidth use two.
fn cells(c: char) -> usize {
    match c as u32 {
        0x0300..=0x036f
        | 0x1ab0..=0x1aff
        | 0x1dc0..=0x1dff
        | 0x20d0..=0x20ff
        | 0xfe00..=0xfe0f
        | 0xfe20..=0xfe2f
        | 0x200d => 0,
        0x1100..=0x115f
        | 0x2329..=0x232a
        | 0x2e80..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe10..=0xfe19
        | 0xfe30..=0xfe6f
        | 0xff01..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1faff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}
pub(crate) fn clipped(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut output = String::new();
    for c in clean(text).chars() {
        let w = cells(c);
        if used + w > width {
            break;
        }
        used += w;
        output.push(c);
    }
    output
}
pub(crate) fn wrapped(text: &str, width: usize) -> Vec<String> {
    let width = width.max(2);
    let mut output = vec![];
    let mut line = String::new();
    let mut used = 0;
    for c in clean(text).chars() {
        let w = cells(c);
        if used + w > width {
            output.push(line);
            line = String::new();
            used = 0;
        }
        line.push(c);
        used += w;
    }
    output.push(line);
    output
}
pub(crate) struct Screen {
    pub(crate) ansi: bool,
    /// Only human-facing product pages clear or decorate the terminal. Log/--yes
    /// output must remain scrollable, stable, and machine-readable.
    pub(crate) visual: bool,
}
impl Screen {
    pub(crate) fn line(&self, text: &str) {
        for line in wrapped(text, crate::platform::width().saturating_sub(1)) {
            std::println!("{line}");
        }
    }
    pub(crate) fn tint(&self, text: &str, color: u8) {
        for line in wrapped(text, crate::platform::width().saturating_sub(1)) {
            if self.ansi {
                std::println!("\x1b[{color}m{line}\x1b[0m");
            } else {
                std::println!("{line}");
            }
        }
    }
    pub(crate) fn status(&self, label: &str, text: &str, color: u8) {
        self.tint(&format!("[{label}] {}", clean(text)), color);
    }
    pub(crate) fn section(&self, title: &str) {
        // High-contrast sky blue (#87CEFA) on the terminal's own black backdrop.
        // No background or font changes; preserve plain output if VT is absent.
        for line in wrapped(title, crate::platform::width().saturating_sub(1)) {
            if self.ansi {
                std::println!("\x1b[38;2;135;206;250m{line}\x1b[0m");
            } else {
                std::println!("{line}");
            }
        }
    }
    pub(crate) fn choice(&self, number: usize, kind: &str, title: &str) {
        // Only foreground colors change; the user's Console background and
        // configured font size stay untouched.
        self.tint(&format!("  [{number}] [{kind}] {title}"), 93);
    }
    pub(crate) fn action(&self, key: &str, label: &str, color: u8) {
        self.tint(&format!("  [{key}] {label}"), color);
    }
    pub(crate) fn header(&self, page: &str) {
        if self.visual {
            crate::platform::clear_display(self.ansi);
        } else {
            std::println!();
        }
        self.tint(&format!("CODEX HANDOFF  |  {page}"), 96);
        self.tint(
            &"─".repeat(crate::platform::width().saturating_sub(1).min(56)),
            90,
        );
    }
    pub(crate) fn prompt(&self, text: &str) -> Result<()> {
        std::print!("{text}");
        std::io::stdout().flush()?;
        Ok(())
    }
}

/// Every command, including E/F/B/Q, must be explicitly committed with Enter.
/// During the automatic-folder countdown, a partially typed command suspends
/// the timer rather than stealing focus before the user can press Enter.
pub(crate) fn command(timeout: Option<Duration>) -> Result<Option<String>> {
    let start = Instant::now();
    let mut buffer = String::new();
    loop {
        let remaining = if buffer.is_empty() {
            timeout.map(|t| t.saturating_sub(start.elapsed()))
        } else {
            None // Allow the user time to finish a started command.
        };
        let Some(key) = crate::platform::key(remaining)? else {
            return Ok(None);
        };
        match key {
            crate::platform::Key::End => {
                std::println!();
                return Ok(Some("Q".into()));
            }
            crate::platform::Key::Enter => {
                std::println!();
                return Ok(Some(buffer));
            }
            crate::platform::Key::Backspace => {
                if buffer.pop().is_some() {
                    std::print!("\x08 \x08");
                    std::io::stdout().flush()?;
                }
            }
            crate::platform::Key::Character(c) => {
                if (c.is_ascii_alphanumeric() && buffer.len() < 16)
                    || (c == ' ' && !buffer.is_empty())
                {
                    buffer.push(c.to_ascii_uppercase());
                    std::print!("{c}");
                    std::io::stdout().flush()?;
                }
            }
        }
        if buffer.is_empty() && timeout.is_some_and(|t| start.elapsed() >= t) {
            return Ok(None);
        }
    }
}
pub(crate) fn wait_q() {
    std::println!("[Q] 離開（按 Enter 確認）");
    loop {
        match command(None) {
            Ok(Some(c)) if c.eq_ignore_ascii_case("q") => break,
            Err(_) => break,
            _ => {}
        }
    }
}
pub(crate) fn progress_line(line: &str, ansi: bool, width: usize) {
    let text = clipped(line, width.saturating_sub(1));
    if ansi {
        std::print!("\r\x1b[2K\x1b[36m{text}\x1b[37m");
    } else {
        std::print!("\r{}\r{text}", " ".repeat(width.saturating_sub(1)));
    }
    let _ = std::io::stdout().flush();
}
pub(crate) fn finish_progress(ansi: bool) {
    if ansi {
        std::print!("\r\x1b[2K");
    } else {
        std::print!(
            "\r{}\r",
            " ".repeat(crate::platform::width().saturating_sub(1))
        );
    }
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_event_has_debug_level_and_jsonl_shape() {
        let mut trace = DiagnosticTrace::new();
        trace
            .emit(
                "DEBUG",
                "destination.existing.ownership",
                json!({
                    "accepted": false, "existing_conversation_id": "old-thread"
                }),
            )
            .unwrap();
        let event: Value = serde_json::from_str(&trace.events[0]).unwrap();
        assert_eq!(event["level"], "DEBUG");
        assert_eq!(event["stage"], "destination.existing.ownership");
        assert_eq!(event["details"]["existing_conversation_id"], "old-thread");
        assert!(event["time_utc"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn successful_trace_is_memory_only_and_untruncated() {
        let mut trace = DiagnosticTrace::new();
        for number in 0..20_100 {
            trace
                .emit("DEBUG", "diagnostics.test", json!({"number": number}))
                .unwrap();
        }
        assert_eq!(trace.events.len(), 20_100);
        assert!(!trace.had_error);
        assert!(trace.path.is_none());
        assert!(trace.flush_existing().unwrap().is_none());
        assert!(trace.path.is_none());
    }

    #[test]
    fn error_trace_retains_every_event_and_new_events_after_failure() {
        let mut trace = DiagnosticTrace::new();
        trace
            .emit("DEBUG", "first", json!({"raw": "原始內容\n不省略"}))
            .unwrap();
        trace
            .emit("ERROR", "caught", json!({"message": "任何來源的錯誤"}))
            .unwrap();
        let path = trace.path.clone().unwrap();
        trace
            .emit("DEBUG", "recovery", json!({"accepted": false}))
            .unwrap();
        trace.flush_existing().unwrap();
        let events: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["stage"], "first");
        assert_eq!(events[1]["details"]["message"], "任何來源的錯誤");
        assert_eq!(events[2]["stage"], "recovery");
        assert_eq!(events[2]["event_id"], 3);
        drop(trace);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn no_escape_in_titles() {
        assert_eq!(clean("x\x1b[31m\ny"), "x [31m y");
    }
    #[test]
    fn cjk_wrap_not_byte_length() {
        assert_eq!(wrapped("甲乙abc", 4), vec!["甲乙", "abc"]);
        assert_eq!(clipped("甲乙ab", 5), "甲乙a");
    }
}
