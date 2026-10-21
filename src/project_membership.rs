//! Read-only project-aware session selection. All decisions are made before any
//! handoff is written; rollout cwd and the existing handoff schema are untouched.
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{normalize_windows_path, value_str};

fn project_path(path: &str) -> String {
    // Windows may persist the same path as \?\D:\... and D:\...
    let normalized = normalize_windows_path(path);
    normalized
        .strip_prefix(r"\?\")
        .unwrap_or(&normalized)
        .to_owned()
}

#[derive(Debug, Clone)]
pub(crate) struct Meta {
    pub(crate) id: String,
    pub(crate) cwd: String,
    pub(crate) parent: Option<String>,
    pub(crate) root_hint: Option<String>,
    pub(crate) child: bool,
    pub(crate) guardian: bool,
    pub(crate) history_base: bool,
}

#[derive(Debug)]
pub(crate) struct ProjectScope {
    pub(crate) selected_ids: HashSet<String>,
    pub(crate) root_ids: HashSet<String>,
    pub(crate) explicit_roots: usize,
    pub(crate) cwd_fallback_roots: usize,
    pub(crate) children: usize,
    pub(crate) guardians: usize,
    pub(crate) unattached_guardians_skipped: usize,
    pub(crate) metadata_missing: usize,
    pub(crate) rollout_groups: HashMap<String, Vec<PathBuf>>,
    pub(crate) continuation_ids: HashSet<String>,
}

pub(crate) fn read_stable_state(path: &Path) -> Result<Value> {
    let mut last = None;
    for attempt in 0..3 {
        match fs::read(path)
            .with_context(|| format!("read project state {}", path.display()))
            .and_then(|data| {
                serde_json::from_slice::<Value>(&data)
                    .with_context(|| format!("parse project state {}", path.display()))
            }) {
            Ok(value) => return Ok(value),
            Err(error) => last = Some(error),
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
    Err(last.expect("three attempts"))
}

// The first canonical session_meta determines identity. Never let inherited
// metadata or a later envelope override it. Legacy records without metadata
// retain the existing behavior (not selected).
pub(crate) fn read_first_meta(path: &Path) -> Result<Option<Meta>> {
    if fs::metadata(path)?.len() == 0 {
        if crate::rollout_codec::is_compressed_rollout(path) {
            bail!(
                "Zero-byte compressed rollout is invalid: {}",
                path.display()
            );
        }
        return Ok(None);
    }
    let mut reader: Box<dyn BufRead> = if crate::rollout_codec::is_compressed_rollout(path) {
        Box::new(BufReader::with_capacity(
            16 * 1024,
            std::io::Cursor::new(crate::rollout_codec::read_rollout_snapshot(path)?),
        ))
    } else {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        Box::new(BufReader::with_capacity(16 * 1024, file))
    };
    let mut line = Vec::new();
    for _ in 0..64 {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        if line.len() > 4 * 1024 * 1024 {
            bail!("Oversized metadata candidate in {}", path.display());
        }
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        // Older Codex rollouts can begin with non-session_meta records.
        // Their historical treatment is preserved; do not invent an identity.
        let record: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(error) => bail!("Invalid initial JSONL in {}: {error}", path.display()),
        };
        // Pre-JSONL historical records have no top-level event type; these are
        // the known legacy format, not an unknown modern Codex event stream.
        if record.get("type").is_none() {
            return Ok(None);
        }
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = record
            .get("payload")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("session_meta payload missing in {}", path.display()))?;
        let id = value_str(payload.get("id"))
            .or_else(|| value_str(payload.get("session_id")))
            .ok_or_else(|| anyhow!("session_meta ID missing in {}", path.display()))?
            .to_owned();
        let cwd = value_str(payload.get("cwd"))
            .ok_or_else(|| anyhow!("session_meta cwd missing in {}", path.display()))?
            .to_owned();
        let thread_source = value_str(payload.get("thread_source")).unwrap_or("user");
        let subagent = payload
            .get("source")
            .and_then(|value| value.get("subagent"));
        let guardian = thread_source == "guardian_review"
            || subagent
                .and_then(|obj| obj.get("other"))
                .and_then(Value::as_str)
                == Some("guardian");
        let parent = value_str(payload.get("parent_thread_id"))
            .or_else(|| {
                subagent
                    .and_then(|obj| obj.get("thread_spawn"))
                    .and_then(|spawn| value_str(spawn.get("parent_thread_id")))
            })
            .map(str::to_string);
        let child = guardian || (thread_source == "subagent" && parent.is_some());
        return Ok(Some(Meta {
            id,
            cwd,
            parent,
            root_hint: value_str(payload.get("session_id")).map(str::to_string),
            child,
            guardian,
            history_base: payload.get("history_base").is_some_and(|v| !v.is_null()),
        }));
    }
    // A modern typed file with no canonical metadata must not be dropped
    // silently; this can indicate Codex changed its event format.
    bail!(
        "No canonical session_meta in first 64 records of {}",
        path.display()
    )
}

/// Group physical rollout files by canonical session_meta identity, without
/// ever treating a filename suffix as a separate logical conversation.
pub(crate) fn rollout_file_groups(files: &[PathBuf]) -> Result<HashMap<String, Vec<PathBuf>>> {
    let mut grouped = HashMap::<String, Vec<PathBuf>>::new();
    for path in files {
        if let Some(meta) = read_first_meta(path)? {
            grouped.entry(meta.id).or_default().push(path.clone());
        }
    }
    Ok(grouped)
}

fn select_target_project(
    state: &Value,
    target: &str,
) -> Result<(String, HashSet<String>, HashSet<String>)> {
    let local_projects = state
        .get("local-projects")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("Missing local-projects in Codex global state"))?;
    let mut matching_ids = Vec::new();
    let mut other_roots = HashSet::new();
    for (id, project) in local_projects {
        let roots = project
            .get("rootPaths")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("project {id} has invalid rootPaths"))?;
        let normalized: Vec<String> = roots
            .iter()
            .filter_map(Value::as_str)
            .map(project_path)
            .collect();
        if normalized.iter().any(|cwd| cwd == target) {
            matching_ids.push(id.clone());
        } else {
            other_roots.extend(normalized);
        }
    }
    if matching_ids.len() != 1 {
        bail!(
            "Expected exactly one local Codex project with rootPaths containing the target CWD; found {}. Refusing to guess project identity",
            matching_ids.len()
        );
    }
    let legacy_id = matching_ids.remove(0);
    let mut equivalent_ids = HashSet::from([legacy_id.clone()]);
    if let Some(hosts) = state
        .get("app-server-project-id-by-legacy-project-id-by-host")
        .and_then(Value::as_object)
    {
        for mapping in hosts.values().filter_map(Value::as_object) {
            if let Some(mapped) = value_str(mapping.get(&legacy_id)) {
                equivalent_ids.insert(mapped.to_string());
            }
        }
    }
    Ok((legacy_id, equivalent_ids, other_roots))
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) root_paths: Vec<String>,
    pub(crate) app_server_ids: Vec<String>,
}

