use std::time::{SystemTime, UNIX_EPOCH};

pub const PREVIEW_MAX_CHARS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Text,
    Image,
    Files,
    RichText,
}

impl EntryKind {
    pub fn as_i64(self) -> i64 {
        match self {
            EntryKind::Text => 0,
            EntryKind::Image => 1,
            EntryKind::Files => 2,
            EntryKind::RichText => 3,
        }
    }

    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            0 => Some(EntryKind::Text),
            1 => Some(EntryKind::Image),
            2 => Some(EntryKind::Files),
            3 => Some(EntryKind::RichText),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EntryMeta {
    pub id: i64,
    pub kind: EntryKind,
    pub preview: String,
    pub pinned: bool,
    pub created_ms: i64,
    /// 采集时前台进程基名（无 .exe）；空串表示未知。
    pub source_app: String,
}

#[derive(Debug, Clone)]
pub enum Payload {
    Text {
        full: String,
    },
    Image {
        blob: Vec<u8>,
        width: u32,
        height: u32,
        mime: String,
        thumb: Vec<u8>,
        thumb_w: u32,
        thumb_h: u32,
    },
    Files {
        paths: Vec<String>,
    },
    /// 富文本：纯文本投影 + 原始 HTML（粘贴时优先还原格式，WPF 版没有的增强项）
    RichText {
        full: String,
        html: String,
    },
}

#[derive(Debug, Clone)]
pub struct NewEntry {
    pub kind: EntryKind,
    pub preview: String,
    pub content_hash: String,
    pub payload: Payload,
    /// 采集时前台进程基名（无 .exe）。
    pub source_app: String,
}

/// 缩略图解码宽度上限（WPF 版 ClipboardEntry.CreateThumbnail：DecodePixelWidth = 64）
pub const THUMB_DECODE_WIDTH: u32 = 64;

impl NewEntry {
    pub fn from_text(text: String) -> Self {
        Self {
            kind: EntryKind::Text,
            preview: build_preview(&text),
            content_hash: hash_bytes(b"t", text.as_bytes()),
            payload: Payload::Text { full: text },
            source_app: String::new(),
        }
    }

    pub fn from_image(blob: Vec<u8>, width: u32, height: u32, mime: String) -> Self {
        let (thumb, thumb_w, thumb_h) = make_thumbnail(&blob);
        Self::from_image_with_thumb(blob, width, height, mime, thumb, thumb_w, thumb_h)
    }

    /// 缩略图已由捕获侧从解码位图压出，入库不再解 PNG。
    pub fn from_image_with_thumb(
        blob: Vec<u8>,
        width: u32,
        height: u32,
        mime: String,
        thumb: Vec<u8>,
        thumb_w: u32,
        thumb_h: u32,
    ) -> Self {
        Self {
            kind: EntryKind::Image,
            preview: format!("图片 {width}×{height}"),
            content_hash: hash_bytes(b"i", &blob),
            payload: Payload::Image {
                blob,
                width,
                height,
                mime,
                thumb,
                thumb_w,
                thumb_h,
            },
            source_app: String::new(),
        }
    }

    pub fn from_files(paths: Vec<String>) -> Self {
        Self {
            kind: EntryKind::Files,
            preview: format_files_preview(&paths),
            content_hash: hash_files(&paths),
            payload: Payload::Files { paths },
            source_app: String::new(),
        }
    }

    pub fn from_rich_text(text: String, html: String) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"r");
        hasher.update(text.as_bytes());
        hasher.update(&[0]);
        hasher.update(html.as_bytes());
        Self {
            kind: EntryKind::RichText,
            preview: build_preview(&text),
            content_hash: hasher.finalize().to_hex().to_string(),
            payload: Payload::RichText { full: text, html },
            source_app: String::new(),
        }
    }

    pub fn with_source(mut self, app: impl Into<String>) -> Self {
        self.source_app = app.into();
        self
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn hash_bytes(prefix: &[u8], data: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prefix);
    hasher.update(data);
    hasher.finalize().to_hex().to_string()
}

