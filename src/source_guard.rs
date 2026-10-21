//! Product-mode consistency fence. All hashes are checked before final writes.
//! This detects concurrent changes; it does not claim a filesystem transaction.
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

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
fn tree(root: &Path) -> Result<BTreeMap<PathBuf, [u8; 32]>> {
    let mut result = BTreeMap::new();
    if !root.exists() {
        return Ok(result);
    }
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let e = entry?;
        if e.file_type().is_file() {
            result.insert(e.path().to_owned(), digest(e.path())?);
        }
    }
    Ok(result)
}
pub(crate) struct InputGuard {
    rollouts: BTreeMap<PathBuf, [u8; 32]>,
    index: [u8; 32],
    state: String,
    attachments: BTreeMap<PathBuf, [u8; 32]>,
    database: BTreeMap<PathBuf, [u8; 32]>,
}
fn database(args: &crate::Args) -> Result<BTreeMap<PathBuf, [u8; 32]>> {
    let home = args
        .sessions_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Codex source home missing"))?;
    let mut result = BTreeMap::new();
    if let Some(db) = crate::sqlite_threads::database_path(home)? {
        let wal = PathBuf::from(format!("{}-wal", db.to_string_lossy()));
        result.insert(db.clone(), digest(&db)?);
        if wal.exists() {
            result.insert(wal.clone(), digest(&wal)?);
        }
    }
    Ok(result)
}
impl InputGuard {
    pub(crate) fn capture(args: &crate::Args, files: &[PathBuf]) -> Result<Self> {
        let state = crate::project_membership::read_stable_state(&crate::project_state_path(args))?;
        Ok(Self {
            rollouts: files
                .iter()
                .map(|p| Ok((p.clone(), digest(p)?)))
                .collect::<Result<_>>()?,
            index: digest(&args.session_index)?,
            state: crate::sidebar_targets::state_fingerprint(&state)?,
            attachments: tree(&args.attachment_root)?,
            database: database(args)?,
        })
    }
    pub(crate) fn verify(self, args: &crate::Args) -> Result<()> {
        let files = crate::discover_rollouts(&args.sessions_root)?;
        let after =
            Self::capture(args, &files).context("匯出前檢查來源一致性失敗；尚未覆寫 Handoff")?;
        if self.rollouts != after.rollouts
            || self.index != after.index
            || self.state != after.state
            || self.attachments != after.attachments
            || self.database != after.database
        {
            bail!("Codex 來源在準備匯出期間變更；尚未覆寫 Handoff。請在目前回應穩定後重新執行。");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::fs;
    fn fixture() -> (crate::codex_paths::TempDir, crate::Args, Vec<PathBuf>) {
        let d = crate::codex_paths::TempDir::new("consistency-test").unwrap();
        let mut args = crate::Args::parse_from(["test"]);
        args.sessions_root = d.path().join("sessions");
        args.session_index = d.path().join("session_index.jsonl");
        args.attachment_root = d.path().join("attachments");
        args.project_state = Some(d.path().join(".codex-global-state.json"));
        fs::create_dir(&args.sessions_root).unwrap();
        fs::create_dir(&args.attachment_root).unwrap();
        fs::write(&args.session_index, b"index fixture").unwrap();
        fs::write(
            crate::project_state_path(&args),
            br#"{"project-order":[],"local-projects":{}}"#,
        )
        .unwrap();
        let file = args.sessions_root.join("rollout-synthetic.jsonl");
        fs::write(&file, b"hash-only synthetic fixture\n").unwrap();
        (d, args, vec![file])
    }
    #[test]
    fn unchanged_inputs_and_irrelevant_ui_drafts_are_accepted() {
        let (_d, args, files) = fixture();
        InputGuard::capture(&args, &files)
            .unwrap()
            .verify(&args)
            .unwrap();
        let guard = InputGuard::capture(&args, &files).unwrap();
        fs::write(
            crate::project_state_path(&args),
            br#"{"project-order":[],"local-projects":{},"draft":"changed"}"#,
        )
        .unwrap();
        guard.verify(&args).unwrap();
    }
    #[test]
    fn every_changed_input_category_is_rejected() {
        for category in [
            "rollout",
            "index",
            "state",
            "attachment",
            "database",
            "new-rollout",
        ] {
            let (_d, args, files) = fixture();
            let guard = InputGuard::capture(&args, &files).unwrap();
            match category {
                "rollout" => fs::write(&files[0], b"changed bytes").unwrap(),
                "index" => fs::write(&args.session_index, b"new title").unwrap(),
                "state" => fs::write(
                    crate::project_state_path(&args),
                    br#"{"project-order":["new"],"local-projects":{}}"#,
                )
                .unwrap(),
                "attachment" => {
                    fs::write(args.attachment_root.join("new.bin"), b"new attachment").unwrap()
                }
                "database" => fs::write(
                    args.sessions_root.parent().unwrap().join("state_5.sqlite"),
                    b"new hash-only database fixture",
                )
                .unwrap(),
                "new-rollout" => fs::write(
                    args.sessions_root.join("rollout-new.jsonl"),
                    b"new physical rollout",
                )
                .unwrap(),
                _ => unreachable!(),
            }
            assert!(guard.verify(&args).is_err(), "{category}");
        }
    }
}