// Project names and paths are display metadata, never identity keys. Fail
// closed if distinct projects claim the same App Server ID.
pub(crate) fn registered_projects(state: &Value) -> Result<Vec<ProjectInfo>> {
    let local = state
        .get("local-projects")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("Missing local-projects in Codex global state"))?;
    let mut projects = Vec::new();
    for (id, value) in local {
        if let Some(stored_id) = value.get("id") {
            if stored_id.as_str() != Some(id) {
                bail!("Project registry key and stored ID disagree for {id}");
            }
        }
        let roots = value
            .get("rootPaths")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Project {id} has invalid rootPaths"))?;
        let mut root_paths = Vec::new();
        for path in roots {
            let root = path
                .as_str()
                .ok_or_else(|| anyhow!("Project {id} has non-string rootPath"))?;
            if root.trim().is_empty() {
                bail!("Project {id} has an empty rootPath");
            }
            root_paths.push(root.to_owned());
        }
        projects.push(ProjectInfo {
            id: id.clone(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("(unnamed)")
                .to_owned(),
            root_paths,
            app_server_ids: Vec::new(),
        });
    }
    let index: HashMap<String, usize> = projects
        .iter()
        .enumerate()
        .map(|(index, p)| (p.id.clone(), index))
        .collect();
    let mut owners = HashMap::<String, String>::new();
    for project in &projects {
        owners.insert(project.id.clone(), project.id.clone());
    }
    if let Some(hosts) = state.get("app-server-project-id-by-legacy-project-id-by-host") {
        let hosts = hosts
            .as_object()
            .ok_or_else(|| anyhow!("Invalid app-server project ID mapping"))?;
        for mapping in hosts.values() {
            let mapping = mapping
                .as_object()
                .ok_or_else(|| anyhow!("Invalid per-host project ID mapping"))?;
            for (legacy, app_id) in mapping {
                let Some(&idx) = index.get(legacy) else {
                    // Old migration mappings may outlive projects removed from
                    // the current local registry. They are not selectable.
                    continue;
                };
                let alias = app_id
                    .as_str()
                    .ok_or_else(|| anyhow!("Invalid App Server ID mapped from {legacy}"))?;
                if alias.trim().is_empty() {
                    bail!("Blank App Server ID mapped from {legacy}");
                }
                if let Some(previous) = owners.get(alias) {
                    if previous != legacy {
                        bail!("Ambiguous project ID {alias}: mapped to {previous} and {legacy}");
                    }
                } else {
                    owners.insert(alias.to_string(), legacy.to_string());
                }
                if alias != legacy && !projects[idx].app_server_ids.iter().any(|id| id == alias) {
                    projects[idx].app_server_ids.push(alias.to_owned());
                }
            }
        }
    }
    for info in &mut projects {
        info.app_server_ids.sort();
    }
    projects.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(projects)
}

