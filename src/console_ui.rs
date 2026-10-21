//! Presentation and interaction only. Never decides Session membership.
use anyhow::Result;
use std::io::Write;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

static PRODUCT: AtomicBool = AtomicBool::new(false);
static LOG: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
pub(crate) fn capture() {
    LOG.get_or_init(|| Mutex::new(vec![]))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    PRODUCT.store(true, Ordering::Relaxed);
}
pub(crate) fn legacy_line(error: bool, args: std::fmt::Arguments<'_>) {
    if PRODUCT.load(Ordering::Relaxed) {
        LOG.get_or_init(|| Mutex::new(vec![]))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{args}"));
    } else if error {
        std::eprintln!("{args}");
    } else {
        std::println!("{args}");
    }
}
pub(crate) fn save_error_log(error: &str) -> Option<std::path::PathBuf> {
    let root = std::env::temp_dir().join("CodexHandoff-Diagnostics");
    std::fs::create_dir_all(&root).ok()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let p = root.join(format!("v2-{}-{stamp}.log", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&p)
        .ok()?;
    let lines = LOG
        .get_or_init(|| Mutex::new(vec![]))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for line in lines.iter() {
        writeln!(f, "{line}").ok()?;
    }
    writeln!(f, "[ERROR] {error}").ok()?;
    Some(p)
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
    fn no_escape_in_titles() {
        assert_eq!(clean("x\x1b[31m\ny"), "x [31m y");
    }
    #[test]
    fn cjk_wrap_not_byte_length() {
        assert_eq!(wrapped("甲乙abc", 4), vec!["甲乙", "abc"]);
        assert_eq!(clipped("甲乙ab", 5), "甲乙a");
    }
}
