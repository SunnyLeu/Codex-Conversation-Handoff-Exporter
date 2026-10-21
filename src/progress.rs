//! Observer-only progress. No progress result participates in content selection.
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
#[derive(Default)]
struct State {
    phase: Mutex<(&'static str, usize, u64)>,
    done: AtomicUsize,
    stop: AtomicBool,
}
static WRITTEN: AtomicUsize = AtomicUsize::new(0);
pub(crate) fn wrote() {
    WRITTEN.fetch_add(1, Ordering::Relaxed);
    advance();
}
pub(crate) fn written() -> usize {
    WRITTEN.load(Ordering::Relaxed)
}
static STATE: OnceLock<Arc<State>> = OnceLock::new();
pub(crate) fn stage(label: &'static str, total: usize) {
    if let Some(s) = STATE.get() {
        let mut p = s.phase.lock().unwrap_or_else(|e| e.into_inner());
        s.done.store(0, Ordering::Relaxed);
        *p = (label, total, p.2 + 1);
    }
}
pub(crate) fn advance() {
    if let Some(s) = STATE.get() {
        s.done.fetch_add(1, Ordering::Relaxed);
    }
}
pub(crate) struct Reporter {
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Reporter {
    pub(crate) fn start(ansi: bool, visual: bool) -> Self {
        let state = STATE.get_or_init(|| Arc::new(State::default())).clone();
        state.stop.store(false, Ordering::Relaxed);
        WRITTEN.store(0, Ordering::Relaxed);
        *state.phase.lock().unwrap_or_else(|e| e.into_inner()) = ("", 0, 0);
        state.done.store(0, Ordering::Relaxed);
        let worker = std::thread::spawn(move || {
            let mut last = 0;
            let mut tick = 0usize;
            loop {
                let (label, total, revision) =
                    *state.phase.lock().unwrap_or_else(|e| e.into_inner());
                let done = state.done.load(Ordering::Relaxed).min(total);
                if revision > 0 && (visual || revision != last) {
                    if visual {
                        let width = crate::platform::width();
                        let line = if total > 0 {
                            let bars = (width.saturating_sub(32)).clamp(5, 28);
                            format!("{} {} {done}/{total}", label, bar(done, total, bars))
                        } else {
                            format!("{} {label}…", ["|", "/", "-", "\\"][tick % 4])
                        };
                        crate::console_ui::progress_line(&line, ansi, width);
                    } else {
                        std::println!("[進度] {label}");
                    }
                    last = revision;
                }
                if state.stop.load(Ordering::Relaxed) {
                    break;
                }
                tick += 1;
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            if visual {
                crate::console_ui::finish_progress(ansi);
            }
        });
        Self {
            worker: Some(worker),
        }
    }
    pub(crate) fn finish(mut self) {
        self.stop();
    }
    fn stop(&mut self) {
        if let Some(s) = STATE.get() {
            s.stop.store(true, Ordering::Relaxed);
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
impl Drop for Reporter {
    fn drop(&mut self) {
        self.stop();
    }
}
fn bar(done: usize, total: usize, width: usize) -> String {
    let filled = if total == 0 {
        0
    } else {
        ((done.min(total) as u128 * width as u128) / total as u128) as usize
    };
    format!("[{}{}]", "#".repeat(filled), "-".repeat(width - filled))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_bars_follow_counts() {
        assert_eq!(bar(0, 10, 10), "[----------]");
        assert_eq!(bar(3, 10, 10), "[###-------]");
        assert_eq!(bar(15, 10, 10), "[##########]");
        assert_eq!(bar(0, 0, 5), "[-----]");
    }
}