pub(crate) fn list_projects(state_path: &Path) -> Result<Vec<ProjectInfo>> {
    registered_projects(&read_stable_state(state_path)?)
}

fn explicit_project(
    state: &Value,
    requested_id: &str,
) -> Result<(
    String,
    HashSet<String>,
    HashSet<String>,
    HashSet<String>,
    HashSet<String>,
)> {
    if requested_id.trim().is_empty() || requested_id.chars().any(char::is_control) {
        bail!("--project-id must be a nonempty registered project ID");
    }
    let projects = registered_projects(state)?;
    let mut matched = projects
        .iter()
        .filter(|p| p.id == requested_id || p.app_server_ids.iter().any(|v| v == requested_id));
    let chosen = matched.next().ok_or_else(|| {
        anyhow!("Unknown --project-id {requested_id:?}. Run --list-projects to see registered IDs")
    })?;
    if matched.next().is_some() {
        bail!("Ambiguous --project-id {requested_id:?}");
    }
    let project_ids: HashSet<String> = std::iter::once(chosen.id.clone())
        .chain(chosen.app_server_ids.iter().cloned())
        .collect();
    let mut owners = HashMap::<String, HashSet<String>>::new();
    for project in &projects {
        for root in &project.root_paths {
            owners
                .entry(project_path(root))
                .or_default()
                .insert(project.id.clone());
        }
    }
    let mut fallback_roots = HashSet::new();
    let mut shared_roots = HashSet::new();
    let mut other_roots = HashSet::new();
    for (path, owner_ids) in owners {
        if owner_ids.contains(&chosen.id) {
            if owner_ids.len() == 1 {
                fallback_roots.insert(path);
            } else {
                shared_roots.insert(path);
            }
        } else {
            other_roots.insert(path);
        }
    }
    Ok((
        chosen.id.clone(),
        project_ids,
        fallback_roots,
        shared_roots,
        other_roots,
    ))
}

// Only needed to authorize the historical daily output directory. An
// ambiguous default path must never authorize another project's overwrite.
pub(crate) fn is_default_project(
    state_path: &Path,
    project_id: &str,
    default_cwd: &str,
) -> Result<bool> {
    let state = read_stable_state(state_path)?;
    let (default_id, _, _) = select_target_project(&state, &project_path(default_cwd))?;
    let (requested_id, _, _, _, _) = explicit_project(&state, project_id)?;
    Ok(default_id == requested_id)
}

