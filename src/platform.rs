//! OS boundary: native picker, Console key input and terminal width.
use anyhow::{Result, bail};
use std::path::PathBuf;
use std::time::Duration;

pub(crate) fn pick_folder() -> Result<Option<PathBuf>> {
    // COM's apartment belongs to a dedicated short-lived thread.
    std::thread::spawn(native_picker)
        .join()
        .map_err(|_| anyhow::anyhow!("資料夾選擇器執行緒異常"))?
}

#[cfg(windows)]
fn native_picker() -> Result<Option<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;
    unsafe extern "C" {
        fn codex_pick_folder(out: *mut *mut u16) -> i32;
        fn codex_free_folder(out: *mut u16);
    }
    let mut pointer = std::ptr::null_mut();
    // SAFETY: the bridge initializes the out pointer, allocates with CoTaskMem,
    // and guarantees a NUL terminated UTF-16 string on success.
    let status = unsafe { codex_pick_folder(&mut pointer) };
    if status == 0 {
        return Ok(None);
    }
    if status != 1 || pointer.is_null() {
        if !pointer.is_null() {
            unsafe { codex_free_folder(pointer) };
        }
        bail!(
            "無法開啟資料夾選擇器（HRESULT 0x{:08X}）。可重新按 F，或使用 --output-directory 指定路徑。",
            status as u32
        );
    }
    let result = unsafe {
        let mut len = 0;
        while len < 32768 && *pointer.add(len) != 0 {
            len += 1;
        }
        let result = if len == 32768 {
            None
        } else {
            Some(PathBuf::from(std::ffi::OsString::from_wide(
                std::slice::from_raw_parts(pointer, len),
            )))
        };
        codex_free_folder(pointer);
        result
    };
    result
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("選擇器回傳的路徑超出可處理長度"))
}
#[cfg(not(windows))]
fn native_picker() -> Result<Option<PathBuf>> {
    bail!("原生資料夾選擇器僅支援 Windows；請使用 --output-directory 指定路徑。")
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Key {
    Character(char),
    Enter,
    Backspace,
    End,
}

#[cfg(any(windows, feature = "v2-windows-api-typecheck"))]
pub(crate) fn key(timeout: Option<Duration>) -> Result<Option<Key>> {
    use windows_sys::Win32::System::{
        Console::*,
        Threading::{INFINITE, WaitForSingleObject},
    };
    let start = std::time::Instant::now();
    loop {
        let remaining = timeout.map(|t| t.saturating_sub(start.elapsed()));
        let milliseconds = remaining
            .map(|d| d.as_millis().min(u32::MAX as u128 - 1) as u32)
            .unwrap_or(INFINITE);
        // SAFETY: stdin is checked as a Console by the caller; API returns are
        // validated before reading the initialized input record union.
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let status = WaitForSingleObject(input, milliseconds);
            if status == 258 {
                return Ok(None);
            } // WAIT_TIMEOUT
            if status != 0 {
                bail!("讀取 Console 輸入失敗：{}", std::io::Error::last_os_error());
            }
            let mut record: INPUT_RECORD = std::mem::zeroed();
            let mut read = 0;
            if ReadConsoleInputW(input, &mut record, 1, &mut read) == 0 {
                bail!("無法讀取 Console 按鍵：{}", std::io::Error::last_os_error());
            }
            if read == 1 && record.EventType as u32 == KEY_EVENT {
                let event = record.Event.KeyEvent;
                if event.bKeyDown != 0 {
                    match event.uChar.UnicodeChar {
                        3 | 4 | 26 => return Ok(Some(Key::End)),
                        13 => return Ok(Some(Key::Enter)),
                        8 => return Ok(Some(Key::Backspace)),
                        c => {
                            if let Some(c) = char::from_u32(c as u32).filter(|c| !c.is_control()) {
                                return Ok(Some(Key::Character(c)));
                            }
                        }
                    }
                }
            }
        }
        if timeout.is_some_and(|t| start.elapsed() >= t) {
            return Ok(None);
        }
    }
}

