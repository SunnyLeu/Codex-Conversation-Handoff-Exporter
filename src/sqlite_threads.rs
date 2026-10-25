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
}
