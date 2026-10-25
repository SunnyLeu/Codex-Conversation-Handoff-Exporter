//! A fast sidebar catalogue. Never enumerate or parse rollout files here.
//! For the interactive chooser only, cross-check independent IDs against the
//! current read-only SQLite thread index; the global state may retain ghosts.
//! The catalogue describes *candidate* choices; export-time source, ancestry,
//! archive and destination validation remain mandatory before any write.
use crate::project_membership;
use anyhow::{Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Project,
    IndependentThread,
}
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub(crate) kind: Kind,
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) aliases: Vec<String>,
    pub(crate) detail: String,
    pub(crate) pinned: bool,
}
impl Target {
    pub(crate) fn matches(&self, id: &str) -> bool {
        self.id == id || self.aliases.iter().any(|a| a == id)
    }
}
#[derive(Debug)]
pub(crate) struct Catalogue {
    pub(crate) targets: Vec<Target>,
    pub(crate) unavailable: Vec<(String, String, String)>,
    pub(crate) fingerprint: String,
}
impl Catalogue {
    pub(crate) fn select(&self, kind: Kind, id: &str) -> Result<Target> {
        self.targets
            .iter()
            .find(|t| t.kind == kind && t.matches(id))
            .cloned()
            .ok_or_else(|| {
                let detail = self
                    .unavailable
                    .iter()
                    .find(|(i, _, _)| i == id)
                    .map(|(_, _, reason)| reason.as_str())
                    .unwrap_or("不在側邊欄可選範圍");
                anyhow::anyhow!("無法選擇來源 {id}：{detail}")
            })
    }
}

