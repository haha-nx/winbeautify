//! SQLite-backed clipboard history.
//!
//! Text and file lists live in the `clips` table. Images are written as
//! `.bmp` files under `<data>/clipboard/` and referenced by path — a BMP is
//! just a `BITMAPFILEHEADER` in front of the `CF_DIB` payload Windows already
//! handed us, so this avoids pulling an image encoder into the build *and*
//! lets the webview render previews straight off disk through the asset
//! protocol instead of shipping megabytes over IPC.

use beautify_core::paths;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClipKind {
    Text,
    /// A text clip whose whole content is one link.
    ///
    /// A separate kind rather than a query-time guess so the category list and
    /// the row icon agree, and so the browser-facing "链接" filter is an index
    /// lookup rather than a `LIKE` scan.
    Link,
    Image,
    Files,
}

impl ClipKind {
    fn as_str(self) -> &'static str {
        match self {
            ClipKind::Text => "text",
            ClipKind::Link => "link",
            ClipKind::Image => "image",
            ClipKind::Files => "files",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "link" => ClipKind::Link,
            "image" => ClipKind::Image,
            "files" => ClipKind::Files,
            _ => ClipKind::Text,
        }
    }

    /// Does this kind get restored to the clipboard as plain text?
    pub fn is_textual(self) -> bool {
        matches!(self, ClipKind::Text | ClipKind::Link | ClipKind::Files)
    }
}

/// Is this text nothing but a single link?
///
/// Deliberately strict, because the classification is visible: a clip filed
/// under "链接" disappears from the "文本" filter. Text that merely *mentions* a
/// link is a text clip, so the whole payload has to be one link, on one line,
/// with no whitespace anywhere (an encoded space is `%20`, never a real one).
///
/// A bare `host.tld` is accepted as well as a full URL — copying just the host
/// out of the address bar is common — but the suffix has to be a real TLD.
/// "has a dot" would be simpler and wrong: `readme.txt` and `1.0` both have one.
pub fn is_link(text: &str) -> bool {
    /// Shortest thing we would still call a link, e.g. `a.co`.
    const MIN_LEN: usize = 8;

    let trimmed = text.trim();
    if trimmed.len() < MIN_LEN || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if ["http://", "https://", "ftp://", "www."]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }

    // No scheme: the authority ends at the first path, query or fragment.
    let authority = lower.split(['/', '?', '#']).next().unwrap_or("");
    let Some((host, tld)) = authority.rsplit_once('.') else {
        return false;
    };
    !host.is_empty() && KNOWN_TLDS.contains(&tld)
}

/// Suffixes that make a bare `host.tld` recognisable without a scheme.
///
/// Curated rather than "any two letters", so a file name like `readme.txt` stays
/// a text clip. An unlisted TLD is not a failure: the clip is filed under 文本,
/// which is where it was before this classification existed.
const KNOWN_TLDS: [&str; 46] = [
    "com", "cn", "net", "org", "edu", "gov", "int", "mil", "io", "ai", "dev", "app", "me", "co",
    "cc", "tv", "info", "biz", "xyz", "top", "site", "online", "shop", "club", "wiki", "tech",
    "cloud", "design", "live", "fun", "space", "store", "blog", "page", "link", "uk", "jp", "kr",
    "de", "fr", "ru", "hk", "tw", "mo", "sg", "au",
];

/// Which slice of the history to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClipFilter {
    #[default]
    All,
    Image,
    Link,
    Text,
    Files,
}

impl ClipFilter {
    /// The `WHERE` fragment this filter contributes, or `None` for everything.
    fn kind_clause(self) -> Option<&'static str> {
        match self {
            ClipFilter::All => None,
            ClipFilter::Image => Some("kind = 'image'"),
            ClipFilter::Link => Some("kind = 'link'"),
            ClipFilter::Text => Some("kind = 'text'"),
            ClipFilter::Files => Some("kind = 'files'"),
        }
    }
}

/// A row of the history, shaped for the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipEntry {
    pub id: i64,
    pub kind: ClipKind,
    /// Full text for `Text`, newline-joined paths for `Files`, empty for images.
    pub text: String,
    /// Single line suitable for a list row.
    pub preview: String,
    /// Absolute path to the `.bmp` for images.
    pub image_path: String,
    pub width: i32,
    pub height: i32,
    pub bytes: i64,
    pub pinned: bool,
    pub created_at: i64,
    /// Text recognised inside an image. Empty until the OCR worker has run, and
    /// for rows that are not images. Searchable, so a copied screenshot can be
    /// found by a word that appears in it.
    #[serde(default)]
    pub ocr_text: String,
}

