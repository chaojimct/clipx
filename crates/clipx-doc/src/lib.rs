//! 文档型文件预览（ADR-010）：类型判定 + 文本摘录 + 元数据卡。
//!
//! 纯库，无 UI 依赖。文件内容现读现放，读后即释，不缓存、不索引。
//! - 文本：头部 64KB 摘录（UTF-8/BOM/UTF-16LE/GBK 回退）。
//! - 表格（xlsx/xls/xlsm/xlsb/ods）：`calamine` 读首表转文本（前 30 行）。
//! - 文档（docx）/ 演示文稿（pptx）：zip 手解取 `w:t`/`a:t`（不引 writer 向大库）。
//! - PDF：`pdf-extract` 提文本（只提文本不渲染；扫描件无文本属正常）。
//! - Office/PDF 解析上限 32MB，超限与失败一律降级为元数据卡，预览永不 panic。

use std::io::Read;

/// 单文件头部读取上限：摘录只看头部，超限标"已截断"。
const HEAD_MAX_BYTES: u64 = 64 * 1024;
/// Office/PDF 全量解析上限（内存纪律：超限走元数据卡，不读内容）。
const DOC_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// 二进制嗅探窗口。
const SNIFF_LEN: usize = 8192;
/// 文件夹子项列出上限。
const DIR_LIST_MAX: usize = 30;
/// 表格摘录上限（行/列），超限截断并标注。
const TABLE_MAX_ROWS: usize = 30;
const TABLE_MAX_COLS: usize = 20;

/// 等宽字体展示的扩展名（代码/数据文件）；其余文本用比例字体。
const MONO_EXTS: &[&str] = &[
    "json", "jsonc", "xml", "yml", "yaml", "toml", "ini", "cfg", "conf", "csv", "tsv", "rs",
    "py", "js", "ts", "tsx", "jsx", "java", "c", "h", "hpp", "cpp", "cc", "cs", "go", "rb",
    "php", "swift", "kt", "kts", "sql", "sh", "ps1", "psm1", "bat", "cmd", "css", "scss",
    "less", "vue", "html", "htm", "svg", "srt", "vtt",
];
/// 比例字体展示的扩展名（文档/日志类）。
const TEXT_EXTS: &[&str] = &["txt", "md", "markdown", "mdown", "log"];

pub struct FileView {
    pub text: String,
    pub info: String,
    pub mono: bool,
}

/// 文件条目预览。
/// - 0 路径：空占位；多路径：保持路径清单（Step2 做按文件切换）；
/// - 单路径：文本摘录 / 文件夹清单 / 元数据卡 / 不存在提示。
pub fn preview(paths: &[String]) -> FileView {
    if paths.is_empty() {
        return FileView {
            text: "（空内容）".to_string(),
            info: "文件 · 0 项".to_string(),
            mono: false,
        };
    }
    if paths.len() > 1 {
        return FileView {
            text: paths.join("\n"),
            info: format!("文件 · {} 项", paths.len()),
            mono: false,
        };
    }
    preview_single(&paths[0])
}

fn preview_single(path: &str) -> FileView {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            return FileView {
                text: format!("（文件不存在，可能已移动或删除）\n{path}"),
                info: "文件 · 不存在".to_string(),
                mono: false,
            }
        }
    };
    if meta.is_dir() {
        return preview_dir(path);
    }
    let size = meta.len();
    let ext = extension(path);
    // 结构化文档优先走专用提取（失败内部降级为元数据卡）。
    match ext.as_str() {
        "xlsx" | "xls" | "xlsm" | "xlsb" | "ods" => return preview_table(path, size, &meta),
        "docx" => return preview_office(path, size, &meta, OfficeKind::Docx),
        "pptx" => return preview_office(path, size, &meta, OfficeKind::Pptx),
        "pdf" => return preview_pdf(path, size, &meta),
        _ => {}
    }
    let head = read_head(path);
    if ext_hit(&ext) || sniff_text(&head) {
        return preview_text(path, &ext, size, &head);
    }
    // 二进制：元数据卡（大小/类型/修改时间），内容不读。
    binary_card(path, &ext, size, &meta, "二进制文件，暂不支持内容预览")
}

