//! Reconstruct Codex paginated history from immutable physical rollout snapshots.
//!
//! A resumed Codex thread can have multiple rollout files with the same
//! `session_meta.id`. `history_base.thread_id` identifies the *physical* prior
//! rollout, not necessarily the canonical session ID. Its byte offset is local
//! to that prior file, while the ordinal belongs to the logical history.
//! Only verified retained prefixes are included. Original Codex data is never
//! modified.
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::rollout_codec;

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct PhysicalRollout {
    path: PathBuf,
    bytes: Vec<u8>,
    meta: Value,
    id: String,
    physical_id: String,
    timestamp: String,
    history_base: Option<(String, usize, usize)>,
    meta_end: usize,
}

// Codex uses `rollout-<timestamp>-<logical_id>[_<physical_id>].jsonl[.zst]`.
// Only the final UUID names the physical file for an actual Codex rollout.
// Plain short basenames are kept for isolated synthetic/legacy fixtures.
fn physical_rollout_id(path: &Path, logical_id: &str) -> Result<String> {
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("Non-UTF-8 rollout filename: {}", path.display()))?;
    let stem = filename
        .strip_suffix(".jsonl.zst")
        .or_else(|| filename.strip_suffix(".jsonl"))
        .ok_or_else(|| anyhow!("Unexpected rollout filename: {}", path.display()))?;
    if stem.starts_with("rollout-") {
        let (prefix, physical) = stem
            .rsplit_once('_')
            .map_or((stem, logical_id), |(prefix, suffix)| (prefix, suffix));
        let is_uuid = |id: &str| {
            id.len() == 36
                && id.bytes().enumerate().all(|(i, b)| {
                    if [8, 13, 18, 23].contains(&i) {
                        b == b'-'
                    } else {
                        b.is_ascii_hexdigit()
                    }
                })
        };
        ensure!(
            is_uuid(logical_id) && is_uuid(physical) && prefix.ends_with(&format!("-{logical_id}")),
            "Rollout filename/metadata identity mismatch: {}",
            path.display()
        );
        return Ok(physical.to_owned());
    }
    ensure!(
        !stem.is_empty() && !stem.contains('_'),
        "Cannot identify physical rollout from {}",
        path.display()
    );
    Ok(stem.to_owned())
}

fn load(path: &Path) -> Result<PhysicalRollout> {
    let bytes = rollout_codec::read_rollout_snapshot(path)?;
    ensure!(
        !bytes.is_empty(),
        "Empty continuation candidate: {}",
        path.display()
    );
    let meta_end = bytes
        .iter()
        .position(|b| *b == b'\n')
        .map(|i| i + 1)
        .ok_or_else(|| anyhow!("Missing complete session_meta line: {}", path.display()))?;
    let initial: Value = serde_json::from_slice(&bytes[..meta_end])
        .with_context(|| format!("Invalid canonical session_meta in {}", path.display()))?;
    ensure!(
        initial.get("type").and_then(Value::as_str) == Some("session_meta"),
        "Continuation requires initial session_meta in {}",
        path.display()
    );
    let meta = initial
        .get("payload")
        .ok_or_else(|| anyhow!("Missing session_meta payload: {}", path.display()))?;
    let id = meta
        .get("id")
        .or_else(|| meta.get("session_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Missing session_meta ID: {}", path.display()))?
        .to_owned();
    let timestamp = meta
        .get("timestamp")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Missing session_meta timestamp: {}", path.display()))?
        .to_owned();
    let history_base = match meta.get("history_base").filter(|v| !v.is_null()) {
        None => None,
        Some(base) => {
            let thread_id = base
                .get("thread_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("history_base thread_id missing: {}", path.display()))?;
            let ordinal = base
                .get("end_ordinal_exclusive")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| anyhow!("history_base ordinal invalid: {}", path.display()))?;
            let offset = base
                .get("end_byte_offset")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| anyhow!("history_base byte offset invalid: {}", path.display()))?;
            ensure!(
                meta.get("history_mode").and_then(Value::as_str) == Some("paginated"),
                "history_base without paginated history_mode in {}",
                path.display()
            );
            Some((thread_id.to_owned(), ordinal, offset))
        }
    };
    // The superseded tail of a physical file is *not* logical history.
    // Parse the retained merged stream below instead of rejecting unrelated
    // abandoned bytes here. Zstandard framing was still fully verified.
    let physical_id = physical_rollout_id(path, &id)?;
    Ok(PhysicalRollout {
        path: path.to_path_buf(),
        bytes,
        meta: meta.clone(),
        id,
        physical_id,
        timestamp,
        history_base,
        meta_end,
    })
}

