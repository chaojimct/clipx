use std::path::Path;
use std::sync::mpsc;
use std::thread;

use anyhow::{anyhow, bail, Result};
use clipx_core::{now_ms, EntryKind, EntryMeta, NewEntry, Payload};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

const SCHEMA_VERSION: i64 = 4;

const SCHEMA_SQL: &str = r#"
CREATE TABLE entries (
    id           INTEGER PRIMARY KEY,
    kind         INTEGER NOT NULL,
    preview      TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    pinned       INTEGER NOT NULL DEFAULT 0,
    ocr_state    INTEGER NOT NULL DEFAULT 0,
    created_ms   INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_entries_hash ON entries(content_hash);
CREATE INDEX idx_entries_order ON entries(pinned DESC, created_ms DESC);

CREATE TABLE payloads (
    entry_id        INTEGER PRIMARY KEY REFERENCES entries(id) ON DELETE CASCADE,
    full_text       TEXT,
    image_blob      BLOB,
    image_w         INTEGER,
    image_h         INTEGER,
    image_mime      TEXT,
    thumb_blob      BLOB,
    thumb_w         INTEGER,
    thumb_h         INTEGER,
    file_paths_json TEXT,
    pinyin_blob     TEXT,
    ocr_text        TEXT
);

-- 拼音走 payloads.pinyin_blob + LIKE 子串（对齐 WPF Contains 语义），
-- FTS 仅承担英文/数字词前缀匹配
CREATE VIRTUAL TABLE entries_fts USING fts5(
    entry_id UNINDEXED,
    text,
    ocr
);
"#;

/// 库容上限：总条数与图片条数各自独立（WPF 版 MaxItems / MaxImageItems 语义）
#[derive(Debug, Clone, Copy)]
pub struct StoreLimits {
    pub max_items: i64,
    pub max_image_items: i64,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self { max_items: 2000, max_image_items: 150 }
    }
}

/// 图片条目缩略图（列表展示用，原图 blob 不出库）
#[derive(Debug, Clone)]
pub struct ThumbRow {
    pub blob: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

/// 图片条目原图（粘贴/预览按需加载，用完即弃）
#[derive(Debug, Clone)]
pub struct ImageRow {
    pub blob: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub mime: String,
}

/// OCR 状态与文本（预览展示用）
#[derive(Debug, Clone)]
pub struct OcrRow {
    pub state: i64,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted(i64),
    Bumped(i64),
}

enum Cmd {
    Insert { entry: NewEntry, reply: mpsc::Sender<Result<InsertOutcome>> },
    ListRecent { limit: i64, reply: mpsc::Sender<Vec<EntryMeta>> },
    ListThumbs { limit: i64, reply: mpsc::Sender<Vec<(i64, ThumbRow)>> },
    Search { query: String, kind: Option<EntryKind>, limit: i64, reply: mpsc::Sender<Vec<EntryMeta>> },
    GetText { id: i64, reply: mpsc::Sender<Option<String>> },
    GetThumb { id: i64, reply: mpsc::Sender<Option<ThumbRow>> },
    GetImage { id: i64, reply: mpsc::Sender<Option<ImageRow>> },
    GetOcr { id: i64, reply: mpsc::Sender<Option<OcrRow>> },
    MarkOcrPending { id: i64 },
    SetOcrText { id: i64, text: String },
    ListOcrBackfill { limit: i64, reply: mpsc::Sender<Vec<i64>> },
    Delete { id: i64, reply: mpsc::Sender<bool> },
    Seed { count: usize, reply: mpsc::Sender<Result<usize>> },
}

#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<Cmd>,
}

impl Store {
    pub fn open(path: &Path, limits: StoreLimits) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut conn = Connection::open(path)?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
        migrate(&mut conn)?;

