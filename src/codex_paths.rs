use anyhow::{Result, bail};
use std::path::PathBuf;

pub(crate) fn home() -> Result<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(s).join(".codex"))
        })
        .ok_or_else(|| anyhow::anyhow!("找不到目前使用者的 Codex 資料位置"))?;
    if !home.is_absolute() {
        bail!("Codex 資料位置必須是絕對路徑：{}", home.display());
    }
    Ok(home)
}

pub(crate) fn default_path(name: &str) -> PathBuf {
    // Clap defaults must not invent a user or a project. On missing home the
    // dispatcher produces a useful error before these defaults can be used.
    home().map(|p| p.join(name)).unwrap_or_default()
}

pub(crate) struct TempDir(PathBuf);
impl TempDir {
    pub(crate) fn new(label: &str) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        for _ in 0..10 {
            let path = std::env::temp_dir().join(format!(
                "CodexHandoff-{label}-{}-{nanos}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        bail!("無法建立獨立暫存目錄")
    }
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