/// 元数据卡：大小/类型/修改时间 + 一句备注，内容不读。
fn binary_card(
    path: &str,
    ext: &str,
    size: u64,
    meta: &std::fs::Metadata,
    note: &str,
) -> FileView {
    let kind = if ext.is_empty() {
        "未知类型文件".to_string()
    } else {
        format!("{} 文件", ext.to_uppercase())
    };
    FileView {
        text: format!("（{note}）\n{path}"),
        info: format!("文件 · {} · {kind} · {}", fmt_size(size), modified_ago(meta)),
        mono: false,
    }
}

/// 文件夹：子项清单（目录优先按名排序，超限截断）。
fn preview_dir(path: &str) -> FileView {
    let mut entries: Vec<(String, bool, u64)> = Vec::new();
    let mut total = 0usize;
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            total += 1;
            let ft = e.file_type().ok();
            let is_dir = ft.map(|t| t.is_dir()).unwrap_or(false);
            let size = if is_dir {
                0
            } else {
                e.metadata().map(|m| m.len()).unwrap_or(0)
            };
            entries.push((e.file_name().to_string_lossy().to_string(), is_dir, size));
        }
    }
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    let shown = entries.len().min(DIR_LIST_MAX);
    let mut lines = Vec::with_capacity(shown);
    for (name, is_dir, size) in entries.iter().take(shown) {
        if *is_dir {
            lines.push(format!("📁 {name}/"));
        } else {
            lines.push(format!("📄 {name} · {}", fmt_size(*size)));
        }
    }
    if lines.is_empty() {
        lines.push("（空文件夹）".to_string());
    }
    let mut info = format!("文件夹 · {total} 项");
    if total > shown {
        info.push_str(&format!("（仅列前 {shown} 项）"));
    }
    FileView {
        text: lines.join("\n"),
        info,
        mono: false,
    }
}

/// 文本摘录：BOM/UTF-16LE 解码，其余按 UTF-8（失败则 lossy，保证永不 panic）。
fn preview_text(path: &str, ext: &str, size: u64, head: &[u8]) -> FileView {
    let truncated = size > head.len() as u64;
    let mut text = decode_text(head);
    if text.trim().is_empty() {
        text = "（空文件）".to_string();
    }
    let name = file_name(path);
    let mut info = format!(
        "文件 · {name} · {} · {} 行",
        fmt_size(size),
        text.lines().count()
    );
    if truncated {
        info.push_str("（仅头部预览）");
    }
    FileView {
        text,
        info,
        mono: MONO_EXTS.contains(&ext),
    }
}

