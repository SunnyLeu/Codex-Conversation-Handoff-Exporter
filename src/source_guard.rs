//! Target-scoped, read-only consistency fence for product exports.
//!
//! The selected physical Rollouts are read at a fixed byte boundary, so Codex
//! can append records while the exporter runs. The captured prefix is hashed
//! and must remain unchanged; replacing or truncating that prefix still blocks
//! the output. Newly created threads/continuations belong to the next export.
//! Membership identity and consulted attachments remain protected. This is a
//! bounded read of each physical file, not an atomic filesystem transaction.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::project_membership::{self, ProjectScope};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RolloutSnapshot {
    pub(crate) len: u64,
    pub(crate) digest: [u8; 32],
}

// Product exports run once per process. Lookups are read-only and lock briefly;
// no worker holds this mutex during file IO. Unselected files are unaffected.
struct SnapshotRoster {
    physical: BTreeMap<PathBuf, RolloutSnapshot>,
    logical: HashMap<String, Vec<PathBuf>>,
}
static ROLLOUT_SNAPSHOTS: OnceLock<Mutex<Option<SnapshotRoster>>> = OnceLock::new();
fn rollout_snapshots() -> &'static Mutex<Option<SnapshotRoster>> {
    ROLLOUT_SNAPSHOTS.get_or_init(|| Mutex::new(None))
}

pub(crate) fn snapshot_for(path: &Path) -> Option<RolloutSnapshot> {
    rollout_snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|snapshots| snapshots.physical.get(path))
        .copied()
}

/// Physical source byte bounds are part of each exported Handoff's evidence.
/// This does not assert that several mutable files form an atomic transaction.
/// A new physical continuation or new thread is not in this frozen roster.
pub(crate) fn session_snapshot_metadata(session_id: &str) -> Option<Value> {
    let state = rollout_snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let roster = state.as_ref()?;
    let mut paths = roster.logical.get(session_id)?.clone();
    paths.sort();
    let mut entries = Vec::with_capacity(paths.len());
    for path in &paths {
        let snapshot = roster.physical.get(path)?;
        let sha256 = snapshot
            .digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        entries.push(serde_json::json!({
            "path": path.to_string_lossy(),
            "captured_bytes": snapshot.len,
            "source_sha256": sha256,
            "byte_scope": if crate::rollout_codec::is_compressed_rollout(path) {
                "compressed_physical_file"
            } else {
                "physical_jsonl_prefix"
            },
        }));
    }
    Some(serde_json::json!({
        "mode": "selected_physical_files_with_fixed_byte_limits",
        "physical_files": entries,
        "later_appends_and_new_files": "excluded_until_next_export",
        "consistency": "per_file_verified_prefix_not_atomic_cross_file_snapshot"
    }))
}

fn digest_prefix(path: &Path, len: u64) -> Result<[u8; 32]> {
    let mut file = crate::open_shared_read(path)?;
    let mut h = Sha256::new();
    let mut remaining = len;
    let mut buffer = [0u8; 65536];
    while remaining > 0 {
        let request = usize::try_from(remaining.min(buffer.len() as u64))?;
        let n = file
            .read(&mut buffer[..request])
            .with_context(|| format!("讀取來源快照 {}", path.display()))?;
        if n == 0 {
            bail!("Session 快照已截斷：{}；尚未覆寫 Handoff", path.display());
        }
        h.update(&buffer[..n]);
        remaining -= n as u64;
    }
    Ok(h.finalize().into())
}

fn capture_snapshot(path: &Path) -> Result<RolloutSnapshot> {
    let file = crate::open_shared_read(path)?;
    let len = file.metadata()?.len();
    drop(file);
    Ok(RolloutSnapshot {
        len,
        digest: digest_prefix(path, len)?,
    })
}

fn digest(path: &Path) -> Result<[u8; 32]> {
    let len = crate::open_shared_read(path)?.metadata()?.len();
    digest_prefix(path, len)
}

