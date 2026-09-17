//! Task list storage.
//!
//! Unlike the media and taskbar modules this one owns no threads or native
//! resources — it is a plain store the UI calls into, plus Markdown/JSON
//! export. The [`Module`] implementation is therefore a thin wrapper that just
//! opens the database and reports health.

use beautify_core::config::Config;
use beautify_core::module::{Module, ModuleContext, ModuleResult};
use beautify_core::paths;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// Priority buckets shown as coloured dots in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Priority {
    None,
    Low,
    Medium,
    High,
}

impl Priority {
    pub const ALL: [Priority; 4] = [
        Priority::None,
        Priority::Low,
        Priority::Medium,
        Priority::High,
    ];

    pub const fn as_i32(self) -> i32 {
        match self {
            Priority::None => 0,
            Priority::Low => 1,
            Priority::Medium => 2,
            Priority::High => 3,
        }
    }

    pub const fn from_i32(v: i32) -> Self {
        match v {
            1 => Priority::Low,
            2 => Priority::Medium,
            3 => Priority::High,
            _ => Priority::None,
        }
    }
}

/// A single task row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub title: String,
    pub note: String,
    pub done: bool,
    pub priority: Priority,
    /// Unix milliseconds, or `None` when the task is undated.
    pub due_at: Option<i64>,
    /// Free-form bucket name; the flyout shows "today" by default.
    pub list: String,
    pub position: i64,
    pub created_at: i64,
    pub completed_at: Option<i64>,
}

/// Field changes for [`TaskStore::update`]. `None` means "leave alone".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub note: Option<String>,
    pub done: Option<bool>,
    pub priority: Option<Priority>,
    /// Doubly-wrapped because "clear the due date" and "do not touch it" are
    /// different requests.
    pub due_at: Option<Option<i64>>,
    pub list: Option<String>,
}

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}
impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}
impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub struct TaskStore {
    conn: Mutex<Connection>,
}

/// Which slice of the list to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskFilter {
    /// Open tasks only, due today or undated.
    #[default]
    Today,
    /// Open tasks only, everything.
    Open,
    /// Everything, newest first.
    All,
    Done,
}