fn assigned_project<'a>(state: &'a Value, id: &str) -> Option<&'a str> {
    state
        .get("thread-project-assignments")
        .and_then(|v| v.get(id))
        .and_then(|v| value_str(v.get("projectId")))
}

fn follow_to_root(id: &str, metas: &HashMap<String, Meta>) -> Option<String> {
    let meta = metas.get(id)?;
    let mut visited = HashSet::from([id.to_string()]);
    let mut cursor = meta.parent.as_deref().unwrap_or_default();
    while !cursor.is_empty() {
        if !visited.insert(cursor.to_string()) {
            return None;
        }
        let next = metas.get(cursor)?;
        if !next.child {
            return Some(cursor.to_string());
        }
        cursor = next.parent.as_deref().unwrap_or_default();
    }
    // A legacy child may omit the intermediate parent but retain a canonical
    // root session_id. Only accept it if the root really exists in this scan.
    if let Some(root) = meta.root_hint.as_deref() {
        if metas.get(root).is_some_and(|candidate| !candidate.child) {
            return Some(root.to_string());
        }
    }
    None
}

pub(crate) fn resolve(
    files: &[PathBuf],
    state_path: &Path,
    target_cwd: &str,
    requested_project_id: Option<&str>,
) -> Result<ProjectScope> {
    let state = read_stable_state(state_path)?;
    resolve_state(files, &state, target_cwd, requested_project_id)
}