        let (tx, rx) = mpsc::channel::<Cmd>();
        thread::Builder::new()
            .name("clipx-store".into())
            .spawn(move || {
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::Insert { entry, reply } => {
                            let _ = reply.send(handle_insert(&mut conn, entry, limits));
                        }
                        Cmd::ListRecent { limit, reply } => {
                            let _ = reply.send(handle_list(&conn, limit));
                        }
                        Cmd::ListThumbs { limit, reply } => {
                            let _ = reply.send(handle_list_thumbs(&conn, limit));
                        }
                        Cmd::Search { query, kind, limit, reply } => {
                            let _ = reply.send(handle_search(&conn, &query, kind, limit));
                        }
                        Cmd::GetText { id, reply } => {
                            let _ = reply.send(handle_get_text(&conn, id));
                        }
                        Cmd::GetThumb { id, reply } => {
                            let _ = reply.send(handle_get_thumb(&conn, id));
                        }
                        Cmd::GetImage { id, reply } => {
                            let _ = reply.send(handle_get_image(&conn, id));
                        }
                        Cmd::GetOcr { id, reply } => {
                            let _ = reply.send(handle_get_ocr(&conn, id));
                        }
                        Cmd::MarkOcrPending { id } => {
                            let _ = conn.execute(
                                "UPDATE entries SET ocr_state = 1 WHERE id = ?1 AND kind = 1",
                                params![id],
                            );
                        }
                        Cmd::SetOcrText { id, text } => {
                            let _ = handle_set_ocr_text(&mut conn, id, &text);
                        }
                        Cmd::ListOcrBackfill { limit, reply } => {
                            let _ = reply.send(handle_ocr_backfill(&conn, limit));
                        }
                        Cmd::Delete { id, reply } => {
                            let _ = reply.send(handle_delete(&mut conn, id));
                        }
                        Cmd::Seed { count, reply } => {
                            let _ = reply.send(handle_seed(&mut conn, count, limits));
                        }
                    }
                }
            })?;
        Ok(Store { tx })
    }

    pub fn insert(&self, entry: NewEntry) -> Result<InsertOutcome> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::Insert { entry, reply })
            .map_err(|_| anyhow!("store 线程已退出"))?;
        rx.recv().map_err(|_| anyhow!("store 线程已退出"))?
    }

    pub fn list_recent(&self, limit: i64) -> Vec<EntryMeta> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::ListRecent { limit, reply }).is_err() {
            return Vec::new();
        }
        rx.recv().unwrap_or_default()
    }

    /// 批量取缩略图（(entry_id, ThumbRow)）：列表刷新一次往返，
    /// 避免逐条 get_thumb 的 N 次 channel roundtrip。
    pub fn list_thumbs(&self, limit: i64) -> Vec<(i64, ThumbRow)> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::ListThumbs { limit, reply }).is_err() {
            return Vec::new();
        }
        rx.recv().unwrap_or_default()
    }

    /// 空查询等价于按 kind 过滤的最近列表；非空查询 = LIKE 包含 + FTS 拼音前缀。
    pub fn search(&self, query: &str, kind: Option<EntryKind>, limit: i64) -> Vec<EntryMeta> {
        let (reply, rx) = mpsc::channel();
        if self
            .tx
            .send(Cmd::Search { query: query.to_string(), kind, limit, reply })
            .is_err()
        {
            return Vec::new();
        }
        rx.recv().unwrap_or_default()
    }

    pub fn get_text(&self, id: i64) -> Option<String> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::GetText { id, reply }).is_err() {
            return None;
        }
        rx.recv().unwrap_or(None)
    }

    pub fn get_thumb(&self, id: i64) -> Option<ThumbRow> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::GetThumb { id, reply }).is_err() {
            return None;
        }
        rx.recv().unwrap_or(None)
    }

    pub fn get_image(&self, id: i64) -> Option<ImageRow> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::GetImage { id, reply }).is_err() {
            return None;
        }
        rx.recv().unwrap_or(None)
    }

    pub fn get_ocr(&self, id: i64) -> Option<OcrRow> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::GetOcr { id, reply }).is_err() {
            return None;
        }
        rx.recv().unwrap_or(None)
    }

    pub fn mark_ocr_pending(&self, id: i64) {
        let _ = self.tx.send(Cmd::MarkOcrPending { id });
    }

    pub fn set_ocr_text(&self, id: i64, text: String) {
        let _ = self.tx.send(Cmd::SetOcrText { id, text });
    }

    /// 待 OCR 回填的图片条目（未处理或上次中断在队列中的）
    pub fn list_ocr_backfill(&self, limit: i64) -> Vec<i64> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::ListOcrBackfill { limit, reply }).is_err() {
            return Vec::new();
        }
        rx.recv().unwrap_or_default()
    }

    pub fn delete(&self, id: i64) -> bool {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::Delete { id, reply }).is_err() {
            return false;
        }
        rx.recv().unwrap_or(false)
    }

    pub fn seed(&self, count: usize) -> Result<usize> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::Seed { count, reply })
            .map_err(|_| anyhow!("store 线程已退出"))?;
        rx.recv().map_err(|_| anyhow!("store 线程已退出"))?
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if version > SCHEMA_VERSION {
        bail!("数据库 schema 版本 {version} 高于本程序支持的 {SCHEMA_VERSION}");
    }
    if version == 0 {
        conn.execute_batch(SCHEMA_SQL)?;
    } else {
        // v1/v2 → v3：补 pinyin_blob 列并重建 FTS（结构变更），再从 full_text 回填
        let has_blob: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('payloads') WHERE name = 'pinyin_blob'",
            [],
            |r| r.get(0),
        )?;
        if has_blob == 0 {
            conn.execute("ALTER TABLE payloads ADD COLUMN pinyin_blob TEXT", [])?;
        }
        // v3 → v4：补 ocr_text 列
        let has_ocr: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('payloads') WHERE name = 'ocr_text'",
            [],
            |r| r.get(0),
        )?;
        if has_ocr == 0 {
            conn.execute("ALTER TABLE payloads ADD COLUMN ocr_text TEXT", [])?;
        }
        conn.execute("DROP TABLE entries_fts", [])?;
        conn.execute(
            "CREATE VIRTUAL TABLE entries_fts USING fts5(entry_id UNINDEXED, text, ocr)",
            [],
        )?;
        backfill_search_columns(conn)?;
    }
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    Ok(())
}

