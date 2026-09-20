//! WPF ClipboardX 历史库读取（clipboard_history 表 → MigrationRow）。
//! 只读打开源库；迁移在一次性命令中完成，不与运行期 store 线程共享连接。
//!
//! WPF schema（ClipboardHistoryStore.cs）：
//! entry_type 0=Text 1=Image 2=Files；image_blob 为 PNG bytes；
//! file_paths_json 为 JSON 数组；copied_at_ms 为 Unix 毫秒；
//! ocr_text 为 WPF 已做过的图片 OCR（实测只在 entry_type=1 上有值）。
//! WPF 版不持久化置顶状态，迁移后为空。

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::MigrationRow;

/// 迁移读取的列（一次性与分批共用；`rowid` 作游标，不依赖 id 连续）。
const COLUMNS: &str = "rowid, entry_type, text_content, image_blob, image_w, image_h, \
                       file_paths_json, copied_at_ms, ocr_text";

struct RawRow {
    rowid: i64,
    entry_type: i64,
    text: Option<String>,
    image_blob: Option<Vec<u8>>,
    w: i64,
    h: i64,
    files_json: Option<String>,
    copied_at_ms: i64,
    ocr_text: Option<String>,
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        rowid: r.get(0)?,
        entry_type: r.get(1)?,
        text: r.get(2)?,
        image_blob: r.get(3)?,
        w: r.get(4)?,
        h: r.get(5)?,
        files_json: r.get(6)?,
        copied_at_ms: r.get(7)?,
        ocr_text: r.get(8)?,
    })
}

/// 只读打开（WPF 库可能正被 WPF 版使用，绝不写它）。
fn open_readonly(path: &std::path::Path) -> Result<Connection> {
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("打开 WPF 数据库失败: {}", path.display()))
}

/// 读取全部可迁移行。空内容/缺 blob 的坏行跳过并计入返回的第二个值。
/// 大库请改用 [`BatchReader`]（本函数会把整库读进内存，只适合 CLI 一次性进程）。
pub fn read_rows(path: &std::path::Path) -> Result<(Vec<MigrationRow>, usize)> {
    let mut reader = BatchReader::open(path)?;
    let mut rows = Vec::new();
    let mut bad = 0usize;
    loop {
        let batch = reader.next_batch(usize::MAX, usize::MAX)?;
        bad += batch.bad;
        rows.extend(batch.rows);
        if batch.eof {
            break;
        }
    }
    Ok((rows, bad))
}

/// 一次分批读取的结果。
pub struct WpfBatch {
    pub rows: Vec<MigrationRow>,
    /// 本批里内容为空/缺 blob 的坏行数
    pub bad: usize,
    /// 已读到源库末尾
    pub eof: bool,
}

/// 游标式分批读取器：GUI 首启自动导入用它逐批消费，
/// 既避免整库（实测 197MB / 7002 条，图片 blob 47MB）一次性驻留内存，
/// 也让 store 线程能在批次之间插空处理 UI 的查询请求。
pub struct BatchReader {
    conn: Connection,
    cursor: i64,
}

impl BatchReader {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = open_readonly(path)?;
        // 探测表结构：不是 WPF 库时给出明确错误（而不是后面才报）。
        conn.query_row("SELECT COUNT(*) FROM clipboard_history", [], |_| Ok(()))
            .context("读取 clipboard_history 表失败（不是 WPF 版数据库？）")?;
        Ok(Self { conn, cursor: 0 })
    }

    /// 源库总行数（含坏行），用于进度提示。
    pub fn total(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM clipboard_history", [], |r| r.get(0))
            .context("统计 WPF 历史行数失败")
    }

    /// 取下一批：最多 `max_rows` 行，且图片 blob 累计不超过 `max_bytes`
    /// （单行就超限时仍会返回它，保证游标必然前进）。
    pub fn next_batch(&mut self, max_rows: usize, max_bytes: usize) -> Result<WpfBatch> {
        let limit = max_rows.min(i64::MAX as usize) as i64;
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM clipboard_history WHERE rowid > ?1 ORDER BY rowid LIMIT ?2"
        ))?;
        let raw: Vec<RawRow> = stmt
            .query_map(rusqlite::params![self.cursor, limit], row_from)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("遍历 WPF 历史行失败")?;
        let fetched = raw.len();

        let mut rows = Vec::with_capacity(fetched);
        let mut bad = 0usize;
        let mut bytes = 0usize;
        let mut consumed = self.cursor;
        let mut truncated = false;
        for r in raw {
            bytes = bytes.saturating_add(r.image_blob.as_ref().map_or(0, |b| b.len()));
            consumed = r.rowid;
            match convert(r) {
                Some(m) => rows.push(m),
                None => bad += 1,
            }
            // 这一行已收下（游标已前进）；再收下去会超内存预算，剩下的留到下批。
            if bytes >= max_bytes && consumed != self.cursor {
                truncated = true;
                break;
            }
        }
        self.cursor = consumed;
        Ok(WpfBatch {
            rows,
            bad,
            eof: !truncated && fetched < max_rows,
        })
    }
}