fn validate_identity(base: &PhysicalRollout, next: &PhysicalRollout) -> Result<()> {
    ensure!(base.id == next.id, "Continuation Session ID changed");
    // Legitimate metadata such as timestamp, model, git revision or settings
    // may change on resume; identity and project ownership must not.
    for field in [
        "session_id",
        "parent_thread_id",
        "forked_from_id",
        "thread_source",
        "source",
        "originator",
    ] {
        ensure!(
            base.meta.get(field) == next.meta.get(field),
            "Conflicting {field} across continuation for Session {}: {} and {}",
            base.id,
            base.path.display(),
            next.path.display()
        );
    }
    let old_cwd = base
        .meta
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Missing base continuation cwd"))?;
    let new_cwd = next
        .meta
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Missing resumed continuation cwd"))?;
    ensure!(
        crate::normalize_windows_path(old_cwd) == crate::normalize_windows_path(new_cwd),
        "Conflicting cwd across continuation for Session {}",
        base.id
    );
    Ok(())
}

// A `history_base` cutoff is a byte offset into its named physical parent,
// never the concatenation assembled so far. Check both the physical JSONL
// boundary and the logical ordinal before allowing any bytes to be emitted.
fn validate_parent_cut(
    parent: &PhysicalRollout,
    resumed: &PhysicalRollout,
    ordinal: usize,
    offset: usize,
) -> Result<()> {
    ensure!(
        ordinal > 0 && offset >= parent.meta_end && offset <= parent.bytes.len(),
        "Invalid history_base extent in {} for Session {}",
        resumed.path.display(),
        parent.id
    );
    ensure!(
        parent.bytes[offset - 1] == b'\n',
        "history_base byte offset splits a JSONL record in {}",
        resumed.path.display()
    );
    let start = parent.bytes[..offset - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let last: Value = serde_json::from_slice(&parent.bytes[start..offset]).with_context(|| {
        format!(
            "Invalid JSONL record at history_base cutoff in {}",
            parent.path.display()
        )
    })?;
    if let Some(last_ordinal) = last.get("ordinal").and_then(Value::as_u64) {
        ensure!(
            last_ordinal.checked_add(1) == u64::try_from(ordinal).ok(),
            "history_base ordinal/byte mismatch in {}: claimed {}, previous ordinal {}",
            resumed.path.display(),
            ordinal,
            last_ordinal
        );
    } else {
        // Historical synthetic fixtures without per-record ordinal still use
        // the same physical offset model. Their starting ordinal is inherited
        // from the prior page (or zero for the full-history root).
        let physical_start = parent
            .history_base
            .as_ref()
            .map_or(0, |(_, start, _)| *start);
        let count = parent.bytes[..offset]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        ensure!(
            physical_start.checked_add(count) == Some(ordinal),
            "history_base ordinal/byte mismatch in {}: claimed {}, physical prefix starts at {} and contains {} records",
            resumed.path.display(),
            ordinal,
            physical_start,
            count
        );
    }
    let resumed_meta: Value = serde_json::from_slice(&resumed.bytes[..resumed.meta_end])?;
    if let Some(first_ordinal) = resumed_meta.get("ordinal").and_then(Value::as_u64) {
        ensure!(
            u64::try_from(ordinal).ok() == Some(first_ordinal),
            "Resumed session_meta ordinal does not match history_base in {}",
            resumed.path.display()
        );
    }
    // The resumed metadata occupies one logical ordinal even though the
    // merged history retains the original canonical session_meta instead.
    let rest = &resumed.bytes[resumed.meta_end..];
    if let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
        let first_event: Value = serde_json::from_slice(&rest[..=end]).with_context(|| {
            format!("Invalid first resumed event in {}", resumed.path.display())
        })?;
        if let Some(first_event_ordinal) = first_event.get("ordinal").and_then(Value::as_u64) {
            ensure!(
                u64::try_from(ordinal).ok().and_then(|n| n.checked_add(1))
                    == Some(first_event_ordinal),
                "Resumed event ordinal is discontinuous in {}",
                resumed.path.display()
            );
        }
    }
    Ok(())
}

