use anyhow::{anyhow, bail, Result};
use clipx_core::{now_ms, EntryKind, EntryMeta, NewEntry, Payload};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::path::Path;
use std::sync::mpsc;
use std::thread;

pub mod wpf;

const SCHEMA_VERSION: i64 = 7;

const SCHEMA_SQL: &str = r#"
CREATE TABLE entries (
    id           INTEGER PRIMARY KEY,
    kind         INTEGER NOT NULL,
    preview      TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    pinned       INTEGER NOT NULL DEFAULT 0,
    ocr_state    INTEGER NOT NULL DEFAULT 0,
    created_ms   INTEGER NOT NULL,
    source_app   TEXT NOT NULL DEFAULT ''
);
CREATE UNIQUE INDEX idx_entries_hash ON entries(content_hash);
CREATE INDEX idx_entries_order ON entries(pinned DESC, created_ms DESC);

CREATE TABLE payloads (
    entry_id        INTEGER PRIMARY KEY REFERENCES entries(id) ON DELETE CASCADE,
    full_text       TEXT,
    html            TEXT,
    image_blob      BLOB,
    image_w         INTEGER,
    image_h         INTEGER,
    image_mime      TEXT,
    thumb_blob      BLOB,
    thumb_w         INTEGER,
    thumb_h         INTEGER,
    file_paths_json TEXT,
    pinyin_blob     TEXT,
    ocr_text        TEXT,
    ocr_boxes       TEXT
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
        Self {
            max_items: 2000,
            max_image_items: 150,
        }
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

/// OCR 状态、文本与行框（预览展示用；boxes 为行框 JSON，PreviewData 解析）
#[derive(Debug, Clone)]
pub struct OcrRow {
    pub state: i64,
    pub text: Option<String>,
    pub boxes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted(i64),
    Bumped(i64),
}

enum Cmd {
    Insert {
        entry: NewEntry,
        reply: mpsc::Sender<Result<InsertOutcome>>,
    },
    ListRecent {
        limit: i64,
        reply: mpsc::Sender<Vec<EntryMeta>>,
    },
    ListThumbs {
        limit: i64,
        reply: mpsc::Sender<Vec<(i64, ThumbRow)>>,
    },
    BackfillFileThumbs {
        limit: i64,
        reply: mpsc::Sender<Vec<(i64, ThumbRow)>>,
    },
    Search {
        query: String,
        kind: Option<EntryKind>,
        source: Option<String>,
        deep: bool,
        limit: i64,
        reply: mpsc::Sender<Vec<EntryMeta>>,
    },
    UpdateText {
        id: i64,
        text: String,
        reply: mpsc::Sender<Result<bool>>,
    },
    ListSources {
        reply: mpsc::Sender<Vec<String>>,
    },
    ExportJson {
        path: String,
        reply: mpsc::Sender<Result<usize>>,
    },
    ImportJson {
        path: String,
        reply: mpsc::Sender<Result<ImportStats>>,
    },
    GetText {
        id: i64,
        reply: mpsc::Sender<Option<String>>,
    },
    GetHtml {
        id: i64,
        reply: mpsc::Sender<Option<String>>,
    },
    GetFiles {
        id: i64,
        reply: mpsc::Sender<Option<Vec<String>>>,
    },
    GetThumb {
        id: i64,
        reply: mpsc::Sender<Option<ThumbRow>>,
    },
    GetImage {
        id: i64,
        reply: mpsc::Sender<Option<ImageRow>>,
    },
    GetOcr {
        id: i64,
        reply: mpsc::Sender<Option<OcrRow>>,
    },
    MarkOcrPending {
        id: i64,
    },
    MarkOcrFailed {
        id: i64,
    },
    SetOcrText {
        id: i64,
        text: String,
    },
    SetOcrResult {
        id: i64,
        text: String,
        boxes: String,
    },
    ListOcrBackfill {
        limit: i64,
        reply: mpsc::Sender<Vec<i64>>,
    },
    Delete {
        id: i64,
        reply: mpsc::Sender<bool>,
    },
    /// 清空全部历史（WPF 设置页“清空所有历史记录”；快捷短语存 JSON 不受影响）。
    ClearAll {
        reply: mpsc::Sender<usize>,
    },
    TogglePin {
        id: i64,
        reply: mpsc::Sender<Option<bool>>,
    },
    /// 粘贴触顶（对齐 WPF TouchCopiedTime）：成功粘贴后把条目时间刷新到现在，
    /// 下次列表按序即置顶。快捷短语不在库中，调用方自行跳过。
    Touch {
        id: i64,
        reply: mpsc::Sender<bool>,
    },
    ImportBatch {
        rows: Vec<MigrationRow>,
        reply: mpsc::Sender<Result<ImportStats>>,
    },
    Seed {
        count: usize,
        reply: mpsc::Sender<Result<usize>>,
    },
    SetLimits {
        limits: StoreLimits,
    },
}

/// WPF 版迁移源行：条目 + 保留的原时间戳 + 原始 OCR 文本
#[derive(Debug, Clone)]
pub struct MigrationRow {
    pub entry: NewEntry,
    pub created_ms: i64,
    pub ocr_text: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ImportStats {
    pub inserted: usize,
    pub skipped_dup: usize,
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
        conn.execute_batch(
            "PRAGMA synchronous = NORMAL; \
             PRAGMA foreign_keys = ON; \
             PRAGMA temp_store = MEMORY; \
             PRAGMA cache_size = -8000; \
             PRAGMA busy_timeout = 5000;",
        )?;
        migrate(&mut conn)?;

        let (tx, rx) = mpsc::channel::<Cmd>();
        thread::Builder::new()
            .name("clipx-store".into())
            .spawn(move || {
                let mut limits = limits;
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
                        Cmd::BackfillFileThumbs { limit, reply } => {
                            let _ = reply.send(handle_backfill_file_thumbs(&conn, limit));
                        }
                        Cmd::Search {
                            query,
                            kind,
                            source,
                            deep,
                            limit,
                            reply,
                        } => {
                            let _ = reply.send(handle_search(
                                &conn,
                                &query,
                                kind,
                                source.as_deref(),
                                deep,
                                limit,
                            ));
                        }
                        Cmd::UpdateText { id, text, reply } => {
                            let _ = reply.send(handle_update_text(&mut conn, id, &text));
                        }
                        Cmd::ListSources { reply } => {
                            let _ = reply.send(handle_list_sources(&conn));
                        }
                        Cmd::ExportJson { path, reply } => {
                            let _ = reply.send(handle_export_json(&conn, &path));
                        }
                        Cmd::ImportJson { path, reply } => {
                            let _ = reply.send(handle_import_json(&mut conn, &path, limits));
                        }
                        Cmd::GetText { id, reply } => {
                            let _ = reply.send(handle_get_text(&conn, id));
                        }
                        Cmd::GetHtml { id, reply } => {
                            let _ = reply.send(handle_get_html(&conn, id));
                        }
                        Cmd::GetFiles { id, reply } => {
                            let _ = reply.send(handle_get_files(&conn, id));
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
                        Cmd::MarkOcrFailed { id } => {
                            // 终态：引擎报错/负载缺失。回填查询只取 (0,1)，避免 worker 自驱循环死转
                            let _ = conn.execute(
                                "UPDATE entries SET ocr_state = 3 WHERE id = ?1 AND kind = 1",
                                params![id],
                            );
                        }
                        Cmd::SetOcrText { id, text } => {
                            let _ = handle_set_ocr_text(&mut conn, id, &text);
                        }
                        Cmd::SetOcrResult { id, text, boxes } => {
                            let _ = handle_set_ocr_result(&mut conn, id, &text, &boxes);
                        }
                        Cmd::ListOcrBackfill { limit, reply } => {
                            let _ = reply.send(handle_ocr_backfill(&conn, limit));
                        }
                        Cmd::Delete { id, reply } => {
                            let _ = reply.send(handle_delete(&mut conn, id));
                        }
                        Cmd::ClearAll { reply } => {
                            let _ = reply.send(handle_clear_all(&mut conn));
                        }
                        Cmd::TogglePin { id, reply } => {
                            let _ = reply.send(handle_toggle_pin(&conn, id));
                        }
                        Cmd::Touch { id, reply } => {
                            let _ = reply.send(handle_touch(&conn, id));
                        }
                        Cmd::ImportBatch { rows, reply } => {
                            let _ = reply.send(handle_import_batch(&mut conn, rows, limits));
                        }
                        Cmd::Seed { count, reply } => {
                            let _ = reply.send(handle_seed(&mut conn, count, limits));
                        }
                        Cmd::SetLimits { limits: next } => {
                            limits = next;
                        }
                    }
                }
            })?;
        Ok(Store { tx })
    }

    pub fn set_limits(&self, limits: StoreLimits) {
        let _ = self.tx.send(Cmd::SetLimits { limits });
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

    /// 给历史里还没缩略图的文件条目补 64px 图（一次最多 `limit` 条）。
    pub fn backfill_file_thumbs(&self, limit: i64) -> Vec<(i64, ThumbRow)> {
        let (reply, rx) = mpsc::channel();
        if self
            .tx
            .send(Cmd::BackfillFileThumbs { limit, reply })
            .is_err()
        {
            return Vec::new();
        }
        rx.recv().unwrap_or_default()
    }

    /// 空查询等价于按 kind 过滤的最近列表；非空查询 = LIKE 包含 + FTS 拼音前缀。
    pub fn search(&self, query: &str, kind: Option<EntryKind>, limit: i64) -> Vec<EntryMeta> {
        self.search_ex(query, kind, None, false, limit)
    }

    /// 深搜扫 full_text/OCR；`source` 非空则只看来自该进程。
    pub fn search_ex(
        &self,
        query: &str,
        kind: Option<EntryKind>,
        source: Option<&str>,
        deep: bool,
        limit: i64,
    ) -> Vec<EntryMeta> {
        let (reply, rx) = mpsc::channel();
        if self
            .tx
            .send(Cmd::Search {
                query: query.to_string(),
                kind,
                source: source.map(|s| s.to_string()),
                deep,
                limit,
                reply,
            })
            .is_err()
        {
            return Vec::new();
        }
        rx.recv().unwrap_or_default()
    }

    /// 编辑文本条目：改 preview / hash / FTS，保留 id。非文本或哈希冲突返回 Ok(false)。
    pub fn update_text(&self, id: i64, text: String) -> Result<bool> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::UpdateText { id, text, reply })
            .map_err(|_| anyhow!("store 线程已退出"))?;
        rx.recv().map_err(|_| anyhow!("store 线程已退出"))?
    }

    pub fn list_sources(&self) -> Vec<String> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::ListSources { reply }).is_err() {
            return Vec::new();
        }
        rx.recv().unwrap_or_default()
    }

    pub fn export_json(&self, path: &Path) -> Result<usize> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::ExportJson {
                path: path.to_string_lossy().into_owned(),
                reply,
            })
            .map_err(|_| anyhow!("store 线程已退出"))?;
        rx.recv().map_err(|_| anyhow!("store 线程已退出"))?
    }

    pub fn import_json(&self, path: &Path) -> Result<ImportStats> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::ImportJson {
                path: path.to_string_lossy().into_owned(),
                reply,
            })
            .map_err(|_| anyhow!("store 线程已退出"))?;
        rx.recv().map_err(|_| anyhow!("store 线程已退出"))?
    }

    pub fn get_text(&self, id: i64) -> Option<String> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::GetText { id, reply }).is_err() {
            return None;
        }
        rx.recv().unwrap_or(None)
    }

    pub fn get_html(&self, id: i64) -> Option<String> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::GetHtml { id, reply }).is_err() {
            return None;
        }
        rx.recv().unwrap_or(None)
    }

    pub fn get_files(&self, id: i64) -> Option<Vec<String>> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::GetFiles { id, reply }).is_err() {
            return None;
        }
        rx.recv().unwrap_or(None)
    }

    /// 置顶/取消置顶：返回翻转后的新状态（条目不存在 → None）
    pub fn toggle_pin(&self, id: i64) -> Option<bool> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::TogglePin { id, reply }).is_err() {
            return None;
        }
        rx.recv().unwrap_or(None)
    }

    /// 粘贴触顶：刷新条目时间为现在（不存在 → false）。
    pub fn touch(&self, id: i64) -> bool {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::Touch { id, reply }).is_err() {
            return false;
        }
        rx.recv().unwrap_or(false)
    }

    /// WPF 版历史批量导入：单事务去重插入（已存在的 content_hash 跳过），
    /// 图片直接写缩略图与 OCR 文本，最后统一裁剪容量。老库时间戳保留。
    pub fn import_batch(&self, rows: Vec<MigrationRow>) -> Result<ImportStats> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::ImportBatch { rows, reply })
            .map_err(|_| anyhow!("store 线程已退出"))?;
        rx.recv().map_err(|_| anyhow!("store 线程已退出"))?
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

    pub fn mark_ocr_failed(&self, id: i64) {
        let _ = self.tx.send(Cmd::MarkOcrFailed { id });
    }

    pub fn set_ocr_text(&self, id: i64, text: String) {
        let _ = self.tx.send(Cmd::SetOcrText { id, text });
    }

    /// OCR 完成（含行框 JSON；空串表示无框，不占空间）。
    pub fn set_ocr_result(&self, id: i64, text: String, boxes: String) {
        let _ = self.tx.send(Cmd::SetOcrResult { id, text, boxes });
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

    /// 清空全部历史（含置顶），返回删除条数。
    pub fn clear_all(&self) -> usize {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::ClearAll { reply }).is_err() {
            return 0;
        }
        rx.recv().unwrap_or(0)
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
        // v4 → v5：补 html 列（富文本格式，M3）
        let has_html: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('payloads') WHERE name = 'html'",
            [],
            |r| r.get(0),
        )?;
        if has_html == 0 {
            conn.execute("ALTER TABLE payloads ADD COLUMN html TEXT", [])?;
        }
        // v5 → v6：来源应用
        let has_src: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('entries') WHERE name = 'source_app'",
            [],
            |r| r.get(0),
        )?;
        if has_src == 0 {
            conn.execute(
                "ALTER TABLE entries ADD COLUMN source_app TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        // v6 → v7：OCR 行框 JSON（P1a；旧行保持 NULL，预览降级为全文）
        let has_boxes: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('payloads') WHERE name = 'ocr_boxes'",
            [],
            |r| r.get(0),
        )?;
        if has_boxes == 0 {
            conn.execute("ALTER TABLE payloads ADD COLUMN ocr_boxes TEXT", [])?;
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
        let mut stmt =
            conn.prepare("SELECT entry_id, full_text FROM payloads WHERE full_text IS NOT NULL")?;
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
        tx.query_row("SELECT COUNT(*) FROM entries WHERE kind = 1", [], |r| {
            r.get(0)
        })?;
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
        let text = format!(
            "Seed 条目 #{i} — The quick brown fox jumps over the lazy dog 剪贴板压测数据 {i}"
        );
        let entry = NewEntry::from_text(text);
        upsert_entry(&tx, &entry, base + i as i64)?;
    }
    trim_to_max(&tx, limits)?;
    tx.commit()?;
    Ok(count)
}

