//! Read-only output preflight. No force option, no destination guessing.
use anyhow::{Context, Result, bail};
use serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::json;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Collision on an actual planned output path. Foreign files at distinct
/// paths are never automatically removed merely because their Codex thread
/// was deleted or their display title was reused.
#[derive(Debug)]
pub(crate) struct PlannedHandoffConflict {
    pub(crate) file: PathBuf,
    pub(crate) existing_id: String,
    pub(crate) expected_id: String,
}
impl fmt::Display for PlannedHandoffConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "同名 Handoff 檔案已屬於另一條對話，已停止以保護資料。\n衝突檔案：{}\n舊 conversation_id：{}\n新 conversation_id：{}\n請改選資料夾，或先自行備份並搬離舊檔再重試；不會根據名稱猜測兩條對話相同，也不會覆寫舊檔。",
            self.file.display(),
            self.existing_id,
            self.expected_id
        )
    }
}
impl std::error::Error for PlannedHandoffConflict {}

pub(crate) fn failure_code(error: &anyhow::Error) -> &'static str {
    if error.downcast_ref::<PlannedHandoffConflict>().is_some() {
        "DESTINATION_NAME_COLLISION"
    } else {
        "EXPORT_ERROR"
    }
}

// Existing Handoffs can be very large. Ownership checks need only the root
// conversation_id, but must still reject malformed JSON anywhere in the file.
// Deserialize all other values as IgnoredAny instead of allocating a complete
// serde_json::Value tree. Repeated conversation_id keys retain serde_json::Value's
// historical last-key-wins behavior; a final non-string/blank key still fails.
struct ExistingIdentity(Option<String>);

impl<'de> Deserialize<'de> for ExistingIdentity {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct IdentityVisitor;

        impl<'de> Visitor<'de> for IdentityVisitor {
            type Value = ExistingIdentity;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a Handoff JSON object")
            }

            fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut identity = None;
                while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                    if key == "conversation_id" {
                        let value: serde_json::Value = map.next_value()?;
                        identity = value
                            .as_str()
                            .filter(|id| !id.trim().is_empty())
                            .map(str::to_owned);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(ExistingIdentity(identity))
            }
        }

        deserializer.deserialize_map(IdentityVisitor)
    }
}