fn convert(r: RawRow) -> Option<MigrationRow> {
    use clipx_core::NewEntry;
    let entry = match r.entry_type {
        1 => {
            let blob = r.image_blob.filter(|b| !b.is_empty())?;
            NewEntry::from_image(
                blob,
                r.w.max(0) as u32,
                r.h.max(0) as u32,
                "image/png".into(),
            )
        }
        2 => {
            let paths: Vec<String> = r
                .files_json
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
                .filter(|p: &Vec<String>| !p.is_empty())?;
            NewEntry::from_files(paths)
        }
        _ => {
            let text = r.text.filter(|t| !t.trim().is_empty())?;
            NewEntry::from_text(text)
        }
    };
    Some(MigrationRow {
        entry,
        created_ms: r.copied_at_ms,
        // WPF 已做过的图片 OCR：带上它，迁移后图片里的文字可直接被搜到
        // （否则要等 clipx 重新 OCR 一遍，且 WPF 的识别结果不一定能复现）。
        ocr_text: r.ocr_text.filter(|t| !t.trim().is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipx_core::EntryKind;

    const SCHEMA: &str = "CREATE TABLE clipboard_history (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         entry_type INTEGER NOT NULL,
         text_content TEXT,
         image_blob BLOB,
         image_w INTEGER DEFAULT 0,
         image_h INTEGER DEFAULT 0,
         file_paths_json TEXT,
         copied_at_ms INTEGER NOT NULL,
         ocr_text TEXT);";

    /// 假 PNG 头（够 `convert` 判非空；缩略图解码失败会自然退化为空，不影响迁移）。
    const FAKE_PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    /// 5 行：文本 / 坏行（全空白）/ 图片（带 OCR）/ 文件 / 文本。
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipboard_history.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO clipboard_history (entry_type, text_content, copied_at_ms) VALUES (0, '第一条', 1000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO clipboard_history (entry_type, text_content, copied_at_ms) VALUES (0, '   ', 2000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO clipboard_history (entry_type, image_blob, image_w, image_h, copied_at_ms, ocr_text)
             VALUES (1, ?1, 1, 1, 3000, '图上文字')",
            rusqlite::params![FAKE_PNG],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO clipboard_history (entry_type, file_paths_json, copied_at_ms)
             VALUES (2, '[\"C:\\\\tmp\\\\a.txt\"]', 4000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO clipboard_history (entry_type, text_content, copied_at_ms) VALUES (0, '第五条', 5000)",
            [],
        )
        .unwrap();
        drop(conn);
        (dir, path)
    }

    #[test]
    fn batch_reader_pages_by_cursor_and_counts_bad_rows() {
        let (_dir, path) = fixture();
        let mut reader = BatchReader::open(&path).unwrap();
        assert_eq!(reader.total().unwrap(), 5);

        let b1 = reader.next_batch(2, usize::MAX).unwrap();
        assert_eq!(b1.rows.len(), 1, "第一条是文本");
        assert_eq!(b1.bad, 1, "全空白的文本行算坏行");
        assert!(!b1.eof);

        let b2 = reader.next_batch(2, usize::MAX).unwrap();
        assert_eq!(b2.rows.len(), 2, "图片 + 文件");
        assert_eq!(b2.rows[0].entry.kind, EntryKind::Image);
        assert_eq!(b2.rows[0].created_ms, 3000, "原时间戳必须保留");
        assert_eq!(b2.rows[1].entry.kind, EntryKind::Files);
        assert!(!b2.eof);

        let b3 = reader.next_batch(2, usize::MAX).unwrap();
        assert_eq!(b3.rows.len(), 1);
        assert_eq!(b3.rows[0].created_ms, 5000);
        assert!(b3.eof, "取不满一批即为末尾");

        let b4 = reader.next_batch(2, usize::MAX).unwrap();
        assert!(b4.rows.is_empty() && b4.eof);
    }

    #[test]
    fn image_rows_keep_wpf_ocr_text() {
        let (_dir, path) = fixture();
        let (rows, bad) = read_rows(&path).unwrap();
        assert_eq!(bad, 1);
        let img = rows
            .iter()
            .find(|r| r.entry.kind == EntryKind::Image)
            .expect("图片行");
        assert_eq!(
            img.ocr_text.as_deref(),
            Some("图上文字"),
            "WPF 已做过的 OCR 必须随迁移带走（PRD 数据迁移含 ocr_text）"
        );
        assert!(
            rows.iter()
                .filter(|r| r.entry.kind != EntryKind::Image)
                .all(|r| r.ocr_text.is_none()),
            "非图片条目不该带 OCR"
        );
    }

    #[test]
    fn byte_budget_truncates_batches_without_losing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipboard_history.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        for i in 0..3i64 {
            conn.execute(
                "INSERT INTO clipboard_history (entry_type, image_blob, image_w, image_h, copied_at_ms)
                 VALUES (1, ?1, 1, 1, ?2)",
                rusqlite::params![vec![0u8; 10], 1000 + i],
            )
            .unwrap();
        }
        drop(conn);

        // 每张图 10 字节、预算 15 字节：一批最多两张，且游标必须继续前进。
        let mut reader = BatchReader::open(&path).unwrap();
        let mut total = 0usize;
        let mut batches = 0usize;
        loop {
            let b = reader.next_batch(100, 15).unwrap();
            total += b.rows.len();
            batches += 1;
            assert!(batches < 10, "批次异常多：游标可能没前进");
            if b.eof {
                break;
            }
        }
        assert_eq!(total, 3, "分批不能丢行");
        assert!(batches >= 2, "15 字节预算装不下两张 10 字节图");
    }

    #[test]
    fn read_rows_equals_batched_read() {
        let (_dir, path) = fixture();
        let (all, bad_all) = read_rows(&path).unwrap();
        let mut reader = BatchReader::open(&path).unwrap();
        let mut batched = Vec::new();
        let mut bad = 0usize;
        loop {
            let b = reader.next_batch(1, usize::MAX).unwrap();
            bad += b.bad;
            batched.extend(b.rows);
            if b.eof {
                break;
            }
        }
        assert_eq!(bad_all, bad);
        assert_eq!(all.len(), batched.len());
        for (a, b) in all.iter().zip(batched.iter()) {
            assert_eq!(a.created_ms, b.created_ms);
            assert_eq!(a.ocr_text, b.ocr_text);
            assert_eq!(a.entry.content_hash, b.entry.content_hash);
        }
    }
}