fn backfill_search_columns(conn: &Connection) -> Result<()> {
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn
            .prepare("SELECT entry_id, full_text FROM payloads WHERE full_text IS NOT NULL")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };
    for (id, text) in rows {
        conn.execute(
            "INSERT INTO entries_fts (entry_id, text) VALUES (?1, ?2)",
            params![id, text],
        )?;
        conn.execute(
            "UPDATE payloads SET pinyin_blob = ?2 WHERE entry_id = ?1",
            params![id, clipx_core::pinyin::to_pinyin_blob(&text)],
        )?;
    }
    Ok(())
}

fn handle_insert(
    conn: &mut Connection,
    entry: NewEntry,
    limits: StoreLimits,
) -> Result<InsertOutcome> {
    let tx = conn.transaction()?;
    let outcome = upsert_entry(&tx, &entry, now_ms())?;
    trim_to_max(&tx, limits)?;
    tx.commit()?;
    Ok(outcome)
}

fn trim_to_max(tx: &Transaction, limits: StoreLimits) -> Result<()> {
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
    if count > limits.max_items {
        tx.execute(
            "DELETE FROM entries_fts WHERE entry_id IN (
                SELECT id FROM entries WHERE id NOT IN (
                    SELECT id FROM entries ORDER BY pinned DESC, created_ms DESC LIMIT ?1
                )
            )",
            params![limits.max_items],
        )?;
        tx.execute(
            "DELETE FROM entries WHERE id NOT IN (
                SELECT id FROM entries ORDER BY pinned DESC, created_ms DESC LIMIT ?1
            )",
            params![limits.max_items],
        )?;
    }
    // 图片单独限量（WPF 版 PruneExcessImages：按时间保留最新 N 张）
    let image_count: i64 =
        tx.query_row("SELECT COUNT(*) FROM entries WHERE kind = 1", [], |r| r.get(0))?;
    if image_count > limits.max_image_items {
        tx.execute(
            "DELETE FROM entries_fts WHERE entry_id IN (
                SELECT id FROM entries WHERE kind = 1 AND id NOT IN (
                    SELECT id FROM entries WHERE kind = 1
                    ORDER BY created_ms DESC LIMIT ?1
                )
            )",
            params![limits.max_image_items],
        )?;
        tx.execute(
            "DELETE FROM entries WHERE kind = 1 AND id NOT IN (
                SELECT id FROM entries WHERE kind = 1
                ORDER BY created_ms DESC LIMIT ?1
            )",
            params![limits.max_image_items],
        )?;
    }
    Ok(())
}

