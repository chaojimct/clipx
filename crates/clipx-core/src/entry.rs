use std::time::{SystemTime, UNIX_EPOCH};

pub const PREVIEW_MAX_CHARS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Text,
    Image,
    Files,
}

impl EntryKind {
    pub fn as_i64(self) -> i64 {
        match self {
            EntryKind::Text => 0,
            EntryKind::Image => 1,
            EntryKind::Files => 2,
        }
    }

    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            0 => Some(EntryKind::Text),
            1 => Some(EntryKind::Image),
            2 => Some(EntryKind::Files),
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
}

#[derive(Debug, Clone)]
pub enum Payload {
    Text { full: String },
    Image { blob: Vec<u8>, width: u32, height: u32, mime: String },
    Files { paths: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct NewEntry {
    pub kind: EntryKind,
    pub preview: String,
    pub content_hash: String,
    pub payload: Payload,
}

impl NewEntry {
    pub fn from_text(text: String) -> Self {
        Self {
            kind: EntryKind::Text,
            preview: build_preview(&text),
            content_hash: hash_bytes(b"t", text.as_bytes()),
            payload: Payload::Text { full: text },
        }
    }

    pub fn from_image(blob: Vec<u8>, width: u32, height: u32, mime: String) -> Self {
        Self {
            kind: EntryKind::Image,
            preview: format!("图片 {width}×{height}"),
            content_hash: hash_bytes(b"i", &blob),
            payload: Payload::Image { blob, width, height, mime },
        }
    }

    pub fn from_files(paths: Vec<String>) -> Self {
        Self {
            kind: EntryKind::Files,
            preview: build_preview(&paths.join("  ")),
            content_hash: hash_files(&paths),
            payload: Payload::Files { paths },
        }
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

pub fn build_preview(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
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

    #[test]
    fn files_hash_is_path_order_sensitive() {
        let a = NewEntry::from_files(vec!["a".into(), "b".into()]);
        let b = NewEntry::from_files(vec!["b".into(), "a".into()]);
        assert_ne!(a.content_hash, b.content_hash);
    }
}