fn upsert_entry(tx: &Transaction, entry: &NewEntry, now_ms: i64) -> Result<InsertOutcome> {
    // 文本类跨 kind 去重（对齐 WPF DeduplicateText；clipx 多了 RichText 种）：
    // 同正文的 Text/RichText 旧行全部删除后再按新 kind 插入（pin 继承），
    // 否则同文换源（终端纯文本 vs 浏览器富文本）各存一行，旧行永远 bump 不到，
    // 同源重抄（HTML 包装微差）则无限堆重复行。
    // 同 kind 同哈希仍走经典 bump（id 稳定，批量队列/缩略图缓存不受影响）。
    let full_text: Option<&str> = match &entry.payload {
        Payload::Text { full } | Payload::RichText { full, .. } => Some(full),
        _ => None,
    };
    if let Some(text) = full_text {
        let mut stmt = tx.prepare(
            "SELECT e.id, e.pinned, e.content_hash FROM entries e
             JOIN payloads p ON p.entry_id = e.id
             WHERE e.kind IN (0, 3) AND p.full_text = ?1",
        )?;
        let dups: Vec<(i64, i64, String)> = stmt
            .query_map([text], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        if !dups.is_empty() {
            if dups.len() == 1 && dups[0].2 == entry.content_hash {
                // 完全相同（含 HTML）：经典 bump，id 不变。
                tx.execute(
                    "UPDATE entries SET created_ms = ?1, source_app = ?3 WHERE id = ?2",
                    params![now_ms, dups[0].0, entry.source_app],
                )?;
                return Ok(InsertOutcome::Bumped(dups[0].0));
            }
            let pinned = dups.iter().any(|(_, p, _)| *p != 0);
            for (id, _, _) in &dups {
                tx.execute("DELETE FROM entries_fts WHERE entry_id = ?1", params![id])?;
                tx.execute("DELETE FROM payloads WHERE entry_id = ?1", params![id])?;
                tx.execute("DELETE FROM entries WHERE id = ?1", params![id])?;
            }
            tx.execute(
                "INSERT INTO entries (kind, preview, content_hash, pinned, ocr_state, created_ms, source_app)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
                params![
                    entry.kind.as_i64(),
                    entry.preview,
                    entry.content_hash,
                    if pinned { 1 } else { 0 },
                    now_ms,
                    entry.source_app
                ],
            )?;
            let id = tx.last_insert_rowid();
            insert_payload(tx, id, &entry.payload)?;
            return Ok(InsertOutcome::Inserted(id));
        }
    }
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM entries WHERE content_hash = ?1",
            [&entry.content_hash],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        tx.execute(
            "UPDATE entries SET created_ms = ?1, source_app = ?3 WHERE id = ?2",
            params![now_ms, id, entry.source_app],
        )?;
        return Ok(InsertOutcome::Bumped(id));
    }

    tx.execute(
        "INSERT INTO entries (kind, preview, content_hash, pinned, ocr_state, created_ms, source_app)
         VALUES (?1, ?2, ?3, 0, 0, ?4, ?5)",
        params![
            entry.kind.as_i64(),
            entry.preview,
            entry.content_hash,
            now_ms,
            entry.source_app
        ],
    )?;
    let id = tx.last_insert_rowid();
    insert_payload(tx, id, &entry.payload)?;
    Ok(InsertOutcome::Inserted(id))
}