fn handle_seed(conn: &mut Connection, count: usize, limits: StoreLimits) -> Result<usize> {
    let tx = conn.transaction()?;
    let base = now_ms();
    for i in 0..count {
        let text = format!("Seed 条目 #{i} — The quick brown fox jumps over the lazy dog 剪贴板压测数据 {i}");
        let entry = NewEntry::from_text(text);
        upsert_entry(&tx, &entry, base + i as i64)?;
    }
    trim_to_max(&tx, limits)?;
    tx.commit()?;
    Ok(count)
}

fn upsert_entry(tx: &Transaction, entry: &NewEntry, now_ms: i64) -> Result<InsertOutcome> {
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM entries WHERE content_hash = ?1",
            [&entry.content_hash],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        tx.execute("UPDATE entries SET created_ms = ?1 WHERE id = ?2", params![now_ms, id])?;
        return Ok(InsertOutcome::Bumped(id));
    }

    tx.execute(
        "INSERT INTO entries (kind, preview, content_hash, pinned, ocr_state, created_ms)
         VALUES (?1, ?2, ?3, 0, 0, ?4)",
        params![entry.kind.as_i64(), entry.preview, entry.content_hash, now_ms],
    )?;
    let id = tx.last_insert_rowid();

    match &entry.payload {
        Payload::Text { full } => {
            tx.execute(
                "INSERT INTO payloads (entry_id, full_text, pinyin_blob) VALUES (?1, ?2, ?3)",
                params![id, full, clipx_core::pinyin::to_pinyin_blob(full)],
            )?;
            tx.execute(
                "INSERT INTO entries_fts (entry_id, text) VALUES (?1, ?2)",
                params![id, full],
            )?;
        }
        Payload::Image { blob, width, height, mime, thumb, thumb_w, thumb_h } => {
            tx.execute(
                "INSERT INTO payloads (entry_id, image_blob, image_w, image_h, image_mime,
                                       thumb_blob, thumb_w, thumb_h)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, blob, width, height, mime, thumb, thumb_w, thumb_h],
            )?;
        }
        Payload::Files { paths } => {
            let joined = paths.join("\n");
            let json = serde_json::to_string(paths)?;
            tx.execute(
                "INSERT INTO payloads (entry_id, full_text, file_paths_json, pinyin_blob)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, joined, json, clipx_core::pinyin::to_pinyin_blob(&joined)],
            )?;
            tx.execute(
                "INSERT INTO entries_fts (entry_id, text) VALUES (?1, ?2)",
                params![id, joined],
            )?;
        }
    }
    Ok(InsertOutcome::Inserted(id))
}

fn handle_list(conn: &Connection, limit: i64) -> Vec<EntryMeta> {
    select_metas(
        conn,
        "SELECT id, kind, preview, pinned, created_ms
         FROM entries ORDER BY pinned DESC, created_ms DESC LIMIT ?1",
        params![limit],
    )
}

fn handle_list_thumbs(conn: &Connection, limit: i64) -> Vec<(i64, ThumbRow)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT p.entry_id, p.thumb_blob, p.thumb_w, p.thumb_h
         FROM payloads p
         JOIN entries e ON e.id = p.entry_id
         WHERE p.thumb_blob IS NOT NULL
         ORDER BY e.created_ms DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![limit], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            ThumbRow {
                blob: r.get(1)?,
                w: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u32,
                h: r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u32,
            },
        ))
    }) else {
        return Vec::new();
    };
    rows.filter_map(|r| r.ok()).collect()
}

