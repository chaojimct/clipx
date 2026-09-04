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
            preview: build_preview(&paths.join("  ")),
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

/// 生成 PNG 缩略图：宽度压到 THUMB_DECODE_WIDTH 以内（保持宽高比，只缩不放）。
/// 解码失败时回退空缩略图（列表退化为图标展示，不影响入库）。
pub fn make_thumbnail(png: &[u8]) -> (Vec<u8>, u32, u32) {
    let Ok(img) = image::load_from_memory(png) else {
        return (Vec::new(), 0, 0);
    };
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

pub fn build_preview(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    truncate_chars(line, PREVIEW_MAX_CHARS)
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
    fn files_hash_is_path_order_sensitive() {
        let a = NewEntry::from_files(vec!["a".into(), "b".into()]);
        let b = NewEntry::from_files(vec!["b".into(), "a".into()]);
        assert_ne!(a.content_hash, b.content_hash);
    }
}