const META_COLS: &str = "id, kind, preview, pinned, created_ms, COALESCE(source_app, '')";

fn handle_list(conn: &Connection, limit: i64) -> Vec<EntryMeta> {
    select_metas(
        conn,
        &format!(
            "SELECT {META_COLS}
             FROM entries ORDER BY pinned DESC, created_ms DESC LIMIT ?1"
        ),
        params![limit],
    )
}

fn handle_list_thumbs(conn: &Connection, limit: i64) -> Vec<(i64, ThumbRow)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT p.entry_id, p.thumb_blob, p.thumb_w, p.thumb_h
         FROM payloads p
         JOIN entries e ON e.id = p.entry_id
         WHERE p.thumb_blob IS NOT NULL AND length(p.thumb_blob) > 0
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

fn handle_backfill_file_thumbs(conn: &Connection, limit: i64) -> Vec<(i64, ThumbRow)> {
    let rows: Vec<(i64, String)> = {
        let Ok(mut stmt) = conn.prepare(
            "SELECT e.id, p.file_paths_json
             FROM entries e
             JOIN payloads p ON p.entry_id = e.id
             WHERE e.kind = 2
               AND p.thumb_blob IS NULL
               AND p.file_paths_json IS NOT NULL
             ORDER BY e.created_ms DESC
             LIMIT ?1",
        ) else {
            return Vec::new();
        };
        let Ok(mapped) = stmt.query_map(params![limit.max(0)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        }) else {
            return Vec::new();
        };
        mapped.filter_map(|r| r.ok()).collect()
    };
    let mut out = Vec::new();
    for (id, json) in rows {
        let paths: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
        let (blob, w, h) = clipx_core::make_file_list_thumbnail(&paths);
        let _ = conn.execute(
            "UPDATE payloads SET thumb_blob = ?1, thumb_w = ?2, thumb_h = ?3 WHERE entry_id = ?4",
            params![blob.as_slice(), w as i64, h as i64, id],
        );
        if !blob.is_empty() {
            out.push((id, ThumbRow { blob, w, h }));
        }
    }
    out
}