fn handle_search(
    conn: &Connection,
    query: &str,
    kind: Option<EntryKind>,
    limit: i64,
) -> Vec<EntryMeta> {
    let query = query.trim();
    if query.is_empty() {
        let kind_i64 = kind.map(|k| k.as_i64());
        return select_metas(
            conn,
            "SELECT id, kind, preview, pinned, created_ms
             FROM entries
             WHERE (?1 IS NULL OR kind = ?1)
             ORDER BY pinned DESC, created_ms DESC LIMIT ?2",
            params![kind_i64, limit],
        );
    }

    let like = like_pattern(query);
    let kind_i64 = kind.map(|k| k.as_i64());
    let order = " ORDER BY e.pinned DESC, e.created_ms DESC LIMIT ";
    match build_fts_query(query) {
        // 空字符串 MATCH 是 FTS5 语法错误，必须按需拼接而非传 ""
        Some(fts) => select_metas(
            conn,
            &format!(
                "SELECT e.id, e.kind, e.preview, e.pinned, e.created_ms
                 FROM entries e
                 LEFT JOIN payloads p ON p.entry_id = e.id
                 WHERE (?1 IS NULL OR e.kind = ?1)
                   AND (
                        e.preview LIKE ?2 ESCAPE '\\'
                        OR p.full_text LIKE ?2 ESCAPE '\\'
                        OR p.pinyin_blob LIKE ?2 ESCAPE '\\'
                        OR p.ocr_text LIKE ?2 ESCAPE '\\'
                        OR e.id IN (SELECT entry_id FROM entries_fts WHERE entries_fts MATCH ?3)
                   ){order}?4"
            ),
            params![kind_i64, like, fts, limit],
        ),
        None => select_metas(
            conn,
            &format!(
                "SELECT e.id, e.kind, e.preview, e.pinned, e.created_ms
                 FROM entries e
                 LEFT JOIN payloads p ON p.entry_id = e.id
                 WHERE (?1 IS NULL OR e.kind = ?1)
                   AND (
                        e.preview LIKE ?2 ESCAPE '\\'
                        OR p.full_text LIKE ?2 ESCAPE '\\'
                        OR p.pinyin_blob LIKE ?2 ESCAPE '\\'
                        OR p.ocr_text LIKE ?2 ESCAPE '\\'
                   ){order}?3"
            ),
            params![kind_i64, like, limit],
        ),
    }
}