fn hash_files(paths: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"f");
    for p in paths {
        hasher.update(p.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

/// 文件路径是否像图片（扩展名，对齐 WPF `ImageExtensions`）。
pub fn is_image_file_path(path: &str) -> bool {
    matches!(
        std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some(
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "ico"
                | "jfif" | "jpe"
        )
    )
}

/// 魔数判断（微信等复制出来可能没有扩展名）。
pub fn looks_like_image_bytes(b: &[u8]) -> bool {
    if b.len() >= 3 && b[0] == 0xFF && b[1] == 0xD8 && b[2] == 0xFF {
        return true;
    }
    if b.len() >= 8 && b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return true;
    }
    if b.len() >= 6 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return true;
    }
    if b.len() >= 2 && b.starts_with(b"BM") {
        return true;
    }
    if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        return true;
    }
    if b.len() >= 4 && (b.starts_with(b"II*\0") || b.starts_with(b"MM\0*")) {
        return true;
    }
    if b.len() >= 4 && b[0] == 0 && b[1] == 0 && b[2] == 1 && b[3] == 0 {
        return true; // ICO
    }
    false
}

fn file_header_is_image(path: &str) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 16];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    looks_like_image_bytes(&buf[..n])
}

/// 扩展名或文件头像图片。
pub fn path_looks_like_image(path: &str) -> bool {
    is_image_file_path(path) || file_header_is_image(path)
}

const FILE_THUMB_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// 文件列表里第一张能解码的图片 → 64px PNG 缩略图（失败空，不入库大图）。
pub fn make_file_list_thumbnail(paths: &[String]) -> (Vec<u8>, u32, u32) {
    for p in paths {
        if p.is_empty() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(p) else {
            continue;
        };
        if !meta.is_file() || meta.len() == 0 || meta.len() > FILE_THUMB_MAX_BYTES {
            continue;
        }
        if !is_image_file_path(p) && !file_header_is_image(p) {
            continue;
        }
        let t = make_thumbnail_from_path(p);
        if !t.0.is_empty() {
            return t;
        }
    }
    (Vec::new(), 0, 0)
}

fn make_thumbnail_from_path(path: &str) -> (Vec<u8>, u32, u32) {
    let Ok(reader) = image::ImageReader::open(path) else {
        return (Vec::new(), 0, 0);
    };
    let Ok(reader) = reader.with_guessed_format() else {
        return (Vec::new(), 0, 0);
    };
    let Ok(img) = reader.decode() else {
        return (Vec::new(), 0, 0);
    };
    encode_thumb(img)
}

fn encode_thumb(img: image::DynamicImage) -> (Vec<u8>, u32, u32) {
    let small = if img.width() > THUMB_DECODE_WIDTH {
        img.thumbnail(THUMB_DECODE_WIDTH, u32::MAX)
    } else {
        img
    };
    let (w, h) = (small.width(), small.height());
    let mut buf = Vec::new();
    match small
        .to_rgba8()
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
    {
        Ok(()) => (buf, w, h),
        Err(_) => (Vec::new(), 0, 0),
    }
}

/// 生成 PNG 缩略图：宽度压到 THUMB_DECODE_WIDTH 以内（保持宽高比，只缩不放）。
/// 解码失败时回退空缩略图（列表退化为图标展示，不影响入库）。
pub fn make_thumbnail(png: &[u8]) -> (Vec<u8>, u32, u32) {
    let Ok(img) = image::load_from_memory(png) else {
        return (Vec::new(), 0, 0);
    };
    encode_thumb(img)
}