#[cfg(all(unix, not(feature = "v2-windows-api-typecheck")))]
pub(crate) fn key(timeout: Option<Duration>) -> Result<Option<Key>> {
    // Unix support is for development/PTY tests; Windows uses Console APIs.
    struct Restore(libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.0);
            }
        }
    }
    unsafe {
        let mut original = std::mem::zeroed();
        if libc::tcgetattr(0, &mut original) != 0 {
            bail!("stdin 不是互動終端");
        }
        let _restore = Restore(original);
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
            bail!("無法設定終端輸入");
        }
        let mut poll = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout
            .map(|d| d.as_millis().min(i32::MAX as u128) as i32)
            .unwrap_or(-1);
        let status = libc::poll(&mut poll, 1, ms);
        if status == 0 {
            return Ok(None);
        }
        if status < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut byte = 0u8;
        if libc::read(0, (&mut byte as *mut u8).cast(), 1) <= 0 {
            return Ok(Some(Key::End));
        }
        Ok(Some(match byte {
            3 | 4 | 26 => Key::End,
            10 | 13 => Key::Enter,
            8 | 127 => Key::Backspace,
            _ => Key::Character(byte as char),
        }))
    }
}

/// Clear only the active interactive page. Redirected and headless output
/// must never receive ANSI cursor controls or destructive console operations.
pub(crate) fn clear_display(ansi: bool) {
    if ansi {
        std::print!("\x1b[2J\x1b[H");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        return;
    }
    #[cfg(any(windows, feature = "v2-windows-api-typecheck"))]
    unsafe {
        use windows_sys::Win32::System::Console::*;
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(out, &mut info) != 0 {
            let origin = COORD {
                X: 0,
                Y: info.srWindow.Top,
            };
            let count = (info.dwSize.X as u32)
                .saturating_mul((info.srWindow.Bottom - info.srWindow.Top + 1) as u32);
            let mut written = 0;
            FillConsoleOutputCharacterW(out, ' ' as u16, count, origin, &mut written);
            FillConsoleOutputAttribute(out, info.wAttributes, count, origin, &mut written);
            SetConsoleCursorPosition(out, origin);
        }
    }
}

pub(crate) fn clear_input() {
    #[cfg(any(windows, feature = "v2-windows-api-typecheck"))]
    unsafe {
        use windows_sys::Win32::System::Console::*;
        FlushConsoleInputBuffer(GetStdHandle(STD_INPUT_HANDLE));
    }
    #[cfg(all(unix, not(feature = "v2-windows-api-typecheck")))]
    unsafe {
        libc::tcflush(0, libc::TCIFLUSH);
    }
}

pub(crate) fn width() -> usize {
    #[cfg(any(windows, feature = "v2-windows-api-typecheck"))]
    unsafe {
        use windows_sys::Win32::System::Console::*;
        let mut info = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) != 0 {
            return ((info.srWindow.Right - info.srWindow.Left + 1) as usize).clamp(20, 160);
        }
    }
    #[cfg(all(unix, not(feature = "v2-windows-api-typecheck")))]
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0 {
            return (size.ws_col as usize).clamp(20, 160);
        }
    }
    80
}

/// Own only temporary Console attributes, never change font or user's profile.
pub(crate) struct ConsoleMode {
    #[cfg(any(windows, feature = "v2-windows-api-typecheck"))]
    original: Option<(u32, u32, u16)>,
    pub(crate) ansi: bool,
}
impl ConsoleMode {
    pub(crate) fn new(visual: bool) -> Self {
        #[cfg(any(windows, feature = "v2-windows-api-typecheck"))]
        unsafe {
            use windows_sys::Win32::System::Console::*;
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode = 0;
            let mut info = std::mem::zeroed();
            let original = if visual
                && GetConsoleMode(handle, &mut mode) != 0
                && GetConsoleScreenBufferInfo(handle, &mut info) != 0
            {
                let cp = GetConsoleOutputCP();
                SetConsoleOutputCP(65001);
                // Equal-size text, black background, neutral foreground.
                SetConsoleTextAttribute(handle, 7);
                Some((mode, cp, info.wAttributes))
            } else {
                None
            };
            let ansi = visual
                && original.is_some()
                && SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0;
            return Self { original, ansi };
        }
        #[cfg(not(any(windows, feature = "v2-windows-api-typecheck")))]
        Self {
            ansi: visual && std::env::var_os("TERM").is_some_and(|t| t != "dumb"),
        }
    }
}
impl Drop for ConsoleMode {
    fn drop(&mut self) {
        #[cfg(any(windows, feature = "v2-windows-api-typecheck"))]
        if let Some((mode, cp, attr)) = self.original {
            unsafe {
                use windows_sys::Win32::System::Console::*;
                let handle = GetStdHandle(STD_OUTPUT_HANDLE);
                SetConsoleTextAttribute(handle, attr);
                SetConsoleMode(handle, mode);
                SetConsoleOutputCP(cp);
            }
        }
    }
}