fn handle_get_text(conn: &Connection, id: i64) -> Option<String> {
    conn.query_row(
        "SELECT full_text FROM payloads WHERE entry_id = ?1",
        params![id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .ok()
    .flatten()
    .flatten()
}

fn handle_get_thumb(conn: &Connection, id: i64) -> Option<ThumbRow> {
    conn.query_row(
        "SELECT thumb_blob, thumb_w, thumb_h FROM payloads
         WHERE entry_id = ?1 AND thumb_blob IS NOT NULL",
        params![id],
        |r| {
            Ok(ThumbRow {
                blob: r.get(0)?,
                w: r.get::<_, Option<i64>>(1)?.unwrap_or(0) as u32,
                h: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u32,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

fn handle_get_image(conn: &Connection, id: i64) -> Option<ImageRow> {
    conn.query_row(
        "SELECT image_blob, image_w, image_h, image_mime FROM payloads
         WHERE entry_id = ?1 AND image_blob IS NOT NULL",
        params![id],
        |r| {
            Ok(ImageRow {
                blob: r.get(0)?,
                w: r.get::<_, Option<i64>>(1)?.unwrap_or(0) as u32,
                h: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u32,
                mime: r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "image/png".into()),
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

fn handle_get_ocr(conn: &Connection, id: i64) -> Option<OcrRow> {
    conn.query_row(
        "SELECT ocr_state, ocr_text FROM entries e
         LEFT JOIN payloads p ON p.entry_id = e.id
         WHERE e.id = ?1 AND e.kind = 1",
        params![id],
        |r| {
            Ok(OcrRow {
                state: r.get(0)?,
                text: r.get::<_, Option<String>>(1)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// OCR 完成：写 ocr_text、置 state=2、重建该条 FTS（含 ocr 列）、拼音 blob 纳入 OCR 文本。
fn handle_set_ocr_text(conn: &mut Connection, id: i64, text: &str) -> Result<()> {
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE entries SET ocr_state = 2 WHERE id = ?1 AND kind = 1",
        params![id],
    )?;
    if changed == 0 {
        tx.commit()?;
        return Ok(());
    }
    tx.execute(
        "UPDATE payloads SET ocr_text = ?2, pinyin_blob = ?3 WHERE entry_id = ?1",
        params![id, text, clipx_core::pinyin::to_pinyin_blob(text)],
    )?;
    if !text.trim().is_empty() {
        tx.execute("DELETE FROM entries_fts WHERE entry_id = ?1", params![id])?;
        tx.execute(
            "INSERT INTO entries_fts (entry_id, ocr) VALUES (?1, ?2)",
            params![id, text],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn handle_ocr_backfill(conn: &Connection, limit: i64) -> Vec<i64> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id FROM entries WHERE kind = 1 AND ocr_state IN (0, 1)
         ORDER BY created_ms DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![limit], |r| r.get::<_, i64>(0)) else {
        return Vec::new();
    };
    rows.filter_map(|r| r.ok()).collect()
}

fn handle_delete(conn: &mut Connection, id: i64) -> bool {
    let Ok(tx) = conn.transaction() else { return false };
    let _ = tx.execute("DELETE FROM entries_fts WHERE entry_id = ?1", params![id]);
    let n = tx.execute("DELETE FROM entries WHERE id = ?1", params![id]).unwrap_or(0);
    tx.commit().is_ok() && n > 0
}

fn select_metas(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Vec<EntryMeta> {
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(p, |r| {
        let kind: i64 = r.get(1)?;
        Ok(EntryMeta {
            id: r.get(0)?,
            kind: EntryKind::from_i64(kind).unwrap_or(EntryKind::Text),
            preview: r.get(2)?,
            pinned: r.get(3)?,
            created_ms: r.get(4)?,
        })
    }) else {
        return Vec::new();
    };
    rows.filter_map(|r| r.ok()).collect()
}

/// 仅 ASCII 字母数字走 FTS 前缀匹配（英文词/数字）；中文与拼音走 LIKE 子串。
fn build_fts_query(query: &str) -> Option<String> {
    if !query.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ') {
        return None;
    }
    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"*", t))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

fn like_pattern(q: &str) -> String {
    let mut s = String::from("%");
    for c in q.chars() {
        match c {
            '\\' | '%' | '_' => {
                s.push('\\');
                s.push(c);
            }
            _ => s.push(c),
        }
    }
    s.push('%');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("clipx.db"), StoreLimits::default()).unwrap();
        (store, dir)
    }

    fn temp_store_with_limits(limits: StoreLimits) -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("clipx.db"), limits).unwrap();
        (store, dir)
    }

    fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::new(w, h);
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn insert_and_list_roundtrip() {
        let (store, _keep) = temp_store();
        let outcome = store.insert(NewEntry::from_text("第一条记录".into())).unwrap();
        assert!(matches!(outcome, InsertOutcome::Inserted(_)));

        let rows = store.list_recent(100);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].preview, "第一条记录");
        assert!(!rows[0].pinned);
    }

    #[test]
    fn duplicate_content_bumps_to_top() {
        let (store, _keep) = temp_store();
        store.insert(NewEntry::from_text("same".into())).unwrap();
        store.insert(NewEntry::from_text("other".into())).unwrap();
        thread::sleep(Duration::from_millis(5));

        let outcome = store.insert(NewEntry::from_text("same".into())).unwrap();
        assert!(matches!(outcome, InsertOutcome::Bumped(_)));

        let rows = store.list_recent(100);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].preview, "same");
    }

    #[test]
    fn seed_fts_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipx.db");

        {
            let store = Store::open(&path, StoreLimits::default()).unwrap();
            store.seed(300).unwrap();
            assert_eq!(store.list_recent(500).len(), 300);
        }

        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 4);

        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM entries_fts WHERE entries_fts MATCH 'fox'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 300);
        drop(conn);

        let store = Store::open(&path, StoreLimits::default()).unwrap();
        assert_eq!(store.list_recent(500).len(), 300);
    }

    #[test]
    fn list_thumbs_returns_only_images_ordered_by_recency() {
        let (store, _keep) = temp_store();
        let text = store.insert(NewEntry::from_text("文本无缩略图".into())).unwrap();
        assert!(matches!(text, InsertOutcome::Inserted(_)));

        let mut image_ids = Vec::new();
        for i in 0..3 {
            let png = tiny_png(100 + i, 40);
            let outcome = store
                .insert(NewEntry::from_image(png, 100 + i, 40, "image/png".into()))
                .unwrap();
            let InsertOutcome::Inserted(id) = outcome else { panic!() };
            image_ids.push(id);
            thread::sleep(Duration::from_millis(5));
        }

        let thumbs = store.list_thumbs(100);
        assert_eq!(thumbs.len(), 3);
        let ids: Vec<i64> = thumbs.iter().map(|(id, _)| *id).collect();
        // 最新优先
        assert_eq!(ids, image_ids.iter().rev().copied().collect::<Vec<_>>());
        assert!(thumbs.iter().all(|(_, t)| !t.blob.is_empty()));
    }

    #[test]
    fn search_by_substring_and_pinyin() {
        let (store, _keep) = temp_store();
        store.insert(NewEntry::from_text("你好世界".into())).unwrap();
        store.insert(NewEntry::from_text("部署 dev 环境".into())).unwrap();
        store.insert(NewEntry::from_text("hello world".into())).unwrap();

        // 中文包含
        assert_eq!(store.search("世界", None, 10).len(), 1);
        assert_eq!(store.search("环境", None, 10).len(), 1);
        // 拼音全拼前缀
        assert_eq!(store.search("niha", None, 10).len(), 1);
        assert_eq!(store.search("shijie", None, 10).len(), 1);
        // 拼音首字母
        assert_eq!(store.search("nh", None, 10).len(), 1);
        assert_eq!(store.search("bushu", None, 10).len(), 1);
        assert_eq!(store.search("bs", None, 10).len(), 1);
        // 英文前缀（FTS text 列）
        assert_eq!(store.search("hell", None, 10).len(), 1);
        // 无命中
        assert!(store.search("不存在的词条", None, 10).is_empty());
        // like 转义：% 字面量不爆炸
        assert!(store.search("%", None, 10).is_empty());
    }

    #[test]
    fn search_with_kind_filter() {
        let (store, _keep) = temp_store();
        store.insert(NewEntry::from_text("text item".into())).unwrap();

        assert_eq!(store.search("", Some(EntryKind::Text), 10).len(), 1);
        assert_eq!(store.search("", Some(EntryKind::Image), 10).len(), 0);
        assert_eq!(store.search("text", Some(EntryKind::Image), 10).len(), 0);
    }

    #[test]
    fn get_text_and_delete() {
        let (store, _keep) = temp_store();
        let outcome = store.insert(NewEntry::from_text("完整正文内容".into())).unwrap();
        let InsertOutcome::Inserted(id) = outcome else { panic!() };

        assert_eq!(store.get_text(id).as_deref(), Some("完整正文内容"));
        assert!(store.delete(id));
        assert_eq!(store.get_text(id), None);
        assert!(store.list_recent(10).is_empty());
        assert!(!store.delete(id));
    }

    #[test]
    fn v1_migration_backfills_pinyin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipx.db");

        {
            // 手工构造 v1 库（无拼音）
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE entries (id INTEGER PRIMARY KEY, kind INTEGER NOT NULL, preview TEXT NOT NULL,
                    content_hash TEXT NOT NULL, pinned INTEGER NOT NULL DEFAULT 0,
                    ocr_state INTEGER NOT NULL DEFAULT 0, created_ms INTEGER NOT NULL);
                CREATE UNIQUE INDEX idx_entries_hash ON entries(content_hash);
                CREATE INDEX idx_entries_order ON entries(pinned DESC, created_ms DESC);
                CREATE TABLE payloads (entry_id INTEGER PRIMARY KEY REFERENCES entries(id) ON DELETE CASCADE,
                    full_text TEXT, image_blob BLOB, image_w INTEGER, image_h INTEGER, image_mime TEXT,
                    thumb_blob BLOB, thumb_w INTEGER, thumb_h INTEGER, file_paths_json TEXT);
                CREATE VIRTUAL TABLE entries_fts USING fts5(entry_id UNINDEXED, text, pinyin, ocr);
                INSERT INTO entries (kind, preview, content_hash, created_ms) VALUES (0, '你好世界', 'h1', 1000);
                INSERT INTO payloads (entry_id, full_text) VALUES (1, '你好世界');
                INSERT INTO entries_fts (entry_id, text) VALUES (1, '你好世界');
                PRAGMA user_version = 1;
                "#,
            )
            .unwrap();
        }

        let store = Store::open(&path, StoreLimits::default()).unwrap();
        // 迁移后拼音可查
        assert_eq!(store.search("nihaoshijie", None, 10).len(), 1);
        assert_eq!(store.search("nh", None, 10).len(), 1);
        // 旧数据仍在
        assert_eq!(store.list_recent(10).len(), 1);
    }

    #[test]
    fn trim_keeps_recent_within_max() {
        // max_items=2000：seed 2050 条应裁剪到 2000
        let (store, _keep) =
            temp_store_with_limits(StoreLimits { max_items: 2000, max_image_items: 150 });
        store.seed(2050).unwrap();
        assert_eq!(store.list_recent(3000).len(), 2000);
    }

    #[test]
    fn image_roundtrip_thumb_and_lazy_load() {
        let (store, _keep) = temp_store();
        let png = tiny_png(320, 120);
        let outcome = store
            .insert(NewEntry::from_image(png.clone(), 320, 120, "image/png".into()))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else { panic!() };

        let thumb = store.get_thumb(id).unwrap();
        assert!(!thumb.blob.is_empty());
        assert_eq!((thumb.w, thumb.h), (64, 24));

        let img = store.get_image(id).unwrap();
        assert_eq!(img.blob, png);
        assert_eq!((img.w, img.h), (320, 120));
        assert_eq!(img.mime, "image/png");

        // 文本条目没有图片负载
        let t = store.insert(NewEntry::from_text("纯文本".into())).unwrap();
        let InsertOutcome::Inserted(tid) = t else { panic!() };
        assert!(store.get_thumb(tid).is_none());
        assert!(store.get_image(tid).is_none());
    }

    #[test]
    fn ocr_text_flow_and_search() {
        let (store, _keep) = temp_store();
        let outcome = store
            .insert(NewEntry::from_image(tiny_png(100, 40), 100, 40, "image/png".into()))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else { panic!() };

        // 初始：未处理 → 进入回填列表
        assert_eq!(store.list_ocr_backfill(100), vec![id]);
        store.mark_ocr_pending(id);
        assert_eq!(store.get_ocr(id).unwrap().state, 1);
        // pending（中断态）也应回填
        assert_eq!(store.list_ocr_backfill(100), vec![id]);

        store.set_ocr_text(id, "你好世界 hello".into());
        let ocr = store.get_ocr(id).unwrap();
        assert_eq!(ocr.state, 2);
        assert_eq!(ocr.text.as_deref(), Some("你好世界 hello"));
        // 完成后不再回填
        assert!(store.list_ocr_backfill(100).is_empty());

        // OCR 文本可搜：中文子串、拼音、英文词
        assert_eq!(store.search("世界", None, 10).len(), 1);
        assert_eq!(store.search("shijie", None, 10).len(), 1);
        assert_eq!(store.search("hello", None, 10).len(), 1);
        // 图片 kind 过滤仍生效
        assert_eq!(store.search("世界", Some(EntryKind::Image), 10).len(), 1);
        assert_eq!(store.search("世界", Some(EntryKind::Text), 10).len(), 0);
    }

    #[test]
    fn ocr_empty_text_marks_done_without_fts() {
        let (store, _keep) = temp_store();
        let outcome = store
            .insert(NewEntry::from_image(tiny_png(10, 10), 10, 10, "image/png".into()))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else { panic!() };

        store.set_ocr_text(id, String::new());
        assert_eq!(store.get_ocr(id).unwrap().state, 2);
        assert!(store.list_ocr_backfill(100).is_empty());
    }

    #[test]
    fn image_prune_keeps_recent_images_only() {
        let (store, _keep) =
            temp_store_with_limits(StoreLimits { max_items: 2000, max_image_items: 3 });
        let mut ids = Vec::new();
        for i in 0..5 {
            // 每张内容不同（尺寸递增）→ 不同 hash
            let png = tiny_png(100 + i, 40);
            let outcome = store.insert(NewEntry::from_image(png, 100 + i, 40, "image/png".into()))
                .unwrap();
            let InsertOutcome::Inserted(id) = outcome else { panic!() };
            ids.push(id);
            thread::sleep(Duration::from_millis(5));
        }
        let rows = store.list_recent(100);
        assert_eq!(rows.len(), 3);
        // 最旧的 2 张被裁掉
        let remaining: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert!(!remaining.contains(&ids[0]));
        assert!(!remaining.contains(&ids[1]));
        assert!(remaining.contains(&ids[4]));
    }
}