/// 预览中尺寸渲染图宽度：预览面板 440 逻辑像素，200% DPI 下 880 物理像素，
/// 1280 给出约 1.5× 余量，滚轮放大 2× 内依然清晰；再往上由 1600 全解精化。
pub const PREVIEW_RENDITION_WIDTH: u32 = 1280;
/// 渲染图 JPEG 质量：截图文字在 2× 内无可见 artifact，文件只有同尺寸 PNG 的约 1/4。
pub const PREVIEW_RENDITION_JPEG_Q: u8 = 88;

/// 捕获/入库共用：从已解码位图一次压出列表缩略图 + 预览 JPEG。
#[derive(Debug, Clone)]
pub struct ImageDerivatives {
    pub thumb: Vec<u8>,
    pub thumb_w: u32,
    pub thumb_h: u32,
    pub rendition_jpeg: Vec<u8>,
}

/// 已解码位图 → 64 宽 PNG 缩略图 + 1280 JPEG。调用方不再解 PNG。
pub fn make_image_derivatives(img: image::DynamicImage) -> ImageDerivatives {
    let mid = scale_to_rendition(img);
    let jpeg = encode_jpeg_dyn(&mid).unwrap_or_default();
    let (thumb, thumb_w, thumb_h) = encode_thumb(mid);
    ImageDerivatives {
        thumb,
        thumb_w,
        thumb_h,
        rendition_jpeg: jpeg,
    }
}

/// 预览中尺寸渲染图：1280 宽 JPEG（小文件、解码快），只做显示，粘贴仍走原图。
/// 入库 worker 与预览 miss 路径共用；失败回 None（调用方走全解路径）。
pub fn make_preview_rendition(png: &[u8]) -> Option<Vec<u8>> {
    let Ok(img) = image::load_from_memory(png) else {
        return None;
    };
    encode_jpeg_dyn(&scale_to_rendition(img))
}

/// RGBA 已是目标尺寸时直接编 JPEG（WIC 边解边缩后的 rendition 补做路径）。
pub fn encode_preview_jpeg_rgba(rgba: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let n = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if rgba.len() != n {
        return None;
    }
    let mut rgb = Vec::with_capacity(n / 4 * 3);
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    encode_jpeg_rgb(&rgb, w, h)
}

fn scale_to_rendition(img: image::DynamicImage) -> image::DynamicImage {
    if img.width().max(img.height()) > PREVIEW_RENDITION_WIDTH {
        img.thumbnail(PREVIEW_RENDITION_WIDTH, PREVIEW_RENDITION_WIDTH)
    } else {
        img
    }
}

fn encode_jpeg_dyn(img: &image::DynamicImage) -> Option<Vec<u8>> {
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    encode_jpeg_rgb(rgb.as_raw(), w, h)
}

fn encode_jpeg_rgb(rgb: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let n = (w as usize).checked_mul(h as usize)?.checked_mul(3)?;
    if rgb.len() != n {
        return None;
    }
    let mut buf = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(
        &mut buf,
        PREVIEW_RENDITION_JPEG_Q,
    );
    use image::ImageEncoder;
    match enc.write_image(rgb, w, h, image::ExtendedColorType::Rgb8) {
        Ok(()) => Some(buf),
        Err(_) => None,
    }
}

pub fn build_preview(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    truncate_chars(line, PREVIEW_MAX_CHARS)
}

fn file_name_only(path: &str) -> String {
    // 剪贴板里的路径可能来自任意平台（`\` 或 `/`），两种分隔符都取尾段。
    let name = path.trim_end_matches(['\\', '/']);
    let name = name.rsplit(['\\', '/']).next().unwrap_or(name);
    if name.is_empty() {
        path.to_string()
    } else {
        name.to_string()
    }
}

/// 列表主文案：最多 3 个文件名，超出加 `(+N)`（对齐 WPF `FormatFilePaths`）。
pub fn files_name_preview(paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let names: Vec<String> = paths.iter().map(|p| file_name_only(p)).take(3).collect();
    let mut s = names.join(", ");
    if paths.len() > 3 {
        s.push_str(&format!(" (+{})", paths.len() - 3));
    }
    s
}