fn build_chain(files: &[PathBuf]) -> Result<Vec<u8>> {
    ensure!(!files.is_empty(), "Empty continuation group");
    let physical: Vec<_> = files.iter().map(|p| load(p)).collect::<Result<_>>()?;
    let id = &physical[0].id;
    ensure!(
        physical.iter().all(|part| part.id == *id),
        "Mixed IDs in continuation group"
    );
    let roots: Vec<_> = physical
        .iter()
        .enumerate()
        .filter(|(_, part)| part.history_base.is_none())
        .map(|(index, _)| index)
        .collect();
    ensure!(
        roots.len() == 1,
        "Session {} requires exactly one full history root; found {}",
        id,
        roots.len()
    );
    let root_index = roots[0];
    ensure!(
        physical[root_index]
            .meta
            .get("history_mode")
            .and_then(Value::as_str)
            == Some("paginated"),
        "Repeated Session {} is not marked as paginated",
        id
    );
    // Physical identities are unique even if timestamps, paths or logical
    // session IDs happen to repeat. No ordering by timestamp or filename can
    // safely substitute for the explicit lineage pointers.
    let mut by_physical_id = HashMap::<&str, usize>::new();
    for (index, part) in physical.iter().enumerate() {
        ensure!(
            by_physical_id.insert(&part.physical_id, index).is_none(),
            "Duplicate physical rollout ID {} for Session {}",
            part.physical_id,
            id
        );
    }
    let mut child_of = HashMap::<usize, usize>::new();
    for (index, part) in physical.iter().enumerate() {
        let Some((parent_id, _, _)) = part.history_base.as_ref() else {
            continue;
        };
        let parent_index = *by_physical_id.get(parent_id.as_str()).ok_or_else(|| {
            anyhow!(
                "history_base physical parent {} missing for Session {} in {}",
                parent_id,
                id,
                part.path.display()
            )
        })?;
        ensure!(
            parent_index != index,
            "Self-referencing history_base for Session {}",
            id
        );
        let parent = &physical[parent_index];
        validate_identity(parent, part)?;
        ensure!(
            part.timestamp > parent.timestamp,
            "Non-increasing continuation timestamps for Session {}",
            id
        );
        ensure!(
            child_of.insert(parent_index, index).is_none(),
            "Ambiguous history_base fork for Session {} (physical parent {})",
            id,
            parent_id
        );
    }
    let mut sequence = Vec::with_capacity(physical.len());
    let mut visited = HashSet::new();
    let mut current = Some(root_index);
    while let Some(index) = current {
        ensure!(
            visited.insert(index),
            "Cyclic history_base chain for Session {}",
            id
        );
        sequence.push(index);
        current = child_of.get(&index).copied();
    }
    ensure!(
        sequence.len() == physical.len(),
        "Unreachable or cyclic continuation parts for Session {}: linked {} of {} physical files",
        id,
        sequence.len(),
        physical.len()
    );
    let mut assembled = Vec::new();
    for (position, index) in sequence.iter().copied().enumerate() {
        let part = &physical[index];
        let end = if let Some(&next_index) = sequence.get(position + 1) {
            let next = &physical[next_index];
            let (parent_id, ordinal, offset) = next
                .history_base
                .as_ref()
                .expect("every continuation has a history_base");
            ensure!(
                parent_id == &part.physical_id,
                "Continuation lineage changed while merging Session {}",
                id
            );
            validate_parent_cut(part, next, *ordinal, *offset)?;
            *offset
        } else {
            part.bytes.len()
        };
        let start = if position == 0 { 0 } else { part.meta_end };
        ensure!(
            end >= start,
            "history_base would truncate the retained session_meta in {}",
            part.path.display()
        );
        assembled.extend_from_slice(&part.bytes[start..end]);
    }
    // Preserve the original reader's policy for a final incomplete JSONL
    // line; complete-but-invalid JSONL records are never accepted.
    let segments: Vec<&[u8]> = assembled.split_inclusive(|b| *b == b'\n').collect();
    let mut previous_ordinal = None;
    for (index, segment) in segments.iter().enumerate() {
        if segment.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: Value = match serde_json::from_slice(segment) {
            Ok(record) => record,
            Err(_) if index + 1 == segments.len() && !segment.ends_with(b"\n") => break,
            Err(error) => bail!(
                "Invalid retained continuation JSONL at record {} for Session {}: {}",
                index + 1,
                id,
                error
            ),
        };
        if index > 0 && record.get("type").and_then(Value::as_str) == Some("session_meta") {
            bail!(
                "Unexpected second session_meta in retained continuation for Session {}",
                id
            );
        }
        if let Some(ordinal) = record.get("ordinal").and_then(Value::as_u64) {
            ensure!(
                previous_ordinal.is_none_or(|prior| ordinal > prior),
                "Repeated or decreasing retained continuation ordinal for Session {} at record {}",
                id,
                index + 1
            );
            previous_ordinal = Some(ordinal);
        }
    }
    Ok(assembled)
}

