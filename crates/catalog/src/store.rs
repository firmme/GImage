use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gimage_core::{
    EditParams, EditSession, ImageFilter, ImageFormat, ImageInfo, ImageMeta, SortDirection,
    SortKey, SortSpec,
};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::CatalogError;
use crate::scan::{ScanOutput, ScannedFile};

/// Bumped whenever `SCHEMA_SQL` changes; stored in `PRAGMA user_version`.
pub const SCHEMA_VERSION: i64 = 1;

const IMAGE_COLUMNS: &str = "id, canonical_path, file_name, file_size, mtime_ns, width, height, \
                             format, orientation, favorite, missing";

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS folders (
    id            INTEGER PRIMARY KEY,
    canonical_path TEXT NOT NULL UNIQUE,
    last_scan_at  INTEGER
);

CREATE TABLE IF NOT EXISTS images (
    id             INTEGER PRIMARY KEY,
    folder_id      INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    canonical_path TEXT NOT NULL UNIQUE,
    file_name      TEXT NOT NULL,
    file_size      INTEGER NOT NULL,
    mtime_ns       INTEGER NOT NULL,
    width          INTEGER,
    height         INTEGER,
    format         TEXT NOT NULL,
    orientation    INTEGER NOT NULL DEFAULT 1,
    favorite       INTEGER NOT NULL DEFAULT 0,
    indexed_at     INTEGER NOT NULL,
    missing        INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_images_folder_id ON images(folder_id);
CREATE INDEX IF NOT EXISTS idx_images_mtime_ns ON images(mtime_ns);
CREATE INDEX IF NOT EXISTS idx_images_format ON images(format);
CREATE INDEX IF NOT EXISTS idx_images_favorite ON images(favorite);

CREATE TABLE IF NOT EXISTS tags (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS image_tags (
    image_id INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    tag_id   INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (image_id, tag_id)
);

CREATE INDEX IF NOT EXISTS idx_image_tags_tag_id ON image_tags(tag_id);

CREATE TABLE IF NOT EXISTS edit_sessions (
    image_id    INTEGER PRIMARY KEY REFERENCES images(id) ON DELETE CASCADE,
    params_json TEXT NOT NULL,
    version     INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderRecord {
    pub id: i64,
    pub path: PathBuf,
    pub last_scan_at_ms: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
}

#[derive(Clone, Debug, Default)]
pub struct QueryPage {
    pub items: Vec<ImageMeta>,
    pub total: u64,
}

#[derive(Clone, Debug, Default)]
pub struct DbStats {
    pub images: u64,
    pub folders: u64,
    pub favorites: u64,
    pub db_bytes: u64,
}

/// SQLite-backed index. Cheap queries only; never stores image bytes.
#[derive(Debug)]
pub struct Catalog {
    conn: rusqlite::Connection,
}

impl Catalog {
    /// Opens (creating if needed) the catalog, enables WAL and applies
    /// migrations idempotently.
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        configure(&conn)?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// In-memory database for tests.
    pub fn open_in_memory() -> Result<Self, CatalogError> {
        let mut conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    pub fn from_file_list(files: Vec<ScannedFile>, image_id_offset: i64) -> Result<(Self, i64), CatalogError> {
        let root = files.first().and_then(|file| file.path.parent())
            .ok_or_else(|| CatalogError::InvalidPath("没有可浏览的图片".into()))?;
        let mut catalog = Self::open_in_memory()?;
        let folder = catalog.upsert_folder(root)?;
        catalog.sync_scan(folder, &ScanOutput { files, ..ScanOutput::default() })?;
        catalog.conn.execute("UPDATE images SET id = id + ?1", [image_id_offset])?;
        Ok((catalog, folder))
    }

    pub fn upsert_folder(&self, path: &Path) -> Result<i64, CatalogError> {
        let canonical = fs::canonicalize(path).map_err(|_| {
            CatalogError::InvalidPath(format!("不是可访问的文件夹：{}", path.display()))
        })?;
        if !canonical.is_dir() {
            return Err(CatalogError::InvalidPath(format!(
                "不是文件夹：{}",
                path.display()
            )));
        }
        let canonical_path = path_to_string(&canonical)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO folders (canonical_path) VALUES (?1)",
            params![canonical_path],
        )?;
        let id = self.conn.query_row(
            "SELECT id FROM folders WHERE canonical_path = ?1",
            params![canonical_path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn touch_folder_scan(&self, folder_id: i64) -> Result<(), CatalogError> {
        self.conn.execute(
            "UPDATE folders SET last_scan_at = ?1 WHERE id = ?2",
            params![now_ms(), folder_id],
        )?;
        Ok(())
    }

    pub fn list_folders(&self) -> Result<Vec<FolderRecord>, CatalogError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, canonical_path, COALESCE(last_scan_at, 0) \
             FROM folders ORDER BY canonical_path COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| {
            let path: String = row.get(1)?;
            Ok(FolderRecord {
                id: row.get(0)?,
                path: PathBuf::from(path),
                last_scan_at_ms: row.get(2)?,
            })
        })?;
        let mut folders = Vec::new();
        for row in rows {
            folders.push(row?);
        }
        Ok(folders)
    }

    /// Diff the scan against the index: insert new, refresh changed mtime/size,
    /// mark removed paths as missing (do not delete rows; edit params survive).
    pub fn sync_folder(
        &mut self,
        folder_id: i64,
        files: &[ScannedFile],
    ) -> Result<SyncSummary, CatalogError> {
        self.sync_files(folder_id, files, true, None)
    }

    /// Only a complete scan can establish that an indexed file is missing.
    /// Partial scans still refresh discovered files and preserve all others.
    pub fn sync_scan(
        &mut self,
        folder_id: i64,
        output: &ScanOutput,
    ) -> Result<SyncSummary, CatalogError> {
        self.sync_files(folder_id, &output.files, output.is_complete(), output.scope.as_ref())
    }

    fn sync_files(
        &mut self,
        folder_id: i64,
        files: &[ScannedFile],
        mark_missing: bool,
        scope: Option<&(PathBuf, usize)>,
    ) -> Result<SyncSummary, CatalogError> {
        let now = now_ms();
        let tx = self.conn.transaction()?;

        // Snapshot this folder once, keyed by path; both the update decision and
        // the removal pass are then pure in-memory work.
        let mut existing: HashMap<String, ExistingRow> = HashMap::with_capacity(files.len());
        {
            let mut stmt = tx.prepare(
                "SELECT id, canonical_path, file_name, file_size, mtime_ns, format, missing \
                 FROM images WHERE folder_id = ?1",
            )?;
            let rows = stmt.query_map(params![folder_id], |row| {
                let path: String = row.get(1)?;
                let state = ExistingRow {
                    id: row.get(0)?,
                    file_name: row.get(2)?,
                    file_size: row.get(3)?,
                    mtime_ns: row.get(4)?,
                    format: row.get(5)?,
                    missing: row.get::<_, i64>(6)? != 0,
                };
                Ok((path, state))
            })?;
            for row in rows {
                let (path, state) = row?;
                existing.insert(path, state);
            }
        }

        let mut summary = SyncSummary::default();
        let mut seen: HashSet<&str> = HashSet::with_capacity(files.len());
        {
            // A conflicting canonical_path means the file moved between indexed
            // folders; the upsert claims it for the folder being scanned.
            let mut insert_stmt = tx.prepare(
                "INSERT INTO images \
                 (folder_id, canonical_path, file_name, file_size, mtime_ns, format, indexed_at, missing) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0) \
                 ON CONFLICT(canonical_path) DO UPDATE SET \
                     folder_id = excluded.folder_id, \
                     file_name = excluded.file_name, \
                     file_size = excluded.file_size, \
                     mtime_ns = excluded.mtime_ns, \
                     format = excluded.format, \
                     indexed_at = excluded.indexed_at, \
                     missing = 0",
            )?;
            let mut update_stmt = tx.prepare(
                "UPDATE images SET file_name = ?1, file_size = ?2, mtime_ns = ?3, \
                 format = ?4, missing = 0 WHERE id = ?5",
            )?;

            for file in files {
                let path = file.path.to_str().ok_or_else(|| {
                    CatalogError::InvalidPath(format!(
                        "无法索引非 UTF-8 路径：{}",
                        file.path.display()
                    ))
                })?;
                // A malformed scan can repeat a path; index it once.
                if !seen.insert(path) {
                    continue;
                }
                let file_name = file
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        CatalogError::InvalidPath(format!(
                            "无法索引缺少文件名的路径：{}",
                            file.path.display()
                        ))
                    })?;
                let file_size = i64::try_from(file.size_bytes).unwrap_or(i64::MAX);
                let format = file.format.as_str();

                match existing.get(path) {
                    Some(current) => {
                        let changed = current.missing
                            || current.file_name != file_name
                            || current.file_size != file_size
                            || current.mtime_ns != file.modified_ns
                            || current.format != format;
                        if changed {
                            update_stmt.execute(params![
                                file_name,
                                file_size,
                                file.modified_ns,
                                format,
                                current.id
                            ])?;
                            summary.updated = summary.updated.saturating_add(1);
                        }
                    }
                    None => {
                        insert_stmt.execute(params![
                            folder_id,
                            path,
                            file_name,
                            file_size,
                            file.modified_ns,
                            format,
                            now
                        ])?;
                        summary.added = summary.added.saturating_add(1);
                    }
                }
            }
        }

        if mark_missing {
            let mut remove_stmt =
                tx.prepare("UPDATE images SET missing = 1 WHERE id = ?1 AND missing = 0")?;
            for (path, current) in &existing {
                if let Some((root, depth)) = scope
                    && !Path::new(path).strip_prefix(root)
                        .is_ok_and(|relative| relative.components().count().saturating_sub(1) <= *depth)
                {
                    continue;
                }
                if !seen.contains(path.as_str()) && remove_stmt.execute(params![current.id])? > 0 {
                    summary.removed = summary.removed.saturating_add(1);
                }
            }
        }

        tx.commit()?;
        Ok(summary)
    }

    pub fn images_page(
        &self,
        folder_id: i64,
        sort: &SortSpec,
        filter: &ImageFilter,
        offset: u32,
        limit: u32,
    ) -> Result<QueryPage, CatalogError> {
        let (predicate, filter_params) = filter_predicate(folder_id, filter);
        let total = self.count_with(&predicate, &filter_params)?;

        let limit = limit.clamp(1, 1000);
        let sql = format!(
            "SELECT {IMAGE_COLUMNS} FROM images WHERE {predicate} ORDER BY {} LIMIT ? OFFSET ?",
            order_by(sort)
        );
        let mut page_params = filter_params;
        page_params.push(Value::Integer(i64::from(limit)));
        page_params.push(Value::Integer(i64::from(offset)));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(page_params.iter()), map_image)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(QueryPage { items, total })
    }

    pub fn count_images(&self, folder_id: i64, filter: &ImageFilter) -> Result<u64, CatalogError> {
        let (predicate, filter_params) = filter_predicate(folder_id, filter);
        self.count_with(&predicate, &filter_params)
    }

    pub fn image_ids(&self, folder_id: i64, filter: &ImageFilter) -> Result<Vec<i64>, CatalogError> {
        let (predicate, params) = filter_predicate(folder_id, filter);
        let sql = format!("SELECT id FROM images WHERE {predicate}");
        let mut stmt = self.conn.prepare(&sql)?;
        let ids = stmt.query_map(params_from_iter(params.iter()), |row| row.get(0))?;
        Ok(ids.collect::<Result<_, _>>()?)
    }

    pub fn get_image(&self, id: i64) -> Result<Option<ImageMeta>, CatalogError> {
        let sql = format!("SELECT {IMAGE_COLUMNS} FROM images WHERE id = ?1");
        let image = self
            .conn
            .prepare_cached(&sql)?
            .query_row(params![id], map_image)
            .optional()?;
        Ok(image)
    }

    pub fn image_at_path(&self, path: &Path) -> Result<Option<ImageMeta>, CatalogError> {
        let sql = format!("SELECT {IMAGE_COLUMNS} FROM images WHERE canonical_path = ?1 AND missing = 0");
        Ok(self.conn.query_row(&sql, params![path.to_string_lossy()], map_image).optional()?)
    }

    pub fn folder_formats(&self, folder_id: i64) -> Result<Vec<ImageFormat>, CatalogError> {
        self.folder_formats_at_depth(folder_id, None)
    }

    pub fn folder_formats_at_depth(&self, folder_id: i64, depth: Option<u8>) -> Result<Vec<ImageFormat>, CatalogError> {
        let (predicate, params) = filter_predicate(folder_id, &ImageFilter {
            max_directory_depth: depth, ..Default::default()
        });
        let mut stmt = self.conn.prepare(&format!("SELECT DISTINCT format FROM images WHERE {predicate} ORDER BY format"))?;
        let rows = stmt.query_map(params_from_iter(params.iter()), |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(decode_format(&row?))).collect()
    }

    pub fn image_position(
        &self,
        folder_id: i64,
        sort: &SortSpec,
        filter: &ImageFilter,
        id: i64,
    ) -> Result<Option<u64>, CatalogError> {
        let (predicate, mut params) = filter_predicate(folder_id, filter);
        let key = sort_expression(sort.key);
        let anchor = format!("SELECT {key} FROM images WHERE {predicate} AND id = ?");
        let mut anchor_params = params.clone();
        anchor_params.push(Value::Integer(id));
        let value = self.conn.query_row(&anchor, params_from_iter(&anchor_params), |row| {
            row.get::<_, Value>(0)
        }).optional()?;
        let Some(value) = value else { return Ok(None); };
        let comparator = if sort.direction == SortDirection::Ascending { "<" } else { ">" };
        let before = format!("{predicate} AND ({key} {comparator} ? OR ({key} = ? AND id {comparator} ?))");
        params.extend([value.clone(), value, Value::Integer(id)]);
        Ok(Some(self.count_with(&before, &params)? + 1))
    }

    /// Previous/next image in the current sort order; `delta` is -1 or 1.
    pub fn neighbor(
        &self,
        folder_id: i64,
        sort: &SortSpec,
        filter: &ImageFilter,
        id: i64,
        delta: i32,
    ) -> Result<Option<ImageMeta>, CatalogError> {
        if delta == 0 {
            return Ok(None);
        }
        let (predicate, filter_params) = filter_predicate(folder_id, filter);
        let key_expr = sort_expression(sort.key);

        // Anchor row: if `id` is outside the filtered set there is no neighbour.
        let anchor_sql = format!("SELECT {key_expr} FROM images WHERE {predicate} AND id = ?");
        let mut anchor_params = filter_params.clone();
        anchor_params.push(Value::Integer(id));
        let anchor = self
            .conn
            .query_row(&anchor_sql, params_from_iter(anchor_params.iter()), |row| {
                row.get::<_, Value>(0)
            })
            .optional()?;
        let Some(anchor) = anchor else {
            return Ok(None);
        };

        // Walk forward or backward along the *display* order. For a descending
        // sort the comparison flips, and with it the subquery's ORDER BY.
        let ascending = sort.direction == SortDirection::Ascending;
        let forward = delta > 0;
        let greater = if ascending { forward } else { !forward };
        let comparator = if greater { ">" } else { "<" };
        let scan_direction = if greater { "ASC" } else { "DESC" };

        let sql = format!(
            "SELECT {IMAGE_COLUMNS} FROM images WHERE {predicate} \
             AND (({key_expr}) {comparator} ? OR (({key_expr}) = ? AND id {comparator} ?)) \
             ORDER BY {key_expr} {scan_direction}, id {scan_direction} LIMIT 1"
        );
        let mut neighbor_params = filter_params;
        neighbor_params.push(anchor.clone());
        neighbor_params.push(anchor);
        neighbor_params.push(Value::Integer(id));

        let image = self
            .conn
            .query_row(&sql, params_from_iter(neighbor_params.iter()), map_image)
            .optional()?;
        Ok(image)
    }

    pub fn update_probe(&self, id: i64, info: &ImageInfo) -> Result<(), CatalogError> {
        self.conn.execute(
            "UPDATE images SET width = ?1, height = ?2, orientation = ?3, format = ?4, \
             missing = 0 WHERE id = ?5",
            params![
                i64::from(info.width),
                i64::from(info.height),
                i64::from(info.orientation),
                info.format.as_str(),
                id
            ],
        )?;
        Ok(())
    }

    pub fn set_favorite(&self, id: i64, favorite: bool) -> Result<(), CatalogError> {
        self.conn.execute(
            "UPDATE images SET favorite = ?1 WHERE id = ?2",
            params![i64::from(favorite), id],
        )?;
        Ok(())
    }

    pub fn set_missing(&self, id: i64, missing: bool) -> Result<(), CatalogError> {
        self.conn.execute(
            "UPDATE images SET missing = ?1 WHERE id = ?2",
            params![i64::from(missing), id],
        )?;
        Ok(())
    }

    pub fn load_edit_session(&self, id: i64) -> Result<EditSession, CatalogError> {
        let row = self
            .conn
            .query_row(
                "SELECT params_json, version, updated_at FROM edit_sessions WHERE image_id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((json, version, updated_at)) = row else {
            return Ok(EditSession::new());
        };
        Ok(EditSession {
            params: EditParams::from_json(&json),
            revision: u32::try_from(version).unwrap_or(0),
            updated_at_ms: updated_at,
        })
    }

    pub fn save_edit_session(
        &self,
        id: i64,
        params: &EditParams,
        revision: u32,
    ) -> Result<(), CatalogError> {
        let json = params.normalized().to_json();
        self.conn.execute(
            "INSERT INTO edit_sessions (image_id, params_json, version, updated_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(image_id) DO UPDATE SET \
                 params_json = excluded.params_json, \
                 version = excluded.version, \
                 updated_at = excluded.updated_at",
            params![id, json, i64::from(revision), now_ms()],
        )?;
        Ok(())
    }

    pub fn clear_edit_session(&self, id: i64) -> Result<(), CatalogError> {
        self.conn.execute("DELETE FROM edit_sessions WHERE image_id = ?1", [id])?;
        Ok(())
    }

    pub fn db_stats(&self) -> Result<DbStats, CatalogError> {
        let images: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM images", [], |row| row.get(0))?;
        let folders: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM folders", [], |row| row.get(0))?;
        let favorites: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM images WHERE favorite = 1",
            [],
            |row| row.get(0),
        )?;
        let page_count: i64 = self
            .conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let page_size: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok(DbStats {
            images: images.max(0) as u64,
            folders: folders.max(0) as u64,
            favorites: favorites.max(0) as u64,
            db_bytes: (page_count.max(0) as u64).saturating_mul(page_size.max(0) as u64),
        })
    }

    /// All indexed formats, for the format filter menu.
    pub fn distinct_formats(&self) -> Result<Vec<ImageFormat>, CatalogError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT format FROM images ORDER BY format COLLATE NOCASE")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut formats = Vec::new();
        for row in rows {
            let stored = row?;
            let format = decode_format(&stored);
            if !formats.contains(&format) {
                formats.push(format);
            }
        }
        Ok(formats)
    }

    fn count_with(&self, predicate: &str, params: &[Value]) -> Result<u64, CatalogError> {
        let sql = format!("SELECT COUNT(*) FROM images WHERE {predicate}");
        let count: i64 = self
            .conn
            .query_row(&sql, params_from_iter(params.iter()), |row| row.get(0))?;
        Ok(count.max(0) as u64)
    }
}