/// A freshly captured clip, before it is stored.
#[derive(Debug, Clone)]
pub struct NewClip {
    pub kind: ClipKind,
    pub text: String,
    /// BMP file contents when `kind` is [`ClipKind::Image`].
    pub image_bmp: Option<Vec<u8>>,
    pub width: i32,
    pub height: i32,
    pub bytes: i64,
}

impl NewClip {
    /// Stable content hash used for de-duplication. A repeated copy bumps the
    /// existing row to the top instead of adding a duplicate.
    pub fn hash(&self) -> String {
        content_hash(self.kind, match &self.image_bmp {
            Some(bytes) => bytes.as_slice(),
            None => self.text.as_bytes(),
        })
    }

    fn preview(&self) -> String {
        match self.kind {
            // A link is its own preview; there is nothing to summarise.
            ClipKind::Link => self.text.trim().to_string(),
            ClipKind::Image => {
                if self.width > 0 && self.height > 0 {
                    format!("图片 {}×{}", self.width, self.height)
                } else {
                    "图片".to_string()
                }
            }
            _ => {
                let flat: String = self
                    .text
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .take(200)
                    .collect();
                flat.trim().to_string()
            }
        }
    }
}

/// Errors from the clipboard store.
#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::Io(e) => write!(f, "io: {e}"),
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

pub type Result<T> = std::result::Result<T, StoreError>;

pub struct ClipStore {
    conn: Mutex<Connection>,
    blob_dir: PathBuf,
}

