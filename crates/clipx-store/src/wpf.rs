//! WPF ClipboardX 历史库读取（clipboard_history 表 → MigrationRow）。
//! 只读打开源库；迁移在一次性命令中完成，不与运行期 store 线程共享连接。
//!
//! WPF schema（ClipboardHistoryStore.cs）：
//! entry_type 0=Text 1=Image 2=Files；image_blob 为 PNG bytes；
//! file_paths_json 为 JSON 数组；copied_at_ms 为 Unix 毫秒。
//! WPF 版不持久化 OCR 文本与置顶状态，二者迁移后为空。

use anyhow::{Context, Result};

use crate::MigrationRow;

struct RawRow {
    entry_type: i64,
    text: Option<String>,
    image_blob: Option<Vec<u8>>,
    w: i64,
    h: i64,
    files_json: Option<String>,
    copied_at_ms: i64,
}

/// 读取全部可迁移行。空内容/缺 blob 的坏行跳过并计入返回的第二个值。
pub fn read_rows(path: &std::path::Path) -> Result<(Vec<MigrationRow>, usize)> {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("打开 WPF 数据库失败: {}", path.display()))?;

    let mut stmt = conn
        .prepare(
            "SELECT entry_type, text_content, image_blob, image_w, image_h, file_paths_json, copied_at_ms
             FROM clipboard_history ORDER BY copied_at_ms",
        )
        .context("读取 clipboard_history 表失败（不是 WPF 版数据库？）")?;

    let raw: Vec<RawRow> = stmt
        .query_map([], |r| {
            Ok(RawRow {
                entry_type: r.get(0)?,
                text: r.get(1)?,
                image_blob: r.get(2)?,
                w: r.get(3)?,
                h: r.get(4)?,
                files_json: r.get(5)?,
                copied_at_ms: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("遍历 WPF 历史行失败")?;

    let mut rows = Vec::with_capacity(raw.len());
    let mut skipped = 0usize;
    for r in raw {
        match convert(r) {
            Some(m) => rows.push(m),
            None => skipped += 1,
        }
    }
    Ok((rows, skipped))
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
        ocr_text: None,
    })
}