impl TaskStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn prepare(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                title        TEXT    NOT NULL,
                note         TEXT    NOT NULL DEFAULT '',
                done         INTEGER NOT NULL DEFAULT 0,
                priority     INTEGER NOT NULL DEFAULT 0,
                due_at       INTEGER,
                list         TEXT    NOT NULL DEFAULT 'today',
                position     INTEGER NOT NULL DEFAULT 0,
                created_at   INTEGER NOT NULL,
                completed_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_open ON tasks(done, position, created_at);
            "#,
        )?;
        Ok(())
    }

    /// Add a task to the top of its list.
    pub fn create(&self, title: &str, list: &str) -> Result<Task> {
        let title = title.trim();
        if title.is_empty() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "task title must not be empty",
            )));
        }
        let conn = self.conn.lock();
        let now = crate::now_millis();
        // Newest first: keep positions descending so a plain ORDER BY works.
        let min_pos: i64 = conn.query_row(
            "SELECT COALESCE(MIN(position), 0) FROM tasks WHERE done = 0",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO tasks (title, note, done, priority, due_at, list, position, created_at)
             VALUES (?1, '', 0, 0, NULL, ?2, ?3, ?4)",
            params![title, list, min_pos - 1, now],
        )?;
        let id = conn.last_insert_rowid();
        Self::get_locked(&conn, id)
    }

    fn get_locked(conn: &Connection, id: i64) -> Result<Task> {
        let task = conn.query_row(
            "SELECT id, title, note, done, priority, due_at, list, position, created_at, completed_at
             FROM tasks WHERE id = ?1",
            params![id],
            row_to_task,
        )?;
        Ok(task)
    }

    pub fn get(&self, id: i64) -> Result<Option<Task>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT id, title, note, done, priority, due_at, list, position, created_at, completed_at
                 FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()?)
    }

    pub fn list(&self, filter: TaskFilter) -> Result<Vec<Task>> {
        /// Shared column list; every variant returns the same shape.
        const COLUMNS: &str = "id, title, note, done, priority, due_at, list, position, created_at, completed_at";

        let conn = self.conn.lock();
        // "Today" is "open, and due before the end of today" — the boundary is
        // the only query that needs a parameter, so each variant is prepared
        // separately rather than binding a parameter no other SQL declares.
        let rows = match filter {
            TaskFilter::Today => {
                let sql = format!(
                    "SELECT {COLUMNS} FROM tasks
                     WHERE done = 0 AND (due_at IS NULL OR due_at < ?1)
                     ORDER BY position ASC, created_at ASC"
                );
                let mut stmt = conn.prepare(&sql)?;
                let end_of_today = end_of_today_millis();
                let mapped = stmt.query_map(params![end_of_today], row_to_task)?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
            TaskFilter::Open => {
                let sql = format!(
                    "SELECT {COLUMNS} FROM tasks WHERE done = 0
                     ORDER BY position ASC, created_at ASC"
                );
                let mut stmt = conn.prepare(&sql)?;
                let mapped = stmt.query_map([], row_to_task)?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
            TaskFilter::All => {
                let sql = format!(
                    "SELECT {COLUMNS} FROM tasks
                     ORDER BY done ASC, position ASC, created_at DESC"
                );
                let mut stmt = conn.prepare(&sql)?;
                let mapped = stmt.query_map([], row_to_task)?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
            TaskFilter::Done => {
                let sql = format!(
                    "SELECT {COLUMNS} FROM tasks WHERE done = 1 ORDER BY completed_at DESC"
                );
                let mut stmt = conn.prepare(&sql)?;
                let mapped = stmt.query_map([], row_to_task)?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// Open task count, for the launcher badge.
    pub fn open_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM tasks WHERE done = 0", [], |r| r.get(0))?)
    }

    pub fn update(&self, id: i64, patch: &TaskPatch) -> Result<Task> {
        let conn = self.conn.lock();
        let now = crate::now_millis();

        if let Some(title) = &patch.title {
            let title = title.trim();
            if title.is_empty() {
                return Err(StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "task title must not be empty",
                )));
            }
            conn.execute(
                "UPDATE tasks SET title = ?1 WHERE id = ?2",
                params![title, id],
            )?;
        }
        if let Some(note) = &patch.note {
            conn.execute("UPDATE tasks SET note = ?1 WHERE id = ?2", params![note, id])?;
        }
        if let Some(priority) = patch.priority {
            conn.execute(
                "UPDATE tasks SET priority = ?1 WHERE id = ?2",
                params![priority.as_i32(), id],
            )?;
        }
        if let Some(list) = &patch.list {
            conn.execute("UPDATE tasks SET list = ?1 WHERE id = ?2", params![list, id])?;
        }
        if let Some(due) = patch.due_at {
            conn.execute("UPDATE tasks SET due_at = ?1 WHERE id = ?2", params![due, id])?;
        }
        if let Some(done) = patch.done {
            // Completing a task moves it out of the ordering race: park it at
            // the top of the completed list and stamp the time.
            conn.execute(
                "UPDATE tasks SET done = ?1, completed_at = ?2 WHERE id = ?3",
                params![done as i32, if done { Some(now) } else { None }, id],
            )?;
            if !done {
                let min_pos: i64 = conn.query_row(
                    "SELECT COALESCE(MIN(position), 0) FROM tasks WHERE done = 0",
                    [],
                    |r| r.get(0),
                )?;
                conn.execute(
                    "UPDATE tasks SET position = ?1 WHERE id = ?2",
                    params![min_pos - 1, id],
                )?;
            }
        }
        Self::get_locked(&conn, id)
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Remove every completed task. Returns how many were removed.
    pub fn clear_completed(&self) -> Result<u32> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM tasks WHERE done = 1", [])? as u32)
    }

    /// Persist a new ordering. `ids` is top-to-bottom.
    pub fn reorder(&self, ids: &[i64]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for (i, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE tasks SET position = ?1 WHERE id = ?2",
                params![i as i64, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Move every still-open task from a previous day into today's list.
    ///
    /// Returns how many rows were touched so the caller can skip the UI
    /// notification when nothing changed.
    pub fn carry_over(&self) -> Result<u32> {
        let conn = self.conn.lock();
        let boundary = end_of_today_millis();
        let changed = conn.execute(
            "UPDATE tasks SET list = 'today' WHERE done = 0 AND list <> 'today' AND (due_at IS NULL OR due_at < ?1)",
            params![boundary],
        )?;
        Ok(changed as u32)
    }

    // -- export ------------------------------------------------------------

    /// GitHub-style task list.
    pub fn export_markdown(&self) -> Result<String> {
        let tasks = self.list(TaskFilter::All)?;
        let mut out = String::from("# WinBeautify 任务清单\n\n");
        let (open, done): (Vec<_>, Vec<_>) = tasks.into_iter().partition(|t| !t.done);

        out.push_str("## 待办\n\n");
        if open.is_empty() {
            out.push_str("_暂无待办_\n\n");
        }
        for t in &open {
            let mark = match t.priority {
                Priority::High => " **(高)**",
                Priority::Medium => " _(中)_",
                Priority::Low => " _(低)_",
                Priority::None => "",
            };
            out.push_str(&format!("- [ ] {}{mark}\n", t.title));
            if !t.note.is_empty() {
                for line in t.note.lines() {
                    out.push_str(&format!("      {line}\n"));
                }
            }
        }

        out.push_str("\n## 已完成\n\n");
        if done.is_empty() {
            out.push_str("_暂无_\n");
        }
        for t in &done {
            out.push_str(&format!("- [x] {}\n", t.title));
        }
        Ok(out)
    }

    pub fn export_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(&self.list(TaskFilter::All)?)?)
    }

    /// Replace the whole list with the contents of a JSON export.
    pub fn import_json(&self, json: &str) -> Result<u32> {
        let tasks: Vec<Task> = serde_json::from_str(json)?;
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM tasks", [])?;
        for t in &tasks {
            tx.execute(
                "INSERT INTO tasks (title, note, done, priority, due_at, list, position, created_at, completed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    t.title,
                    t.note,
                    t.done as i32,
                    t.priority.as_i32(),
                    t.due_at,
                    t.list,
                    t.position,
                    t.created_at,
                    t.completed_at
                ],
            )?;
        }
        tx.commit()?;
        Ok(tasks.len() as u32)
    }
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        title: row.get(1)?,
        note: row.get(2)?,
        done: row.get::<_, i32>(3)? != 0,
        priority: Priority::from_i32(row.get(4)?),
        due_at: row.get(5)?,
        list: row.get(6)?,
        position: row.get(7)?,
        created_at: row.get(8)?,
        completed_at: row.get(9)?,
    })
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Unix millis for the last instant of today, local time.
///
/// Computed from the system clock rather than the crate-local time zone so the
/// "today" filter matches whatever the user's clock says.
fn end_of_today_millis() -> i64 {
    let now = now_millis();
    let day = 24 * 60 * 60 * 1000i64;
    // Seconds since the epoch modulo a day is UTC midnight offset; the local
    // offset is subtracted so the boundary lands on local midnight.
    let local_offset_ms = local_utc_offset_millis();
    let since_local_midnight = (now + local_offset_ms).rem_euclid(day);
    now - since_local_midnight + day
}

#[cfg(windows)]
fn local_utc_offset_millis() -> i64 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    unsafe {
        let mut tz = TIME_ZONE_INFORMATION::default();
        let result = GetTimeZoneInformation(&mut tz);
        // TIME_ZONE_ID_INVALID == 0xFFFFFFFF
        if result == 0xFFFF_FFFF {
            return 0;
        }
        // `Bias` is minutes *west* of UTC, so negate it.
        -(tz.Bias as i64) * 60_000
    }
}