#[derive(Debug)]
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn new_scratch() -> Result<Scratch> {
    let parent = std::env::temp_dir();
    for _ in 0..32 {
        let n = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".Codex-Handoff-Continuation-{}-{n}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(Scratch(path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).context("Cannot create isolated continuation scratch directory");
            }
        }
    }
    bail!("Cannot create a unique continuation scratch directory")
}

/// The scratch guard lives for the entire export and removes only this invocation's
/// generated logical rollouts, never any input, output or pre-existing directory.
#[derive(Debug)]
pub(crate) struct PreparedRollouts {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) merged_count: usize,
    _scratch: Option<Scratch>,
}

pub(crate) fn prepare(
    files: &[PathBuf],
    groups: &HashMap<String, Vec<PathBuf>>,
    selected: Option<&HashSet<String>>,
    continuation_ids: Option<&HashSet<String>>,
) -> Result<PreparedRollouts> {
    let mut work = Vec::<(&String, &Vec<PathBuf>)>::new();
    for (id, group) in groups {
        if selected.is_some_and(|ids| !ids.contains(id)) {
            continue;
        }
        if group.len() > 1 {
            work.push((id, group));
        } else {
            // A surviving standalone continuation is incomplete without its
            // history_base source. Never export it as a full thread.
            let part = if let Some(ids) = continuation_ids {
                ids.contains(id)
            } else {
                load_header(&group[0])?
            };
            if part {
                bail!(
                    "History base missing for standalone continuation Session {id}: {}",
                    group[0].display()
                );
            }
        }
    }
    if work.is_empty() {
        return Ok(PreparedRollouts {
            files: files.to_vec(),
            merged_count: 0,
            _scratch: None,
        });
    }
    work.sort_by(|a, b| a.0.cmp(b.0));
    // Complete in-memory validation precedes *any* scratch creation or output
    // write. It guarantees no partial merge if a later group is malformed.
    let assembled: Vec<_> = work
        .iter()
        .map(|(id, group)| build_chain(group).map(|bytes| ((*id).clone(), bytes)))
        .collect::<Result<_>>()?;
    let scratch = new_scratch()?;
    let mut replace = HashMap::<PathBuf, PathBuf>::new();
    let mut discarded = HashSet::<PathBuf>::new();
    for ((_, group), (_, bytes)) in work.iter().zip(&assembled) {
        let merged = scratch
            .0
            .join(format!("rollout-merged-{}.jsonl", replace.len()));
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&merged)
            .with_context(|| format!("create merged rollout {}", merged.display()))?;
        out.write_all(bytes)
            .with_context(|| format!("write merged rollout {}", merged.display()))?;
        out.flush()?;
        replace.insert(group[0].clone(), merged);
        discarded.extend(group.iter().skip(1).cloned());
    }
    let mut result = Vec::with_capacity(files.len());
    for path in files {
        if let Some(merged) = replace.get(path) {
            result.push(merged.clone());
        } else if !discarded.contains(path) {
            result.push(path.clone());
        }
    }
    Ok(PreparedRollouts {
        files: result,
        merged_count: assembled.len(),
        _scratch: Some(scratch),
    })
}