/// 表格摘录：首表转 TSV 文本（等宽展示）。失败/超限降级元数据卡。
fn preview_table(path: &str, size: u64, meta: &std::fs::Metadata) -> FileView {
    let ext = extension(path);
    let name = file_name(path);
    if size > DOC_MAX_BYTES {
        return binary_card(path, &ext, size, meta, "表格过大，仅显示信息");
    }
    let mut wb = match calamine::open_workbook_auto(path) {
        Ok(w) => w,
        Err(_) => return binary_card(path, &ext, size, meta, "表格解析失败，仅显示信息"),
    };
    use calamine::Reader;
    let names = wb.sheet_names();
    if names.is_empty() {
        return FileView {
            text: "（空表格）".to_string(),
            info: format!("表格 · {name} · {}", fmt_size(size)),
            mono: true,
        };
    }
    let sheet = names[0].clone();
    let range = wb.worksheet_range_at(0).and_then(|r| r.ok());
    let Some(range) = range else {
        return FileView {
            text: "（空表格）".to_string(),
            info: format!("表格 · {sheet} · {}", fmt_size(size)),
            mono: true,
        };
    };
    let (h, w) = (range.height(), range.width());
    if h == 0 || w == 0 {
        return FileView {
            text: "（空表格）".to_string(),
            info: format!("表格 · {sheet} · {}", fmt_size(size)),
            mono: true,
        };
    }
    let mut lines = Vec::new();
    for row in range.rows().take(TABLE_MAX_ROWS) {
        // 行尾空单元格截掉，稀疏表不拖出长串制表符。
        let mut cells: Vec<String> =
            row.iter().take(TABLE_MAX_COLS).map(fmt_cell).collect();
        while cells.last().is_some_and(|c| c.is_empty()) {
            cells.pop();
        }
        lines.push(cells.join("\t"));
    }
    let mut text = lines.join("\n");
    if text.trim().is_empty() {
        text = "（空表格）".to_string();
    }
    let mut info = format!("表格 · {sheet} · {h}行×{w}列 · {}", fmt_size(size));
    if h > TABLE_MAX_ROWS {
        info.push_str(&format!("（仅前{TABLE_MAX_ROWS}行）"));
    }
    if names.len() > 1 {
        info.push_str(&format!("（共{}表）", names.len()));
    }
    FileView {
        text,
        info,
        mono: true,
    }
}