/// Only membership-relevant state may invalidate an in-flight menu selection.
/// Never include draft text, open-window geometry, or rollout timestamps.
pub(crate) fn state_fingerprint(state: &Value) -> Result<String> {
    let mut relevant = serde_json::Map::new();
    for key in [
        "project-order",
        "pinned-project-ids",
        "local-projects",
        "projectless-thread-ids",
        "thread-project-assignments",
        "app-server-project-id-by-legacy-project-id-by-host",
        "sidebar-project-thread-orders",
        "thread-project-membership-host-ids",
    ] {
        relevant.insert(key.into(), state.get(key).cloned().unwrap_or(Value::Null));
    }
    let digest = Sha256::digest(serde_json::to_vec(&relevant)?);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn string_list(state: &Value, key: &str, required: bool) -> Result<Vec<String>> {
    let Some(v) = state.get(key) else {
        if required {
            bail!("Codex 狀態缺少 {key}，無法確認側邊欄範圍");
        }
        return Ok(vec![]);
    };
    let items = v
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Codex {key} 不是清單"))?;
    let mut seen = HashSet::new();
    let mut output = Vec::new();
    for x in items {
        let id = x
            .as_str()
            .filter(|x| !x.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Codex {key} 包含無效 ID"))?;
        if !seen.insert(id) {
            bail!("Codex {key} 包含重複 ID");
        }
        output.push(id.into());
    }
    Ok(output)
}

/// Opening the chooser never scans `sessions/` or parses Zstandard.
/// Title index is only a label source. For the unparameterized interactive
/// chooser, a lightweight SQLite point lookup removes stale global-state IDs.
/// Explicit --project-id/--thread-id invocation keeps its export-time checks.
pub(crate) fn load(args: &crate::Args) -> Result<Catalogue> {
    let path = crate::project_state_path(args);
    let state = project_membership::read_stable_state(&path)?;
    let independent = state
        .get("projectless-thread-ids")
        .and_then(Value::as_array)
        .is_some_and(|v| !v.is_empty());
    let titles = if independent {
        crate::read_title_index(&args.session_index)?
    } else {
        HashMap::new()
    };
    // Project-scoped and explicit independent invocation must not depend on
    // unrelated SQLite rows. They are validated again when exporting.
    let mut catalogue = from_state(&state, &titles)?;
    if independent && args.project_id.is_none() && args.thread_id.is_none() {
        let home = args
            .sessions_root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("無效的 Codex Session 目錄"))?;
        // Preserve the legacy state-only chooser if there is no thread DB at
        // all. A present DB, however, must not resurrect missing/archived IDs.
        if crate::sqlite_threads::database_path(home)?.is_some() {
            retain_live_independent_targets(&mut catalogue, |id| {
                crate::sqlite_threads::lookup_one(home, id).map(|row| row.map(|row| !row.archived))
            })?;
        }
    }
    // Codex can update the sidebar during title/SQLite lookup. At E/--yes
    // the full state, selected target and source files are checked again.
    Ok(catalogue)
}

/// Filter only the interactive independent-thread choices. Never use a missing
/// or archived SQLite row to rewrite Codex state or to infer deletion of source
/// rollout files. Project choices and source validation are unchanged.
fn retain_live_independent_targets(
    catalogue: &mut Catalogue,
    mut live: impl FnMut(&str) -> Result<Option<bool>>,
) -> Result<()> {
    let mut accepted = Vec::with_capacity(catalogue.targets.len());
    for target in &catalogue.targets {
        if target.kind == Kind::IndependentThread && live(&target.id)? != Some(true) {
            continue;
        }
        accepted.push(target.clone());
    }
    catalogue.targets = accepted;
    Ok(())
}

/// Pure sidebar projection, with no filesystem or SQLite access.
pub(crate) fn from_state(state: &Value, index: &HashMap<String, String>) -> Result<Catalogue> {
    let projects = project_membership::registered_projects(state)?;
    let order = string_list(state, "project-order", true)?;
    let pins = string_list(state, "pinned-project-ids", false)?;
    let projectless = string_list(state, "projectless-thread-ids", false)?;
    let mut output = Catalogue {
        targets: vec![],
        unavailable: vec![],
        fingerprint: state_fingerprint(state)?,
    };
    let mut ordered = Vec::new();
    let mut inserted = HashSet::new();
    // Pins change display order; a stale pin cannot resurrect a removed project.
    for id in pins
        .iter()
        .filter(|id| order.contains(id))
        .chain(order.iter())
    {
        if inserted.insert(id.clone()) {
            ordered.push(id.clone());
        }
    }
    for id in ordered {
        let Some(p) = projects
            .iter()
            .find(|p| p.id == id || p.app_server_ids.contains(&id))
        else {
            output.unavailable.push((
                id.clone(),
                id,
                "側邊欄項目沒有對應的本機專案註冊資料".into(),
            ));
            continue;
        };
        output.targets.push(Target {
            kind: Kind::Project,
            id: p.id.clone(),
            title: p.name.clone(),
            aliases: p.app_server_ids.clone(),
            detail: p.root_paths.first().cloned().unwrap_or_default(),
            pinned: pins.contains(&id),
        });
    }
    for id in projectless {
        let title = index
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("未命名對話 · {}", short_id(&id)));
        if state
            .get("thread-project-assignments")
            .and_then(|v| v.get(&id))
            .and_then(|v| v.get("projectId"))
            .and_then(Value::as_str)
            .is_some()
        {
            output.unavailable.push((
                id,
                title,
                "對話同時被標記為無專案及已指派專案，不能安全選擇".into(),
            ));
            continue;
        }
        // State-only candidate projection; the interactive menu filters
        // missing/archived SQLite IDs, while export checks remain mandatory.
        output.targets.push(Target {
            kind: Kind::IndependentThread,
            id: id.clone(),
            title,
            aliases: vec![],
            detail: format!("Thread {}", short_id(&id)),
            pinned: false,
        });
    }
    let mut ids = HashSet::new();
    for t in &output.targets {
        if !ids.insert((format!("{:?}", t.kind), t.id.clone())) {
            bail!("側邊欄重複指向同一匯出目標");
        }
    }
    Ok(output)
}