/// One existing row of the folder being synced.
struct ExistingRow {
    id: i64,
    file_name: String,
    file_size: i64,
    mtime_ns: i64,
    format: String,
    missing: bool,
}

fn configure(conn: &Connection) -> Result<(), CatalogError> {
    // WAL keeps reads cheap while the background scanner writes. In-memory
    // databases answer "memory" here, which is fine.
    conn.query_row("PRAGMA journal_mode = WAL", [], |row| {
        row.get::<_, String>(0)
    })?;
    conn.pragma_update(None, "synchronous", 1i64)?; // NORMAL
    conn.pragma_update(None, "foreign_keys", 1i64)?;
    conn.pragma_update(None, "temp_store", 2i64)?; // MEMORY
    conn.pragma_update(None, "busy_timeout", 5_000i64)?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<(), CatalogError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(CatalogError::Migration(format!(
            "数据库结构版本 {version} 高于本程序支持的版本 {SCHEMA_VERSION}"
        )));
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.transaction()?;
    tx.execute_batch(SCHEMA_SQL)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis().min(i64::MAX as u128) as i64,
        Err(_) => 0,
    }
}

fn path_to_string(path: &Path) -> Result<String, CatalogError> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        CatalogError::InvalidPath(format!("无法索引非 UTF-8 路径：{}", path.display()))
    })
}