fn fmt_cell(c: &calamine::Data) -> String {
    match c {
        calamine::Data::Empty => String::new(),
        calamine::Data::String(s) => s.to_string(),
        calamine::Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        calamine::Data::Bool(b) => {
            if *b {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        _ => String::new(),
    }
}

#[derive(Clone, Copy)]
enum OfficeKind {
    Docx,
    Pptx,
}

/// 文档/演示文稿摘录：docx 取 word/document.xml 的 w:t，
/// pptx 按页码枚举 slides 取 a:t。失败降级元数据卡。
fn preview_office(
    path: &str,
    size: u64,
    meta: &std::fs::Metadata,
    kind: OfficeKind,
) -> FileView {
    let ext = extension(path);
    let name = file_name(path);
    let label = match kind {
        OfficeKind::Docx => "文档",
        OfficeKind::Pptx => "演示文稿",
    };
    if size > DOC_MAX_BYTES {
        return binary_card(path, &ext, size, meta, "文档过大，仅显示信息");
    }
    match read_office_text(path, kind) {
        Some((raw, pages)) => {
            let mut text = collapse_blank_lines(&raw).trim().to_string();
            if text.is_empty() {
                text = "（未提取到正文）".to_string();
            }
            let n = text.chars().count();
            let info = match pages {
                Some(p) => format!("{label} · {name} · {p}页 · {} · {n}字", fmt_size(size)),
                None => format!("{label} · {name} · {} · {n}字", fmt_size(size)),
            };
            FileView {
                text,
                info,
                mono: false,
            }
        }
        None => binary_card(path, &ext, size, meta, "文档解析失败，仅显示信息"),
    }
}

fn read_office_text(path: &str, kind: OfficeKind) -> Option<(String, Option<usize>)> {
    let f = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(f).ok()?;
    match kind {
        OfficeKind::Docx => {
            let xml = read_zip_entry(&mut zip, "word/document.xml")?;
            Some((scan_ooxml_text(&xml), None))
        }
        OfficeKind::Pptx => {
            // 按页码数字排序（字典序会把 slide10 排到 slide2 前）。
            let mut slides: Vec<(usize, String)> = Vec::new();
            for i in 0..zip.len() {
                let ename = zip.by_index(i).ok()?.name().to_string();
                if let Some(n) = parse_slide_name(&ename) {
                    slides.push((n, ename));
                }
            }
            if slides.is_empty() {
                return None;
            }
            slides.sort();
            let n = slides.len();
            let mut out = String::new();
            for (i, (_, ename)) in slides.iter().enumerate() {
                let xml = read_zip_entry(&mut zip, ename)?;
                if i > 0 {
                    out.push_str("\n\n");
                }
                out.push_str(&format!("—— 第{}页 ——\n", i + 1));
                out.push_str(&scan_ooxml_text(&xml));
            }
            Some((out, Some(n)))
        }
    }
}

fn parse_slide_name(name: &str) -> Option<usize> {
    name.strip_prefix("ppt/slides/slide")?
        .strip_suffix(".xml")?
        .parse()
        .ok()
}

fn read_zip_entry(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<Vec<u8>> {
    let f = zip.by_name(name).ok()?;
    // 炸弹包防护：条目解压上限 DOC_MAX_BYTES（逻辑层另有 48k 字符截断）。
    let mut buf = Vec::new();
    f.take(DOC_MAX_BYTES).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// OOXML/DrawingML 文本扫描：w:t 与 a:t 取文本，段落/换行转 `\n`，tab 转 `\t`。
/// 手写扫描器（不引 quick-xml）：只认标签名、属性忽略；实体做最小解码。
/// 输入非良构 XML 也永不 panic（截断处直接丢弃）。
fn scan_ooxml_text(xml: &[u8]) -> String {
    let s = String::from_utf8_lossy(xml);
    let b = s.as_bytes();
    let mut out = String::new();
    let mut capture = false;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<' {
            let Some(end) = s[i..].find('>') else {
                break;
            };
            let tag = s[i + 1..i + end].trim();
            let closing = tag.starts_with('/');
            let name = tag
                .trim_start_matches('/')
                .split([' ', '\t', '\r', '\n'])
                .next()
                .unwrap_or("")
                .trim_end_matches('/');
            match (name, closing) {
                ("w:t", false) | ("a:t", false) => capture = true,
                ("w:t", true) | ("a:t", true) => capture = false,
                ("w:p", true) | ("a:p", true) => out.push('\n'),
                ("w:br", _) | ("a:br", _) => out.push('\n'),
                ("w:tab", _) | ("a:tab", _) => out.push('\t'),
                _ => {}
            }
            i += end + 1;
            continue;
        }
        let next = s[i..].find('<').map(|k| i + k).unwrap_or(b.len());
        if capture {
            push_entity_decoded(&mut out, &s[i..next]);
        }
        i = next;
    }
    out
}

/// XML 最小实体解码（w:t 内常见就这几个；未知实体原样保留）。
fn push_entity_decoded(out: &mut String, mut s: &str) {
    while let Some(k) = s.find('&') {
        out.push_str(&s[..k]);
        let rest = &s[k..];
        let Some(semi) = rest.find(';') else {
            out.push_str(rest);
            return;
        };
        match &rest[..=semi] {
            "&lt;" => out.push('<'),
            "&gt;" => out.push('>'),
            "&amp;" => out.push('&'),
            "&quot;" => out.push('"'),
            "&apos;" => out.push('\''),
            ent if ent.starts_with("&#x") || ent.starts_with("&#X") => {
                if let Ok(cp) = u32::from_str_radix(&ent[3..ent.len() - 1], 16) {
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                } else {
                    out.push_str(ent);
                }
            }
            ent if ent.starts_with("&#") => {
                if let Ok(cp) = ent[2..ent.len() - 1].parse::<u32>() {
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                } else {
                    out.push_str(ent);
                }
            }
            ent => out.push_str(ent),
        }
        s = &rest[semi + 1..];
    }
    out.push_str(s);
}

/// 连续 3+ 换行压成两个（空段落/页分隔会产生多余空行，保留一段间距）。
fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut nl = 0;
    for c in s.chars() {
        if c == '\n' {
            nl += 1;
            if nl <= 2 {
                out.push(c);
            }
        } else {
            nl = 0;
            out.push(c);
        }
    }
    out
}

/// PDF 文本提取（只提文本不渲染；扫描件无文本属正常）。失败降级元数据卡。
fn preview_pdf(path: &str, size: u64, meta: &std::fs::Metadata) -> FileView {
    let ext = extension(path);
    let name = file_name(path);
    if size > DOC_MAX_BYTES {
        return binary_card(path, &ext, size, meta, "PDF 过大，仅显示信息");
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return binary_card(path, &ext, size, meta, "PDF 读取失败，仅显示信息"),
    };
    match pdf_extract::extract_text_from_mem(&bytes) {
        Ok(t) => {
            let text = t.trim().to_string();
            if text.is_empty() {
                return FileView {
                    text: "（未提取到文本，可能是扫描图片型 PDF）".to_string(),
                    info: format!("PDF · {name} · {}", fmt_size(size)),
                    mono: false,
                };
            }
            let n = text.chars().count();
            FileView {
                text,
                info: format!("PDF · {name} · {} · {n}字", fmt_size(size)),
                mono: false,
            }
        }
        Err(_) => binary_card(path, &ext, size, meta, "PDF 解析失败，仅显示信息"),
    }
}

fn extension(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| path.to_string())
}