pub(crate) fn basename(p: &str) -> String {
    p.rsplit(['\\', '/'])
        .next()
        .unwrap_or(p)
        .trim_end_matches(".zst")
        .to_lowercase()
}
pub(crate) fn short_id(id: &str) -> String {
    id.chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ui_drafts_do_not_change_membership_fingerprint() {
        let mut s = serde_json::json!({"project-order":["p"],"local-projects":{}});
        let before = state_fingerprint(&s).unwrap();
        s["composer-draft"] = Value::String("changed".into());
        assert_eq!(before, state_fingerprint(&s).unwrap());
        s["project-order"] = serde_json::json!([]);
        assert_ne!(before, state_fingerprint(&s).unwrap());
    }
    #[test]
    fn duplicate_order_and_nonstring_ids_rejected() {
        assert!(string_list(&serde_json::json!({"x":["p","p"]}), "x", true).is_err());
        assert!(string_list(&serde_json::json!({"x":[3]}), "x", true).is_err());
    }
    #[test]
    fn windows_hint_basename_works_on_both_hosts() {
        assert_eq!(
            basename(r"C:\data\rollout-abc.jsonl"),
            basename("/tmp/rollout-abc.jsonl.zst")
        );
    }
    #[test]
    fn catalogue_needs_only_sidebar_and_optional_titles() {
        let state = serde_json::json!({
            "local-projects": {
                "a": {"id":"a", "name":"Alpha", "rootPaths":["C:\\alpha"]},
                "hidden": {"id":"hidden", "name":"Hidden", "rootPaths":["C:\\hidden"]}
            },
            "project-order":["a"],"pinned-project-ids":["a"],
            "projectless-thread-ids":["thread"],
            "thread-project-assignments":{}
        });
        let titles = HashMap::from([("thread".into(), "獨立對話".into())]);
        let cat = from_state(&state, &titles).unwrap();
        assert_eq!(cat.targets.len(), 2);
        assert_eq!(cat.targets[0].id, "a");
        assert_eq!(cat.targets[1].title, "獨立對話");
        assert!(cat.targets[0].pinned);
        assert!(cat.select(Kind::Project, "hidden").is_err());
    }
    #[test]
    fn interactive_menu_hides_stale_and_archived_independent_ids() {
        // Synthetic analogue of the observed seven global-state IDs, four
        // live SQLite rows and three stale state-only IDs.
        let state = serde_json::json!({
            "local-projects": {"p": {"id":"p", "name":"Project", "rootPaths":["D:\\work"]}},
            "project-order": ["p"],
            "projectless-thread-ids": ["ghost-a", "live-a", "ghost-b", "live-b", "archived", "live-c", "live-d"],
            "thread-project-assignments": {}
        });
        let mut cat = from_state(&state, &HashMap::new()).unwrap();
        let fingerprint = cat.fingerprint.clone();
        let mut queried = Vec::new();
        retain_live_independent_targets(&mut cat, |id| {
            queried.push(id.to_owned());
            Ok(match id {
                "live-a" | "live-b" | "live-c" | "live-d" => Some(true),
                "archived" => Some(false),
                _ => None,
            })
        })
        .unwrap();
        assert_eq!(
            cat.targets
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            ["p", "live-a", "live-b", "live-c", "live-d"]
        );
        assert_eq!(queried.len(), 7);
        assert_eq!(cat.fingerprint, fingerprint);
        assert!(cat.unavailable.is_empty()); // Do not display ghosts elsewhere.
        assert!(cat.select(Kind::IndependentThread, "ghost-a").is_err());
        assert!(cat.select(Kind::Project, "p").is_ok());
    }

    #[test]
    fn interactive_sqlite_lookup_error_is_not_silently_ignored() {
        let state = serde_json::json!({
            "local-projects": {}, "project-order": [],
            "projectless-thread-ids": ["independent"]
        });
        let mut cat = from_state(&state, &HashMap::new()).unwrap();
        assert!(
            retain_live_independent_targets(&mut cat, |_| {
                anyhow::bail!("synthetic SQLite read failure")
            })
            .is_err()
        );
    }

    #[test]
    fn conflicting_projectless_assignment_is_not_selectable() {
        let state = serde_json::json!({
            "local-projects":{},"project-order":[],"projectless-thread-ids":["t"],
            "thread-project-assignments":{"t":{"projectId":"p"}}
        });
        let cat = from_state(&state, &HashMap::new()).unwrap();
        assert!(cat.targets.is_empty());
        assert_eq!(cat.unavailable.len(), 1);
    }
}