/// 入库/展示：单文件只留文件名；多个带数量。
pub fn format_files_preview(paths: &[String]) -> String {
    let names = files_name_preview(paths);
    match paths.len() {
        0 => String::new(),
        1 => names,
        n => format!("{n} 个文件 · {names}"),
    }
}

/// 旧条目 preview 是 `全路径  全路径`；新条目是 `N 个文件 · a.png, b.png`。
/// 返回 (列表主文案=文件名, 数量)。
pub fn files_list_parts(stored: &str) -> (String, usize) {
    if let Some((left, names)) = stored.split_once(" 个文件 · ") {
        if let Ok(n) = left.parse::<usize>() {
            if n > 1 && !names.is_empty() {
                return (names.to_string(), n);
            }
        }
    }
    let paths: Vec<String> = stored
        .split("  ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let looks_path = paths
        .iter()
        .any(|p| p.contains('\\') || p.contains('/') || p.contains(':'));
    if looks_path && !paths.is_empty() {
        return (files_name_preview(&paths), paths.len());
    }
    (stored.to_string(), 1)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_takes_first_non_empty_line() {
        assert_eq!(build_preview("  \nhello world\nsecond line"), "hello world");
    }

    #[test]
    fn preview_truncates_on_char_boundary() {
        let long = "剪切板".repeat(60);
        let preview = build_preview(&long);
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn preview_of_whitespace_only_is_empty() {
        assert_eq!(build_preview("\n \n\t"), "");
    }

    #[test]
    fn text_hash_is_stable() {
        let a = NewEntry::from_text("abc".into());
        let b = NewEntry::from_text("abc".into());
        assert_eq!(a.content_hash, b.content_hash);
        assert_eq!(a.preview, "abc");
    }

    #[test]
    fn hash_prefixes_separate_kinds() {
        let text = NewEntry::from_text("abc".into());
        let image = NewEntry::from_image(b"abc".to_vec(), 1, 1, "image/png".into());
        assert_ne!(text.content_hash, image.content_hash);
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
    fn thumbnail_downscales_to_64_width() {
        let png = tiny_png(320, 120);
        let entry = NewEntry::from_image(png.clone(), 320, 120, "image/png".into());
        let Payload::Image {
            thumb,
            thumb_w,
            thumb_h,
            ..
        } = &entry.payload
        else {
            panic!()
        };
        assert_eq!(*thumb_w, THUMB_DECODE_WIDTH);
        assert_eq!(*thumb_h, 24); // 320:120 = 64:24
        assert!(!thumb.is_empty());
    }

    #[test]
    fn thumbnail_keeps_small_images() {
        let png = tiny_png(32, 16);
        let entry = NewEntry::from_image(png, 32, 16, "image/png".into());
        let Payload::Image {
            thumb_w, thumb_h, ..
        } = &entry.payload
        else {
            panic!()
        };
        assert_eq!(*thumb_w, 32);
        assert_eq!(*thumb_h, 16);
    }

    #[test]
    fn thumbnail_survives_garbage_bytes() {
        let entry = NewEntry::from_image(vec![0u8, 1, 2, 3], 4, 4, "image/png".into());
        let Payload::Image {
            thumb,
            thumb_w,
            thumb_h,
            ..
        } = &entry.payload
        else {
            panic!()
        };
        assert!(thumb.is_empty());
        assert_eq!((*thumb_w, *thumb_h), (0, 0));
    }

    #[test]
    fn rendition_is_capped_jpeg() {
        let png = tiny_png(2560, 1440);
        let jpg = make_preview_rendition(&png).expect("rendition");
        assert_eq!(&jpg[0..2], &[0xFF, 0xD8], "JPEG magic");
        let img = image::load_from_memory(&jpg).expect("decode rendition");
        assert_eq!(img.width(), PREVIEW_RENDITION_WIDTH);
        assert_eq!(img.height(), 720); // 2560:1440 = 1280:720
        assert!(jpg.len() < png.len(), "rendition smaller than source png");
        assert!(make_preview_rendition(&[0u8, 1, 2, 3]).is_none());
    }

    #[test]
    fn derivatives_from_decoded_bitmap() {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(2560, 1440));
        let d = make_image_derivatives(img);
        assert_eq!(d.thumb_w, THUMB_DECODE_WIDTH);
        assert_eq!(d.thumb_h, 36); // 2560:1440 = 64:36
        assert!(!d.thumb.is_empty());
        assert_eq!(&d.rendition_jpeg[0..2], &[0xFF, 0xD8]);
        let jpg = image::load_from_memory(&d.rendition_jpeg).expect("jpeg");
        assert_eq!((jpg.width(), jpg.height()), (PREVIEW_RENDITION_WIDTH, 720));
    }

    #[test]
    fn files_preview_names_and_count() {
        assert_eq!(format_files_preview(&[]), "");
        assert_eq!(format_files_preview(&["C:\\a\\shot.png".into()]), "shot.png");
        assert_eq!(
            format_files_preview(&["/tmp/a.png".into(), "/tmp/b.jpg".into()]),
            "2 个文件 · a.png, b.jpg"
        );
        let five: Vec<String> = (1..=5).map(|i| format!("/tmp/f{i}.png")).collect();
        let p = format_files_preview(&five);
        assert!(p.starts_with("5 个文件 · "), "{p}");
        assert!(p.contains("(+2)"), "{p}");
    }

    #[test]
    fn files_preview_for_list_rewrites_legacy_paths() {
        let old = r"C:\Users\a\one.png  C:\Users\a\two.jpg";
        assert_eq!(
            files_list_parts(old),
            ("one.png, two.jpg".into(), 2)
        );
        assert_eq!(
            files_list_parts("3 个文件 · a.png, b.png, c.png"),
            ("a.png, b.png, c.png".into(), 3)
        );
        assert_eq!(files_list_parts("shot.png"), ("shot.png".into(), 1));
    }

    #[test]
    fn files_hash_is_path_order_sensitive() {
        let a = NewEntry::from_files(vec!["a".into(), "b".into()]);
        let b = NewEntry::from_files(vec!["b".into(), "a".into()]);
        assert_ne!(a.content_hash, b.content_hash);
    }

    #[test]
    fn image_path_and_magic() {
        assert!(is_image_file_path(r"C:\a\b.PNG"));
        assert!(is_image_file_path("foo.jpeg"));
        assert!(!is_image_file_path(r"C:\a\b.txt"));
        assert!(looks_like_image_bytes(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(looks_like_image_bytes(&[
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A
        ]));
        assert!(!looks_like_image_bytes(b"PK\x03\x04"));
    }

    #[test]
    fn file_list_thumbnail_from_png_path() {
        let dir = std::env::temp_dir().join("clipx-file-thumb-test");
        let _ = std::fs::create_dir_all(&dir);
        let png_path = dir.join("shot.png");
        std::fs::write(&png_path, tiny_png(80, 40)).unwrap();
        let no_ext = dir.join("wechat_dat");
        std::fs::write(&no_ext, tiny_png(48, 24)).unwrap();
        let txt = dir.join("note.txt");
        std::fs::write(&txt, b"hello").unwrap();

        let (blob, w, h) =
            make_file_list_thumbnail(&[txt.to_string_lossy().into(), png_path.to_string_lossy().into()]);
        assert!(!blob.is_empty());
        assert_eq!((w, h), (64, 32));

        let (blob2, w2, _) =
            make_file_list_thumbnail(&[no_ext.to_string_lossy().into()]);
        assert!(!blob2.is_empty());
        assert!(w2 > 0);

        let _ = std::fs::remove_file(&png_path);
        let _ = std::fs::remove_file(&no_ext);
        let _ = std::fs::remove_file(&txt);
        let _ = std::fs::remove_dir(&dir);
    }
}
