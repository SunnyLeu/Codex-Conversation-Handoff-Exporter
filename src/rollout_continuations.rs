//! Reconstruct Codex paginated history from immutable physical rollout snapshots.
//!
//! A resumed Codex thread can have multiple rollout files with the same
//! `session_meta.id`. A continuation's `history_base` names the exact prior
//! *logical* JSONL prefix (record ordinal and decoded byte offset); the
//! records after that prefix in the old file are not part of the resumed
//! logical history. This module never changes original Codex data.
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
    timestamp: String,
    history_base: Option<(String, usize, usize)>,
    meta_end: usize,
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
                thread_id == id,
                "history_base references a different thread in {}",
                path.display()
            );
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
    Ok(PhysicalRollout {
        path: path.to_path_buf(),
        bytes,
        meta: meta.clone(),
        id,
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

fn build_chain(files: &[PathBuf]) -> Result<Vec<u8>> {
    let mut physical: Vec<_> = files.iter().map(|p| load(p)).collect::<Result<_>>()?;
    let id = physical[0].id.clone();
    ensure!(
        physical.iter().all(|p| p.id == id),
        "Mixed IDs in continuation group"
    );
    physical.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.path.cmp(&b.path))
    });
    let roots: Vec<_> = physical
        .iter()
        .filter(|p| p.history_base.is_none())
        .collect();
    ensure!(
        roots.len() == 1,
        "Session {} requires exactly one full history root; found {}",
        id,
        roots.len()
    );
    let root = roots[0];
    ensure!(
        physical.first().is_some_and(|p| p.path == root.path),
        "Session {} continuation predates its only full-history root",
        id
    );
    ensure!(
        root.meta.get("history_mode").and_then(Value::as_str) == Some("paginated"),
        "Repeated Session {} is not marked as paginated",
        id
    );
    let mut assembled = root.bytes.clone();
    let mut seen_pointers = HashSet::new();
    let mut previous_timestamp = &root.timestamp;
    for next in physical.iter().skip(1) {
        validate_identity(root, next)?;
        ensure!(
            next.timestamp > *previous_timestamp,
            "Non-increasing continuation timestamps for Session {}",
            id
        );
        let (_, ordinal, offset) = next
            .history_base
            .as_ref()
            .ok_or_else(|| anyhow!("Unlinked duplicate Session {}: {}", id, next.path.display()))?;
        ensure!(
            *ordinal > 0 && *offset > 0 && *offset <= assembled.len(),
            "Invalid history_base extent in {} for Session {}",
            next.path.display(),
            id
        );
        ensure!(
            assembled[*offset - 1] == b'\n',
            "history_base byte offset splits a JSONL record in {}",
            next.path.display()
        );
        let actual_ordinal = assembled[..*offset].iter().filter(|b| **b == b'\n').count();
        ensure!(
            actual_ordinal == *ordinal,
            "history_base ordinal/byte mismatch in {}: claimed {} records, found {}",
            next.path.display(),
            ordinal,
            actual_ordinal
        );
        ensure!(
            seen_pointers.insert((*ordinal, *offset)),
            "Ambiguous repeated history_base (possible fork) for Session {}",
            id
        );
        // Every suffix owns its *own* session_meta. Keep the root's canonical
        // metadata and only append resumed events after the verified prefix.
        assembled.truncate(*offset);
        assembled.extend_from_slice(&next.bytes[next.meta_end..]);
        previous_timestamp = &next.timestamp;
    }
    // Preserve the original reader's policy for an incomplete final JSONL
    // line, while never accepting malformed data in the middle of history.
    let segments: Vec<&[u8]> = assembled.split_inclusive(|b| *b == b'\n').collect();
    for (index, segment) in segments.iter().enumerate() {
        if segment.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: Value = match serde_json::from_slice(segment) {
            Ok(record) => record,
            Err(_) if index + 1 == segments.len() => break,
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
        let mut file = std::io::BufReader::new(fs::File::open(path)?);
        let mut first = Vec::new();
        use std::io::BufRead;
        file.read_until(b'\n', &mut first)?;
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
    fn meta(id: &str, stamp: &str, link: Option<(u64, u64)>) -> Vec<u8> {
        let mut body = json!({"id":id,"session_id":id,"timestamp":stamp,"cwd":"D:\\SYNTHETIC_CONTINUATION", "source":"vscode", "originator":"Codex Desktop", "thread_source":"user","history_mode":"paginated"});
        if let Some((ordinal, offset)) = link {
            body["history_base"] =
                json!({"thread_id":id,"end_ordinal_exclusive":ordinal,"end_byte_offset":offset});
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
        let mut suffix = meta(id, "2026-10-19T11:08:31Z", Some((2, cut as u64)));
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
        let mut tail = meta(id, "2026-10-19T11:08:31Z", Some((99, base.len() as u64)));
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
        let mut second = meta(id, "2026-10-19T10:00:00Z", Some((2, root_cut as u64)));
        second.extend(record(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"second"}}"#,
        ));
        let second_cut = root[..root_cut].len() + second.len()
            - meta(id, "2026-10-19T10:00:00Z", Some((2, root_cut as u64))).len();
        second.extend(record(
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","message":"superseded-two"}}"#,
        ));
        let mut third = meta(id, "2026-10-19T11:00:00Z", Some((3, second_cut as u64)));
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
        let pointer = Some((1, root.len() as u64));
        let a = write(&dir, "a.jsonl", &root);
        let b = write(&dir, "b.jsonl", &meta(id, "2026-10-19T10:00:00Z", pointer));
        let c = write(&dir, "c.jsonl", &meta(id, "2026-10-19T11:00:00Z", pointer));
        assert!(build_chain(&[a, b.clone(), c]).is_err());
        let singleton = HashMap::from([(id.to_owned(), vec![b.clone()])]);
        assert!(prepare(&[b], &singleton, None, None).is_err());
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
        let mut continuation = meta(id, "2026-10-19T11:08:31Z", Some((1, initial.len() as u64)));
        continuation = String::from_utf8(continuation)
            .unwrap()
            .replace("SYNTHETIC_CONTINUATION", "OTHER")
            .into_bytes();
        fs::write(&b, &continuation).unwrap();
        assert!(build_chain(&[a, b]).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