pub(crate) fn resolve_state(
    files: &[PathBuf],
    state: &Value,
    target_cwd: &str,
    requested_project_id: Option<&str>,
) -> Result<ProjectScope> {
    let (project_ids, fallback_roots, shared_roots, other_roots) =
        if let Some(requested) = requested_project_id {
            let (_, ids, fallback, shared, others) = explicit_project(&state, requested)?;
            (ids, fallback, shared, others)
        } else {
            let (_, ids, others) = select_target_project(&state, target_cwd)?;
            (
                ids,
                HashSet::from([target_cwd.to_owned()]),
                HashSet::new(),
                others,
            )
        };
    let explicit_projectless: HashSet<String> = state
        .get("projectless-thread-ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let mut metas = HashMap::<String, Meta>::new();
    let mut metadata_missing = 0usize;
    let mut rollout_groups = HashMap::<String, Vec<PathBuf>>::new();
    let mut continuation_ids = HashSet::<String>::new();
    for path in files {
        match read_first_meta(path)? {
            Some(meta) => {
                let id = meta.id.clone();
                let has_history_base = meta.history_base;
                if let Some(existing) = metas.get(&id) {
                    // A new physical rollout can continue an existing logical
                    // thread. The identity must agree before we consider it
                    // for a history_base merge; its history is checked later.
                    if project_path(&existing.cwd) != project_path(&meta.cwd)
                        || existing.parent != meta.parent
                        || existing.root_hint != meta.root_hint
                        || existing.child != meta.child
                        || existing.guardian != meta.guardian
                    {
                        bail!(
                            "Conflicting session_meta identity for repeated Session ID {id}; refusing unsafe export"
                        );
                    }
                } else {
                    metas.insert(id.clone(), meta);
                }
                if has_history_base {
                    continuation_ids.insert(id.clone());
                }
                rollout_groups.entry(id).or_default().push(path.clone());
            }
            None => metadata_missing += 1,
        }
    }

    let mut root_ids = HashSet::new();
    let mut explicit_roots = 0;
    let mut cwd_fallback_roots = 0;
    let mut unknown = Vec::new();
    for meta in metas.values().filter(|meta| !meta.child) {
        let assignment = assigned_project(&state, &meta.id);
        let projectless = explicit_projectless.contains(&meta.id);
        if projectless && assignment.is_some() {
            bail!(
                "Conflicting projectless/project assignment for session {}",
                meta.id
            );
        }
        if projectless {
            continue;
        }
        match assignment {
            Some(id) if project_ids.contains(id) => {
                explicit_roots += 1;
                root_ids.insert(meta.id.clone());
            }
            Some(_) => { /* Explicitly belongs to another project. */ }
            None if shared_roots.contains(&project_path(&meta.cwd)) => {
                unknown.push(format!(
                    "{} (shared cwd={}, no Project ID assignment)",
                    meta.id, meta.cwd
                ));
            }
            None if fallback_roots.contains(&project_path(&meta.cwd)) => {
                cwd_fallback_roots += 1;
                root_ids.insert(meta.id.clone());
            }
            None if other_roots.contains(&project_path(&meta.cwd)) => {}
            None => unknown.push(format!("{} (cwd={})", meta.id, meta.cwd)),
        }
    }
    if !unknown.is_empty() {
        unknown.sort();
        bail!(
            "{} unassigned root session(s) have unknown project membership; refusing silent exclusion. First entries: {}",
            unknown.len(),
            unknown
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    if root_ids.is_empty() {
        bail!(
            "No target project root sessions resolved; refusing to rewrite/delete existing exports"
        );
    }

    let mut selected_ids = root_ids.clone();
    let mut children = 0usize;
    let mut guardians = 0usize;
    let mut unattached_guardians_skipped = 0usize;
    let mut unresolved = Vec::new();
    for meta in metas.values().filter(|meta| meta.child) {
        let root = follow_to_root(&meta.id, &metas);
        let directly_assigned_target =
            assigned_project(&state, &meta.id).is_some_and(|project| project_ids.contains(project));
        if let Some(root) = root {
            if root_ids.contains(&root) {
                if explicit_projectless.contains(&meta.id) {
                    bail!(
                        "Child {} explicitly projectless but parent belongs to target",
                        meta.id
                    );
                }
                if let Some(other) = assigned_project(&state, &meta.id) {
                    if !project_ids.contains(other) {
                        bail!(
                            "Child {} assigned to project {} contradicts target parent {}",
                            meta.id,
                            other,
                            root
                        );
                    }
                }
                selected_ids.insert(meta.id.clone());
                if meta.guardian {
                    guardians += 1;
                } else {
                    children += 1;
                }
            } else if directly_assigned_target {
                bail!(
                    "Child {} assigned to target but parent belongs to another project",
                    meta.id
                );
            }
        } else if directly_assigned_target
            || fallback_roots.contains(&project_path(&meta.cwd))
            || shared_roots.contains(&project_path(&meta.cwd))
        {
            if meta.guardian {
                // Guardian output is excluded by contract, and an unattached
                // Guardian cannot be integrated into an active Root. It may
                // refer to an archived/deleted parent. Report the omission,
                // but do not block unrelated complete active conversations.
                unattached_guardians_skipped += 1;
            } else {
                // Unlike Guardian, a real spawned subagent carries user-visible
                // content; skipping it would be silent data loss.
                unresolved.push(meta.id.clone());
            }
        }
    }
    if !unresolved.is_empty() {
        unresolved.sort();
        bail!(
            "{} target-likely child session(s) have unresolved parent chains: {}",
            unresolved.len(),
            unresolved
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(ProjectScope {
        selected_ids,
        root_ids,
        explicit_roots,
        cwd_fallback_roots,
        children,
        guardians,
        unattached_guardians_skipped,
        metadata_missing,
        rollout_groups,
        continuation_ids,
    })
}

/// Product-mode bridge for one explicit projectless Root. Identity, ancestry
/// and history_base remain governed by the same metadata and continuation code.
pub(crate) fn resolve_independent(
    files: &[PathBuf],
    state: &Value,
    requested: &str,
) -> Result<ProjectScope> {
    let projectless = state
        .get("projectless-thread-ids")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Missing projectless-thread-ids"))?;
    if !projectless.iter().any(|id| id.as_str() == Some(requested)) {
        bail!("Thread is not an explicitly visible independent conversation: {requested}");
    }
    if assigned_project(state, requested).is_some() {
        bail!("Conflicting projectless/project assignment for session {requested}");
    }
    let mut metas = HashMap::<String, Meta>::new();
    let mut groups = HashMap::<String, Vec<PathBuf>>::new();
    let mut continuations = HashSet::new();
    let mut missing = 0;
    for path in files {
        let Some(meta) = read_first_meta(path)? else {
            missing += 1;
            continue;
        };
        if let Some(old) = metas.get(&meta.id) {
            if project_path(&old.cwd) != project_path(&meta.cwd)
                || old.parent != meta.parent
                || old.root_hint != meta.root_hint
                || old.child != meta.child
                || old.guardian != meta.guardian
            {
                bail!(
                    "Conflicting session_meta identity for repeated Session ID {}",
                    meta.id
                );
            }
        }
        if meta.history_base {
            continuations.insert(meta.id.clone());
        }
        groups
            .entry(meta.id.clone())
            .or_default()
            .push(path.clone());
        metas.entry(meta.id.clone()).or_insert(meta);
    }
    let root = metas
        .get(requested)
        .ok_or_else(|| anyhow!("No local rollout for independent Thread {requested}"))?;
    if root.child || root.guardian || root.parent.is_some() {
        bail!("Independent target is not a Root conversation: {requested}");
    }
    let mut selected = HashSet::from([requested.to_owned()]);
    let mut children = 0;
    let mut guardians = 0;
    for meta in metas.values().filter(|m| m.child) {
        if follow_to_root(&meta.id, &metas).as_deref() == Some(requested) {
            if assigned_project(state, &meta.id).is_some() {
                bail!(
                    "Child {} has a project assignment contradicting independent Root {requested}",
                    meta.id
                );
            }
            selected.insert(meta.id.clone());
            if meta.guardian {
                guardians += 1;
            } else {
                children += 1;
            }
        } else if meta.root_hint.as_deref() == Some(requested)
            || meta.parent.as_deref() == Some(requested)
        {
            if !meta.guardian {
                bail!(
                    "Independent Thread child has an unresolved parent chain: {}",
                    meta.id
                );
            }
        }
    }
    Ok(ProjectScope {
        selected_ids: selected,
        root_ids: HashSet::from([requested.to_owned()]),
        explicit_roots: 1,
        cwd_fallback_roots: 0,
        children,
        guardians,
        unattached_guardians_skipped: 0,
        metadata_missing: missing,
        rollout_groups: groups,
        continuation_ids: continuations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codex-project-aware-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn fixture() -> (PathBuf, PathBuf, Vec<PathBuf>) {
        let dir = tmp();
        let state = dir.join("state.json");
        let json = serde_json::json!({
          "local-projects": {
            "target": {"rootPaths": ["D:\\work"]},
            "other": {"rootPaths": ["D:\\other"]}
          },
          "thread-project-assignments": {
            "root-normal": {"projectKind":"local", "projectId":"target"},
            "root-worktree": {"projectKind":"local", "projectId":"target"},
            "root-other": {"projectKind":"local", "projectId":"other"}
          },
          "projectless-thread-ids": ["root-projectless"]
        });
        fs::write(&state, json.to_string()).unwrap();
        let mut paths = Vec::new();
        for (id, cwd, source, parent) in [
            ("root-normal", "D:\\work", "user", None),
            ("root-legacy", "D:\\work", "code_review", None),
            ("root-worktree", "D:\\worktrees\\a", "user", None),
            ("root-other", "D:\\work", "user", None),
            ("root-projectless", "D:\\work", "user", None),
            (
                "child",
                "D:\\worktrees\\a",
                "subagent",
                Some("root-worktree"),
            ),
            (
                "guardian",
                "D:\\worktrees\\a",
                "guardian_review",
                Some("child"),
            ),
        ] {
            let p = dir.join(format!("{id}.jsonl"));
            let record = serde_json::json!({"type":"session_meta", "payload":{
                "id": id, "cwd": cwd, "thread_source":source,
                "parent_thread_id":parent, "session_id": if id == "child" || id == "guardian" { "root-worktree" } else { id }
            }});
            fs::write(&p, format!("{}\n", record)).unwrap();
            paths.push(p);
        }
        (dir, state, paths)
    }
    #[test]
    fn strips_windows_extended_length_prefix_for_membership() {
        assert_eq!(project_path(r"\\?\D:\Work\"), project_path(r"D:\work"));
    }
    #[test]
    fn uses_explicit_membership_cwd_fallback_and_parent_chain() {
        let (dir, state, files) = fixture();
        let result = resolve(&files, &state, &normalize_windows_path("D:\\work"), None).unwrap();
        assert_eq!(
            (
                result.explicit_roots,
                result.cwd_fallback_roots,
                result.children,
                result.guardians
            ),
            (2, 1, 1, 1)
        );
        assert_eq!(result.root_ids.len(), 3);
        assert_eq!(result.selected_ids.len(), 5);
        assert!(!result.selected_ids.contains("root-other"));
        assert!(!result.selected_ids.contains("root-projectless"));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn missing_state_and_unknown_worktree_are_fatal() {
        let (dir, state, mut files) = fixture();
        let err = resolve(
            &files,
            &dir.join("missing.json"),
            &normalize_windows_path("D:\\work"),
            None,
        );
        assert!(err.is_err());
        let p = dir.join("orphan-worktree.jsonl");
        fs::write(&p, r#"{"type":"session_meta","payload":{"id":"orphan","cwd":"D:\\worktrees\\unknown","thread_source":"user"}}"#).unwrap();
        files.push(p);
        assert!(resolve(&files, &state, &normalize_windows_path("D:\\work"), None).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn project_ids_are_unique_even_when_names_and_roots_overlap() {
        let state = serde_json::json!({
            "local-projects": {
                "aaa": {"id":"aaa", "name":"同名", "rootPaths":["D:\\shared"]},
                "bbb": {"id":"bbb", "name":"同名", "rootPaths":["D:\\shared"]}
            },
            "app-server-project-id-by-legacy-project-id-by-host": {
                "local:host": {"aaa":"app-aaa", "bbb":"app-bbb"}
            },
            "thread-project-assignments": {
                "root-aaa": {"projectKind":"local", "projectId":"app-aaa"},
                "root-bbb": {"projectKind":"local", "projectId":"bbb"}
            }
        });
        let projects = registered_projects(&state).unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, projects[1].name);
        let (legacy, aliases, fallback, shared, _) = explicit_project(&state, "app-aaa").unwrap();
        assert_eq!(legacy, "aaa");
        assert!(aliases.contains("aaa") && aliases.contains("app-aaa"));
        assert!(fallback.is_empty());
        assert!(shared.contains(&project_path("D:\\shared")));
        assert!(explicit_project(&state, "同名").is_err()); // names are not IDs
        assert!(explicit_project(&state, "absent-id").is_err());

        let dir = tmp();
        let file = dir.join("state.json");
        fs::write(&file, state.to_string()).unwrap();
        let files: Vec<PathBuf> = ["root-aaa", "root-bbb"]
            .iter()
            .map(|id| {
                let path = dir.join(format!("{id}.jsonl"));
                let row = serde_json::json!({"type":"session_meta", "payload":{
                    "id":id, "cwd":"D:\\shared", "thread_source":"user"
                }});
                fs::write(&path, format!("{row}\n")).unwrap();
                path
            })
            .collect();
        let selected = resolve(&files, &file, "D:\\unused", Some("app-aaa")).unwrap();
        assert_eq!(selected.root_ids, HashSet::from(["root-aaa".to_owned()]));
        assert_eq!(selected.cwd_fallback_roots, 0);
        let selected_b = resolve(&files, &file, "D:\\unused", Some("bbb")).unwrap();
        assert_eq!(selected_b.root_ids, HashSet::from(["root-bbb".to_owned()]));

        let orphan = dir.join("orphan.jsonl");
        fs::write(&orphan,r#"{"type":"session_meta","payload":{"id":"unassigned","cwd":"D:\\shared","thread_source":"user"}}"#).unwrap();
        let mut with_orphan = files;
        with_orphan.push(orphan);
        assert!(
            resolve(&with_orphan, &file, "D:\\unused", Some("aaa"))
                .unwrap_err()
                .to_string()
                .contains("unknown project membership")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn conflicting_app_alias_is_fatal() {
        let state = serde_json::json!({
            "local-projects": {
                "one":{"rootPaths":["D:\\one"]},
                "two":{"rootPaths":["D:\\two"]}
            },
            "app-server-project-id-by-legacy-project-id-by-host": {
                "local:host":{"one":"shared-alias","two":"shared-alias"}
            }
        });
        assert!(
            registered_projects(&state)
                .unwrap_err()
                .to_string()
                .contains("Ambiguous project ID")
        );
        assert!(explicit_project(&state, "shared-alias").is_err());
    }

    #[test]
    fn explicit_id_can_fallback_only_on_uniquely_owned_registered_root() {
        let (dir, state, files) = fixture();
        let selected = resolve(&files, &state, "D:\\unrelated", Some("target")).unwrap();
        assert_eq!(
            (
                selected.explicit_roots,
                selected.cwd_fallback_roots,
                selected.children,
                selected.guardians
            ),
            (2, 1, 1, 1)
        );
        assert_eq!(selected.root_ids.len(), 3);
        let other = resolve(&files, &state, "D:\\unrelated", Some("other")).unwrap();
        assert!(other.root_ids.contains("root-other"));
        assert!(!other.root_ids.contains("root-normal"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn list_is_read_only_and_exposes_legacy_and_app_ids() {
        let dir = tmp();
        let p = dir.join("state.json");
        let state = serde_json::json!({
            "local-projects": {"id-one": {"name":"範例遊戲 繁體中文翻譯", "rootPaths":["D:\\a"]}},
            "app-server-project-id-by-legacy-project-id-by-host":{"local:host":{"id-one":"app-one"}}
        });
        fs::write(&p, state.to_string()).unwrap();
        let before = fs::read(&p).unwrap();
        let found = list_projects(&p).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "id-one");
        assert_eq!(found[0].app_server_ids, vec!["app-one"]);
        assert_eq!(found[0].name, "範例遊戲 繁體中文翻譯");
        assert_eq!(fs::read(&p).unwrap(), before);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn daily_output_authorized_only_for_default_project_id() {
        let (dir, state, _) = fixture();
        assert!(is_default_project(&state, "target", "D:\\work").unwrap());
        assert!(!is_default_project(&state, "other", "D:\\work").unwrap());
        assert!(is_default_project(&state, "unknown-id", "D:\\work").is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn orphan_guardian_is_reported_but_real_orphan_subagent_is_fatal() {
        let (dir, state, mut files) = fixture();
        let guardian = dir.join("guardian-orphan.jsonl");
        fs::write(&guardian,r#"{"type":"session_meta","payload":{"id":"orphan-guardian","cwd":"D:\\work","thread_source":"subagent","source":{"subagent":{"other":"guardian"}},"parent_thread_id":"missing-parent"}}"#).unwrap();
        files.push(guardian);
        let safe = resolve(
            &files,
            &state,
            &normalize_windows_path("D:\\work"),
            Some("target"),
        )
        .unwrap();
        assert_eq!(safe.unattached_guardians_skipped, 1);
        assert!(!safe.selected_ids.contains("orphan-guardian"));
        let subagent = dir.join("real-orphan.jsonl");
        fs::write(&subagent,r#"{"type":"session_meta","payload":{"id":"orphan-real-child","cwd":"D:\\work","thread_source":"subagent","source":{"subagent":{"thread_spawn":{"parent_thread_id":"missing-parent"}}},"parent_thread_id":"missing-parent"}}"#).unwrap();
        files.push(subagent);
        assert!(
            resolve(
                &files,
                &state,
                &normalize_windows_path("D:\\work"),
                Some("target")
            )
            .unwrap_err()
            .to_string()
            .contains("unresolved parent chains")
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