fn ext_hit(ext: &str) -> bool {
    !ext.is_empty() && (TEXT_EXTS.contains(&ext) || MONO_EXTS.contains(&ext))
}

/// 无 NUL 即当文本（覆盖无扩展名的脚本/日志；空文件也当文本）。
fn sniff_text(head: &[u8]) -> bool {
    head.iter().take(SNIFF_LEN).all(|&b| b != 0)
}

fn read_head(path: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(HEAD_MAX_BYTES).read_to_end(&mut buf);
    }
    buf
}

fn decode_text(head: &[u8]) -> String {
    // UTF-8 BOM
    if let Some(rest) = head.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    // UTF-16LE BOM（Windows 记事本常见）
    if let Some(rest) = head.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Ok(s) = std::str::from_utf8(head) {
        return s.to_owned();
    }
    // 非 UTF-8：中文 Windows 记事本默认存 GBK；解出来含 CJK 则采用，
    // 否则回落 lossy（保证永不 panic、不吐乱码占位）。
    let (gbk, _, _) = encoding_rs::GBK.decode(head);
    if gbk.chars().any(is_cjk) {
        return gbk.into_owned();
    }
    String::from_utf8_lossy(head).into_owned()
}

fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{3040}'..='\u{30FF}'
        | '\u{AC00}'..='\u{D7AF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF00}'..='\u{FFEF}')
}

fn fmt_size(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if n < KB {
        format!("{n} B")
    } else if n < MB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{:.1} MB", n as f64 / MB as f64)
    }
}

