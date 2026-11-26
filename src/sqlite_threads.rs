//! SQLite is read only in the source tree. Copy database + WAL into an owned
//! temporary directory, verify stable bytes, and query the copy with SQLite.
//! Missing rows do not prove deletion; explicit archived rows exclude targets.
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};

#[cfg_attr(windows, link(name = "winsqlite3"))]
#[cfg_attr(not(windows), link(name = "sqlite3"))]
unsafe extern "C" {
    fn sqlite3_open_v2(
        name: *const c_char,
        db: *mut *mut c_void,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut c_void) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut c_void,
        sql: *const c_char,
        len: c_int,
        statement: *mut *mut c_void,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(statement: *mut c_void) -> c_int;
    fn sqlite3_finalize(statement: *mut c_void) -> c_int;
    fn sqlite3_column_text(statement: *mut c_void, column: c_int) -> *const u8;
    fn sqlite3_column_type(statement: *mut c_void, column: c_int) -> c_int;
    fn sqlite3_column_bytes(statement: *mut c_void, column: c_int) -> c_int;
    fn sqlite3_column_int(statement: *mut c_void, column: c_int) -> c_int;
    fn sqlite3_errmsg(db: *mut c_void) -> *const c_char;
    fn sqlite3_busy_timeout(db: *mut c_void, ms: c_int) -> c_int;
    fn sqlite3_bind_text(
        statement: *mut c_void,
        index: c_int,
        value: *const c_char,
        len: c_int,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
}
struct Db(*mut c_void);
impl Drop for Db {
    fn drop(&mut self) {
        unsafe {
            sqlite3_close(self.0);
        }
    }
}
struct Statement(*mut c_void);
impl Drop for Statement {
    fn drop(&mut self) {
        unsafe {
            sqlite3_finalize(self.0);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadRow {
    pub(crate) title: String,
    pub(crate) archived: bool,
    pub(crate) rollout_path: String,
}

pub(crate) fn database_path(home: &Path) -> Result<Option<PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(home)
        .with_context(|| format!("讀取 Codex 資料目錄 {}", home.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(n) = name
            .strip_prefix("state_")
            .and_then(|s| s.strip_suffix(".sqlite"))
            .and_then(|s| s.parse::<u32>().ok())
        {
            found.push((n, entry.path()));
        }
    }
    found.sort_by_key(|(n, _)| *n);
    Ok(found.pop().map(|(_, p)| p))
}

pub(crate) fn load(home: &Path) -> Result<HashMap<String, ThreadRow>> {
    let Some(path) = database_path(home)? else {
        return Ok(HashMap::new());
    };
    let wal = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
    for attempt in 0..3 {
        let temp = crate::codex_paths::TempDir::new("thread-index")?;
        let copy = temp.path().join("state.sqlite");
        let original = std::fs::read(&path).context("讀取 Thread 資料庫")?;
        let wal_before = optional_bytes(&wal)?;
        std::fs::write(&copy, &original)?;
        if let Some(bytes) = &wal_before {
            std::fs::write(temp.path().join("state.sqlite-wal"), bytes)?;
        }
        if std::fs::read(&path)? == original && optional_bytes(&wal)? == wal_before {
            return query(&copy).context("讀取 Thread 索引失敗；未變更原始資料庫");
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
    bail!("Codex Thread 資料庫持續變動，無法取得一致索引，請稍後再試")
}
/// Point lookup for one explicitly selected independent Thread.
/// The live SQLite WAL provides a transactionally consistent read snapshot;
/// this runs only after E/--yes, never while displaying the sidebar.
pub(crate) fn lookup_one(home: &Path, id: &str) -> Result<Option<ThreadRow>> {
    let Some(path) = database_path(home)? else {
        return Ok(None);
    };
    let filename = CString::new(path.to_string_lossy().as_bytes())?;
    let id_c = CString::new(id)?;
    let mut raw = std::ptr::null_mut();
    let status = unsafe { sqlite3_open_v2(filename.as_ptr(), &mut raw, 1, std::ptr::null()) };
    if raw.is_null() {
        bail!("SQLite 無法配置唯讀連線（{status}）");
    }
    let db = Db(raw);
    if status != 0 {
        bail!("SQLite 唯讀開啟失敗：{}", message(db.0));
    }
    unsafe {
        sqlite3_busy_timeout(db.0, 1500);
    }
    let sql = c"SELECT title, archived, rollout_path FROM threads WHERE id = ?1 LIMIT 1";
    let mut ptr = std::ptr::null_mut();
    let status =
        unsafe { sqlite3_prepare_v2(db.0, sql.as_ptr(), -1, &mut ptr, std::ptr::null_mut()) };
    if status != 0 {
        if !ptr.is_null() {
            unsafe {
                sqlite3_finalize(ptr);
            }
        }
        bail!("無法查詢 Thread 資料表：{}", message(db.0));
    }
    if ptr.is_null() {
        bail!("SQLite 沒有回傳 Thread 查詢");
    }
    let statement = Statement(ptr);
    // Destructor=NULL (SQLITE_STATIC) is sound: id_c is still alive through
    // the full statement lifetime, and is never mutated.
    let status = unsafe { sqlite3_bind_text(statement.0, 1, id_c.as_ptr(), -1, None) };
    if status != 0 {
        bail!("SQLite Thread ID 綁定失敗：{}", message(db.0));
    }
    let status = unsafe { sqlite3_step(statement.0) };
    if status == 101 {
        return Ok(None);
    } // SQLITE_DONE
    if status != 100 {
        bail!("SQLite Thread 查詢失敗：{}", message(db.0));
    }
    let text = |column| -> String {
        let ptr = unsafe { sqlite3_column_text(statement.0, column) };
        if ptr.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(ptr.cast()) }
                .to_string_lossy()
                .into_owned()
        }
    };
    let row = ThreadRow {
        title: text(0),
        archived: unsafe { sqlite3_column_int(statement.0, 1) } != 0,
        rollout_path: text(2),
    };
    // A single-row query should complete successfully, not hide database errors.
    let done = unsafe { sqlite3_step(statement.0) };
    if done != 101 {
        bail!("SQLite Thread 唯讀查詢未正常結束：{}", message(db.0));
    }
    Ok(Some(row))
}

/// A single SQLite read transaction supplies both the current project registry
/// and non-null explicit Thread assignments. A NULL assignment is not a
/// projectless declaration (older clients still persist assignments in JSON).
#[derive(Debug, Clone)]
pub(crate) struct ProjectRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) roots: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct ProjectSnapshot {
    pub(crate) projects: Vec<ProjectRow>,
    pub(crate) assignments: Vec<(String, String)>,
}

fn prepare(db: &Db, sql: &str) -> Result<Statement> {
    let sql = CString::new(sql)?;
    let mut ptr = std::ptr::null_mut();
    let rc = unsafe { sqlite3_prepare_v2(db.0, sql.as_ptr(), -1, &mut ptr, std::ptr::null_mut()) };
    if rc != 0 || ptr.is_null() {
        if !ptr.is_null() {
            unsafe { sqlite3_finalize(ptr) };
        }
        bail!(
            "SQLite project query prepare failed ({rc}): {}",
            message(db.0)
        );
    }
    Ok(Statement(ptr))
}

fn open_database(path: &Path, flags: c_int) -> Result<Db> {
    let filename = CString::new(
        path.to_str()
            .context("SQLite database path is not Unicode")?,
    )?;
    let mut raw = std::ptr::null_mut();
    let rc = unsafe { sqlite3_open_v2(filename.as_ptr(), &mut raw, flags, std::ptr::null()) };
    if raw.is_null() {
        bail!("SQLite connection allocation failed ({rc})");
    }
    let db = Db(raw);
    if rc != 0 {
        bail!("SQLite open failed ({rc}): {}", message(db.0));
    }
    let rc = unsafe { sqlite3_busy_timeout(db.0, 1500) };
    if rc != 0 {
        bail!("SQLite busy timeout failed ({rc}): {}", message(db.0));
    }
    Ok(db)
}

fn execute(db: &Db, sql: &str) -> Result<()> {
    let statement = prepare(db, sql)?;
    let rc = unsafe { sqlite3_step(statement.0) };
    if rc != 101 {
        bail!("SQLite statement failed ({rc}): {}", message(db.0));
    }
    Ok(())
}

fn text_rows(db: &Db, sql: &str, columns: usize) -> Result<Vec<Vec<String>>> {
    let statement = prepare(db, sql)?;
    let mut rows = Vec::new();
    loop {
        let rc = unsafe { sqlite3_step(statement.0) };
        if rc == 101 {
            break;
        }
        if rc != 100 {
            bail!("SQLite project query failed ({rc}): {}", message(db.0));
        }
        let mut row = Vec::with_capacity(columns);
        for column in 0..columns as c_int {
            // Reject unknown/null/non-text identity rather than silently
            // converting it. Copy UTF-8 while SQLite owns the column buffer.
            if unsafe { sqlite3_column_type(statement.0, column) } != 3 {
                bail!("SQLite project query contains non-text column {column}");
            }
            let ptr = unsafe { sqlite3_column_text(statement.0, column) };
            let len = unsafe { sqlite3_column_bytes(statement.0, column) };
            if ptr.is_null() || len < 0 {
                bail!("SQLite project query could not read column {column}");
            }
            let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
            let value =
                std::str::from_utf8(bytes).context("Invalid UTF-8 in SQLite project data")?;
            if value.contains('\0') {
                bail!("SQLite project query contains embedded NUL in column {column}");
            }
            row.push(value.to_owned());
        }
        rows.push(row);
    }
    Ok(rows)
}

/// SQLite read-only connections can still create/update WAL-index sidecars.
/// Query an owned copy instead, so even an inactive source DB is untouched.
/// Allow an append-only WAL tail after capture (normal active Codex writes),
/// but never a reset, truncation, rewritten prefix or changed main DB. A
/// partial final WAL frame is interpreted by SQLite's own recovery rules.
fn stable_project_copy(path: &Path) -> Result<crate::codex_paths::TempDir> {
    let wal = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
    for attempt in 0..3 {
        let original = std::fs::read(path).context("Read project database snapshot")?;
        let wal_before = optional_bytes(&wal)?;
        let main_unchanged = std::fs::read(path)? == original;
        let wal_after = optional_bytes(&wal)?;
        let wal_stable = match (&wal_before, &wal_after) {
            (None, None) => true,
            (Some(before), Some(after)) => {
                before == after || (before.len() >= 32 && after.starts_with(before))
            }
            _ => false,
        };
        if main_unchanged && wal_stable {
            let temp = crate::codex_paths::TempDir::new("project-registry")?;
            std::fs::write(temp.path().join("state.sqlite"), &original)?;
            if let Some(bytes) = &wal_before {
                std::fs::write(temp.path().join("state.sqlite-wal"), bytes)?;
            }
            return Ok(temp);
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
    bail!(
        "Codex project database/WAL changed during capture; cannot obtain a consistent read-only snapshot: {}",
        path.display()
    )
}

pub(crate) fn load_project_snapshot(home: &Path) -> Result<Option<ProjectSnapshot>> {
    let Some(path) = database_path(home)? else {
        return Ok(None);
    };
    let copy = stable_project_copy(&path)?;
    let db = open_database(&copy.path().join("state.sqlite"), 1).with_context(|| {
        format!(
            "Read isolated Codex project database snapshot of {}",
            path.display()
        )
    })?;
    // Establish a transaction before inspecting the schema, so roots and
    // assignments cannot be assembled from two different WAL snapshots.
    execute(&db, "BEGIN")?;
    let tables = text_rows(
        &db,
        "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('projects','project_roots','threads') ORDER BY name",
        1,
    )?;
    let has = |name: &str| tables.iter().any(|row| row[0] == name);
    if !has("projects") && !has("project_roots") {
        return Ok(None); // explicitly observed legacy schema
    }
    if !has("projects") || !has("project_roots") || !has("threads") {
        bail!(
            "Incomplete SQLite project schema (projects/project_roots/threads): {}",
            path.display()
        );
    }
    let mut projects = Vec::new();
    let mut index = HashMap::new();
    for row in text_rows(
        &db,
        "SELECT id, name FROM projects ORDER BY position, id",
        2,
    )? {
        let id = &row[0];
        if id.trim().is_empty()
            || id.chars().any(char::is_control)
            || index.insert(id.clone(), projects.len()).is_some()
        {
            bail!("Invalid or duplicate SQLite project ID: {id:?}");
        }
        projects.push(ProjectRow {
            id: id.clone(),
            name: row[1].clone(),
            roots: Vec::new(),
        });
    }
    for row in text_rows(
        &db,
        "SELECT project_id, path FROM project_roots ORDER BY project_id, position",
        2,
    )? {
        let idx = index
            .get(&row[0])
            .with_context(|| format!("SQLite project root has no registered owner: {}", row[0]))?;
        if row[1].trim().is_empty() || row[1].chars().any(char::is_control) {
            bail!("Invalid SQLite root path for project {}", row[0]);
        }
        projects[*idx].roots.push(row[1].clone());
    }
    // Older intermediate schemas may have the project registry but not the
    // Thread project_id column. Inspect actual schema, not exception text.
    let columns = text_rows(&db, "SELECT name FROM pragma_table_info('threads')", 1)?;
    let mut assignments = Vec::new();
    if columns.iter().any(|row| row[0] == "project_id") {
        for row in text_rows(
            &db,
            "SELECT id, project_id FROM threads WHERE project_id IS NOT NULL ORDER BY id",
            2,
        )? {
            if row
                .iter()
                .any(|v| v.trim().is_empty() || v.chars().any(char::is_control))
            {
                bail!("Invalid SQLite Thread/project assignment");
            }
            assignments.push((row[0].clone(), row[1].clone()));
        }
    }
    // Drop the connection before its owned temporary directory.
    Ok(Some(ProjectSnapshot {
        projects,
        assignments,
    }))
}

#[cfg(test)]
pub(crate) fn test_execute(home: &Path, statements: &[&str]) -> Result<()> {
    let db = open_database(&home.join("state_5.sqlite"), 6)?;
    for sql in statements {
        execute(&db, sql)?;
    }
    Ok(())
}

fn optional_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn query(path: &Path) -> Result<HashMap<String, ThreadRow>> {
    let file = CString::new(
        path.to_str()
            .ok_or_else(|| anyhow::anyhow!("資料庫路徑不是有效 Unicode"))?,
    )?;
    let mut db = std::ptr::null_mut();
    // SAFETY: NUL-terminated filename; SQLite manages the returned opaque handle.
    let status = unsafe { sqlite3_open_v2(file.as_ptr(), &mut db, 1, std::ptr::null()) }; // SQLITE_OPEN_READONLY
    if db.is_null() {
        bail!("SQLite 無法配置連線（{status}）");
    }
    let db = Db(db);
    if status != 0 {
        bail!("SQLite 開啟失敗：{}", message(db.0));
    }
    unsafe {
        sqlite3_busy_timeout(db.0, 1000);
    }
    let mut ptr = std::ptr::null_mut();
    let sql = c"SELECT id, title, archived, rollout_path FROM threads";
    let status =
        unsafe { sqlite3_prepare_v2(db.0, sql.as_ptr(), -1, &mut ptr, std::ptr::null_mut()) };
    if status != 0 {
        if !ptr.is_null() {
            unsafe {
                sqlite3_finalize(ptr);
            }
        }
        bail!("不支援或損壞的 Thread 資料表：{}", message(db.0));
    }
    if ptr.is_null() {
        bail!("SQLite 沒有回傳查詢");
    }
    let statement = Statement(ptr);
    let mut rows = HashMap::new();
    loop {
        let status = unsafe { sqlite3_step(statement.0) };
        if status == 101 {
            break;
        } // SQLITE_DONE
        if status != 100 {
            bail!("SQLite 查詢失敗：{}", message(db.0));
        }
        let text = |index| -> String {
            // SAFETY: SQLite's column pointer lives until the next step/finalize;
            // copy it immediately and never retain the borrowed allocation.
            let ptr = unsafe { sqlite3_column_text(statement.0, index) };
            if ptr.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(ptr.cast()) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        let id = text(0);
        if id.is_empty() {
            bail!("Thread 資料庫包含空白 ID");
        }
        let row = ThreadRow {
            title: text(1),
            archived: unsafe { sqlite3_column_int(statement.0, 2) } != 0,
            rollout_path: text(3),
        };
        if rows.insert(id, row).is_some() {
            bail!("Thread 資料庫 ID 重複");
        }
    }
    Ok(rows)
}
fn message(db: *mut c_void) -> String {
    unsafe { CStr::from_ptr(sqlite3_errmsg(db)) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_database_is_not_a_deleted_thread() {
        let d = crate::codex_paths::TempDir::new("test-db").unwrap();
        assert!(load(d.path()).unwrap().is_empty());
    }
    #[test]
    fn corrupt_database_is_not_silently_ignored() {
        let d = crate::codex_paths::TempDir::new("test-db").unwrap();
        std::fs::write(d.path().join("state_5.sqlite"), b"bad").unwrap();
        assert!(load(d.path()).is_err());
    }
    #[test]
    fn project_snapshot_reads_committed_wal_without_source_write() {
        let d = crate::codex_paths::TempDir::new("project-wal-test").unwrap();
        let path = d.path().join("state_5.sqlite");
        let writer = open_database(&path, 6).unwrap();
        assert_eq!(
            text_rows(&writer, "PRAGMA journal_mode=WAL", 1).unwrap()[0][0],
            "wal"
        );
        for sql in [
            "CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT,position INTEGER)",
            "CREATE TABLE project_roots(project_id TEXT,position INTEGER,path TEXT,PRIMARY KEY(project_id,position))",
            "CREATE TABLE threads(id TEXT PRIMARY KEY,project_id TEXT)",
            "INSERT INTO projects VALUES('p','Current',0)",
            r"INSERT INTO project_roots VALUES('p',1,'D:\old'),('p',0,'D:\new')",
            "INSERT INTO threads VALUES('a','p'),('b',NULL)",
        ] {
            execute(&writer, sql).unwrap();
        }
        let wal = d.path().join("state_5.sqlite-wal");
        let before = std::fs::read(&path).unwrap();
        let wal_before = std::fs::read(&wal).unwrap();
        let current = load_project_snapshot(d.path()).unwrap().unwrap();
        assert_eq!(current.projects[0].roots, [r"D:\new", r"D:\old"]);
        assert_eq!(current.assignments, [("a".into(), "p".into())]);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
        execute(&writer, "BEGIN").unwrap();
        execute(&writer, "DELETE FROM project_roots").unwrap();
        assert_eq!(
            load_project_snapshot(d.path()).unwrap().unwrap().projects[0]
                .roots
                .len(),
            2
        );
        execute(&writer, "COMMIT").unwrap();
        assert!(
            load_project_snapshot(d.path()).unwrap().unwrap().projects[0]
                .roots
                .is_empty()
        );
    }
    #[test]
    fn project_snapshot_does_not_create_source_wal_sidecars() {
        let d = crate::codex_paths::TempDir::new("project-no-sidecars-test").unwrap();
        let path = d.path().join("state_5.sqlite");
        {
            let db = open_database(&path, 6).unwrap();
            assert_eq!(
                text_rows(&db, "PRAGMA journal_mode=WAL", 1).unwrap()[0][0],
                "wal"
            );
            for sql in [
                "CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT,position INTEGER)",
                "CREATE TABLE project_roots(project_id TEXT,position INTEGER,path TEXT,PRIMARY KEY(project_id,position))",
                "CREATE TABLE threads(id TEXT PRIMARY KEY,project_id TEXT)",
                "INSERT INTO projects VALUES('p','Current',0)",
                r"INSERT INTO project_roots VALUES('p',0,'D:\new')",
            ] {
                execute(&db, sql).unwrap();
            }
        }
        let roster = || -> std::collections::BTreeSet<_> {
            std::fs::read_dir(d.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect()
        };
        let before = roster();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            load_project_snapshot(d.path()).unwrap().unwrap().projects[0].roots,
            [r"D:\new"]
        );
        assert_eq!(roster(), before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