fn decode_format(stored: &str) -> ImageFormat {
    ImageFormat::from_extension(stored).unwrap_or_else(|| ImageFormat::Other(stored.to_owned()))
}

fn map_image(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImageMeta> {
    let canonical_path: String = row.get(1)?;
    let path = PathBuf::from(&canonical_path);
    let file_name: String = row.get(2)?;
    let size_bytes: i64 = row.get(3)?;
    let mtime_ns: i64 = row.get(4)?;
    let width: Option<i64> = row.get(5)?;
    let height: Option<i64> = row.get(6)?;
    let stored_format: String = row.get(7)?;
    let orientation: i64 = row.get(8)?;
    let favorite: i64 = row.get(9)?;
    let missing: i64 = row.get(10)?;
    Ok(ImageMeta {
        id: row.get(0)?,
        dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
        path,
        file_name,
        size_bytes: size_bytes.max(0) as u64,
        modified_ns: mtime_ns,
        width: width.and_then(|value| u32::try_from(value).ok()),
        height: height.and_then(|value| u32::try_from(value).ok()),
        format: decode_format(&stored_format),
        orientation: u16::try_from(orientation).unwrap_or(1),
        favorite: favorite != 0,
        missing: missing != 0,
    })
}

fn sort_expression(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "file_name COLLATE NOCASE",
        SortKey::Modified => "mtime_ns",
        SortKey::Size => "file_size",
        SortKey::Format => "format COLLATE NOCASE",
    }
}

