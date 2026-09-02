use std::path::Path;
use std::sync::mpsc;
use std::thread;

use anyhow::{anyhow, bail, Result};
use clipx_core::{now_ms, EntryKind, EntryMeta, NewEntry, Payload};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

const SCHEMA_VERSION: i64 = 1;

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
    file_paths_json TEXT
);

CREATE VIRTUAL TABLE entries_fts USING fts5(
    entry_id UNINDEXED,
    text,
    pinyin,
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
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::Insert { entry, reply } => {
                            let _ = reply.send(handle_insert(&mut conn, entry));
                        }
                        Cmd::ListRecent { limit, reply } => {
                            let _ = reply.send(handle_list(&conn, limit));
                        }
                        Cmd::Seed { count, reply } => {
                            let _ = reply.send(handle_seed(&mut conn, count));
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
    if version != 0 {
        bail!("数据库 schema 版本 {version} 高于本程序支持的 {SCHEMA_VERSION}");
    }
    conn.execute_batch(SCHEMA_SQL)?;
    conn.execute_batch("PRAGMA user_version = 1;")?;
    Ok(())
}

fn handle_insert(conn: &mut Connection, entry: NewEntry) -> Result<InsertOutcome> {
    let tx = conn.transaction()?;
    let outcome = upsert_entry(&tx, &entry, now_ms())?;
    tx.commit()?;
    Ok(outcome)
}

fn handle_seed(conn: &mut Connection, count: usize) -> Result<usize> {
    let tx = conn.transaction()?;
    let base = now_ms();
    for i in 0..count {
        let text = format!("Seed 条目 #{i} — The quick brown fox jumps over the lazy dog 剪贴板压测数据 {i}");
        let entry = NewEntry::from_text(text);
        upsert_entry(&tx, &entry, base + i as i64)?;
    }
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
                "INSERT INTO payloads (entry_id, full_text) VALUES (?1, ?2)",
                params![id, full],
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
                "INSERT INTO payloads (entry_id, full_text, file_paths_json) VALUES (?1, ?2, ?3)",
                params![id, joined, json],
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
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, kind, preview, pinned, created_ms
         FROM entries ORDER BY pinned DESC, created_ms DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![limit], |r| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("clipx.db")).unwrap();
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
        assert_eq!(version, 1);

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
}