#[cfg(not(windows))]
fn local_utc_offset_millis() -> i64 {
    0
}

// ---------------------------------------------------------------------------
// Module wrapper
// ---------------------------------------------------------------------------

/// Owns the shared [`TaskStore`] for the host process.
pub struct TodoModule {
    store: parking_lot::RwLock<Option<Arc<TaskStore>>>,
    enabled: std::sync::atomic::AtomicBool,
    carry_over: std::sync::atomic::AtomicBool,
}

impl Default for TodoModule {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoModule {
    pub fn new() -> Self {
        use std::sync::atomic::AtomicBool;
        Self {
            store: parking_lot::RwLock::new(None),
            enabled: AtomicBool::new(false),
            carry_over: AtomicBool::new(true),
        }
    }

    /// Store shared with the UI layer, opening it on first use so the flyout
    /// works even if the module was disabled at startup.
    pub fn store(&self) -> Option<Arc<TaskStore>> {
        if let Some(existing) = self.store.read().clone() {
            return Some(existing);
        }
        let opened = Arc::new(TaskStore::open(&paths::database_path()).ok()?);
        *self.store.write() = Some(Arc::clone(&opened));
        Some(opened)
    }
}

impl Module for TodoModule {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn is_enabled(&self, config: &Config) -> bool {
        config.todo.enabled
    }

    fn start(&self, ctx: ModuleContext) -> ModuleResult {
        use std::sync::atomic::Ordering;
        let cfg = ctx.config.get();
        self.enabled.store(cfg.todo.enabled, Ordering::Release);
        self.carry_over.store(cfg.todo.carry_over, Ordering::Release);
        let store = Arc::new(TaskStore::open(&paths::database_path())?);
        if cfg.todo.carry_over {
            let moved = store.carry_over()?;
            if moved > 0 {
                tracing::info!(tasks = moved, "carried unfinished tasks into today");
            }
        }
        *self.store.write() = Some(store);
        Ok(())
    }

