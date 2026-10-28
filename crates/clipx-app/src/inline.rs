//! 行内补全（Inline Suggest，实验性）：任意应用键入时跟随小窗。
//!
//! - buffer 只收 `[a-zA-Z0-9@._/:-]`，空格/标点/切焦点即重置（调用方经 `InlineReset`）。
//! - 检索：收藏前缀优先，其次常用+最近（store.search_inline），短语前缀放最前。
//! - 密码框/黑名单/自家窗口永不触发（调用方在 handle 前过滤）。

use clipx_core::EntryMeta;
use clipx_store::Store;

use crate::settings::Settings;

#[derive(Debug, Clone)]
pub struct InlineItem {
    pub id: i64,
    pub preview: String,
    pub pinned: bool,
    pub badge: &'static str,
}

/// 行内条目 id 约定（与主列表 `ItemId` 独立，Step 1 迁移 inline 时再统一）：
/// 短语行为 `-((i)+1)`（见 `phrase_prefix_items`），库行即入库 id。
pub(crate) fn is_inline_phrase(id: i64) -> bool {
    id < 0
}

/// 负 id 还原短语下标（`is_inline_phrase` 为真时调用）。
pub(crate) fn inline_phrase_idx(id: i64) -> Option<usize> {
    if id < 0 {
        Some(((-id) as usize).saturating_sub(1))
    } else {
        None
    }
}

pub fn is_allowed(settings: &Settings) -> bool {
    settings.inline_enabled
}

pub fn normalize_query(buf: &str) -> String {
    buf.trim().to_string()
}

/// 单字符是否进入 buffer。
pub fn push_char(buf: &mut String, c: char) -> bool {
    if buf.len() >= 48 {
        return false;
    }
    if c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | '/' | ':') {
        buf.push(c);
        true
    } else {
        false
    }
}

fn phrase_prefix_items(settings: &Settings, q: &str) -> Vec<InlineItem> {
    let ql = q.to_lowercase();
    let mut out = Vec::new();
    for (i, p) in settings.phrases.iter().enumerate() {
        if p.content.is_empty() {
            continue;
        }
        let cl = p.content.to_lowercase();
        let pl = p.phrase.to_lowercase();
        if cl.starts_with(&ql) || (!pl.is_empty() && pl.starts_with(&ql)) {
            let mut preview = p.content.lines().next().unwrap_or("").trim().to_string();
            if preview.chars().count() > 60 {
                let end = preview
                    .char_indices()
                    .nth(60)
                    .map(|(i, _)| i)
                    .unwrap_or(preview.len());
                preview.truncate(end);
                preview.push('…');
            }
            out.push(InlineItem {
                id: -((i as i64) + 1),
                preview,
                pinned: true,
                badge: "短语",
            });
            if out.len() >= 3 {
                break;
            }
        }
    }
    out
}

pub fn query_items(store: &Store, settings: &Settings, query: &str) -> Vec<InlineItem> {
    let q = normalize_query(query);
    if (q.chars().count() as i64) < settings.inline_min_len.max(2) {
        return Vec::new();
    }
    let mut out = phrase_prefix_items(settings, &q);
    let limit = settings.inline_max_items.clamp(1, 8) as i64;
    let rows: Vec<EntryMeta> = store.search_inline(&q, limit);
    for m in rows {
        if out.len() >= limit as usize {
            break;
        }
        // 短语已覆盖同文时去重（preview 相同即跳过）。
        if out.iter().any(|it| it.preview == m.preview) {
            continue;
        }
        let mut preview = m.preview.lines().next().unwrap_or("").to_string();
        if preview.chars().count() > 60 {
            let end = preview
                .char_indices()
                .nth(60)
                .map(|(i, _)| i)
                .unwrap_or(preview.len());
            preview.truncate(end);
            preview.push('…');
        }
        let badge = if m.pinned { "★" } else { "🕘" };
        out.push(InlineItem {
            id: m.id,
            preview,
            pinned: m.pinned,
            badge,
        });
    }
    out
}

/// 前台应用是否命中黑名单（小写比对，支持 `wechat` 匹配 `WeChat.exe`）。
pub fn app_suppressed(exe_base_lower: &str, settings: &Settings) -> bool {
    if exe_base_lower.is_empty() {
        return false;
    }
    let hit = |list: &[String]| {
        list.iter().any(|a| {
            let t = a.trim().to_lowercase();
            if t.is_empty() {
                return false;
            }
            let t = t.strip_suffix(".exe").unwrap_or(&t).to_string();
            exe_base_lower == t || exe_base_lower.contains(&t) || t.contains(exe_base_lower)
        })
    };
    hit(&settings.inline_blacklist) || hit(&settings.exclusion_apps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_char_filters() {
        let mut s = String::new();
        assert!(push_char(&mut s, 'a'));
        assert!(push_char(&mut s, '@'));
        assert!(!push_char(&mut s, ' '));
        assert!(!push_char(&mut s, '，'));
    }

    #[test]
    fn app_suppressed_matches() {
        let mut st = Settings::default();
        st.inline_blacklist = vec!["WeChat".into()];
        assert!(app_suppressed("wechat", &st));
        assert!(!app_suppressed("chrome", &st));
    }
}
