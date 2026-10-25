//! Target-scoped, read-only consistency fence for product exports.
//!
//! The selected physical Rollouts, relevant titles, target membership and
//! attachments actually consulted must stay consistent until final writes.
//! Updates to unrelated Codex projects, attachments or SQLite WAL records must
//! not turn an otherwise complete export into a false failure. This is not a
//! filesystem transaction and cannot predict a future, not-yet-written Session.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::project_membership::{self, ProjectScope};

fn digest(path: &Path) -> Result<[u8; 32]> {
    let mut h = Sha256::new();
    let mut file = crate::open_shared_read(path)?;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        h.update(&buffer[..n]);
    }
    Ok(h.finalize().into())
}

fn optional_digest(path: &Path) -> Result<Option<[u8; 32]>> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => digest(path).map(Some),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// The disk attachment fallback is used only for references not recovered from
// the selected Rollouts' tool output. Record the file at the moment it is read;
// no other project's attachment tree is a dependency of this export.
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

fn selected_rollouts(scope: &ProjectScope) -> Vec<PathBuf> {
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
    rollouts: BTreeMap<PathBuf, [u8; 32]>,
    titles: BTreeMap<String, Option<String>>,
    selected_ids: HashSet<String>,
    root_ids: HashSet<String>,
    project: Option<TargetProject>,
    independent_row: Option<crate::sqlite_threads::ThreadRow>,
}

impl Drop for InputGuard {
    fn drop(&mut self) {
        *observations().lock().unwrap_or_else(|e| e.into_inner()) = None;
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
            .map(|path| Ok((path.clone(), digest(&path)?)))
            .collect::<Result<_>>()?;
        let guard = Self {
            rollouts,
            titles: selected_titles(scope, titles),
            selected_ids: scope.selected_ids.clone(),
            root_ids: scope.root_ids.clone(),
            project: target_project(state, args.project_id.as_deref())?,
            independent_row: target_sqlite_row(args)?,
        };
        active_attachment_root(&args.attachment_root);
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
        if self.independent_row != target_sqlite_row(args)? {
            bail!("本次獨立對話的 SQLite Thread 紀錄已變更；尚未覆寫 Handoff");
        }

        // Re-scan the complete directory to catch a newly created target
        // session. Only the chosen project's files are hashed again.
        let files = crate::discover_rollouts(&args.sessions_root)?;
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
            bail!(
                "匯出期間本次目標的主對話或子代理清單改變（原主對話 {} 筆，現 {} 筆）；尚未覆寫 Handoff，請重新匯出",
                self.root_ids.len(),
                updated.root_ids.len()
            );
        }
        let paths = selected_rollouts(&updated);
        if paths.len() != self.rollouts.len()
            || paths.iter().any(|path| !self.rollouts.contains_key(path))
        {
            bail!("匯出期間本次目標的實體 Rollout 清單改變；尚未覆寫 Handoff，請重新匯出");
        }
        for path in paths {
            if self.rollouts.get(&path) != Some(&digest(&path)?) {
                bail!(
                    "匯出期間本次目標的 Session 內容變更：{}；尚未覆寫 Handoff",
                    path.display()
                );
            }
        }
        let titles = crate::read_title_index(&args.session_index)?;
        if self.titles != selected_titles(&updated, &titles) {
            bail!("匯出期間本次目標的對話標題索引變更；尚未覆寫 Handoff");
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
            "rollout",
            "new-root",
            "new-continuation",
            "title",
            "assignment",
            "project-root",
            "attachment",
            "new-attachment",
        ] {
            let (_d, args, files) = fixture();
            let guard = start(&args, &files);
            match kind {
                "rollout" => fs::write(
                    &files[0],
                    format!(
                        "{}{}",
                        meta("root-target", "D:\\target"),
                        "{\"type\":\"event_msg\",\"payload\":{}}\n"
                    ),
                )
                .unwrap(),
                "new-root" => fs::write(
                    args.sessions_root.join("rollout-new-target.jsonl"),
                    meta("new-target", "D:\\target"),
                )
                .unwrap(),
                "new-continuation" => fs::write(
                    args.sessions_root.join("rollout-second-root-target.jsonl"),
                    meta("root-target", "D:\\target"),
                )
                .unwrap(),
                "title" => fs::write(
                    &args.session_index,
                    b"{\"id\":\"root-target\",\"thread_name\":\"Updated\"}\n",
                )
                .unwrap(),
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