fn load_header(path: &Path) -> Result<bool> {
    // Only read a single JSONL header for the overwhelmingly common singleton;
    // decompress a zstd candidate if necessary. The project resolver has
    // already established canonical metadata identity.
    let raw = if rollout_codec::is_compressed_rollout(path) {
        rollout_codec::read_rollout_snapshot(path)?
    } else {
        use std::io::{BufRead, Read};
        let file = crate::open_shared_read(path)?;
        let limit = crate::source_guard::snapshot_for(path)
            .map(|snapshot| snapshot.len)
            .unwrap_or(file.metadata()?.len());
        let mut reader = std::io::BufReader::new(file.take(limit));
        let mut first = Vec::new();
        reader.read_until(b'\n', &mut first)?;
        first
    };
    let line = raw.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let value: Value = serde_json::from_slice(line)
        .with_context(|| format!("read session_meta header {}", path.display()))?;
    Ok(value
        .get("payload")
        .and_then(|p| p.get("history_base"))
        .is_some_and(|v| !v.is_null()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(s: &str) -> Vec<u8> {
        format!("{s}\n").into_bytes()
    }
    fn meta(id: &str, stamp: &str, link: Option<(&str, u64, u64)>) -> Vec<u8> {
        let mut body = json!({"id":id,"session_id":id,"timestamp":stamp,"cwd":"D:\\SYNTHETIC_CONTINUATION", "source":"vscode", "originator":"Codex Desktop", "thread_source":"user","history_mode":"paginated"});
        if let Some((physical_parent, ordinal, offset)) = link {
            body["history_base"] = json!({"thread_id":physical_parent,"end_ordinal_exclusive":ordinal,"end_byte_offset":offset});
        }
        format!("{}\n", json!({"type":"session_meta", "payload":body})).into_bytes()
    }
    fn write(folder: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = folder.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
    fn test_folder() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "codex-continuation-test-{}-{}",
            std::process::id(),
            NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        p
    }

    #[test]
    fn reconstructs_verified_history_base_and_drops_aborted_branch() {
        let dir = test_folder();
        let id = "00000000-0000-7000-8000-000000000800";
        let mut base = meta(id, "2026-10-19T09:04:07Z", None);
        base.extend(record(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ));
        let cut = base.len();
        base.extend(record(
            r#"{"type":"event_msg","payload":{"type":"turn_aborted"}}"#,
        ));
        let mut suffix = meta(id, "2026-10-19T11:08:31Z", Some(("a", 2, cut as u64)));
        suffix.extend(record(
            r#"{"type":"response_item","payload":{"type":"message","content":"resume"}}"#,
        ));
        let a = write(&dir, "a.jsonl", &base);
        let b = write(&dir, "b.jsonl", &suffix);
        let merged = build_chain(&[b.clone(), a.clone()]).unwrap();
        assert!(merged.windows(b"resume".len()).any(|w| w == b"resume"));
        assert!(
            !merged
                .windows(b"turn_aborted".len())
                .any(|w| w == b"turn_aborted")
        );
        assert_eq!(merged.iter().filter(|&&c| c == b'\n').count(), 3);
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn rejects_wrong_history_base_and_preserves_existing_files() {
        let dir = test_folder();
        let id = "thread";
        let base = meta(id, "2026-10-19T09:04:07Z", None);
        let a = write(&dir, "a.jsonl", &base);
        let mut tail = meta(
            id,
            "2026-10-19T11:08:31Z",
            Some(("a", 99, base.len() as u64)),
        );
        tail.extend(record(r#"{"type":"event_msg","payload":{}}"#));
        let b = write(&dir, "b.jsonl", &tail);
        assert!(build_chain(&[a.clone(), b.clone()]).is_err());
        assert_eq!(fs::read(&a).unwrap(), base);
        assert_eq!(fs::read(&b).unwrap(), tail);
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn reconstructs_two_successive_history_base_resumes() {
        let dir = test_folder();
        let id = "00000000-0000-7000-8000-000000000800";
        let mut root = meta(id, "2026-10-19T09:00:00Z", None);
        root.extend(record(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"root"}}"#,
        ));
        let root_cut = root.len();
        root.extend(record(
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","message":"superseded-one"}}"#,
        ));
        let mut second = meta(id, "2026-10-19T10:00:00Z", Some(("a", 2, root_cut as u64)));
        second.extend(record(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"second"}}"#,
        ));
        // A physical cut into b.jsonl, not an offset into root + b.
        let second_cut = second.len();
        second.extend(record(
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","message":"superseded-two"}}"#,
        ));
        let mut third = meta(
            id,
            "2026-10-19T11:00:00Z",
            Some(("b", 4, second_cut as u64)),
        );
        third.extend(record(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"third"}}"#,
        ));
        let a = write(&dir, "a.jsonl", &root);
        let b = write(&dir, "b.jsonl", &second);
        let c = write(&dir, "c.jsonl", &third);
        let merged = String::from_utf8(build_chain(&[a, b, c]).unwrap()).unwrap();
        assert!(merged.contains("\"message\":\"root\""));
        assert!(merged.contains("\"message\":\"second\""));
        assert!(merged.contains("\"message\":\"third\""));
        assert!(!merged.contains("superseded-one"));
        assert!(!merged.contains("superseded-two"));
        assert_eq!(merged.bytes().filter(|c| *c == b'\n').count(), 4);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_ambiguous_fork_and_missing_history_root() {
        let dir = test_folder();
        let id = "00000000-0000-7000-8000-000000000800";
        let root = meta(id, "2026-10-19T09:00:00Z", None);
        let pointer = Some(("a", 1, root.len() as u64));
        let a = write(&dir, "a.jsonl", &root);
        let b = write(&dir, "b.jsonl", &meta(id, "2026-10-19T10:00:00Z", pointer));
        let c = write(&dir, "c.jsonl", &meta(id, "2026-10-19T11:00:00Z", pointer));
        assert!(build_chain(&[a, b.clone(), c]).is_err());
        let singleton = HashMap::from([(id.to_owned(), vec![b.clone()])]);
        assert!(prepare(&[b], &singleton, None, None).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn physical_rollout_uuid_chain_reconstructs_three_pages() {
        let dir = test_folder();
        let id = "01a16681-f457-7013-9e48-b9982ef68e33";
        let middle_id = "01a16699-882b-7203-92f9-b859785f19d7";
        let newest_id = "01a16699-c1af-7c62-9026-18aa2216d3a6";
        let mut first = meta(id, "2026-10-23T10:05:55Z", None);
        first.extend(record(
            r#"{"ordinal":1,"type":"event_msg","payload":{"message":"root-event"}}"#,
        ));
        // Synthetic file headers include a canonical session_meta but the
        // ordinal is attached to this record for boundary verification.
        let mut first_meta: Value =
            serde_json::from_slice(&first[..first.iter().position(|b| *b == b'\n').unwrap()])
                .unwrap();
        first_meta["ordinal"] = json!(0);
        let first_line = record(&first_meta.to_string());
        let header_end = first.iter().position(|b| *b == b'\n').unwrap() + 1;
        first.splice(..header_end, first_line);
        let root_cut = first.len();
        first.extend(record(
            r#"{"ordinal":2,"type":"event_msg","payload":{"message":"aborted-root"}}"#,
        ));

        let mut mid = meta(id, "2026-10-23T10:31:40Z", Some((id, 2, root_cut as u64)));
        mid.extend(record(
            r#"{"ordinal":3,"type":"event_msg","payload":{"message":"middle-event"}}"#,
        ));
        let mut mid_meta: Value =
            serde_json::from_slice(&mid[..mid.iter().position(|b| *b == b'\n').unwrap()]).unwrap();
        mid_meta["ordinal"] = json!(2);
        let mid_line = record(&mid_meta.to_string());
        let header_end = mid.iter().position(|b| *b == b'\n').unwrap() + 1;
        mid.splice(..header_end, mid_line);
        let mid_cut = mid.len();
        mid.extend(record(
            r#"{"ordinal":4,"type":"event_msg","payload":{"message":"aborted-mid"}}"#,
        ));

        let mut last = meta(
            id,
            "2026-10-23T10:31:55Z",
            Some((middle_id, 4, mid_cut as u64)),
        );
        last.extend(record(
            r#"{"ordinal":5,"type":"event_msg","payload":{"message":"newest-event"}}"#,
        ));
        let mut last_meta: Value =
            serde_json::from_slice(&last[..last.iter().position(|b| *b == b'\n').unwrap()])
                .unwrap();
        last_meta["ordinal"] = json!(4);
        let last_line = record(&last_meta.to_string());
        let header_end = last.iter().position(|b| *b == b'\n').unwrap() + 1;
        last.splice(..header_end, last_line);

        let a = write(
            &dir,
            &format!("rollout-2026-10-23T10-05-55-{id}.jsonl"),
            &first,
        );
        let b = write(
            &dir,
            &format!("rollout-2026-10-23T10-31-40-{id}_{middle_id}.jsonl"),
            &mid,
        );
        let c = write(
            &dir,
            &format!("rollout-2026-10-23T10-31-55-{id}_{newest_id}.jsonl"),
            &last,
        );
        let reconstructed = build_chain(&[c, b, a]).unwrap();
        let rows: Vec<Value> = reconstructed
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 4); // original meta + three kept events
        assert_eq!(
            rows.iter()
                .map(|r| r["ordinal"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![0, 1, 3, 5]
        );
        let text = String::from_utf8(reconstructed).unwrap();
        assert!(!text.contains("aborted-root"));
        assert!(!text.contains("aborted-mid"));
        assert_eq!(text.matches("session_meta").count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fails_closed_for_missing_physical_parent_and_fork() {
        let dir = test_folder();
        let id = "thread";
        let mut root = meta(id, "2026-10-19T09:00:00Z", None);
        root.extend(record(
            r#"{"type":"event_msg","payload":{"message":"first"}}"#,
        ));
        let a = write(&dir, "a.jsonl", &root);
        let pointer = Some(("a", 2, root.len() as u64));
        let b = write(&dir, "b.jsonl", &meta(id, "2026-10-19T10:00:00Z", pointer));
        let c = write(&dir, "c.jsonl", &meta(id, "2026-10-19T11:00:00Z", pointer));
        assert!(build_chain(&[a.clone(), b.clone(), c]).is_err()); // fork
        let missing = write(
            &dir,
            "d.jsonl",
            &meta(
                id,
                "2026-10-19T12:00:00Z",
                Some(("missing", 2, root.len() as u64)),
            ),
        );
        assert!(build_chain(&[a.clone(), missing]).is_err());
        let out_of_bounds = write(
            &dir,
            "e.jsonl",
            &meta(
                id,
                "2026-10-19T13:00:00Z",
                Some(("a", 2, (root.len() + 1) as u64)),
            ),
        );
        assert!(build_chain(&[a.clone(), out_of_bounds]).is_err());
        let wrong_ordinal = write(
            &dir,
            "f.jsonl",
            &meta(
                id,
                "2026-10-19T14:00:00Z",
                Some(("a", 3, root.len() as u64)),
            ),
        );
        assert!(build_chain(&[a.clone(), wrong_ordinal]).is_err());
        let split = write(
            &dir,
            "g.jsonl",
            &meta(
                id,
                "2026-10-19T15:00:00Z",
                Some(("a", 2, (root.len() - 2) as u64)),
            ),
        );
        assert!(build_chain(&[a, split]).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_unreachable_cycle_and_understands_compressed_physical_names() {
        let id = "01a16681-f457-7013-9e48-b9982ef68e33";
        let next = "01a16699-882b-7203-92f9-b859785f19d7";
        let compressed = format!("rollout-2026-10-23T10-31-40-{id}_{next}.jsonl.zst");
        assert_eq!(
            physical_rollout_id(Path::new(&compressed), id).unwrap(),
            next
        );
        let dir = test_folder();
        let a = write(&dir, "a.jsonl", &meta(id, "2026-10-23T10:00:00Z", None));
        let b = write(
            &dir,
            "b.jsonl",
            &meta(id, "2026-10-23T10:02:00Z", Some(("c", 1, 1))),
        );
        let c = write(
            &dir,
            "c.jsonl",
            &meta(id, "2026-10-23T10:01:00Z", Some(("b", 1, 1))),
        );
        assert!(build_chain(&[a, b, c]).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_wrong_filename_logical_identity() {
        let dir = test_folder();
        let id = "01a16681-f457-7013-9e48-b9982ef68e33";
        let impostor = "01a1665e-ea61-7530-bfd6-4359b4ad9aa5";
        let a = write(
            &dir,
            &format!("rollout-2026-10-23T10-05-55-{impostor}.jsonl"),
            &meta(id, "2026-10-23T10:05:55Z", None),
        );
        assert!(load(&a).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_duplicate_complete_roots_and_conflicting_cwd() {
        let dir = test_folder();
        let id = "thread";
        let a = write(&dir, "a.jsonl", &meta(id, "2026-10-19T09:04:07Z", None));
        let b = write(&dir, "b.jsonl", &meta(id, "2026-10-19T11:08:31Z", None));
        assert!(build_chain(&[a.clone(), b.clone()]).is_err());
        let initial = fs::read(&a).unwrap();
        let mut continuation = meta(
            id,
            "2026-10-19T11:08:31Z",
            Some(("a", 1, initial.len() as u64)),
        );
        continuation = String::from_utf8(continuation)
            .unwrap()
            .replace("SYNTHETIC_CONTINUATION", "OTHER")
            .into_bytes();
        fs::write(&b, &continuation).unwrap();
        assert!(build_chain(&[a, b]).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