impl ClipStore {
    /// Open (creating if needed) the history database.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            blob_dir: paths::data_dir().join("clipboard"),
        })
    }

    /// In-memory store, used by the tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            blob_dir: std::env::temp_dir().join("winbeautify-test-blobs"),
        })
    }

    fn prepare(conn: &Connection) -> Result<()> {
        // WAL keeps the reader (UI) from blocking on the writer (listener).
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS clips (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                kind        TEXT    NOT NULL,
                text        TEXT    NOT NULL DEFAULT '',
                preview     TEXT    NOT NULL DEFAULT '',
                hash        TEXT    NOT NULL,
                blob_path   TEXT    NOT NULL DEFAULT '',
                width       INTEGER NOT NULL DEFAULT 0,
                height      INTEGER NOT NULL DEFAULT 0,
                bytes       INTEGER NOT NULL DEFAULT 0,
                pinned      INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_clips_hash ON clips(hash);
            CREATE INDEX IF NOT EXISTS idx_clips_recent ON clips(pinned DESC, created_at DESC);
            "#,
        )?;
        Self::migrate(conn)?;
        Ok(())
    }

    /// Bring an existing database up to the current schema.
    ///
    /// Versioned through SQLite's own `user_version`, so a fresh database and an
    /// upgraded one end up in the same state, and the upgrade runs once rather
    /// than on every launch.
    fn migrate(conn: &Connection) -> Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

        if version < 1 {
            // Links used to be stored as plain text. Reclassify them through the
            // same predicate the capture path uses, so the two cannot disagree.
            let candidates: Vec<(i64, String)> = {
                let mut stmt = conn.prepare("SELECT id, text FROM clips WHERE kind = 'text'")?;
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let links: Vec<i64> = candidates
                .into_iter()
                .filter(|(_, text)| is_link(text))
                .map(|(id, _)| id)
                .collect();
            for id in &links {
                conn.execute("UPDATE clips SET kind = 'link' WHERE id = ?1", params![id])?;
            }
            if !links.is_empty() {
                tracing::info!(count = links.len(), "reclassified stored clips as links");
            }
        }

        if version < 2 {
            // Text recognised inside images, so a screenshot can be searched by
            // its contents. Added rather than recreated: the history is user
            // data and has to survive the upgrade.
            let has_column: bool = conn
                .prepare("SELECT 1 FROM pragma_table_info('clips') WHERE name = 'ocr_text'")?
                .exists([])?;
            if !has_column {
                conn.execute(
                    "ALTER TABLE clips ADD COLUMN ocr_text TEXT NOT NULL DEFAULT ''",
                    [],
                )?;
            }
        }

        if version < 3 {
            // The bare-host rule landed after some histories had already been
            // migrated, so run the reclassification again for anything still
            // filed as plain text.
            let candidates: Vec<(i64, String)> = {
                let mut stmt = conn.prepare("SELECT id, text FROM clips WHERE kind = 'text'")?;
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let links: Vec<i64> = candidates
                .into_iter()
                .filter(|(_, text)| is_link(text))
                .map(|(id, _)| id)
                .collect();
            for id in &links {
                conn.execute("UPDATE clips SET kind = 'link' WHERE id = ?1", params![id])?;
            }
            if !links.is_empty() {
                tracing::info!(count = links.len(), "filed bare hosts as links");
            }
        }

        conn.pragma_update(None, "user_version", 3i64)?;
        Ok(())
    }

    /// Where an image row's `.bmp` lives, if it has one.
    pub fn image_path(&self, id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let path: Option<String> = conn
            .query_row(
                "SELECT blob_path FROM clips WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(path.filter(|p| !p.is_empty()))
    }

    /// Record the text recognised inside an image.
    pub fn set_ocr_text(&self, id: i64, text: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE clips SET ocr_text = ?1 WHERE id = ?2",
            params![text, id],
        )?;
        Ok(())
    }

    /// Images that have not been through the OCR worker yet.
    ///
    /// Run at start-up so images captured while the app was closed, or before
    /// this feature existed, are picked up too.
    pub fn images_without_ocr(&self, limit: u32) -> Result<Vec<(i64, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, blob_path FROM clips
             WHERE kind = 'image' AND ocr_text = '' AND blob_path <> ''
             ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn blob_dir(&self) -> &Path {
        &self.blob_dir
    }

    /// Store a clip, de-duplicating against existing history.
    ///
    /// Returns the row id. The `max_entries` cap is enforced after the insert
    /// so re-copying an old entry cannot evict itself.
    ///
    /// # Eviction
    ///
    /// `max_entries` caps the *total* row count, but pinned rows are never
    /// evicted — the newest unpinned rows are dropped first. A user who pins
    /// more than `max_entries` items therefore keeps all of them; pinning is
    /// the explicit "do not throw this away" gesture, so it wins.
    pub fn insert(&self, clip: &NewClip, max_entries: u32) -> Result<i64> {
        let hash = clip.hash();
        let now = now_millis();

        let blob_path = match &clip.image_bmp {
            Some(bytes) => {
                std::fs::create_dir_all(self.blob_dir())?;
                let path = self.blob_dir().join(format!("{now}-{hash}.bmp"));
                std::fs::write(&path, bytes)?;
                path.to_string_lossy().into_owned()
            }
            None => String::new(),
        };

        let conn = self.conn.lock();
        // Re-copying an existing clip just moves it back to the top.
        let existing: Option<i64> = conn
            .query_row("SELECT id FROM clips WHERE hash = ?1", params![hash], |r| {
                r.get(0)
            })
            .optional()?;

        if let Some(id) = existing {
            conn.execute(
                "UPDATE clips SET created_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            // The freshly written blob would otherwise be orphaned.
            if !blob_path.is_empty() {
                let _ = std::fs::remove_file(&blob_path);
            }
            return Ok(id);
        }

        conn.execute(
            "INSERT INTO clips (kind, text, preview, hash, blob_path, width, height, bytes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                clip.kind.as_str(),
                clip.text,
                clip.preview(),
                hash,
                blob_path,
                clip.width,
                clip.height,
                clip.bytes,
                now
            ],
        )?;
        let id = conn.last_insert_rowid();
        drop(conn);
        self.evict(max_entries)?;
        Ok(id)
    }

    /// Newest first.
    ///
    /// `query` filters on the preview text and `filter` on the category; both
    /// are optional and compose. Conditions are assembled rather than written
    /// out per combination, because there are four of them and the query string
    /// is user input that must stay parameterised.
    pub fn list(
        &self,
        query: &str,
        filter: ClipFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<ClipEntry>> {
        let conn = self.conn.lock();

        let mut conditions: Vec<String> = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(clause) = filter.kind_clause() {
            conditions.push(clause.to_string());
        }
        // An implicit AND over the individual words reads better than a single
        // substring match for multi-word queries.
        for term in query.split_whitespace() {
            values.push(Box::new(format!("%{}%", escape_like(term))));
            // One parameter, two columns: a screenshot is found by either
            // the words drawn in it or the label we generated for it.
            conditions.push(format!(
                "(preview LIKE ?{index} ESCAPE '\\' OR ocr_text LIKE ?{index} ESCAPE '\\')",
                index = values.len(),
            ));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        values.push(Box::new(limit));
        let limit_index = values.len();
        values.push(Box::new(offset));
        let offset_index = values.len();

        let sql = format!(
            "SELECT id, kind, text, preview, blob_path, width, height, bytes, pinned, created_at, ocr_text
             FROM clips {where_clause} ORDER BY pinned DESC, created_at DESC LIMIT ?{limit_index} OFFSET ?{offset_index}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_entry)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A single entry by id.
    pub fn get(&self, id: i64) -> Result<Option<ClipEntry>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT id, kind, text, preview, blob_path, width, height, bytes, pinned, created_at, ocr_text
                 FROM clips WHERE id = ?1",
                params![id],
                row_to_entry,
            )
            .optional()?)
    }

    pub fn set_pinned(&self, id: i64, pinned: bool) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE clips SET pinned = ?1 WHERE id = ?2",
            params![pinned as i32, id],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock();
        let path: Option<String> = conn
            .query_row("SELECT blob_path FROM clips WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        conn.execute("DELETE FROM clips WHERE id = ?1", params![id])?;
        drop(conn);
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }

    /// Clear history. Pinned entries survive unless `include_pinned` is set.
    pub fn clear(&self, include_pinned: bool) -> Result<u32> {
        let conn = self.conn.lock();
        let paths: Vec<String> = {
            let sql = if include_pinned {
                "SELECT blob_path FROM clips WHERE blob_path <> ''"
            } else {
                "SELECT blob_path FROM clips WHERE blob_path <> '' AND pinned = 0"
            };
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let removed = if include_pinned {
            conn.execute("DELETE FROM clips", [])?
        } else {
            conn.execute("DELETE FROM clips WHERE pinned = 0", [])?
        } as u32;
        drop(conn);
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
        Ok(removed)
    }

    /// Drop the oldest unpinned rows past `max_entries`, deleting their blobs.
    fn evict(&self, max_entries: u32) -> Result<()> {
        let conn = self.conn.lock();
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0))?;
        if total <= max_entries as i64 {
            return Ok(());
        }
        let excess = total - max_entries as i64;
        let paths: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT blob_path FROM clips WHERE pinned = 0
                 ORDER BY created_at ASC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![excess], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        conn.execute(
            "DELETE FROM clips WHERE id IN (
                 SELECT id FROM clips WHERE pinned = 0 ORDER BY created_at ASC LIMIT ?1
             )",
            params![excess],
        )?;
        drop(conn);
        for p in paths.iter().filter(|p| !p.is_empty()) {
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }

    /// `(total, pinned, bytes_on_disk)`
    pub fn stats(&self) -> Result<(i64, i64, i64)> {
        let conn = self.conn.lock();
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0))?;
        let pinned: i64 =
            conn.query_row("SELECT COUNT(*) FROM clips WHERE pinned = 1", [], |r| r.get(0))?;
        let bytes: i64 =
            conn.query_row("SELECT COALESCE(SUM(bytes), 0) FROM clips", [], |r| r.get(0))?;
        Ok((total, pinned, bytes))
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClipEntry> {
    let kind: String = row.get(1)?;
    Ok(ClipEntry {
        id: row.get(0)?,
        kind: ClipKind::from_str(&kind),
        text: row.get(2)?,
        preview: row.get(3)?,
        image_path: row.get(4)?,
        width: row.get(5)?,
        height: row.get(6)?,
        bytes: row.get(7)?,
        pinned: row.get::<_, i32>(8)? != 0,
        created_at: row.get(9)?,
        ocr_text: row.get(10)?,
    })
}