fn order_by(sort: &SortSpec) -> String {
    let direction = match sort.direction {
        SortDirection::Ascending => "ASC",
        SortDirection::Descending => "DESC",
    };
    format!("{} {direction}, id {direction}", sort_expression(sort.key))
}

/// Escape user text for `LIKE ... ESCAPE '\'` so `%` and `_` are literals.
fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Build the shared WHERE clause. Every user value is bound, never interpolated.
fn filter_predicate(folder_id: i64, filter: &ImageFilter) -> (String, Vec<Value>) {
    let mut clauses = vec!["folder_id = ?".to_string()];
    let mut params = vec![Value::Integer(folder_id)];
    if let Some(depth) = filter.max_directory_depth {
        let relative = "ltrim(substr(images.canonical_path, length((SELECT canonical_path FROM folders WHERE id = images.folder_id)) + 1), '/\\')";
        clauses.push(format!("length({relative}) - length(replace(replace({relative}, '/', ''), char(92), '')) <= ?"));
        params.push(Value::Integer(i64::from(depth)));
    }

    let query = filter.query.trim();
    if !query.is_empty() {
        clauses.push("file_name LIKE ? ESCAPE '\\'".to_string());
        params.push(Value::Text(format!("%{}%", escape_like(query))));
    }

    if !filter.formats.is_empty() {
        let placeholders = std::iter::repeat_n("?", filter.formats.len())
            .collect::<Vec<_>>()
            .join(", ");
        clauses.push(format!("format IN ({placeholders})"));
        params.extend(
            filter
                .formats
                .iter()
                .map(|format| Value::Text(format.as_str().to_owned())),
        );
    }

    if filter.favorites_only {
        clauses.push("favorite = 1".to_string());
    }
    if !filter.include_missing {
        clauses.push("missing = 0".to_string());
    }

    (clauses.join(" AND "), params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan_folder};
    use gimage_core::{CancellationToken, Rotation};
    use std::time::Duration;

    fn write_file(path: &Path, len: usize) -> PathBuf {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, vec![0u8; len]).unwrap();
        path.to_path_buf()
    }

    fn new_catalog() -> (tempfile::TempDir, Catalog) {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(&tmp.path().join("catalog.db")).unwrap();
        (tmp, catalog)
    }

    fn index_folder(catalog: &mut Catalog, root: &Path) -> (i64, SyncSummary) {
        let scanned = scan_folder(
            root,
            &ScanOptions::default(),
            &CancellationToken::new(),
            |_| {},
        )
        .unwrap();
        assert!(!scanned.canceled);
        let folder_id = catalog.upsert_folder(root).unwrap();
        let summary = catalog.sync_folder(folder_id, &scanned.files).unwrap();
        (folder_id, summary)
    }

    fn name_sort(direction: SortDirection) -> SortSpec {
        SortSpec {
            key: SortKey::Name,
            direction,
        }
    }

    fn page_names(page: &QueryPage) -> Vec<String> {
        page.items
            .iter()
            .map(|image| image.file_name.clone())
            .collect()
    }

    fn page(
        catalog: &Catalog,
        folder_id: i64,
        sort: &SortSpec,
        filter: &ImageFilter,
        offset: u32,
        limit: u32,
    ) -> QueryPage {
        catalog
            .images_page(folder_id, sort, filter, offset, limit)
            .unwrap()
    }

    fn image_id(catalog: &Catalog, folder_id: i64, file_name: &str) -> i64 {
        catalog
            .images_page(
                folder_id,
                &name_sort(SortDirection::Ascending),
                &ImageFilter::default(),
                0,
                1000,
            )
            .unwrap()
            .items
            .into_iter()
            .find(|image| image.file_name == file_name)
            .unwrap_or_else(|| panic!("missing indexed image {file_name}"))
            .id
    }

    #[test]
    fn partial_scans_preserve_unseen_images_and_saved_edits() {
        for reason in ["canceled", "truncated", "unreadable"] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            write_file(&root.join("one.jpg"), 1);
            let other = write_file(&root.join("two.png"), 1);
            let mut catalog = Catalog::open_in_memory().unwrap();
            let (folder_id, _) = index_folder(&mut catalog, root);
            let other_id = image_id(&catalog, folder_id, "two.png");
            catalog.set_favorite(other_id, true).unwrap();
            let edits = EditParams {
                rotation: Rotation::Cw90,
                ..EditParams::default()
            };
            catalog.save_edit_session(other_id, &edits, 1).unwrap();

            fs::remove_file(&other).unwrap();
            write_file(&root.join("new.jpg"), 1);
            let mut output = scan_folder(
                root,
                &ScanOptions::default(),
                &CancellationToken::new(),
                |_| {},
            )
            .unwrap();
            match reason {
                "canceled" => output.canceled = true,
                "truncated" => output.truncated = true,
                _ => output.errors = 1,
            }
            let summary = catalog.sync_scan(folder_id, &output).unwrap();
            assert_eq!(summary.added, 1, "{reason}");
            assert_eq!(summary.removed, 0, "{reason}");
            let unseen = catalog.get_image(other_id).unwrap().unwrap();
            assert!(!unseen.missing, "{reason}");
            assert!(unseen.favorite);
            assert_eq!(
                catalog.load_edit_session(other_id).unwrap().params.rotation,
                Rotation::Cw90
            );

            // A later successful full scan can mark a truly deleted file missing.
            output.canceled = false;
            output.truncated = false;
            output.errors = 0;
            assert_eq!(catalog.sync_scan(folder_id, &output).unwrap().removed, 1);
            assert!(catalog.get_image(other_id).unwrap().unwrap().missing);
        }
    }

    #[test]
    fn capped_scan_does_not_hide_existing_catalog_entries() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..3 {
            write_file(&tmp.path().join(format!("{i}.jpg")), 1);
        }
        let mut catalog = Catalog::open_in_memory().unwrap();
        let (folder_id, _) = index_folder(&mut catalog, tmp.path());
        let options = ScanOptions {
            max_files: 1,
            ..ScanOptions::default()
        };
        let output = scan_folder(tmp.path(), &options, &CancellationToken::new(), |_| {}).unwrap();
        assert!(output.truncated);
        assert_eq!(catalog.sync_scan(folder_id, &output).unwrap().removed, 0);
        assert_eq!(
            catalog
                .count_images(folder_id, &ImageFilter::default())
                .unwrap(),
            3
        );
    }

    #[test]
    fn migration_is_idempotent_and_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("nested/catalog.db");

        {
            let catalog = Catalog::open(&db_path).unwrap();
            let version: i64 = catalog
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION);
            catalog.upsert_folder(tmp.path()).unwrap();
        }

        // Re-opening the same file must not fail on the already-applied schema.
        let catalog = Catalog::open(&db_path).unwrap();
        let version: i64 = catalog
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(catalog.db_stats().unwrap().folders, 1);
    }

    #[test]
    fn in_memory_catalog_opens() {
        let catalog = Catalog::open_in_memory().unwrap();
        let stats = catalog.db_stats().unwrap();
        assert_eq!(stats.images, 0);
        assert_eq!(stats.folders, 0);
        assert_eq!(stats.favorites, 0);
        assert!(catalog.distinct_formats().unwrap().is_empty());
    }

    #[test]
    fn upsert_folder_is_idempotent_and_validates() {
        let (tmp, catalog) = new_catalog();

        let first = catalog.upsert_folder(tmp.path()).unwrap();
        let second = catalog.upsert_folder(tmp.path()).unwrap();
        assert_eq!(first, second);

        catalog.touch_folder_scan(first).unwrap();
        let folders = catalog.list_folders().unwrap();
        assert_eq!(folders.len(), 1);
        assert!(folders[0].last_scan_at_ms > 0);
        assert_eq!(folders[0].path, fs::canonicalize(tmp.path()).unwrap());

        let missing = tmp.path().join("missing");
        assert!(matches!(
            catalog.upsert_folder(&missing),
            Err(CatalogError::InvalidPath(_))
        ));

        let file = write_file(&tmp.path().join("not-a-dir.jpg"), 1);
        assert!(matches!(
            catalog.upsert_folder(&file),
            Err(CatalogError::InvalidPath(_))
        ));
    }

    #[test]
    fn sync_adds_updates_and_marks_missing() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        let a = write_file(&root.join("a.jpg"), 10);
        write_file(&root.join("b.png"), 20);

        let (folder_id, first) = index_folder(&mut catalog, &root);
        assert_eq!(
            first,
            SyncSummary {
                added: 2,
                updated: 0,
                removed: 0
            }
        );

        let (_, second) = index_folder(&mut catalog, &root);
        assert_eq!(second, SyncSummary::default());

        let a_id = image_id(&catalog, folder_id, "a.jpg");

        // Favourite + edit session must survive every later sync.
        catalog.set_favorite(a_id, true).unwrap();
        let session = EditParams {
            exposure: 0.5,
            ..EditParams::default()
        };
        catalog.save_edit_session(a_id, &session, 4).unwrap();

        // Modification is detected through mtime alone (size is unchanged).
        let handle = fs::OpenOptions::new().write(true).open(&a).unwrap();
        handle
            .set_modified(SystemTime::now() + Duration::from_secs(120))
            .unwrap();
        drop(handle);

        let (_, third) = index_folder(&mut catalog, &root);
        assert_eq!(
            third,
            SyncSummary {
                added: 0,
                updated: 1,
                removed: 0
            }
        );

        let meta = catalog.get_image(a_id).unwrap().unwrap();
        assert!(meta.favorite);
        assert!(!meta.missing);
        assert_eq!(meta.path, fs::canonicalize(&a).unwrap());

        // Removing from disk keeps the row but flags it missing; dependent
        // state (favourites, edit sessions) remains loadable.
        fs::remove_file(&a).unwrap();
        let (_, fourth) = index_folder(&mut catalog, &root);
        assert_eq!(
            fourth,
            SyncSummary {
                added: 0,
                updated: 0,
                removed: 1
            }
        );

        let meta = catalog.get_image(a_id).unwrap().unwrap();
        assert!(meta.missing);
        assert!(meta.favorite);

        let loaded = catalog.load_edit_session(a_id).unwrap();
        assert_eq!(loaded.revision, 4);
        assert_eq!(loaded.params, session.normalized());

        // Already-missing rows are not counted as removed again.
        let (_, fifth) = index_folder(&mut catalog, &root);
        assert_eq!(fifth, SyncSummary::default());
        assert_eq!(catalog.db_stats().unwrap().images, 2);
    }

    #[test]
    fn sync_clears_missing_when_the_file_returns() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        let a = write_file(&root.join("a.jpg"), 10);

        let (folder_id, _) = index_folder(&mut catalog, &root);
        let a_id = image_id(&catalog, folder_id, "a.jpg");

        fs::remove_file(&a).unwrap();
        let (_, removed) = index_folder(&mut catalog, &root);
        assert_eq!(removed.removed, 1);
        assert!(catalog.get_image(a_id).unwrap().unwrap().missing);

        write_file(&a, 10);
        let (_, restored) = index_folder(&mut catalog, &root);
        assert_eq!(
            restored,
            SyncSummary {
                added: 0,
                updated: 1,
                removed: 0
            }
        );
        assert!(!catalog.get_image(a_id).unwrap().unwrap().missing);
    }

    #[test]
    fn paging_sorting_and_filtering() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        write_file(&root.join("alpha.jpg"), 50);
        write_file(&root.join("Beta.png"), 100);
        write_file(&root.join("gamma.JPG"), 150);
        write_file(&root.join("delta_1.bmp"), 200);
        write_file(&root.join("epsilon.webp"), 250);

        let (folder_id, summary) = index_folder(&mut catalog, &root);
        assert_eq!(summary.added, 5);

        let asc = page(
            &catalog,
            folder_id,
            &name_sort(SortDirection::Ascending),
            &ImageFilter::default(),
            0,
            100,
        );
        assert_eq!(asc.total, 5);
        assert_eq!(
            page_names(&asc),
            vec![
                "alpha.jpg",
                "Beta.png",
                "delta_1.bmp",
                "epsilon.webp",
                "gamma.JPG"
            ]
        );

        let desc = page(
            &catalog,
            folder_id,
            &name_sort(SortDirection::Descending),
            &ImageFilter::default(),
            0,
            100,
        );
        assert_eq!(
            page_names(&desc),
            vec![
                "gamma.JPG",
                "epsilon.webp",
                "delta_1.bmp",
                "Beta.png",
                "alpha.jpg"
            ]
        );

        let size_asc = page(
            &catalog,
            folder_id,
            &SortSpec {
                key: SortKey::Size,
                direction: SortDirection::Ascending,
            },
            &ImageFilter::default(),
            0,
            100,
        );
        assert_eq!(page_names(&size_asc)[0], "alpha.jpg");
        assert_eq!(page_names(&size_asc)[4], "epsilon.webp");

        // Paging keeps the filtered total, not the page length.
        let page_two = page(
            &catalog,
            folder_id,
            &name_sort(SortDirection::Ascending),
            &ImageFilter::default(),
            2,
            2,
        );
        assert_eq!(page_two.total, 5);
        assert_eq!(page_names(&page_two), vec!["delta_1.bmp", "epsilon.webp"]);

        let beyond = page(
            &catalog,
            folder_id,
            &name_sort(SortDirection::Ascending),
            &ImageFilter::default(),
            99,
            10,
        );
        assert!(beyond.items.is_empty());
        assert_eq!(beyond.total, 5);

        // Search is a case-insensitive substring match.
        let query = ImageFilter {
            query: "A".to_string(),
            ..ImageFilter::default()
        };
        let by_query = page(
            &catalog,
            folder_id,
            &name_sort(SortDirection::Ascending),
            &query,
            0,
            100,
        );
        assert_eq!(by_query.total, 4);

        let query = ImageFilter {
            query: "GAMMA".to_string(),
            ..ImageFilter::default()
        };
        assert_eq!(
            page(
                &catalog,
                folder_id,
                &name_sort(SortDirection::Ascending),
                &query,
                0,
                100
            )
            .total,
            1
        );

        // Format filter uses the canonical format ids, not the file extension.
        let formats = ImageFilter {
            formats: vec![ImageFormat::Jpeg],
            ..ImageFilter::default()
        };
        let by_format = page(
            &catalog,
            folder_id,
            &name_sort(SortDirection::Ascending),
            &formats,
            0,
            100,
        );
        assert_eq!(page_names(&by_format), vec!["alpha.jpg", "gamma.JPG"]);

        let formats = ImageFilter {
            formats: vec![ImageFormat::Jpeg, ImageFormat::Png],
            ..ImageFilter::default()
        };
        assert_eq!(
            page(
                &catalog,
                folder_id,
                &name_sort(SortDirection::Ascending),
                &formats,
                0,
                100
            )
            .total,
            3
        );

        // Favourites only.
        let gamma_id = image_id(&catalog, folder_id, "gamma.JPG");
        let alpha_id = image_id(&catalog, folder_id, "alpha.jpg");
        catalog.set_favorite(alpha_id, true).unwrap();
        let favorites = ImageFilter {
            favorites_only: true,
            ..ImageFilter::default()
        };
        let by_favorite = page(
            &catalog,
            folder_id,
            &name_sort(SortDirection::Ascending),
            &favorites,
            0,
            100,
        );
        assert_eq!(page_names(&by_favorite), vec!["alpha.jpg"]);

        // Missing images are hidden by default and opt-in via include_missing.
        catalog.set_missing(gamma_id, true).unwrap();
        let visible = page(
            &catalog,
            folder_id,
            &name_sort(SortDirection::Ascending),
            &ImageFilter::default(),
            0,
            100,
        );
        assert_eq!(visible.total, 4);
        assert!(!page_names(&visible).contains(&"gamma.JPG".to_string()));

        let with_missing = ImageFilter {
            include_missing: true,
            ..ImageFilter::default()
        };
        let all = page(
            &catalog,
            folder_id,
            &name_sort(SortDirection::Ascending),
            &with_missing,
            0,
            100,
        );
        assert_eq!(all.total, 5);
        assert!(page_names(&all).contains(&"gamma.JPG".to_string()));

        // Both filters combine with AND.
        let combined = ImageFilter {
            formats: vec![ImageFormat::Jpeg],
            include_missing: true,
            favorites_only: true,
            ..ImageFilter::default()
        };
        assert_eq!(
            page(
                &catalog,
                folder_id,
                &name_sort(SortDirection::Ascending),
                &combined,
                0,
                100
            )
            .total,
            1
        );
    }

    #[test]
    fn query_escapes_like_wildcards() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        write_file(&root.join("under_score.jpg"), 1);
        write_file(&root.join("underXscore.jpg"), 1);
        write_file(&root.join("100%_done.jpg"), 1);
        write_file(&root.join("plain.jpg"), 1);

        let (folder_id, _) = index_folder(&mut catalog, &root);

        let search = |query: &str| {
            let filter = ImageFilter {
                query: query.to_string(),
                ..ImageFilter::default()
            };
            page(
                &catalog,
                folder_id,
                &name_sort(SortDirection::Ascending),
                &filter,
                0,
                100,
            )
            .total
        };

        assert_eq!(search("under_score"), 1);
        assert_eq!(search("under%score"), 0);
        assert_eq!(search("_"), 2); // under_score + 100%_done, not underXscore
        assert_eq!(search("%"), 1); // only the literal percent name
        assert_eq!(search("under_"), 1); // literal underscore, not a wildcard
    }

    #[test]
    fn neighbor_navigation_follows_sort_and_filter() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        for name in ["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg"] {
            write_file(&root.join(name), 10);
        }

        let (folder_id, _) = index_folder(&mut catalog, &root);
        let filter = ImageFilter::default();
        let sort = name_sort(SortDirection::Ascending);
        let ids: Vec<i64> = catalog
            .images_page(folder_id, &sort, &filter, 0, 100)
            .unwrap()
            .items
            .iter()
            .map(|image| image.id)
            .collect();
        assert_eq!(ids.len(), 5);

        for window in ids.windows(2) {
            let next = catalog
                .neighbor(folder_id, &sort, &filter, window[0], 1)
                .unwrap();
            assert_eq!(next.map(|image| image.id), Some(window[1]));
            let previous = catalog
                .neighbor(folder_id, &sort, &filter, window[1], -1)
                .unwrap();
            assert_eq!(previous.map(|image| image.id), Some(window[0]));
        }
        assert!(
            catalog
                .neighbor(folder_id, &sort, &filter, ids[0], -1)
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .neighbor(folder_id, &sort, &filter, ids[4], 1)
                .unwrap()
                .is_none()
        );

        // Descending display order: the same ids walk backwards.
        let desc = name_sort(SortDirection::Descending);
        let desc_ids: Vec<i64> = catalog
            .images_page(folder_id, &desc, &filter, 0, 100)
            .unwrap()
            .items
            .iter()
            .map(|image| image.id)
            .collect();
        assert_eq!(desc_ids, ids.iter().rev().copied().collect::<Vec<_>>());
        assert_eq!(
            catalog
                .neighbor(folder_id, &desc, &filter, desc_ids[0], 1)
                .unwrap()
                .map(|i| i.id),
            Some(desc_ids[1])
        );

        // A filtered-out anchor has no neighbour, and missing rows are skipped.
        catalog.set_missing(ids[2], true).unwrap();
        assert!(
            catalog
                .neighbor(folder_id, &sort, &filter, ids[2], 1)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            catalog
                .neighbor(folder_id, &sort, &filter, ids[1], 1)
                .unwrap()
                .map(|i| i.id),
            Some(ids[3])
        );
        let with_missing = ImageFilter {
            include_missing: true,
            ..ImageFilter::default()
        };
        assert_eq!(
            catalog
                .neighbor(folder_id, &sort, &with_missing, ids[1], 1)
                .unwrap()
                .map(|i| i.id),
            Some(ids[2])
        );
    }

    #[test]
    fn favorite_and_edit_session_round_trip() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        write_file(&root.join("a.jpg"), 10);
        write_file(&root.join("b.jpg"), 10);

        let (folder_id, _) = index_folder(&mut catalog, &root);
        let a_id = image_id(&catalog, folder_id, "a.jpg");
        let b_id = image_id(&catalog, folder_id, "b.jpg");

        assert!(!catalog.get_image(a_id).unwrap().unwrap().favorite);
        catalog.set_favorite(a_id, true).unwrap();
        assert!(catalog.get_image(a_id).unwrap().unwrap().favorite);
        assert_eq!(catalog.db_stats().unwrap().favorites, 1);
        catalog.set_favorite(a_id, false).unwrap();
        assert_eq!(catalog.db_stats().unwrap().favorites, 0);

        // Absent session -> defaults.
        let missing_session = catalog.load_edit_session(b_id).unwrap();
        assert_eq!(missing_session, EditSession::new());
        assert_eq!(missing_session.revision, 0);
        assert_eq!(missing_session.updated_at_ms, 0);

        let params = EditParams {
            rotation: Rotation::Cw90,
            exposure: 1.25,
            saturation: -0.5,
            ..EditParams::default()
        };
        catalog.save_edit_session(b_id, &params, 7).unwrap();
        let loaded = catalog.load_edit_session(b_id).unwrap();
        assert_eq!(loaded.params, params.normalized());
        assert_eq!(loaded.revision, 7);
        assert!(loaded.updated_at_ms > 0);

        // Re-saving replaces the row instead of failing on the primary key.
        let params = EditParams {
            contrast: 0.25,
            ..EditParams::default()
        };
        catalog.save_edit_session(b_id, &params, 8).unwrap();
        let loaded = catalog.load_edit_session(b_id).unwrap();
        assert_eq!(loaded.params, params.normalized());
        assert_eq!(loaded.revision, 8);

        // Out-of-range values are clamped on the way in.
        let wild = EditParams {
            exposure: 9.0,
            ..EditParams::default()
        };
        catalog.save_edit_session(b_id, &wild, 9).unwrap();
        assert_eq!(
            catalog.load_edit_session(b_id).unwrap().params.exposure,
            3.0
        );
    }

    #[test]
    fn update_probe_persists_dimensions_and_clears_missing() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        write_file(&root.join("a.jpg"), 10);

        let (folder_id, _) = index_folder(&mut catalog, &root);
        let a_id = image_id(&catalog, folder_id, "a.jpg");
        catalog.set_missing(a_id, true).unwrap();

        let info = ImageInfo {
            width: 1920,
            height: 1080,
            format: ImageFormat::Jpeg,
            orientation: 6,
            has_alpha: false,
            animated: false,
            frames: 1,
        };
        catalog.update_probe(a_id, &info).unwrap();

        let meta = catalog.get_image(a_id).unwrap().unwrap();
        assert_eq!(meta.width, Some(1920));
        assert_eq!(meta.height, Some(1080));
        assert_eq!(meta.orientation, 6);
        assert_eq!(meta.format, ImageFormat::Jpeg);
        assert!(!meta.missing);
    }

    #[test]
    fn get_image_builds_path_and_metadata() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        let file = write_file(&root.join("nested/photo.png"), 42);

        let (folder_id, _) = index_folder(&mut catalog, &root);
        let id = image_id(&catalog, folder_id, "photo.png");
        let meta = catalog.get_image(id).unwrap().unwrap();

        assert_eq!(meta.path, fs::canonicalize(&file).unwrap());
        assert_eq!(meta.dir, fs::canonicalize(&file).unwrap().parent().unwrap());
        assert_eq!(meta.file_name, "photo.png");
        assert_eq!(meta.size_bytes, 42);
        assert!(meta.modified_ns > 0);
        assert_eq!(meta.format, ImageFormat::Png);
        assert_eq!(meta.orientation, 1);
        assert!(!meta.favorite);
        assert!(!meta.missing);
        assert!(meta.dimension_text().is_none());
        assert!(catalog.get_image(999_999).unwrap().is_none());
    }

    #[test]
    fn db_stats_and_distinct_formats() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        write_file(&root.join("a.jpg"), 10);
        write_file(&root.join("b.png"), 20);
        write_file(&root.join("c.webp"), 30);

        let (folder_id, _) = index_folder(&mut catalog, &root);
        let a_id = image_id(&catalog, folder_id, "a.jpg");
        catalog.set_favorite(a_id, true).unwrap();
        catalog.touch_folder_scan(folder_id).unwrap();

        let stats = catalog.db_stats().unwrap();
        assert_eq!(stats.images, 3);
        assert_eq!(stats.folders, 1);
        assert_eq!(stats.favorites, 1);
        assert!(stats.db_bytes > 0);

        assert_eq!(
            catalog.distinct_formats().unwrap(),
            vec![ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::WebP]
        );
    }

    #[test]
    fn count_images_matches_page_total() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        write_file(&root.join("a.jpg"), 1);
        write_file(&root.join("b.png"), 1);

        let (folder_id, _) = index_folder(&mut catalog, &root);
        let filter = ImageFilter {
            formats: vec![ImageFormat::Jpeg],
            ..ImageFilter::default()
        };
        assert_eq!(catalog.count_images(folder_id, &filter).unwrap(), 1);
        assert_eq!(
            catalog
                .count_images(folder_id, &ImageFilter::default())
                .unwrap(),
            2
        );
        assert_eq!(
            page(
                &catalog,
                folder_id,
                &name_sort(SortDirection::Ascending),
                &filter,
                0,
                10
            )
            .total,
            1
        );
    }

    #[test]
    fn limit_is_clamped() {
        let (tmp, mut catalog) = new_catalog();
        let root = tmp.path().join("photos");
        fs::create_dir_all(&root).unwrap();
        for index in 0..3 {
            write_file(&root.join(format!("img{index}.jpg")), 1);
        }
        let (folder_id, _) = index_folder(&mut catalog, &root);
        let sort = name_sort(SortDirection::Ascending);

        assert_eq!(
            page(&catalog, folder_id, &sort, &ImageFilter::default(), 0, 0)
                .items
                .len(),
            1
        );
        assert_eq!(
            page(
                &catalog,
                folder_id,
                &sort,
                &ImageFilter::default(),
                0,
                10_000
            )
            .items
            .len(),
            3
        );
    }
}
