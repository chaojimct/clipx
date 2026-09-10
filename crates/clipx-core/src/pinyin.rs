use pinyin::ToPinyin;

/// 拼音串源文本上限：blob 走 LIKE 子串匹配，限制体积保证按键级查询性能。
/// WPF 版上限 8192（内存线性扫描）；DB 场景 512 已覆盖绝大多数检索目标。
const MAX_SOURCE_CHARS: usize = 512;

/// 生成拼音检索 blob（对齐 WPF PinyinSearchIndex.BuildBlob）：
/// 全拼连写 + 首字母连写，统一小写无分隔。
/// "你好世界" → "nihaoshijienhsj"
/// 查询侧用 LIKE '%q%' 子串匹配，因此支持任意起始位置（"nihao"/"shijie"/"nh"）。
pub fn to_pinyin_blob(text: &str) -> String {
    let mut full = String::new();
    let mut initials = String::new();
    let mut any = false;
    for (i, py) in text.to_pinyin().enumerate() {
        if i >= MAX_SOURCE_CHARS {
            break;
        }
        let Some(py) = py else { continue };
        any = true;
        full.push_str(py.plain());
        initials.push_str(py.first_letter());
    }
    if !any {
        return String::new();
    }
    let mut blob = full;
    blob.push_str(&initials);
    blob
}

/// 空格分词 AND：每段需为原文子串（忽略大小写）或拼音 blob 子串。
///
/// - `"qs"` 命中「青松…」（首字母）
/// - `"qingsong"` 命中「青松…」（全拼）
/// - `"ai edu"` 命中 `ai-edu-dataset`（两段都是原文子串）
pub fn text_matches_query(text: &str, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    let lower = text.to_lowercase();
    let blob = to_pinyin_blob(text);
    q.split_whitespace().all(|tok| {
        let t = tok.to_lowercase();
        if t.is_empty() {
            return true;
        }
        contains_ci(&lower, &t) || (!blob.is_empty() && blob.contains(t.as_str()))
    })
}

/// 拼音/首字母命中的字符区间 [start, end)（Unicode 标量下标）。
/// `machuntian` →「马春天.pdf」的「马春天」；`mct` / `qs` 同样。
pub fn pinyin_hit_span(text: &str, query: &str) -> Option<(usize, usize)> {
    let q = query
        .trim()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if q.is_empty() {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    for i in 0..chars.len() {
        if let Some(end) = consume_pinyin(&chars, i, &q) {
            if end > i {
                return Some((i, end));
            }
        }
    }
    None
}

fn consume_pinyin(chars: &[char], start: usize, q: &str) -> Option<usize> {
    let mut rest = q;
    let mut j = start;
    while !rest.is_empty() && j < chars.len() {
        let c = chars[j];
        if let Some(py) = c.to_pinyin() {
            let full = py.plain();
            let init = py.first_letter();
            if rest.starts_with(full) {
                rest = &rest[full.len()..];
                j += 1;
                continue;
            }
            if rest.starts_with(init) {
                rest = &rest[init.len()..];
                j += 1;
                continue;
            }
            if full.starts_with(rest) {
                return Some(j + 1);
            }
            return None;
        }
        let lower = c.to_lowercase().to_string();
        if rest.starts_with(&lower) {
            rest = &rest[lower.len()..];
            j += 1;
            continue;
        }
        return None;
    }
    rest.is_empty().then_some(j)
}

fn contains_ci(haystack_lower: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    haystack_lower.contains(needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_concatenates_full_and_initials() {
        let blob = to_pinyin_blob("你好");
        assert_eq!(blob, "nihaonh");
    }

    #[test]
    fn pure_ascii_returns_empty() {
        assert!(to_pinyin_blob("hello world 123").is_empty());
    }

    #[test]
    fn mixed_text_skips_non_han() {
        let blob = to_pinyin_blob("部署 dev");
        assert_eq!(blob, "bushubs");
    }

    #[test]
    fn supports_mid_string_starts() {
        let blob = to_pinyin_blob("你好世界");
        assert!(blob.contains("niha"));
        assert!(blob.contains("shijie"));
        assert!(blob.contains("nh"));
    }

    #[test]
    fn long_text_is_capped() {
        let blob = to_pinyin_blob(&"你好".repeat(1000));
        // 截断于 512 源字符 = 256 组「你好」：全拼 256×5 + 首字母 256×2
        assert_eq!(blob.len(), 256 * 5 + 256 * 2);
    }

    #[test]
    fn query_pinyin_initials_and_full() {
        assert!(text_matches_query("青松AI教育数字化平台.pdf", "qs"));
        assert!(text_matches_query("青松AI教育数字化平台.pdf", "qingsong"));
        assert!(text_matches_query("青松AI教育数字化平台.pdf", "ai"));
        assert!(!text_matches_query("青松AI教育数字化平台.pdf", "xyz"));
        assert!(text_matches_query("马春天.pdf", "machuntian"));
        assert!(text_matches_query("马春天.pdf", "mct"));
        assert_eq!(pinyin_hit_span("马春天.pdf", "machuntian"), Some((0, 3)));
        assert_eq!(pinyin_hit_span("马春天.pdf", "mct"), Some((0, 3)));
        assert_eq!(pinyin_hit_span("报告_马春天_v2.pdf", "machuntian"), Some((3, 6)));
        assert_eq!(pinyin_hit_span("青松AI.pdf", "qs"), Some((0, 2)));
    }

    #[test]
    fn query_spaces_are_and_tokens() {
        assert!(text_matches_query("ai-edu-dataset", "ai edu"));
        assert!(text_matches_query("ai-edu-dataset", "ai  edu"));
        assert!(!text_matches_query("ai-edu-dataset", "ai xyz"));
        assert!(text_matches_query("foo", ""));
    }
}