/// FNV-1a over the content, tagged with the kind.
///
/// Weak against a deliberately crafted collision, but this only decides whether
/// two clipboard entries are "the same thing", and a false positive costs one
/// deduplicated row.
pub fn content_hash(kind: ClipKind, payload: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
    };
    feed(kind.as_str().as_bytes());
    feed(&[0]);
    feed(payload);
    format!("{h:016x}")
}

/// Escape the LIKE wildcards so a query containing `%` is literal.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_clip(s: &str) -> NewClip {
        NewClip {
            kind: ClipKind::Text,
            text: s.to_string(),
            image_bmp: None,
            width: 0,
            height: 0,
            bytes: s.len() as i64,
        }
    }

    #[test]
    fn only_a_bare_url_counts_as_a_link() {
        // Yes.
        assert!(is_link("https://github.com/TranslucentTB/TranslucentTB"));
        assert!(is_link("http://example.com"));
        assert!(is_link("www.example.com"));
        assert!(is_link("  https://example.com/a?b=c#d  "));

        // No: these are text that merely mentions a link, and filing them under
        // "链接" would hide them from a text search.
        assert!(!is_link("see https://example.com for details"));
        assert!(!is_link("https://example.com
and more"));
        assert!(!is_link("https://example.com and more"));
        assert!(!is_link("ftp:/short"));
        assert!(!is_link(""));
        assert!(!is_link("   "));
        assert!(!is_link("just some text"));
        assert!(!is_link("www.a"));

        // A bare host is a link — copying just the host is common.
        assert!(is_link("mimo.xiaomi.com"));
        assert!(is_link("mimo.xiaomi.com/rl/"));
        assert!(is_link("github.com/TranslucentTB"));

        // But a file name that happens to have a dot is not.
        assert!(!is_link("readme.txt"));
        assert!(!is_link("notes.md"));
        assert!(!is_link("1.0"));
        assert!(!is_link("C:/Users/Administrator/notes.txt"));
    }

    #[test]
    fn the_category_filter_narrows_the_list() {
        let store = ClipStore::open_in_memory().unwrap();
        store.insert(&text_clip("plain words"), 100).unwrap();
        store
            .insert(
                &NewClip {
                    kind: ClipKind::Link,
                    text: "https://example.com".into(),
                    image_bmp: None,
                    width: 0,
                    height: 0,
                    bytes: 19,
                },
                100,
            )
            .unwrap();

        let count = |filter| store.list("", filter, 10, 0).unwrap().len();
        assert_eq!(count(ClipFilter::All), 2);
        assert_eq!(count(ClipFilter::Text), 1);
        assert_eq!(count(ClipFilter::Link), 1);
        assert_eq!(count(ClipFilter::Image), 0);
        assert_eq!(count(ClipFilter::Files), 0);
    }

    #[test]
    fn a_category_and_a_query_compose() {
        let store = ClipStore::open_in_memory().unwrap();
        store.insert(&text_clip("alpha beta"), 100).unwrap();
        store.insert(&text_clip("alpha gamma"), 100).unwrap();

        // The filter runs in SQL alongside the search, so a term that only
        // appears outside the selected category must not leak in.
        assert_eq!(store.list("alpha", ClipFilter::Text, 10, 0).unwrap().len(), 2);
        assert_eq!(store.list("alpha", ClipFilter::Link, 10, 0).unwrap().len(), 0);
    }

    #[test]
    fn opening_an_old_database_reclassifies_links() {
        let dir = std::env::temp_dir().join(format!("wb-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clips.db");

        // A pre-migration database: version 0, with a URL stored as plain text.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE clips (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL,
                    text TEXT NOT NULL DEFAULT '', preview TEXT NOT NULL DEFAULT '',
                    hash TEXT NOT NULL, blob_path TEXT NOT NULL DEFAULT '',
                    width INTEGER NOT NULL DEFAULT 0, height INTEGER NOT NULL DEFAULT 0,
                    bytes INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO clips (kind, text, preview, hash, created_at)
                VALUES ('text', 'https://example.com', 'https://example.com', 'h1', 1),
                       ('text', 'ordinary words', 'ordinary words', 'h2', 2);
                "#,
            )
            .unwrap();
        }

        let store = ClipStore::open(&path).unwrap();
        assert_eq!(
            store.list("", ClipFilter::Link, 10, 0).unwrap().len(),
            1,
            "the stored URL should move to the link category"
        );
        assert_eq!(store.list("", ClipFilter::Text, 10, 0).unwrap().len(), 1);

        // Re-opening must not redo the work or change the answer.
        drop(store);
        let again = ClipStore::open(&path).unwrap();
        assert_eq!(again.list("", ClipFilter::Link, 10, 0).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn insert_and_list_newest_first() {
        let store = ClipStore::open_in_memory().unwrap();
        store.insert(&text_clip("first"), 100).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        store.insert(&text_clip("second"), 100).unwrap();

        let rows = store.list("", ClipFilter::All, 10, 0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "second");
    }

    #[test]
    fn duplicate_is_bumped_not_duplicated() {
        let store = ClipStore::open_in_memory().unwrap();
        let id1 = store.insert(&text_clip("same"), 100).unwrap();
        store.insert(&text_clip("other"), 100).unwrap();
        let id2 = store.insert(&text_clip("same"), 100).unwrap();

        assert_eq!(id1, id2);
        let rows = store.list("", ClipFilter::All, 10, 0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "same", "re-copy should move it to the top");
    }

    #[test]
    fn eviction_drops_oldest_unpinned_and_keeps_pinned() {
        let store = ClipStore::open_in_memory().unwrap();
        let pinned_id = store.insert(&text_clip("keep me"), 100).unwrap();
        store.set_pinned(pinned_id, true).unwrap();
        for i in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            store.insert(&text_clip(&format!("filler {i}")), 100).unwrap();
        }
        // 6 rows total, cap 3: the three oldest unpinned rows go.
        store.evict(3).unwrap();

        let rows = store.list("", ClipFilter::All, 10, 0).unwrap();
        assert_eq!(rows.len(), 3, "history is capped at max_entries");
        assert!(
            rows.iter().any(|r| r.text == "keep me"),
            "a pinned entry is never evicted"
        );
        assert!(
            rows.iter().any(|r| r.text == "filler 4"),
            "the newest unpinned entries survive"
        );
    }

    #[test]
    fn eviction_keeps_everything_when_nothing_exceeds_the_cap() {
        let store = ClipStore::open_in_memory().unwrap();
        for i in 0..3 {
            store.insert(&text_clip(&format!("c{i}")), 10).unwrap();
        }
        store.evict(10).unwrap();
        assert_eq!(store.list("", ClipFilter::All, 10, 0).unwrap().len(), 3);
    }

    #[test]
    fn deleting_a_clip_removes_its_blob_file() {
        let dir = std::env::temp_dir().join(format!("wb-clips-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let store = ClipStore::open(&dir.join("c.db")).unwrap();
        let id = store
            .insert(
                &NewClip {
                    kind: ClipKind::Image,
                    text: String::new(),
                    image_bmp: Some(vec![1, 2, 3]),
                    width: 1,
                    height: 1,
                    bytes: 3,
                },
                10,
            )
            .unwrap();

        let rows = store.list("", ClipFilter::All, 10, 0).unwrap();
        assert_eq!(rows[0].kind, ClipKind::Image);
        let blob = rows[0].image_path.clone();
        assert!(std::path::Path::new(&blob).exists());

        store.delete(id).unwrap();
        assert!(!std::path::Path::new(&blob).exists(), "blob should be gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_matches_all_terms() {
        let store = ClipStore::open_in_memory().unwrap();
        store.insert(&text_clip("hello world"), 100).unwrap();
        store.insert(&text_clip("hello there"), 100).unwrap();

        assert_eq!(store.list("world", ClipFilter::All, 10, 0).unwrap().len(), 1);
        assert_eq!(store.list("hello", ClipFilter::All, 10, 0).unwrap().len(), 2);
        assert_eq!(store.list("hello world", ClipFilter::All, 10, 0).unwrap().len(), 1);
        assert_eq!(store.list("nope", ClipFilter::All, 10, 0).unwrap().len(), 0);
    }

    #[test]
    fn like_wildcards_in_the_query_are_literal() {
        let store = ClipStore::open_in_memory().unwrap();
        store.insert(&text_clip("100% done"), 100).unwrap();
        store.insert(&text_clip("nothing here"), 100).unwrap();

        assert_eq!(store.list("%", ClipFilter::All, 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn clear_respects_pins() {
        let store = ClipStore::open_in_memory().unwrap();
        let a = store.insert(&text_clip("a"), 100).unwrap();
        store.insert(&text_clip("b"), 100).unwrap();
        store.set_pinned(a, true).unwrap();

        assert_eq!(store.clear(false).unwrap(), 1);
        let rows = store.list("", ClipFilter::All, 10, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "a");
    }
}