fn handle_search(
    conn: &Connection,
    query: &str,
    kind: Option<EntryKind>,
    source: Option<&str>,
    deep: bool,
    limit: i64,
) -> Vec<EntryMeta> {
    let query = query.trim();
    let kind_i64 = kind.map(|k| k.as_i64());
    let src = source
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if query.is_empty() {
        let sql = format!(
            "SELECT {META_COLS} FROM entries e
             WHERE (?1 IS NULL OR (?1 = 0 AND e.kind IN (0, 3)) OR e.kind = ?1)
             {src_empty}
             ORDER BY e.pinned DESC, e.created_ms DESC LIMIT ?2",
            src_empty = if src.is_some() {
                "AND COALESCE(e.source_app, '') = ?3"
            } else {
                ""
            }
        );
        return if let Some(s) = src {
            select_metas(conn, &sql, params![kind_i64, limit, s])
        } else {
            select_metas(conn, &sql, params![kind_i64, limit])
        };
    }

    let like = like_pattern(query);
    let kind_cond = "(?1 IS NULL OR (?1 = 0 AND e.kind IN (0, 3)) OR e.kind = ?1)";
    let deep_cond = if deep {
        "OR p.full_text LIKE ?2 ESCAPE '\\' OR p.ocr_text LIKE ?2 ESCAPE '\\'"
    } else {
        ""
    };
    let cols = "e.id, e.kind, e.preview, e.pinned, e.created_ms, COALESCE(e.source_app, '')";
    match (build_fts_query(query), src.as_deref()) {
        (Some(fts), Some(s)) => select_metas(
            conn,
            &format!(
                "SELECT {cols} FROM entries e LEFT JOIN payloads p ON p.entry_id = e.id
                 WHERE {kind_cond} AND COALESCE(e.source_app, '') = ?5 AND (
                    e.preview LIKE ?2 ESCAPE '\\' OR p.pinyin_blob LIKE ?2 ESCAPE '\\'
                    {deep_cond}
                    OR e.id IN (SELECT entry_id FROM entries_fts WHERE entries_fts MATCH ?3)
                 ) ORDER BY e.pinned DESC, e.created_ms DESC LIMIT ?4"
            ),
            params![kind_i64, like, fts, limit, s],
        ),
        (Some(fts), None) => select_metas(
            conn,
            &format!(
                "SELECT {cols} FROM entries e LEFT JOIN payloads p ON p.entry_id = e.id
                 WHERE {kind_cond} AND (
                    e.preview LIKE ?2 ESCAPE '\\' OR p.pinyin_blob LIKE ?2 ESCAPE '\\'
                    {deep_cond}
                    OR e.id IN (SELECT entry_id FROM entries_fts WHERE entries_fts MATCH ?3)
                 ) ORDER BY e.pinned DESC, e.created_ms DESC LIMIT ?4"
            ),
            params![kind_i64, like, fts, limit],
        ),
        (None, Some(s)) => select_metas(
            conn,
            &format!(
                "SELECT {cols} FROM entries e LEFT JOIN payloads p ON p.entry_id = e.id
                 WHERE {kind_cond} AND COALESCE(e.source_app, '') = ?4 AND (
                    e.preview LIKE ?2 ESCAPE '\\' OR p.pinyin_blob LIKE ?2 ESCAPE '\\'
                    {deep_cond}
                 ) ORDER BY e.pinned DESC, e.created_ms DESC LIMIT ?3"
            ),
            params![kind_i64, like, limit, s],
        ),
        (None, None) => select_metas(
            conn,
            &format!(
                "SELECT {cols} FROM entries e LEFT JOIN payloads p ON p.entry_id = e.id
                 WHERE {kind_cond} AND (
                    e.preview LIKE ?2 ESCAPE '\\' OR p.pinyin_blob LIKE ?2 ESCAPE '\\'
                    {deep_cond}
                 ) ORDER BY e.pinned DESC, e.created_ms DESC LIMIT ?3"
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

fn handle_get_html(conn: &Connection, id: i64) -> Option<String> {
    conn.query_row(
        "SELECT html FROM payloads WHERE entry_id = ?1",
        params![id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .ok()
    .flatten()
    .flatten()
}

fn handle_get_files(conn: &Connection, id: i64) -> Option<Vec<String>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT file_paths_json FROM payloads WHERE entry_id = ?1",
            params![id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .ok()
        .flatten()
        .flatten();
    json.and_then(|j| serde_json::from_str(&j).ok())
}

fn handle_update_text(conn: &mut Connection, id: i64, text: &str) -> Result<bool> {
    let kind: Option<i64> = conn
        .query_row("SELECT kind FROM entries WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(kind) = kind else {
        return Ok(false);
    };
    if kind != 0 && kind != 3 {
        return Ok(false);
    }
    let rebuilt = NewEntry::from_text(text.to_string());
    let other: Option<i64> = conn
        .query_row(
            "SELECT id FROM entries WHERE content_hash = ?1 AND id != ?2",
            params![rebuilt.content_hash, id],
            |r| r.get(0),
        )
        .optional()?;
    if other.is_some() {
        return Ok(false);
    }
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE entries SET preview = ?2, content_hash = ?3 WHERE id = ?1",
        params![id, rebuilt.preview, rebuilt.content_hash],
    )?;
    tx.execute(
        "UPDATE payloads SET full_text = ?2, pinyin_blob = ?3 WHERE entry_id = ?1",
        params![id, text, clipx_core::pinyin::to_pinyin_blob(text)],
    )?;
    tx.execute("DELETE FROM entries_fts WHERE entry_id = ?1", params![id])?;
    tx.execute(
        "INSERT INTO entries_fts (entry_id, text) VALUES (?1, ?2)",
        params![id, text],
    )?;
    tx.commit()?;
    Ok(true)
}

fn handle_list_sources(conn: &Connection) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT source_app FROM entries
         WHERE source_app IS NOT NULL AND source_app != ''
         ORDER BY source_app COLLATE NOCASE",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
        return Vec::new();
    };
    rows.filter_map(|r| r.ok()).collect()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HistoryDump {
    version: u32,
    entries: Vec<HistoryDumpEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HistoryDumpEntry {
    kind: i64,
    preview: String,
    pinned: bool,
    created_ms: i64,
    #[serde(default)]
    source_app: String,
    text: Option<String>,
    html: Option<String>,
    files: Option<Vec<String>>,
    ocr_text: Option<String>,
    #[serde(default)]
    image_png_b64: Option<String>,
}

fn handle_export_json(conn: &Connection, path: &str) -> Result<usize> {
    let metas = handle_list(conn, 100_000);
    let mut entries = Vec::with_capacity(metas.len());
    for m in &metas {
        let mut row = HistoryDumpEntry {
            kind: m.kind.as_i64(),
            preview: m.preview.clone(),
            pinned: m.pinned,
            created_ms: m.created_ms,
            source_app: m.source_app.clone(),
            text: None,
            html: None,
            files: None,
            ocr_text: None,
            image_png_b64: None,
        };
        match m.kind {
            EntryKind::Text | EntryKind::RichText => {
                row.text = handle_get_text(conn, m.id);
                row.html = handle_get_html(conn, m.id);
            }
            EntryKind::Files => {
                row.files = handle_get_files(conn, m.id);
            }
            EntryKind::Image => {
                row.ocr_text = handle_get_ocr(conn, m.id).and_then(|o| o.text);
                if let Some(img) = handle_get_image(conn, m.id) {
                    row.image_png_b64 = Some(b64_encode(&img.blob));
                }
            }
        }
        entries.push(row);
    }
    let n = entries.len();
    let dump = HistoryDump {
        version: 1,
        entries,
    };
    if let Some(dir) = Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&dump)?)?;
    Ok(n)
}

fn handle_import_json(
    conn: &mut Connection,
    path: &str,
    limits: StoreLimits,
) -> Result<ImportStats> {
    let raw = std::fs::read_to_string(path)?;
    let dump: HistoryDump = serde_json::from_str(&raw)?;
    let mut rows = Vec::new();
    for e in dump.entries {
        let mut entry = match e.kind {
            1 => {
                let Some(b64) = e.image_png_b64.as_deref() else {
                    continue;
                };
                let Ok(blob) = b64_decode(b64) else {
                    continue;
                };
                NewEntry::from_image(blob, 0, 0, "image/png".into())
            }
            2 => NewEntry::from_files(e.files.unwrap_or_default()),
            3 => NewEntry::from_rich_text(e.text.unwrap_or_default(), e.html.unwrap_or_default()),
            _ => NewEntry::from_text(e.text.unwrap_or_else(|| e.preview.clone())),
        };
        entry.source_app = e.source_app;
        rows.push(MigrationRow {
            entry,
            created_ms: e.created_ms,
            ocr_text: e.ocr_text,
        });
    }
    handle_import_batch(conn, rows, limits)
}

fn b64_encode(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (a << 16) | (b << 8) | c;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(T[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn b64_decode(s: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let a = val(bytes[i]).ok_or_else(|| anyhow!("invalid b64"))?;
        let b = val(bytes[i + 1]).ok_or_else(|| anyhow!("invalid b64"))?;
        let c = if bytes[i + 2] == b'=' {
            0
        } else {
            val(bytes[i + 2]).ok_or_else(|| anyhow!("invalid b64"))?
        };
        let d = if bytes[i + 3] == b'=' {
            0
        } else {
            val(bytes[i + 3]).ok_or_else(|| anyhow!("invalid b64"))?
        };
        out.push((a << 2) | (b >> 4));
        if bytes[i + 2] != b'=' {
            out.push((b << 4) | (c >> 2));
        }
        if bytes[i + 3] != b'=' {
            out.push((c << 6) | d);
        }
        i += 4;
    }
    Ok(out)
}

fn handle_toggle_pin(conn: &Connection, id: i64) -> Option<bool> {
    let pinned: Option<i64> = conn
        .query_row(
            "SELECT pinned FROM entries WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let pinned = pinned?;
    let next = if pinned == 0 { 1 } else { 0 };
    match conn.execute(
        "UPDATE entries SET pinned = ?2 WHERE id = ?1",
        params![id, next],
    ) {
        Ok(_) => Some(next == 1),
        Err(_) => None,
    }
}

fn handle_touch(conn: &Connection, id: i64) -> bool {
    conn.execute(
        "UPDATE entries SET created_ms = ?2 WHERE id = ?1",
        params![id, now_ms()],
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// WPF 迁移批量导入：老库按时间升序送入（同 hash 保留更新的），
/// 已存在（含 clipx 自身历史）的 content_hash 直接跳过不覆盖。
fn handle_import_batch(
    conn: &mut Connection,
    rows: Vec<MigrationRow>,
    _limits: StoreLimits,
) -> Result<ImportStats> {
    let tx = conn.transaction()?;
    let mut stats = ImportStats::default();
    for row in rows {
        let exists: Option<i64> = tx
            .query_row(
                "SELECT id FROM entries WHERE content_hash = ?1",
                [&row.entry.content_hash],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            stats.skipped_dup += 1;
            continue;
        }
        insert_entry_at(&tx, &row.entry, row.created_ms)?;
        if let Some(ocr) = row.ocr_text.as_deref().filter(|t| !t.trim().is_empty()) {
            if row.entry.kind == EntryKind::Image {
                tx.execute(
                    "UPDATE entries SET ocr_state = 2 WHERE id = (SELECT id FROM entries WHERE content_hash = ?1)",
                    [&row.entry.content_hash],
                )?;
                tx.execute(
                    "UPDATE payloads SET ocr_text = ?2, pinyin_blob = ?3
                     WHERE entry_id = (SELECT id FROM entries WHERE content_hash = ?1)",
                    params![
                        row.entry.content_hash,
                        ocr,
                        clipx_core::pinyin::to_pinyin_blob(ocr)
                    ],
                )?;
                tx.execute(
                    "INSERT INTO entries_fts (entry_id, ocr) VALUES (
                        (SELECT id FROM entries WHERE content_hash = ?1), ?2)",
                    params![row.entry.content_hash, ocr],
                )?;
            }
        }
        stats.inserted += 1;
    }
    // 迁移不裁剪（验收：WPF 历史无丢失）；容量上限在后续新条目插入时自然滚动淘汰
    tx.commit()?;
    Ok(stats)
}

/// 指定 created_ms 的插入（迁移路径）；正常采集走 upsert_entry（now + bump 语义）。
fn insert_entry_at(tx: &Transaction, entry: &NewEntry, created_ms: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO entries (kind, preview, content_hash, pinned, ocr_state, created_ms, source_app)
         VALUES (?1, ?2, ?3, 0, 0, ?4, ?5)",
        params![
            entry.kind.as_i64(),
            entry.preview,
            entry.content_hash,
            created_ms,
            entry.source_app
        ],
    )?;
    let id = tx.last_insert_rowid();
    insert_payload(tx, id, &entry.payload)?;
    Ok(())
}

/// 载荷写入（insert_entry_at / upsert_entry 共用）
fn insert_payload(tx: &Transaction, id: i64, payload: &Payload) -> Result<()> {
    match payload {
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
        Payload::RichText { full, html } => {
            tx.execute(
                "INSERT INTO payloads (entry_id, full_text, html, pinyin_blob) VALUES (?1, ?2, ?3, ?4)",
                params![id, full, html, clipx_core::pinyin::to_pinyin_blob(full)],
            )?;
            tx.execute(
                "INSERT INTO entries_fts (entry_id, text) VALUES (?1, ?2)",
                params![id, full],
            )?;
        }
        Payload::Image {
            blob,
            width,
            height,
            mime,
            thumb,
            thumb_w,
            thumb_h,
        } => {
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
            let (thumb, thumb_w, thumb_h) = clipx_core::make_file_list_thumbnail(paths);
            tx.execute(
                "INSERT INTO payloads (entry_id, full_text, file_paths_json, pinyin_blob,
                                       thumb_blob, thumb_w, thumb_h)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    joined,
                    json,
                    clipx_core::pinyin::to_pinyin_blob(&joined),
                    thumb,
                    thumb_w as i64,
                    thumb_h as i64,
                ],
            )?;
            tx.execute(
                "INSERT INTO entries_fts (entry_id, text) VALUES (?1, ?2)",
                params![id, joined],
            )?;
        }
    }
    Ok(())
}

fn handle_get_thumb(conn: &Connection, id: i64) -> Option<ThumbRow> {
    conn.query_row(
        "SELECT thumb_blob, thumb_w, thumb_h FROM payloads
         WHERE entry_id = ?1 AND thumb_blob IS NOT NULL AND length(thumb_blob) > 0",
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
                mime: r
                    .get::<_, Option<String>>(3)?
                    .unwrap_or_else(|| "image/png".into()),
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

fn handle_get_ocr(conn: &Connection, id: i64) -> Option<OcrRow> {
    // ocr_boxes 列在 v7 加入； defenses：旧库迁移失败时仍按无框返回（列缺失则整体 None 会吞行，
    // 故先查列存在性，缺失走降级查询）。
    let has_boxes: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('payloads') WHERE name = 'ocr_boxes'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    if !has_boxes {
        return conn
            .query_row(
                "SELECT ocr_state, ocr_text FROM entries e
                 LEFT JOIN payloads p ON p.entry_id = e.id
                 WHERE e.id = ?1 AND e.kind = 1",
                params![id],
                |r| {
                    Ok(OcrRow {
                        state: r.get(0)?,
                        text: r.get::<_, Option<String>>(1)?,
                        boxes: None,
                    })
                },
            )
            .optional()
            .ok()
            .flatten();
    }
    conn.query_row(
        "SELECT ocr_state, ocr_text, ocr_boxes FROM entries e
         LEFT JOIN payloads p ON p.entry_id = e.id
         WHERE e.id = ?1 AND e.kind = 1",
        params![id],
        |r| {
            Ok(OcrRow {
                state: r.get(0)?,
                text: r.get::<_, Option<String>>(1)?,
                boxes: r.get::<_, Option<String>>(2)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// OCR 完成：写 ocr_text、置 state=2、重建该条 FTS（含 ocr 列）、拼音 blob 纳入 OCR 文本。
/// 纯文本路径不碰 ocr_boxes（框由 set_ocr_result 专管，避免文本更新清掉已框）。
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

/// OCR 完成（含行框）：boxes 原样落库（"[]"=已处理无框，NULL 仅属于 v7 前旧行）。
/// text 为空时 FTS 跳过（沿用旧语义），但 boxes 照写（回填收敛标记）。
fn handle_set_ocr_result(conn: &mut Connection, id: i64, text: &str, boxes: &str) -> Result<()> {
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
        "UPDATE payloads SET ocr_text = ?2, ocr_boxes = ?3, pinyin_blob = ?4 WHERE entry_id = ?1",
        params![
            id,
            text,
            // 原样落库："[]"=已处理无框；NULL 只属于 v7 前旧行（回填收敛标记）。
            Some(boxes.to_string()),
            clipx_core::pinyin::to_pinyin_blob(text)
        ],
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
    // v7 前旧行（state=2 但框为 NULL）一次性补框，之后收敛（worker 落 "[]" 标记）。
    let Ok(mut stmt) = conn.prepare(
        "SELECT e.id FROM entries e
         LEFT JOIN payloads p ON p.entry_id = e.id
         WHERE e.kind = 1 AND (e.ocr_state IN (0, 1)
            OR (e.ocr_state = 2 AND p.ocr_boxes IS NULL))
         ORDER BY e.created_ms DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![limit], |r| r.get::<_, i64>(0)) else {
        return Vec::new();
    };
    rows.filter_map(|r| r.ok()).collect()
}

fn handle_clear_all(conn: &mut Connection) -> usize {
    let Ok(tx) = conn.transaction() else {
        return 0;
    };
    let _ = tx.execute("DELETE FROM entries_fts", []);
    let _ = tx.execute("DELETE FROM payloads", []);
    let n = tx.execute("DELETE FROM entries", []).unwrap_or(0);
    if tx.commit().is_err() {
        return 0;
    }
    n as usize
}

fn handle_delete(conn: &mut Connection, id: i64) -> bool {
    let Ok(tx) = conn.transaction() else {
        return false;
    };
    let _ = tx.execute("DELETE FROM entries_fts WHERE entry_id = ?1", params![id]);
    let n = tx
        .execute("DELETE FROM entries WHERE id = ?1", params![id])
        .unwrap_or(0);
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
            source_app: r.get::<_, String>(5).unwrap_or_default(),
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
        let outcome = store
            .insert(NewEntry::from_text("第一条记录".into()))
            .unwrap();
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
    fn same_text_across_kinds_collapses_to_newest() {
        // 终端纯文本 vs 浏览器富文本同文：旧行删除、新行置顶，不堆重复。
        let (store, _keep) = temp_store();
        let InsertOutcome::Inserted(text_id) =
            store.insert(NewEntry::from_text("root".into())).unwrap()
        else {
            panic!()
        };
        store.insert(NewEntry::from_text("other".into())).unwrap();
        thread::sleep(Duration::from_millis(5));

        let outcome = store
            .insert(NewEntry::from_rich_text("root".into(), "<b>root</b>".into()))
            .unwrap();
        let InsertOutcome::Inserted(new_id) = outcome else {
            panic!("跨 kind 应删除旧行重新插入，got {outcome:?}")
        };
        assert_ne!(new_id, text_id);
        let rows = store.list_recent(100);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, new_id);
        assert_eq!(rows[0].kind, EntryKind::RichText);
        // HTML 载荷随新行走。
        assert_eq!(
            store.get_html(new_id).as_deref(),
            Some("<b>root</b>")
        );
        assert!(store.get_text(text_id).is_none());
    }

    #[test]
    fn recopy_rich_text_with_different_html_replaces() {
        // 同源重抄（HTML 包装微差）：不堆行，旧行删除、新行置顶。
        let (store, _keep) = temp_store();
        store
            .insert(NewEntry::from_rich_text("x".into(), "<b>x</b>".into()))
            .unwrap();
        thread::sleep(Duration::from_millis(5));
        let outcome = store
            .insert(NewEntry::from_rich_text("x".into(), "<i>x</i>".into()))
            .unwrap();
        assert!(matches!(outcome, InsertOutcome::Inserted(_)));
        let rows = store.list_recent(100);
        assert_eq!(rows.len(), 1);
        assert_eq!(store.get_html(rows[0].id).as_deref(), Some("<i>x</i>"));
    }

    #[test]
    fn dedup_collapse_keeps_pin() {
        // 被折叠的旧行若置顶，新行继承置顶（重抄不掉钉）。
        let (store, _keep) = temp_store();
        let InsertOutcome::Inserted(id) = store.insert(NewEntry::from_text("pin".into())).unwrap()
        else {
            panic!()
        };
        assert_eq!(store.toggle_pin(id), Some(true));
        store
            .insert(NewEntry::from_rich_text("pin".into(), "<b>pin</b>".into()))
            .unwrap();
        let rows = store.list_recent(100);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].pinned);
    }

    #[test]
    fn touch_moves_entry_to_top() {
        // 粘贴触顶（对齐 WPF TouchCopiedTime）：id 不变，时间刷新即置顶。
        let (store, _keep) = temp_store();
        let InsertOutcome::Inserted(a) = store.insert(NewEntry::from_text("a".into())).unwrap()
        else {
            panic!()
        };
        store.insert(NewEntry::from_text("b".into())).unwrap();
        thread::sleep(Duration::from_millis(5));
        assert!(store.touch(a));
        let rows = store.list_recent(100);
        assert_eq!(rows[0].id, a);
        assert!(!store.touch(999_999));
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
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

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
        let text = store
            .insert(NewEntry::from_text("文本无缩略图".into()))
            .unwrap();
        assert!(matches!(text, InsertOutcome::Inserted(_)));

        let mut image_ids = Vec::new();
        for i in 0..3 {
            let png = tiny_png(100 + i, 40);
            let outcome = store
                .insert(NewEntry::from_image(png, 100 + i, 40, "image/png".into()))
                .unwrap();
            let InsertOutcome::Inserted(id) = outcome else {
                panic!()
            };
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
        store
            .insert(NewEntry::from_text("你好世界".into()))
            .unwrap();
        store
            .insert(NewEntry::from_text("部署 dev 环境".into()))
            .unwrap();
        store
            .insert(NewEntry::from_text("hello world".into()))
            .unwrap();

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
        // 短查询不得误伤无关条目（「ti」不是 hello / 你好 的子串或拼音）
        assert!(store.search("ti", None, 10).is_empty());
        // 无命中
        assert!(store.search("不存在的词条", None, 10).is_empty());
        // like 转义：% 字面量不爆炸
        assert!(store.search("%", None, 10).is_empty());
    }

    #[test]
    fn search_with_kind_filter() {
        let (store, _keep) = temp_store();
        store
            .insert(NewEntry::from_text("text item".into()))
            .unwrap();

        assert_eq!(store.search("", Some(EntryKind::Text), 10).len(), 1);
        assert_eq!(store.search("", Some(EntryKind::Image), 10).len(), 0);
        assert_eq!(store.search("text", Some(EntryKind::Image), 10).len(), 0);
    }

    #[test]
    fn get_text_and_delete() {
        let (store, _keep) = temp_store();
        let outcome = store
            .insert(NewEntry::from_text("完整正文内容".into()))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else {
            panic!()
        };

        assert_eq!(store.get_text(id).as_deref(), Some("完整正文内容"));
        assert!(store
            .update_text(id, "改后的正文".into())
            .unwrap());
        assert_eq!(store.get_text(id).as_deref(), Some("改后的正文"));
        assert_eq!(store.list_recent(10)[0].preview, "改后的正文");
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
    fn v6_to_v7_migration_adds_ocr_boxes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipx.db");

        {
            // 手工构造 v6 库（有 source_app/ocr_text，无 ocr_boxes）
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE entries (id INTEGER PRIMARY KEY, kind INTEGER NOT NULL, preview TEXT NOT NULL,
                    content_hash TEXT NOT NULL, pinned INTEGER NOT NULL DEFAULT 0,
                    ocr_state INTEGER NOT NULL DEFAULT 2, created_ms INTEGER NOT NULL,
                    source_app TEXT NOT NULL DEFAULT '');
                CREATE UNIQUE INDEX idx_entries_hash ON entries(content_hash);
                CREATE INDEX idx_entries_order ON entries(pinned DESC, created_ms DESC);
                CREATE TABLE payloads (entry_id INTEGER PRIMARY KEY REFERENCES entries(id) ON DELETE CASCADE,
                    full_text TEXT, html TEXT, image_blob BLOB, image_w INTEGER, image_h INTEGER, image_mime TEXT,
                    thumb_blob BLOB, thumb_w INTEGER, thumb_h INTEGER, file_paths_json TEXT,
                    pinyin_blob TEXT, ocr_text TEXT);
                CREATE VIRTUAL TABLE entries_fts USING fts5(entry_id UNINDEXED, text, ocr);
                INSERT INTO entries (kind, preview, content_hash, created_ms) VALUES (1, 'img', 'img1', 1000);
                INSERT INTO payloads (entry_id, ocr_text, pinyin_blob) VALUES (1, '老图文字', 'laotuwenzi ltwz');
                INSERT INTO entries_fts (entry_id, ocr) VALUES (1, '老图文字');
                PRAGMA user_version = 6;
                "#,
            )
            .unwrap();
        }

        let store = Store::open(&path, StoreLimits::default()).unwrap();
        // 旧 OCR 文本仍可查，框为 NULL（= 未处理，回填会捞到补框）
        let ocr = store.get_ocr(1).unwrap();
        assert_eq!(ocr.state, 2);
        assert_eq!(ocr.text.as_deref(), Some("老图文字"));
        assert!(ocr.boxes.is_none());
        assert_eq!(store.list_ocr_backfill(100), vec![1]);
        assert_eq!(store.search_ex("老图", None, None, true, 10).len(), 1);
        // 新框可写回
        store.set_ocr_result(
            1,
            "老图文字".into(),
            r#"[{"text":"老图文字","x":0.0,"y":0.0,"w":1.0,"h":0.2}]"#.into(),
        );
        assert!(store
            .get_ocr(1)
            .unwrap()
            .boxes
            .as_deref()
            .unwrap_or_default()
            .contains("老图文字"));
    }

    #[test]
    fn trim_keeps_recent_within_max() {
        // max_items=2000：seed 2050 条应裁剪到 2000
        let (store, _keep) = temp_store_with_limits(StoreLimits {
            max_items: 2000,
            max_image_items: 150,
        });
        store.seed(2050).unwrap();
        assert_eq!(store.list_recent(3000).len(), 2000);
    }

    #[test]
    fn image_roundtrip_thumb_and_lazy_load() {
        let (store, _keep) = temp_store();
        let png = tiny_png(320, 120);
        let outcome = store
            .insert(NewEntry::from_image(
                png.clone(),
                320,
                120,
                "image/png".into(),
            ))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else {
            panic!()
        };

        let thumb = store.get_thumb(id).unwrap();
        assert!(!thumb.blob.is_empty());
        assert_eq!((thumb.w, thumb.h), (64, 24));

        let img = store.get_image(id).unwrap();
        assert_eq!(img.blob, png);
        assert_eq!((img.w, img.h), (320, 120));
        assert_eq!(img.mime, "image/png");

        // 文本条目没有图片负载
        let t = store.insert(NewEntry::from_text("纯文本".into())).unwrap();
        let InsertOutcome::Inserted(tid) = t else {
            panic!()
        };
        assert!(store.get_thumb(tid).is_none());
        assert!(store.get_image(tid).is_none());
    }

    #[test]
    fn file_image_entry_stores_thumbnail() {
        let (store, _keep) = temp_store();
        let dir = std::env::temp_dir().join("clipx-store-file-thumb");
        let _ = std::fs::create_dir_all(&dir);
        let png_path = dir.join("a.png");
        std::fs::write(&png_path, tiny_png(80, 40)).unwrap();
        let outcome = store
            .insert(NewEntry::from_files(vec![png_path.to_string_lossy().into()]))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else {
            panic!()
        };
        let thumb = store.get_thumb(id).expect("file image thumb");
        assert!(!thumb.blob.is_empty());
        assert_eq!((thumb.w, thumb.h), (64, 32));
        let thumbs = store.list_thumbs(10);
        assert!(thumbs.iter().any(|(i, _)| *i == id));
        let _ = std::fs::remove_file(&png_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn ocr_text_flow_and_search() {
        let (store, _keep) = temp_store();
        let outcome = store
            .insert(NewEntry::from_image(
                tiny_png(100, 40),
                100,
                40,
                "image/png".into(),
            ))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else {
            panic!()
        };

        // 初始：未处理 → 进入回填列表
        assert_eq!(store.list_ocr_backfill(100), vec![id]);
        store.mark_ocr_pending(id);
        assert_eq!(store.get_ocr(id).unwrap().state, 1);
        // pending（中断态）也应回填
        assert_eq!(store.list_ocr_backfill(100), vec![id]);

        store.set_ocr_result(id, "你好世界 hello".into(), "[]".into());
        let ocr = store.get_ocr(id).unwrap();
        assert_eq!(ocr.state, 2);
        assert_eq!(ocr.text.as_deref(), Some("你好世界 hello"));
        assert_eq!(ocr.boxes.as_deref(), Some("[]"));
        // 完成后不再回填
        assert!(store.list_ocr_backfill(100).is_empty());

        // 默认浅搜只扫 preview/拼音；OCR 需深搜开关
        assert_eq!(store.search("世界", None, 10).len(), 0);
        assert_eq!(store.search_ex("世界", None, None, true, 10).len(), 1);
        assert_eq!(store.search_ex("shijie", None, None, true, 10).len(), 1);
        assert_eq!(store.search_ex("hello", None, None, true, 10).len(), 1);
        // 图片 kind 过滤仍生效
        assert_eq!(
            store.search_ex("世界", Some(EntryKind::Image), None, true, 10).len(),
            1
        );
        assert_eq!(
            store.search_ex("世界", Some(EntryKind::Text), None, true, 10).len(),
            0
        );
    }

    #[test]
    fn ocr_empty_text_marks_done_without_fts() {
        let (store, _keep) = temp_store();
        let outcome = store
            .insert(NewEntry::from_image(
                tiny_png(10, 10),
                10,
                10,
                "image/png".into(),
            ))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else {
            panic!()
        };

        store.set_ocr_result(id, String::new(), "[]".into());
        let ocr = store.get_ocr(id).unwrap();
        assert_eq!(ocr.state, 2);
        assert_eq!(ocr.boxes.as_deref(), Some("[]"));
        assert!(store.list_ocr_backfill(100).is_empty());
    }

    #[test]
    fn ocr_boxes_roundtrip_and_text_path_keeps_boxes() {
        let (store, _keep) = temp_store();
        let outcome = store
            .insert(NewEntry::from_image(
                tiny_png(100, 40),
                100,
                40,
                "image/png".into(),
            ))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else {
            panic!()
        };

        // 纯文本路径不碰框列：boxes 保持 NULL（= 未处理，回填仍会捞到）
        store.set_ocr_text(id, "第一行 hello".into());
        let ocr = store.get_ocr(id).unwrap();
        assert_eq!(ocr.state, 2);
        assert!(ocr.boxes.is_none());
        assert_eq!(store.list_ocr_backfill(100), vec![id]);

        // 新路径：文本 + 行框 JSON 一起落库，回填收敛
        let boxes = r#"[{"text":"第一行","x":0.1,"y":0.1,"w":0.5,"h":0.2}]"#;
        store.set_ocr_result(id, "第一行 hello".into(), boxes.into());
        let ocr = store.get_ocr(id).unwrap();
        assert_eq!(ocr.state, 2);
        assert_eq!(ocr.boxes.as_deref(), Some(boxes));
        assert!(store.list_ocr_backfill(100).is_empty());

        // 空文本也照写框标记（不进 FTS），回填照样收敛
        store.set_ocr_result(id, String::new(), "[]".into());
        let ocr = store.get_ocr(id).unwrap();
        assert_eq!(ocr.state, 2);
        assert_eq!(ocr.boxes.as_deref(), Some("[]"));
        assert!(store.list_ocr_backfill(100).is_empty());
    }

    #[test]
    fn image_prune_keeps_recent_images_only() {
        let (store, _keep) = temp_store_with_limits(StoreLimits {
            max_items: 2000,
            max_image_items: 3,
        });
        let mut ids = Vec::new();
        for i in 0..5 {
            // 每张内容不同（尺寸递增）→ 不同 hash
            let png = tiny_png(100 + i, 40);
            let outcome = store
                .insert(NewEntry::from_image(png, 100 + i, 40, "image/png".into()))
                .unwrap();
            let InsertOutcome::Inserted(id) = outcome else {
                panic!()
            };
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

    #[test]
    fn richtext_roundtrip_search_and_paste_payload() {
        let (store, _keep) = temp_store();
        let outcome = store
            .insert(NewEntry::from_rich_text(
                "格式化标题 hello".into(),
                "<b>格式化标题 hello</b>".into(),
            ))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else {
            panic!()
        };

        let meta = &store.list_recent(10)[0];
        assert_eq!(meta.kind, EntryKind::RichText);
        // 文本与 HTML 均可取回
        assert_eq!(store.get_text(id).as_deref(), Some("格式化标题 hello"));
        assert_eq!(
            store.get_html(id).as_deref(),
            Some("<b>格式化标题 hello</b>")
        );
        // 搜索走文本投影（子串 + 拼音）
        assert_eq!(store.search("标题", None, 10).len(), 1);
        assert_eq!(store.search("geshi", None, 10).len(), 1);
        // 「文本」筛选涵盖富文本
        assert_eq!(store.search("", Some(EntryKind::Text), 10).len(), 1);
        assert_eq!(store.search("", Some(EntryKind::Files), 10).len(), 0);
    }

    #[test]
    fn toggle_pin_sorts_first_and_survives_trim() {
        let (store, _keep) = temp_store_with_limits(StoreLimits {
            max_items: 3,
            max_image_items: 150,
        });
        for i in 0..4 {
            store
                .insert(NewEntry::from_text(format!("条目{i}")))
                .unwrap();
            thread::sleep(Duration::from_millis(5));
        }
        // 最旧的条目 0 置顶
        let oldest = store.list_recent(10).last().unwrap().clone();
        assert!(store.toggle_pin(oldest.id).unwrap());
        // 再插一条触发裁剪到 3 条
        store.insert(NewEntry::from_text("新条目".into())).unwrap();

        let rows = store.list_recent(10);
        assert_eq!(rows.len(), 3);
        // 置顶条目在首位且未被裁剪
        assert_eq!(rows[0].id, oldest.id);
        assert!(rows[0].pinned);
        // 取消置顶
        assert!(!store.toggle_pin(oldest.id).unwrap());
        assert!(store.toggle_pin(99999).is_none());
    }

    #[test]
    fn import_batch_dedups_and_preserves_ocr_and_time() {
        let (store, _keep) = temp_store();
        // clipx 已有的内容（应被视为重复跳过）
        store.insert(NewEntry::from_text("已存在".into())).unwrap();

        let png = tiny_png(120, 60);
        let rows = vec![
            MigrationRow {
                entry: NewEntry::from_text("已存在".into()),
                created_ms: 1000,
                ocr_text: None,
            },
            MigrationRow {
                entry: NewEntry::from_text("老库文本".into()),
                created_ms: 2000,
                ocr_text: None,
            },
            MigrationRow {
                entry: NewEntry::from_image(png, 120, 60, "image/png".into()),
                created_ms: 3000,
                ocr_text: Some("老图 OCR 结果".into()),
            },
        ];
        let stats = store.import_batch(rows).unwrap();
        assert_eq!(stats.inserted, 2);
        assert_eq!(stats.skipped_dup, 1);

        // 原时间戳保留：老库文本（2000）比已存在（now）老 → 排在其后
        let metas = store.list_recent(10);
        let old_text = metas.iter().find(|m| m.preview == "老库文本").unwrap();
        assert_eq!(old_text.created_ms, 2000);

        // 老图 OCR 直达完成态且可搜
        let img_meta = metas.iter().find(|m| m.kind == EntryKind::Image).unwrap();
        assert_eq!(store.get_ocr(img_meta.id).unwrap().state, 2);
        assert_eq!(
            store.get_ocr(img_meta.id).unwrap().text.as_deref(),
            Some("老图 OCR 结果")
        );
        assert_eq!(store.search_ex("老图", None, None, true, 10).len(), 1);
    }

    #[test]
    fn source_app_filter_and_json_roundtrip() {
        let (store, dir) = temp_store();
        store
            .insert(NewEntry::from_text("来自记事本".into()).with_source("notepad"))
            .unwrap();
        store
            .insert(NewEntry::from_text("来自浏览器".into()).with_source("msedge"))
            .unwrap();
        assert_eq!(store.list_sources(), vec!["msedge", "notepad"]);
        assert_eq!(
            store
                .search_ex("", None, Some("notepad"), false, 10)
                .len(),
            1
        );
        assert_eq!(
            store.search_ex("浏览器", None, Some("msedge"), false, 10).len(),
            1
        );
        let path = dir.path().join("roundtrip.json");
        assert_eq!(store.export_json(&path).unwrap(), 2);
        let (store2, _keep) = temp_store();
        let st = store2.import_json(&path).unwrap();
        assert_eq!(st.inserted, 2);
        assert_eq!(store2.list_sources(), vec!["msedge", "notepad"]);
    }
}