fn modified_ago(meta: &std::fs::Metadata) -> String {
    let now = clipx_core::now_ms();
    match meta.modified().ok().and_then(|t| t.elapsed().ok()) {
        Some(d) => {
            let ms = now.saturating_sub(d.as_millis() as i64);
            format!("{}修改", clipx_core::time::time_ago(ms, now))
        }
        None => "时间未知".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(name: &str, bytes: &[u8]) -> String {
        let dir = std::env::temp_dir().join("clipx-doc-test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().to_string()
    }

    #[test]
    fn empty_and_missing() {
        let v = preview(&[]);
        assert_eq!(v.text, "（空内容）");
        let v = preview(&["C:\\surely\\not\\exist\\x.txt".to_string()]);
        assert!(v.text.contains("文件不存在"), "{}", v.text);
        assert_eq!(v.info, "文件 · 不存在");
    }

    #[test]
    fn multi_paths_keep_list() {
        let v = preview(&["a".to_string(), "b".to_string()]);
        assert_eq!(v.text, "a\nb");
        assert_eq!(v.info, "文件 · 2 项");
    }

    #[test]
    fn text_excerpt_with_info() {
        let p = tmp_file("a.txt", "hello\n世界\n".as_bytes());
        let v = preview(&[p]);
        assert_eq!(v.text, "hello\n世界\n");
        assert!(!v.mono);
        assert!(v.info.contains("2 行"), "{}", v.info);
        assert!(v.info.contains("B"), "{}", v.info);
    }

    #[test]
    fn code_is_mono_and_binary_is_card() {
        let p = tmp_file("a.json", br#"{"a": 1}"#);
        let v = preview(&[p]);
        assert!(v.mono);
        let p = tmp_file("a.bin", &[0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF]);
        let v = preview(&[p]);
        assert!(!v.mono);
        assert!(v.text.contains("二进制"), "{}", v.text);
        assert!(v.info.contains("BIN 文件"), "{}", v.info);
    }

    #[test]
    fn bom_and_utf16le_decoded() {
        let mut utf8bom = vec![0xEF, 0xBB, 0xBF];
        utf8bom.extend_from_slice("中文".as_bytes());
        let p = tmp_file("bom.txt", &utf8bom);
        assert_eq!(preview(&[p]).text, "中文");

        let mut u16: Vec<u8> = vec![0xFF, 0xFE];
        for u in "中文".encode_utf16() {
            u16.extend_from_slice(&u.to_le_bytes());
        }
        let p = tmp_file("u16.txt", &u16);
        assert_eq!(preview(&[p]).text, "中文");
    }

    #[test]
    fn dir_lists_children() {
        let dir = std::env::temp_dir().join("clipx-doc-test-dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        let v = preview(&[dir.to_string_lossy().to_string()]);
        assert!(v.info.contains("2 项"), "{}", v.info);
        assert!(v.text.contains("sub/"), "{}", v.text);
        assert!(v.text.contains("b.txt"), "{}", v.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gbk_fallback_decoded() {
        // "中文测试" 的 GBK 字节（中文 Windows 记事本默认编码）
        let p = tmp_file(
            "gbk.txt",
            &[0xD6, 0xD0, 0xCE, 0xC4, 0xB2, 0xE2, 0xCA, 0xD4],
        );
        assert_eq!(preview(&[p]).text, "中文测试");
    }

    #[test]
    fn ooxml_entities_decoded() {
        let xml = b"<w:p><w:r><w:t>&lt;&amp;&#65;&#x42;&quot;</w:t></w:r></w:p>";
        assert_eq!(scan_ooxml_text(xml).trim(), "<&AB\"");
    }

    fn tmp_zip_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("clipx-doc-test");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    /// 测试用 zip（Stored 不压缩，不依赖压缩特性）。
    fn write_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
        use std::io::Write;
        let f = std::fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn docx_extracts_paragraphs() {
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t xml:space="preserve"> 世界</w:t></w:r></w:p><w:p><w:r><w:t>第二段&amp;more</w:t></w:r></w:p></w:body></w:document>"#;
        let p = tmp_zip_path("t.docx");
        write_zip(&p, &[("word/document.xml", doc.as_bytes())]);
        let v = preview(&[p.to_string_lossy().to_string()]);
        assert!(v.text.contains("Hello 世界"), "{}", v.text);
        assert!(v.text.contains("第二段&more"), "{}", v.text);
        assert!(v.info.contains("文档"), "{}", v.info);
        assert!(!v.mono);
    }

    #[test]
    fn docx_missing_part_degrades_to_card() {
        let p = tmp_zip_path("bad.docx");
        write_zip(&p, &[("random.txt", b"nope")]);
        let v = preview(&[p.to_string_lossy().to_string()]);
        assert!(v.text.contains("解析失败"), "{}", v.text);
    }

    const XLSX_CT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>"#;
    const XLSX_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
    const XLSX_WB: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
    const XLSX_WBRELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/></Relationships>"#;
    const XLSX_SHEET: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="2"><c r="A2"><v>42</v></c><c r="B2"><v>3.5</v></c></row></sheetData></worksheet>"#;
    const XLSX_SST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="2" uniqueCount="2"><si><t>姓名</t></si><si><t>分数</t></si></sst>"#;

    #[test]
    fn xlsx_first_sheet_to_text() {
        let p = tmp_zip_path("t.xlsx");
        write_zip(
            &p,
            &[
                ("[Content_Types].xml", XLSX_CT.as_bytes()),
                ("_rels/.rels", XLSX_RELS.as_bytes()),
                ("xl/workbook.xml", XLSX_WB.as_bytes()),
                ("xl/_rels/workbook.xml.rels", XLSX_WBRELS.as_bytes()),
                ("xl/worksheets/sheet1.xml", XLSX_SHEET.as_bytes()),
                ("xl/sharedStrings.xml", XLSX_SST.as_bytes()),
            ],
        );
        let v = preview(&[p.to_string_lossy().to_string()]);
        assert!(v.text.contains("姓名"), "{}", v.text);
        assert!(v.text.contains("分数"), "{}", v.text);
        assert!(v.text.contains("42"), "{}", v.text);
        assert!(v.text.contains("3.5"), "{}", v.text);
        assert!(v.info.contains("Sheet1"), "{}", v.info);
        assert!(v.info.contains("2行"), "{}", v.info);
        assert!(v.mono);
    }

    #[test]
    fn pptx_slides_sorted_numerically() {
        let slide = |t: &str| {
            format!(
                r#"<p:sld><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            )
        };
        let p = tmp_zip_path("t.pptx");
        // 故意乱序 + slide10（字典序陷阱）
        let s1 = slide("页1");
        let s2 = slide("页2");
        let s10 = slide("页10");
        write_zip(
            &p,
            &[
                ("ppt/slides/slide10.xml", s10.as_bytes()),
                ("ppt/slides/slide2.xml", s2.as_bytes()),
                ("ppt/slides/slide1.xml", s1.as_bytes()),
            ],
        );
        let v = preview(&[p.to_string_lossy().to_string()]);
        let (i1, i2, i10) = (
            v.text.find("页1").unwrap_or(usize::MAX),
            v.text.find("页2").unwrap_or(usize::MAX),
            v.text.find("页10").unwrap_or(usize::MAX),
        );
        assert!(i1 < i2 && i2 < i10, "{}", v.text);
        assert!(v.info.contains("3页"), "{}", v.info);
    }

    /// 手工最小 PDF（xref 偏移程序计算，保证合法）。
    fn minimal_pdf() -> Vec<u8> {
        let content =
            b"BT /F1 24 Tf 100 700 Td (Hello PDF) Tj ET\nBT /F1 24 Tf 100 660 Td (Second Line) Tj ET\n";
        let mut objs: Vec<Vec<u8>> = Vec::new();
        objs.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
        objs.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
        objs.push(
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
        );
        let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        stream.extend_from_slice(content);
        stream.extend_from_slice(b"endstream");
        objs.push(stream);
        objs.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            pdf.extend_from_slice(o);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
                objs.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn pdf_text_extracted() {
        let dir = std::env::temp_dir().join("clipx-doc-test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("t.pdf");
        std::fs::write(&p, minimal_pdf()).unwrap();
        let v = preview(&[p.to_string_lossy().to_string()]);
        assert!(v.text.contains("Hello PDF"), "{}", v.text);
        assert!(v.text.contains("Second Line"), "{}", v.text);
        assert!(v.info.contains("PDF"), "{}", v.info);
    }

    #[test]
    fn pdf_garbage_degrades_to_card() {
        let p = tmp_file("bad.pdf", b"%PDF-1.4\nnot really a pdf");
        let v = preview(&[p]);
        assert!(
            v.text.contains("解析失败") || v.text.contains("未提取到文本"),
            "{}",
            v.text
        );
    }

    #[test]
    fn oversize_table_degrades_to_card() {
        let p = tmp_file("big.xlsx", b"PK\x03\x04");
        let m = std::fs::metadata(&p).unwrap();
        let v = preview_table(&p, DOC_MAX_BYTES + 1, &m);
        assert!(v.text.contains("过大"), "{}", v.text);
        assert!(v.info.contains("XLSX 文件"), "{}", v.info);
    }
}