    fn apply(&self, config: &Config) -> ModuleResult {
        use std::sync::atomic::Ordering;
        self.enabled.store(config.todo.enabled, Ordering::Release);
        self.carry_over
            .store(config.todo.carry_over, Ordering::Release);
        if self.store.read().is_none() && config.todo.enabled {
            *self.store.write() = Some(Arc::new(TaskStore::open(&paths::database_path())?));
        }
        Ok(())
    }

    fn stop(&self) -> ModuleResult {
        // SQLite commits synchronously per statement, so there is nothing to
        // flush — dropping the connection is the whole teardown.
        *self.store.write() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_list_and_count() {
        let store = TaskStore::open_in_memory().unwrap();
        store.create("编写代码", "today").unwrap();
        store.create("学习 Rust", "today").unwrap();

        assert_eq!(store.open_count().unwrap(), 2);
        let rows = store.list(TaskFilter::Open).unwrap();
        // Newest first: position descends as tasks are added.
        assert_eq!(rows[0].title, "学习 Rust");
    }

    #[test]
    fn blank_titles_are_rejected() {
        let store = TaskStore::open_in_memory().unwrap();
        assert!(store.create("   ", "today").is_err());
        let t = store.create("ok", "today").unwrap();
        assert!(store
            .update(
                t.id,
                &TaskPatch {
                    title: Some("  ".into()),
                    ..Default::default()
                }
            )
            .is_err());
    }

    #[test]
    fn toggling_done_stamps_and_clears_the_completion_time() {
        let store = TaskStore::open_in_memory().unwrap();
        let t = store.create("ship it", "today").unwrap();
        assert!(t.completed_at.is_none());

        let done = store
            .update(
                t.id,
                &TaskPatch {
                    done: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(done.done);
        assert!(done.completed_at.is_some());

        let undone = store
            .update(
                t.id,
                &TaskPatch {
                    done: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!undone.done);
        assert!(undone.completed_at.is_none());
    }

    #[test]
    fn empty_patch_leaves_the_row_untouched() {
        let store = TaskStore::open_in_memory().unwrap();
        let t = store.create("original", "today").unwrap();
        let same = store.update(t.id, &TaskPatch::default()).unwrap();
        assert_eq!(same, t);
    }

    #[test]
    fn due_at_distinguishes_clear_from_untouched() {
        let store = TaskStore::open_in_memory().unwrap();
        let t = store.create("x", "today").unwrap();

        let with_due = store
            .update(
                t.id,
                &TaskPatch {
                    due_at: Some(Some(1234)),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(with_due.due_at, Some(1234));

        let cleared = store
            .update(
                t.id,
                &TaskPatch {
                    due_at: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(cleared.due_at, None);
    }

    #[test]
    fn reorder_is_reflected_in_the_list() {
        let store = TaskStore::open_in_memory().unwrap();
        let a = store.create("a", "today").unwrap();
        let b = store.create("b", "today").unwrap();
        store.reorder(&[a.id, b.id]).unwrap();

        let rows = store.list(TaskFilter::Open).unwrap();
        assert_eq!(rows[0].title, "a");
        assert_eq!(rows[1].title, "b");
    }

    #[test]
    fn clear_completed_keeps_open_tasks() {
        let store = TaskStore::open_in_memory().unwrap();
        let a = store.create("done", "today").unwrap();
        store.create("open", "today").unwrap();
        store
            .update(
                a.id,
                &TaskPatch {
                    done: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(store.clear_completed().unwrap(), 1);
        assert_eq!(store.list(TaskFilter::All).unwrap().len(), 1);
    }

    #[test]
    fn markdown_export_uses_github_task_syntax() {
        let store = TaskStore::open_in_memory().unwrap();
        store.create("编写代码", "today").unwrap();
        let md = store.export_markdown().unwrap();
        assert!(md.contains("- [ ] 编写代码"), "{md}");
    }

    #[test]
    fn json_round_trips() {
        let store = TaskStore::open_in_memory().unwrap();
        store.create("alpha", "today").unwrap();
        store.create("beta", "today").unwrap();
        let json = store.export_json().unwrap();

        let fresh = TaskStore::open_in_memory().unwrap();
        assert_eq!(fresh.import_json(&json).unwrap(), 2);
        assert_eq!(fresh.open_count().unwrap(), 2);
    }

    #[test]
    fn end_of_today_is_in_the_future_and_within_a_day() {
        let end = end_of_today_millis();
        let now = now_millis();
        assert!(end > now);
        assert!(end - now <= 24 * 60 * 60 * 1000);
    }
}