fn existing_conversation_id(
    bytes: &[u8],
) -> std::result::Result<Option<String>, serde_json::Error> {
    serde_json::from_slice::<ExistingIdentity>(bytes).map(|identity| identity.0)
}

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
/// Destination-wide preflight checks the folder shape only. It must not
/// reject a file solely because its conversation_id is outside this export.
/// Actual writes are guarded by validate_planned(), and stale cleanup uses
/// validated IDs of the conversations selected for this particular export.
/// An unrelated malformed handoff is left untouched, not parsed or deleted.
pub(crate) fn validate_existing(output: &Path, allowed: &HashSet<String>) -> Result<()> {
    let mut allowed_ids: Vec<_> = allowed.iter().collect();
    allowed_ids.sort_unstable();
    crate::console_ui::diagnostic(
        "DEBUG",
        "destination.existing.begin",
        json!({
            "directory": output,
            "allowed_conversation_ids": allowed_ids,
            "policy": "foreign Handoffs coexist; only planned paths and owned stale paths may be modified",
        }),
    );
    if !output.exists() {
        return Ok(());
    }
    if !output.is_dir() {
        bail!("匯出目的地不是資料夾：{}", output.display());
    }
    let mut inspected = 0usize;
    for entry in
        fs::read_dir(output).with_context(|| format!("無法讀取匯出目的地：{}", output.display()))?
    {
        let entry = entry.with_context(|| format!("無法列舉目的地項目：{}", output.display()))?;
        if !entry
            .file_name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".handoff.json")
        {
            continue;
        }
        let p = entry.path();
        let meta = fs::symlink_metadata(&p)
            .with_context(|| format!("無法檢查目的地項目：{}", p.display()))?;
        inspected += 1;
        crate::console_ui::diagnostic(
            "DEBUG",
            "destination.existing.coexist",
            json!({
                "file": p, "size_bytes": meta.len(), "file_index": inspected,
                "reparse_point": reparse(&meta), "is_regular_file": meta.is_file(),
                "decision": "do_not_touch_unless_selected_for_cleanup_or_planned_output",
            }),
        );
    }
    crate::console_ui::diagnostic(
        "INFO",
        "destination.existing.pass",
        json!({
            "directory": output, "inspected_handoffs": inspected,
            "foreign_handoffs_allowed": true,
            "selected_conversation_count": allowed.len(),
        }),
    );
    Ok(())
}
/// Verify an existing file that was indexed as belonging to this export
/// before stale cleanup. Prefix-only indexing is an optimization, never
/// sufficient authority to remove a file. Unrelated Handoffs are not read.
pub(crate) fn validate_owned_stale(path: &Path, expected_id: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("無法檢查本次欲清理的既有 Handoff：{}", path.display()))?;
    if reparse(&metadata) || !metadata.is_file() {
        bail!(
            "本次欲清理的 Handoff 為不安全的檔案項目：{}",
            path.display()
        );
    }
    let data = fs::read(path)
        .with_context(|| format!("無法讀取本次欲清理的 Handoff：{}", path.display()))?;
    let actual = existing_conversation_id(&data)
        .with_context(|| format!("本次欲清理的 Handoff JSON 已損壞：{}", path.display()))?
        .ok_or_else(|| {
            anyhow::anyhow!("本次欲清理的 Handoff 缺少有效對話 ID：{}", path.display())
        })?;
    let valid = actual == expected_id;
    crate::console_ui::diagnostic(
        if valid { "DEBUG" } else { "ERROR" },
        "destination.cleanup.ownership",
        json!({
            "file": path, "expected_conversation_id": expected_id,
            "actual_conversation_id": actual, "valid": valid,
            "size_bytes": metadata.len(),
            "will_delete_only_if_valid": true,
        }),
    );
    if !valid {
        bail!(
            "預定清理的既有 Handoff 身分已變更，拒絕刪除或覆寫：{}；預期 ID={}，實際 ID={}",
            path.display(),
            expected_id,
            actual
        );
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
    crate::console_ui::diagnostic(
        "DEBUG",
        "destination.planned.begin",
        json!({
            "directory": output, "candidates": candidates.len(),
        }),
    );
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::console_ui::diagnostic(
                    "DEBUG",
                    "destination.planned.new_file",
                    json!({
                        "file": planned, "conversation_id": expected_id,
                        "decision": "ALLOW_NEW_FILE", "will_modify_existing": false,
                    }),
                );
                continue;
            }
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
        let current_id = existing_conversation_id(&data)
            .with_context(|| format!("目的檔案損壞，不允許覆寫：{}", planned.display()))?;
        let current_id = current_id.as_deref().ok_or_else(|| {
            anyhow::anyhow!("目的檔案身分不明，不允許覆寫：{}", planned.display())
        })?;
        let accepted = current_id == expected_id;
        crate::console_ui::diagnostic(
            if accepted { "DEBUG" } else { "ERROR" },
            "destination.planned.ownership",
            json!({
                "file": planned, "existing_conversation_id": current_id,
                "incoming_conversation_id": expected_id, "accepted": accepted,
                "size_bytes": meta.len(),
            }),
        );
        if !accepted {
            return Err(PlannedHandoffConflict {
                file: planned,
                existing_id: current_id.to_owned(),
                expected_id: expected_id.to_owned(),
            }
            .into());
        }
    }
    crate::console_ui::diagnostic(
        "INFO",
        "destination.planned.pass",
        json!({
            "directory": output, "checked_candidates": candidates.len(),
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_reader_validates_full_document_without_materializing_unknown_values() {
        let valid = br#"{"messages":[{"content":[1,2,{"nested":true}]}],"conversation_id":"same","activities":[]}"#;
        assert_eq!(
            existing_conversation_id(valid).unwrap().as_deref(),
            Some("same")
        );

        // Match serde_json::Value's handling of repeated object keys exactly.
        assert_eq!(
            existing_conversation_id(br#"{"conversation_id":"before","conversation_id":"after"}"#)
                .unwrap()
                .as_deref(),
            Some("after")
        );
        assert_eq!(
            existing_conversation_id(br#"{"conversation_id":"before","conversation_id":null}"#)
                .unwrap(),
            None
        );
        for invalid in [
            &br#"{"conversation_id":"same","messages":[1,]}"#[..],
            &br#"{"conversation_id":"same"} {}"#[..],
            &br#"{"conversation_id":"same","broken":"\uQQQQ"}"#[..],
        ] {
            assert!(existing_conversation_id(invalid).is_err());
        }
        for missing in [
            &br#"{}"#[..],
            &br#"{"conversation_id":null}"#[..],
            &br#"{"conversation_id":123}"#[..],
            &br#"{"conversation_id":"  "}"#[..],
        ] {
            assert_eq!(existing_conversation_id(missing).unwrap(), None);
        }
    }
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
            assert!(validate_existing(d.path(), &HashSet::from(["same".into()])).is_ok());
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
        assert!(validate_existing(d.path(), &HashSet::from(["same".into()])).is_ok());
    }
    #[test]
    fn single_handoff_can_share_folder_but_not_overwrite_foreign() {
        let d = crate::codex_paths::TempDir::new("dest-share-test").unwrap();
        let foreign = d.path().join("02. existing.handoff.json");
        let own = d.path().join("01. own.handoff.json");
        let bytes = br#"{"conversation_id":"foreign-thread"}"#;
        fs::write(&foreign, bytes).unwrap();
        assert!(validate_existing(d.path(), &HashSet::from(["own-thread".into()])).is_ok());
        assert!(validate_existing(d.path(), &HashSet::from(["own-thread".into()])).is_ok());
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
    fn deleted_old_thread_can_coexist_with_recreated_same_title_at_another_path() {
        let dir = crate::codex_paths::TempDir::new("deleted-recreated-same-title").unwrap();
        let old_path = dir
            .path()
            .join("01. reused-name-20261122110000.handoff.json");
        let original = br#"{"conversation_id":"deleted-old-id","messages":["keep me"]}"#;
        fs::write(&old_path, original).unwrap();
        let scope = HashSet::from(["new-thread-id".to_string()]);
        validate_existing(dir.path(), &scope).unwrap();
        let incoming = vec![(
            "01. reused-name-20261122120000.handoff.json".to_string(),
            serde_json::json!({"conversation_id":"new-thread-id"}),
        )];
        validate_planned(dir.path(), &incoming).unwrap();
        assert_eq!(fs::read(&old_path).unwrap(), original);
        // Only an actual output-path collision can reject the new thread.
        let colliding = vec![(
            "01. reused-name-20261122110000.handoff.json".to_string(),
            serde_json::json!({"conversation_id":"new-thread-id"}),
        )];
        let err = validate_planned(dir.path(), &colliding).unwrap_err();
        assert_eq!(failure_code(&err), "DESTINATION_NAME_COLLISION");
        let conflict = err.downcast_ref::<PlannedHandoffConflict>().unwrap();
        assert_eq!(conflict.file, old_path);
        assert_eq!(conflict.existing_id, "deleted-old-id");
        assert_eq!(conflict.expected_id, "new-thread-id");
        assert_eq!(fs::read(&old_path).unwrap(), original);
    }

    #[test]
    fn unowned_unreadable_or_malformed_handoffs_are_not_a_reason_to_delete_anything() {
        let d = crate::codex_paths::TempDir::new("foreign-malformed-do-not-touch").unwrap();
        let foreign = d.path().join("foreign.handoff.json");
        for bytes in [&b"invalid"[..], &b"{}"[..]] {
            fs::write(&foreign, bytes).unwrap();
            validate_existing(d.path(), &HashSet::from(["own-thread".to_owned()])).unwrap();
            validate_planned(
                d.path(),
                &[(
                    "new.handoff.json".to_owned(),
                    serde_json::json!({"conversation_id":"own-thread"}),
                )],
            )
            .unwrap();
            assert_eq!(fs::read(&foreign).unwrap(), bytes);
            assert!(
                validate_planned(
                    d.path(),
                    &[(
                        "foreign.handoff.json".to_owned(),
                        serde_json::json!({"conversation_id":"own-thread"})
                    )]
                )
                .is_err()
            );
        }
    }

    #[test]
    fn stale_cleanup_requires_complete_valid_id_not_only_prefix() {
        let d = crate::codex_paths::TempDir::new("owned-stale-strict").unwrap();
        let path = d.path().join("stale.handoff.json");
        fs::write(&path, br#"{"conversation_id":"owned","bad":}"#).unwrap();
        assert!(validate_owned_stale(&path, "owned").is_err());
        fs::write(&path, br#"{"conversation_id":"foreign","messages":[]}"#).unwrap();
        assert!(validate_owned_stale(&path, "owned").is_err());
        fs::write(&path, br#"{"conversation_id":"owned","messages":[]}"#).unwrap();
        validate_owned_stale(&path, "owned").unwrap();
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
        validate_existing(d.path(), &HashSet::from(["same".into()])).unwrap();
        assert!(
            validate_planned(
                d.path(),
                &[(
                    "x.handoff.json".to_string(),
                    serde_json::json!({"conversation_id":"same"})
                )]
            )
            .is_err()
        );
    }
}