fn optional_digest(path: &Path) -> Result<Option<[u8; 32]>> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => digest(path).map(Some),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// Every selected attachment reference consulted for byte-exact verification
// is a target-scoped dependency, even if the Rollout also contains tool output.
// Record that file's state when first consulted; unrelated attachment trees
// must not become export dependencies.
struct AttachmentObservations {
    root: PathBuf,
    before: BTreeMap<PathBuf, Result<Option<[u8; 32]>, String>>,
}
static OBSERVED_ATTACHMENTS: OnceLock<Mutex<Option<AttachmentObservations>>> = OnceLock::new();
fn observations() -> &'static Mutex<Option<AttachmentObservations>> {
    OBSERVED_ATTACHMENTS.get_or_init(|| Mutex::new(None))
}

pub(crate) fn record_attachment_path(path: &Path) {
    let mut state = observations().lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = state.as_mut() else {
        return;
    };
    if path.starts_with(&active.root) && !active.before.contains_key(path) {
        active.before.insert(
            path.to_path_buf(),
            optional_digest(path).map_err(|e| format!("{e:#}")),
        );
    }
}

fn active_attachment_root(root: &Path) {
    let mut state = observations().lock().unwrap_or_else(|e| e.into_inner());
    *state = Some(AttachmentObservations {
        root: root.to_path_buf(),
        before: BTreeMap::new(),
    });
}

