use std::path::Path;
use std::sync::mpsc;
use std::thread;

use anyhow::{anyhow, bail, Result};
use clipx_core::{now_ms, EntryKind, EntryMeta, NewEntry, Payload};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

const SCHEMA_VERSION: i64 = 3;

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
    pinyin_blob     TEXT
);

-- 拼音走 payloads.pinyin_blob + LIKE 子串（对齐 WPF Contains 语义），
-- FTS 仅承担英文/数字词前缀匹配
CREATE VIRTUAL TABLE entries_fts USING fts5(
    entry_id UNINDEXED,
    text,
    ocr
);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted(i64),
    Bumped(i64),
}

enum Cmd {
    Insert { entry: NewEntry, reply: mpsc::Sender<Result<InsertOutcome>> },
    ListRecent { limit: i64, reply: mpsc::Sender<Vec<EntryMeta>> },
    Search { query: String, kind: Option<EntryKind>, limit: i64, reply: mpsc::Sender<Vec<EntryMeta>> },
    GetText { id: i64, reply: mpsc::Sender<Option<String>> },
    Delete { id: i64, reply: mpsc::Sender<bool> },
    Seed { count: usize, reply: mpsc::Sender<Result<usize>> },
}

#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<Cmd>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
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
                let max_items: i64 = 2000;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::Insert { entry, reply } => {
                            let _ = reply.send(handle_insert(&mut conn, entry, max_items));
                        }
                        Cmd::ListRecent { limit, reply } => {
                            let _ = reply.send(handle_list(&conn, limit));
                        }
                        Cmd::Search { query, kind, limit, reply } => {
                            let _ = reply.send(handle_search(&conn, &query, kind, limit));
                        }
                        Cmd::GetText { id, reply } => {
                            let _ = reply.send(handle_get_text(&conn, id));
                        }
                        Cmd::Delete { id, reply } => {
                            let _ = reply.send(handle_delete(&mut conn, id));
                        }
                        Cmd::Seed { count, reply } => {
                            let _ = reply.send(handle_seed(&mut conn, count, max_items));
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

fn handle_insert(conn: &mut Connection, entry: NewEntry, max_items: i64) -> Result<InsertOutcome> {
    let tx = conn.transaction()?;
    let outcome = upsert_entry(&tx, &entry, now_ms())?;
    trim_to_max(&tx, max_items)?;
    tx.commit()?;
    Ok(outcome)
}

fn trim_to_max(tx: &Transaction, max_items: i64) -> Result<()> {
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
    if count <= max_items {
        return Ok(());
    }
    tx.execute(
        "DELETE FROM entries_fts WHERE entry_id IN (
            SELECT id FROM entries WHERE id NOT IN (
                SELECT id FROM entries ORDER BY pinned DESC, created_ms DESC LIMIT ?1
            )
        )",
        params![max_items],
    )?;
    tx.execute(
        "DELETE FROM entries WHERE id NOT IN (
            SELECT id FROM entries ORDER BY pinned DESC, created_ms DESC LIMIT ?1
        )",
        params![max_items],
    )?;
    Ok(())
}

fn handle_seed(conn: &mut Connection, count: usize, max_items: i64) -> Result<usize> {
    let tx = conn.transaction()?;
    let base = now_ms();
    for i in 0..count {
        let text = format!("Seed 条目 #{i} — The quick brown fox jumps over the lazy dog 剪贴板压测数据 {i}");
        let entry = NewEntry::from_text(text);
        upsert_entry(&tx, &entry, base + i as i64)?;
    }
    trim_to_max(&tx, max_items)?;
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
        Payload::Image { blob, width, height, mime } => {
            tx.execute(
                "INSERT INTO payloads (entry_id, image_blob, image_w, image_h, image_mime)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, blob, width, height, mime],
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
        let store = Store::open(&dir.path().join("clipx.db")).unwrap();
        (store, dir)
    }

    fn temp_store_with_max(max: i64) -> (Store, tempfile::TempDir) {
        // 通过 seed + insert 验证 trim；max 固定 2000，这里用大批量 seed 测
        let (store, dir) = temp_store();
        store.seed((max + 50) as usize).unwrap();
        (store, dir)
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
            let store = Store::open(&path).unwrap();
            store.seed(300).unwrap();
            assert_eq!(store.list_recent(500).len(), 300);
        }

        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 3);

        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM entries_fts WHERE entries_fts MATCH 'fox'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 300);
        drop(conn);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_recent(500).len(), 300);
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

        let store = Store::open(&path).unwrap();
        // 迁移后拼音可查
        assert_eq!(store.search("nihaoshijie", None, 10).len(), 1);
        assert_eq!(store.search("nh", None, 10).len(), 1);
        // 旧数据仍在
        assert_eq!(store.list_recent(10).len(), 1);
    }

    #[test]
    fn trim_keeps_recent_within_max() {
        // seed 2050 条但 store 内 max_items=2000：应裁剪到 2000
        let (store, _keep) = temp_store_with_max(2000);
        assert_eq!(store.list_recent(3000).len(), 2000);
    }
}
