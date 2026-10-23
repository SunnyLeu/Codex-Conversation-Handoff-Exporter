//! Read-only output preflight. No force option, no destination guessing.
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn absolute(path: &Path) -> Result<PathBuf> {
    if path.to_string_lossy().trim().is_empty()
        || path.to_string_lossy().chars().any(char::is_control)
    {
        bail!("匯出路徑不可空白或含控制字元");
    }
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for part in p.components() {
        match part {
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    bail!("無效的匯出路徑");
                }
            }
            std::path::Component::CurDir => {}
            part => normalized.push(part.as_os_str()),
        }
    }
    if !normalized.is_absolute() {
        bail!("匯出路徑仍有磁碟機或相對位置歧義，請指定完整路徑");
    }
    Ok(normalized)
}
fn reparse(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}
pub(crate) fn validate_path(output: &Path, args: &crate::Args) -> Result<()> {
    let p = absolute(output)?;
    for ancestor in p.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                if reparse(&meta) {
                    bail!("匯出路徑包含符號連結或重新解析點：{}", ancestor.display());
                }
                if !meta.is_dir() {
                    bail!("匯出路徑不是資料夾：{}", ancestor.display());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("無法檢查匯出路徑 {}", ancestor.display()));
            }
        }
    }
    // Never let Handoff output or cleanup touch the Codex source directory.
    let mut protected = vec![args.sessions_root.clone(), args.attachment_root.clone()];
    if let Some(home) = args.sessions_root.parent() {
        protected.push(home.to_path_buf());
    }
    for root in protected {
        if root.as_os_str().is_empty() {
            continue;
        }
        let root = fs::canonicalize(&root).or_else(|_| absolute(&root))?;
        let compare = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        let a = crate::normalize_windows_path(&compare.to_string_lossy());
        let b = crate::normalize_windows_path(&root.to_string_lossy());
        if a == b || a.starts_with(&(b.clone() + "\\")) {
            bail!("不可將 Handoff 寫入 Codex 原始資料目錄：{}", p.display());
        }
    }
    Ok(())
}
/// A single root handoff may share its folder with unrelated Handoffs.
/// Existing files still have to be well-formed and safe to inspect, and a
/// planned filename collision is checked separately before any deletion.
pub(crate) fn validate_existing(
    output: &Path,
    allowed: &HashSet<String>,
    allow_foreign: bool,
) -> Result<()> {
    if !output.exists() {
        return Ok(());
    }
    if !output.is_dir() {
        bail!("匯出目的地不是資料夾：{}", output.display());
    }
    for entry in fs::read_dir(output)? {
        let entry = entry?;
        let p = entry.path();
        if !entry
            .file_name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".handoff.json")
        {
            continue;
        }
        let meta = fs::symlink_metadata(&p)?;
        if reparse(&meta) || !meta.is_file() {
            bail!("不安全的既有 Handoff 項目：{}", p.display());
        }
        let bytes =
            fs::read(&p).with_context(|| format!("無法讀取既有 Handoff：{}", p.display()))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("既有 Handoff 已損壞，未進行覆寫：{}", p.display()))?;
        let id = value
            .get("conversation_id")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("既有 Handoff 無法確認身分：{}", p.display()))?;
        if !allowed.contains(id) && !allow_foreign {
            bail!("資料夾含其他匯出目標的 Handoff（{id}）；拒絕覆寫或清理。請選擇其他資料夾。");
        }
    }
    Ok(())
}
/// All prospective final filenames are checked before any existing Handoff is
/// removed. This also covers the legacy sequential exporter, whose final rename
/// previously relied solely on the whole-directory ownership preflight.
pub(crate) fn validate_planned(
    output: &Path,
    candidates: &[(String, serde_json::Value)],
) -> Result<()> {
    let mut names = HashSet::new();
    for (filename, handoff) in candidates {
        // A generated filename must never escape the chosen output folder.
        if !Path::new(filename).is_relative() || Path::new(filename).components().count() != 1 {
            bail!("無效的預定 Handoff 檔名：{filename}");
        }
        if !names.insert(filename.to_ascii_lowercase()) {
            bail!("同一次匯出包含重複的目的檔名：{filename}");
        }
        let planned = output.join(filename);
        let expected_id = handoff
            .get("conversation_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("準備匯出的 Handoff 缺少對話身分：{filename}"))?;
        let meta = match fs::symlink_metadata(&planned) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("無法檢查目的檔案：{}", planned.display()));
            }
        };
        if reparse(&meta) || !meta.is_file() {
            bail!("目的檔名已被不安全的項目占用：{}", planned.display());
        }
        let data = fs::read(&planned)
            .with_context(|| format!("無法讀取目的檔案：{}", planned.display()))?;
        let original: serde_json::Value = serde_json::from_slice(&data)
            .with_context(|| format!("目的檔案損壞，不允許覆寫：{}", planned.display()))?;
        let current_id = original
            .get("conversation_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("目的檔案身分不明，不允許覆寫：{}", planned.display())
            })?;
        if current_id != expected_id {
            bail!(
                "目的檔名已屬於另一條對話（{current_id}），拒絕覆寫：{}",
                planned.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn foreign_and_broken_handoffs_are_preserved() {
        let d = crate::codex_paths::TempDir::new("dest-test").unwrap();
        let p = d.path().join("x.handoff.json");
        for data in [
            r#"{"conversation_id":"other"}"#,
            r#"{"conversation_id":"same","broken":}"#,
            r#"{}"#,
        ] {
            fs::write(&p, data).unwrap();
            assert!(validate_existing(d.path(), &HashSet::from(["same".into()]), false).is_err());
            assert_eq!(fs::read_to_string(&p).unwrap(), data);
        }
    }
    #[test]
    fn only_same_scope_is_allowed() {
        let d = crate::codex_paths::TempDir::new("dest-test").unwrap();
        fs::write(
            d.path().join("1. x.handoff.json"),
            r#"{"conversation_id":"same"}"#,
        )
        .unwrap();
        assert!(validate_existing(d.path(), &HashSet::from(["same".into()]), false).is_ok());
    }
    #[test]
    fn single_handoff_can_share_folder_but_not_overwrite_foreign() {
        let d = crate::codex_paths::TempDir::new("dest-share-test").unwrap();
        let foreign = d.path().join("02. existing.handoff.json");
        let own = d.path().join("01. own.handoff.json");
        let bytes = br#"{"conversation_id":"foreign-thread"}"#;
        fs::write(&foreign, bytes).unwrap();
        assert!(validate_existing(d.path(), &HashSet::from(["own-thread".into()]), true).is_ok());
        assert!(validate_existing(d.path(), &HashSet::from(["own-thread".into()]), false).is_err());
        let planned = vec![(
            "01. own.handoff.json".to_string(),
            serde_json::json!({"conversation_id":"own-thread"}),
        )];
        assert!(validate_planned(d.path(), &planned).is_ok());
        fs::write(&own, r#"{"conversation_id":"foreign-thread"}"#).unwrap();
        assert!(validate_planned(d.path(), &planned).is_err());
        assert_eq!(fs::read(&foreign).unwrap(), bytes.to_vec());
        assert_eq!(
            fs::read_to_string(&own).unwrap(),
            r#"{"conversation_id":"foreign-thread"}"#
        );
        fs::write(&own, r#"{"conversation_id":"own-thread"}"#).unwrap();
        assert!(validate_planned(d.path(), &planned).is_ok());
        fs::write(&own, b"{broken").unwrap();
        assert!(validate_planned(d.path(), &planned).is_err());
        let escaping = vec![(
            "../escaping.handoff.json".into(),
            serde_json::json!({"conversation_id":"own-thread"}),
        )];
        assert!(validate_planned(d.path(), &escaping).is_err());
        let duplicates = vec![
            planned[0].clone(),
            (
                "01. OWN.HANDOFF.JSON".into(),
                serde_json::json!({"conversation_id":"own-thread"}),
            ),
        ];
        assert!(validate_planned(d.path(), &duplicates).is_err());
    }

    #[test]
    fn single_handoff_still_rejects_damaged_or_unknown_other_handoffs() {
        let d = crate::codex_paths::TempDir::new("dest-damaged-test").unwrap();
        let foreign = d.path().join("foreign.handoff.json");
        fs::write(&foreign, b"invalid").unwrap();
        assert!(validate_existing(d.path(), &HashSet::from(["own-thread".into()]), true).is_err());
        fs::write(&foreign, br#"{}"#).unwrap();
        assert!(validate_existing(d.path(), &HashSet::from(["own-thread".into()]), true).is_err());
    }

    #[test]
    fn source_directory_cannot_be_destination() {
        use clap::Parser;
        let d = crate::codex_paths::TempDir::new("dest-test").unwrap();
        let mut args = crate::Args::parse_from(["test"]);
        args.sessions_root = d.path().join(".codex/sessions");
        args.attachment_root = d.path().join(".codex/attachments");
        assert!(validate_path(&d.path().join(".codex/export"), &args).is_err());
        assert!(validate_path(&d.path().join("export"), &args).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn symlink_handoff_is_not_followed() {
        let d = crate::codex_paths::TempDir::new("dest-test").unwrap();
        let original = d.path().join("data");
        fs::write(&original, r#"{"conversation_id":"same"}"#).unwrap();
        std::os::unix::fs::symlink(&original, d.path().join("x.handoff.json")).unwrap();
        assert!(validate_existing(d.path(), &HashSet::from(["same".into()]), false).is_err());
    }
}