fn finish_attachment_checks() -> Result<()> {
    let state = observations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(active) = state else {
        bail!("附件來源一致性追蹤未啟動");
    };
    for (path, prior) in active.before {
        let before = prior.map_err(|error| {
            anyhow::anyhow!(
                "初次讀取附件時無法取得穩定狀態：{}：{error}",
                path.display()
            )
        })?;
        let after = optional_digest(&path)
            .with_context(|| format!("再次驗證附件失敗：{}", path.display()))?;
        if before != after {
            bail!(
                "本次匯出參照的附件已變動：{}；尚未覆寫 Handoff",
                path.display()
            );
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct TargetProject {
    id: String,
    root_paths: Vec<String>,
    app_server_ids: Vec<String>,
}

fn target_project(state: &Value, id: Option<&str>) -> Result<Option<TargetProject>> {
    let Some(id) = id else {
        return Ok(None);
    };
    let registered = project_membership::registered_projects(state)?;
    let project = registered
        .into_iter()
        .find(|project| project.id == id || project.app_server_ids.iter().any(|alias| alias == id))
        .ok_or_else(|| anyhow::anyhow!("匯出目標專案已不在註冊資料中：{id}"))?;
    let mut roots = project.root_paths;
    roots.sort();
    Ok(Some(TargetProject {
        id: project.id,
        root_paths: roots,
        app_server_ids: project.app_server_ids,
    }))
}

pub(crate) fn selected_rollouts(scope: &ProjectScope) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = scope
        .selected_ids
        .iter()
        .filter_map(|id| scope.rollout_groups.get(id))
        .flatten()
        .cloned()
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn selected_titles(
    scope: &ProjectScope,
    titles: &HashMap<String, String>,
) -> BTreeMap<String, Option<String>> {
    scope
        .root_ids
        .iter()
        .map(|id| (id.clone(), titles.get(id).cloned()))
        .collect()
}

fn target_sqlite_row(args: &crate::Args) -> Result<Option<crate::sqlite_threads::ThreadRow>> {
    let Some(id) = args.thread_id.as_deref() else {
        return Ok(None);
    };
    let home = args
        .sessions_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("無效的 Codex Session 路徑"))?;
    crate::sqlite_threads::lookup_one(home, id)
}

pub(crate) struct InputGuard {
    rollouts: BTreeMap<PathBuf, RolloutSnapshot>,
    titles: BTreeMap<String, Option<String>>,
    selected_ids: HashSet<String>,
    root_ids: HashSet<String>,
    project: Option<TargetProject>,
    independent_row: Option<crate::sqlite_threads::ThreadRow>,
}

impl Drop for InputGuard {
    fn drop(&mut self) {
        *observations().lock().unwrap_or_else(|e| e.into_inner()) = None;
        *rollout_snapshots()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }
}

impl InputGuard {
    pub(crate) fn capture(
        args: &crate::Args,
        scope: &ProjectScope,
        titles: &HashMap<String, String>,
        state: &Value,
    ) -> Result<Self> {
        let rollouts = selected_rollouts(scope)
            .into_iter()
            .map(|path| Ok((path.clone(), capture_snapshot(&path)?)))
            .collect::<Result<_>>()?;
        let guard = Self {
            rollouts,
            titles: selected_titles(scope, titles),
            selected_ids: scope.selected_ids.clone(),
            root_ids: scope.root_ids.clone(),
            project: target_project(state, args.project_id.as_deref())?,
            independent_row: target_sqlite_row(args)?,
        };
        let logical = scope
            .selected_ids
            .iter()
            .filter_map(|id| {
                scope
                    .rollout_groups
                    .get(id)
                    .map(|paths| (id.clone(), paths.clone()))
            })
            .collect();
        *rollout_snapshots()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(SnapshotRoster {
            physical: guard.rollouts.clone(),
            logical,
        });
        active_attachment_root(&args.attachment_root);
        eprintln!(
            "INFO: 已固定 {} 份 Session 的讀取邊界；正常追加會留待下次匯出。",
            guard.rollouts.len()
        );
        Ok(guard)
    }

    pub(crate) fn verify(self, args: &crate::Args) -> Result<()> {
        // This lookup also rejects stale sidebar targets, while tolerating
        // edits to unrelated menu entries and drafts.
        let state = project_membership::read_stable_state(&crate::project_state_path(args))?;
        let catalogue = crate::sidebar_targets::from_state(&state, &HashMap::new())?;
        if let Some(project) = args.project_id.as_deref() {
            catalogue.select(crate::sidebar_targets::Kind::Project, project)?;
        } else if let Some(thread) = args.thread_id.as_deref() {
            catalogue.select(crate::sidebar_targets::Kind::IndependentThread, thread)?;
        }
        if self.project != target_project(&state, args.project_id.as_deref())? {
            bail!("匯出目標專案的登記根目錄或身分已變更；尚未覆寫 Handoff");
        }
        // Changing only the SQLite title is normal while a thread is active;
        // the export already uses the title captured at selection time.
        let current_row = target_sqlite_row(args)?;
        match (&self.independent_row, &current_row) {
            (None, None) => {}
            (Some(before), Some(after))
                if before.archived == after.archived
                    && before.rollout_path == after.rollout_path => {}
            _ => bail!("本次獨立對話的 SQLite 身分或封存狀態已變更；尚未覆寫 Handoff"),
        }

        // Resolve membership using ONLY the frozen physical-file roster.
        // A new root, child or history_base continuation born later must not
        // be mixed into this output or invalidate the captured snapshot.
        // Rechecking these original files with the CURRENT project registry
        // still rejects reassignments and contradictory parent identities.
        let files: Vec<PathBuf> = self.rollouts.keys().cloned().collect();
        let updated = if let Some(id) = args.thread_id.as_deref() {
            project_membership::resolve_independent(&files, &state, id)?
        } else {
            project_membership::resolve_state(
                &files,
                &state,
                &crate::normalize_windows_path(&args.target_cwd),
                args.project_id.as_deref(),
            )?
        };
        if self.root_ids != updated.root_ids || self.selected_ids != updated.selected_ids {
            bail!("本次快照內的主對話或子代理歸屬已變更；尚未覆寫 Handoff");
        }
        let paths = selected_rollouts(&updated);
        if paths.len() != self.rollouts.len()
            || paths.iter().any(|path| !self.rollouts.contains_key(path))
        {
            bail!("本次快照內的實體 Rollout 歸屬已變更；尚未覆寫 Handoff");
        }

        let mut appended_files = 0usize;
        let mut appended_bytes = 0u64;
        for path in paths {
            let captured = self.rollouts.get(&path).expect("captured path");
            let current_len = crate::open_shared_read(&path)?
                .metadata()
                .with_context(|| format!("再次讀取 Session {}", path.display()))?
                .len();
            if current_len < captured.len {
                bail!(
                    "匯出期間 Session 被截斷：{}；尚未覆寫 Handoff",
                    path.display()
                );
            }
            // A compressed file may have new frames/rewritten dictionary data;
            // only plain JSONL is supported as a live append-only source.
            if crate::rollout_codec::is_compressed_rollout(&path) && current_len != captured.len {
                bail!(
                    "匯出期間壓縮 Session 已變更：{}；尚未覆寫 Handoff",
                    path.display()
                );
            }
            if digest_prefix(&path, captured.len)? != captured.digest {
                bail!(
                    "匯出期間 Session 已有內容遭改寫：{}；尚未覆寫 Handoff",
                    path.display()
                );
            }
            if current_len > captured.len {
                appended_files += 1;
                appended_bytes = appended_bytes.saturating_add(current_len - captured.len);
            }
        }
        // Display titles may be updated or temporarily unavailable while
        // Codex writes the index. The Handoff uses the captured title, so a
        // rename (or a transient unreadable index) is not source corruption.
        match crate::read_title_index(&args.session_index) {
            Ok(titles) if self.titles != selected_titles(&updated, &titles) => {
                eprintln!("INFO: 匯出期間部分對話標題更新；本次保留開始時的標題。");
            }
            Err(_) => {
                eprintln!("INFO: 標題索引正在更新；本次保留開始時已讀取的標題。");
            }
            _ => {}
        }
        if appended_files > 0 {
            eprintln!(
                "INFO: {} 份 Session 在匯出期間追加共 {} bytes；本次使用已固定的原始前綴，新內容留待下次匯出。",
                appended_files, appended_bytes
            );
        }
        finish_attachment_checks()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::fs;
    // The production product mode has one export per process. Keep the guard
    // tests serial because attachment observation is process-wide by design.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn meta(id: &str, cwd: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type":"session_meta", "payload":{
                "id":id,"cwd":cwd,"timestamp":"2026-10-25T01:00:00Z",
                "thread_source":"user"
            }})
        )
    }
    fn fixture() -> (crate::codex_paths::TempDir, crate::Args, Vec<PathBuf>) {
        let d = crate::codex_paths::TempDir::new("scoped-consistency-test").unwrap();
        let mut args = crate::Args::parse_from(["test"]);
        args.sessions_root = d.path().join("sessions");
        args.session_index = d.path().join("session_index.jsonl");
        args.attachment_root = d.path().join("attachments");
        args.project_state = Some(d.path().join(".codex-global-state.json"));
        args.project_id = Some("target".into());
        fs::create_dir(&args.sessions_root).unwrap();
        fs::create_dir(&args.attachment_root).unwrap();
        fs::write(
            &args.session_index,
            b"{\"id\":\"root-target\",\"thread_name\":\"Target\"}\n{\"id\":\"root-other\",\"thread_name\":\"Other\"}\n",
        )
        .unwrap();
        fs::write(
            crate::project_state_path(&args),
            serde_json::json!({
                "project-order":["target", "other"],
                "local-projects":{
                    "target":{"rootPaths":["D:\\target"]},
                    "other":{"rootPaths":["D:\\other"]}
                },
                "thread-project-assignments":{
                    "root-target":{"projectId":"target"},
                    "root-other":{"projectId":"other"}
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut files = vec![];
        for (name, cwd) in [("root-target", "D:\\target"), ("root-other", "D:\\other")] {
            let file = args.sessions_root.join(format!("rollout-{name}.jsonl"));
            fs::write(&file, meta(name, cwd)).unwrap();
            files.push(file);
        }
        (d, args, files)
    }
    fn start(args: &crate::Args, files: &[PathBuf]) -> InputGuard {
        let state =
            project_membership::read_stable_state(&crate::project_state_path(args)).unwrap();
        let scope =
            project_membership::resolve_state(files, &state, "", args.project_id.as_deref())
                .unwrap();
        let titles = crate::read_title_index(&args.session_index).unwrap();
        InputGuard::capture(args, &scope, &titles, &state).unwrap()
    }

    #[test]
    fn unrelated_updates_do_not_block_selected_project() {
        let _serial = TEST_LOCK.lock().unwrap();
        for kind in [
            "rollout",
            "new-rollout",
            "title",
            "state",
            "attachment",
            "database",
            "draft",
        ] {
            let (_d, args, files) = fixture();
            let guard = start(&args, &files);
            match kind {
                "rollout" => fs::write(&files[1], format!("{}{}", meta("root-other", "D:\\other"), "{\"type\":\"event_msg\",\"payload\":{}}\n")).unwrap(),
                "new-rollout" => {
                    let file = args.sessions_root.join("rollout-new-other.jsonl");
                    fs::write(&file, meta("new-other", "D:\\other")).unwrap();
                }
                "title" => fs::write(&args.session_index, b"{\"id\":\"root-target\",\"thread_name\":\"Target\"}\n{\"id\":\"root-other\",\"thread_name\":\"Renamed\"}\n").unwrap(),
                "state" => {
                    let path = crate::project_state_path(&args);
                    let mut value = project_membership::read_stable_state(&path).unwrap();
                    value["thread-project-assignments"]["root-other"]["projectId"] = "other".into();
                    value["local-projects"]["other"]["name"] = "Unrelated".into();
                    fs::write(path, value.to_string()).unwrap();
                }
                "attachment" => fs::write(args.attachment_root.join("unrelated.bin"), b"change").unwrap(),
                "database" => fs::write(args.sessions_root.parent().unwrap().join("state_5.sqlite"), b"unrelated-wal").unwrap(),
                "draft" => {
                    let path = crate::project_state_path(&args);
                    let mut value = project_membership::read_stable_state(&path).unwrap();
                    value["draft"] = "new text".into();
                    fs::write(path, value.to_string()).unwrap();
                }
                _ => unreachable!(),
            }
            guard
                .verify(&args)
                .unwrap_or_else(|e| panic!("{kind}: {e:#}"));
        }
    }
    #[test]
    fn relevant_changes_are_rejected_without_writing() {
        let _serial = TEST_LOCK.lock().unwrap();
        for kind in [
            "rewrite",
            "truncate",
            "assignment",
            "project-root",
            "attachment",
            "new-attachment",
        ] {
            let (_d, args, files) = fixture();
            let guard = start(&args, &files);
            match kind {
                "rewrite" => fs::write(
                    &files[0],
                    meta("root-target", "D:\\target").replace("2026-10-25", "2025-10-25"),
                )
                .unwrap(),
                "truncate" => fs::write(&files[0], b"x").unwrap(),
                "assignment" => {
                    let path = crate::project_state_path(&args);
                    let mut value = project_membership::read_stable_state(&path).unwrap();
                    value["thread-project-assignments"]["root-target"]["projectId"] =
                        "other".into();
                    fs::write(path, value.to_string()).unwrap();
                }
                "project-root" => {
                    let path = crate::project_state_path(&args);
                    let mut value = project_membership::read_stable_state(&path).unwrap();
                    value["local-projects"]["target"]["rootPaths"] =
                        serde_json::json!(["D:\\newtarget"]);
                    fs::write(path, value.to_string()).unwrap();
                }
                "attachment" | "new-attachment" => {
                    let path = args.attachment_root.join("target-attachment.txt");
                    if kind == "attachment" {
                        fs::write(&path, b"old").unwrap();
                    }
                    record_attachment_path(&path);
                    fs::write(&path, b"new").unwrap();
                }
                _ => unreachable!(),
            }
            assert!(guard.verify(&args).is_err(), "{kind}");
        }
    }
    #[test]
    fn append_only_rollout_is_bounded_and_does_not_fail() {
        let _serial = TEST_LOCK.lock().unwrap();
        let (_d, args, files) = fixture();
        let initial = fs::read(&files[0]).unwrap();
        let guard = start(&args, &files);
        let snapshot = session_snapshot_metadata("root-target").unwrap();
        assert_eq!(
            snapshot["physical_files"][0]["captured_bytes"],
            serde_json::json!(initial.len())
        );
        assert_eq!(
            snapshot["later_appends_and_new_files"],
            "excluded_until_next_export"
        );
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(&files[0]).unwrap();
        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{}}\n")
            .unwrap();
        file.flush().unwrap();
        // Both ingestion paths must use the same captured bytes, not the live tail.
        assert_eq!(
            crate::rollout_codec::read_rollout_snapshot(&files[0]).unwrap(),
            initial
        );
        assert_eq!(crate::read_rollout_jsonl(&files[0]).unwrap().len(), 1);
        assert_eq!(
            crate::read_rollout_jsonl_legacy(&files[0]).unwrap().len(),
            1
        );
        guard.verify(&args).unwrap();
        assert_eq!(fs::read(&files[0]).unwrap().len(), initial.len() + 34);
    }

    #[test]
    fn append_to_partial_final_line_does_not_invent_complete_record() {
        let _serial = TEST_LOCK.lock().unwrap();
        let (_d, args, files) = fixture();
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&files[0])
            .unwrap()
            .write_all(b"{\"type\":")
            .unwrap();
        let guard = start(&args, &files);
        fs::OpenOptions::new()
            .append(true)
            .open(&files[0])
            .unwrap()
            .write_all(b"\"event_msg\",\"payload\":{}}\n")
            .unwrap();
        // The incomplete line at the cutoff is skipped, not completed using
        // bytes written by Codex after this snapshot was captured.
        assert_eq!(crate::read_rollout_jsonl(&files[0]).unwrap().len(), 1);
        assert_eq!(
            crate::read_rollout_jsonl_legacy(&files[0]).unwrap().len(),
            1
        );
        guard.verify(&args).unwrap();
    }

    #[test]
    fn newly_created_roots_and_continuations_are_left_for_next_export() {
        let _serial = TEST_LOCK.lock().unwrap();
        let (_d, args, files) = fixture();
        let guard = start(&args, &files);
        fs::write(
            args.sessions_root.join("rollout-new-target.jsonl"),
            meta("new-target", "D:\\target"),
        )
        .unwrap();
        fs::write(
            args.sessions_root.join("rollout-next-root-target.jsonl"),
            meta("root-target", "D:\\target"),
        )
        .unwrap();
        guard.verify(&args).unwrap();
    }

    #[test]
    fn modifying_already_captured_prefix_is_detected_during_read() {
        let _serial = TEST_LOCK.lock().unwrap();
        let (_d, args, files) = fixture();
        let guard = start(&args, &files);
        let altered = meta("root-target", "D:\\target").replace("2026-10-25", "2025-10-25");
        fs::write(&files[0], altered).unwrap();
        assert!(crate::rollout_codec::read_rollout_snapshot(&files[0]).is_err());
        assert!(guard.verify(&args).is_err());
    }

    #[test]
    fn compressed_rollout_is_not_treated_as_plain_append_only_jsonl() {
        let _serial = TEST_LOCK.lock().unwrap();
        let (_d, args, mut files) = fixture();
        let zst = args.sessions_root.join("rollout-root-target.jsonl.zst");
        let compressed =
            zstd::stream::encode_all(meta("root-target", "D:\\target").as_bytes(), 1).unwrap();
        fs::write(&zst, &compressed).unwrap();
        fs::remove_file(&files[0]).unwrap();
        files[0] = zst.clone();
        let guard = start(&args, &files);
        assert_eq!(
            crate::rollout_codec::read_rollout_snapshot(&zst).unwrap(),
            meta("root-target", "D:\\target").into_bytes()
        );
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&zst)
            .unwrap()
            .write_all(&compressed)
            .unwrap();
        assert!(guard.verify(&args).is_err());
    }

    #[test]
    fn incomplete_new_session_header_is_deferred_not_a_project_error() {
        let _serial = TEST_LOCK.lock().unwrap();
        let (_d, args, mut files) = fixture();
        let new_file = args.sessions_root.join("rollout-writing.jsonl");
        fs::write(&new_file, b"{\"type\":\"session_meta\",\"payload\":").unwrap();
        files.push(new_file.clone());
        let state =
            project_membership::read_stable_state(&crate::project_state_path(&args)).unwrap();
        let scope =
            project_membership::resolve_state(&files, &state, "", args.project_id.as_deref())
                .unwrap();
        assert_eq!(selected_rollouts(&scope), vec![files[0].clone()]);
        // A truncated *complete* line is still a real error, not a transient.
        fs::write(&new_file, b"{not-json}\n").unwrap();
        assert!(
            project_membership::resolve_state(&files, &state, "", args.project_id.as_deref(),)
                .is_err()
        );
    }

    #[test]
    fn a_live_title_change_does_not_change_the_captured_handoff_title() {
        let _serial = TEST_LOCK.lock().unwrap();
        let (_d, args, files) = fixture();
        let guard = start(&args, &files);
        fs::write(
            &args.session_index,
            b"{\"id\":\"root-target\",\"thread_name\":\"New Title\"}\n",
        )
        .unwrap();
        guard.verify(&args).unwrap();
    }

    #[test]
    fn only_referenced_attachment_paths_are_guarded() {
        let _serial = TEST_LOCK.lock().unwrap();
        let (_d, args, files) = fixture();
        let path = args.attachment_root.join("referenced.txt");
        fs::write(&path, b"first").unwrap();
        let guard = start(&args, &files);
        record_attachment_path(&path);
        fs::write(args.attachment_root.join("irrelevant.txt"), b"other").unwrap();
        guard.verify(&args).unwrap();
    }
}
