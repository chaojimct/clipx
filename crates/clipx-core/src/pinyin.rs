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

/// 命中的字符区间 [start, end)（Unicode 标量下标）。
/// `machuntian` →「马春天.pdf」的「马春天」；`mct` / `qs` 同样。
///
/// 多 token 查询（空格分词）返回**包络** [最早起点, 最晚终点)；UI 的三段式
/// 高亮只能标一段连续文本，包络是它在多段命中下的最优近似。
pub fn pinyin_hit_span(text: &str, query: &str) -> Option<(usize, usize)> {
    let spans = pinyin_hit_spans(text, query);
    Some((
        spans.iter().map(|s| s.0).min()?,
        spans.iter().map(|s| s.1).max()?,
    ))
}

/// 命中的**全部**字符区间，已合并重叠并按起点升序。
///
/// 判定与 [`text_matches_query`] **完全同构**——这是硬要求：检索侧与高亮侧
/// 一旦漂移，就会出现「结果里有这一行，行内却一个字都不亮」的割裂。每个
/// token 依次尝试：
///   1. 原文子串（忽略大小写）→ 直接取命中区间；
///   2. 否则在**拼音 blob**（全拼连写 + 首字母连写，即 `to_pinyin_blob` 的产物）
///      里找子串，再经「blob 字节 → 源字符」映射还原成字符区间。
///
/// 为什么第 2 步要在 blob 上做子串查找，而不是逐字消费拼音：DB 侧就是
/// `pinyin_blob LIKE '%q%'`，连写的 blob 里**任意子串**都算命中，于是
/// `pin` / `pingj` / `ingj` 这类**不完全音节**同样能检索到。旧的贪心逐字
/// 消费只认「完整音节 或 音节前缀」，这些查询就全都高亮不出来。
///
/// 因此 `"pingj"` →「萍姐」、`"pin"` →「萍」（命中不足一字的部分音节时，
/// 该字整体高亮）。
pub fn pinyin_hit_spans(text: &str, query: &str) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    // blob 与映射表按需构建：命中原文子串的 token 根本用不到拼音。
    let mut blob_cache: Option<(String, Vec<(usize, usize, usize)>)> = None;
    for tok in query.trim().split_whitespace() {
        let q = tok.to_lowercase();
        if q.is_empty() {
            continue;
        }
        if let Some(span) = literal_span(text, &q) {
            spans.push(span);
            continue;
        }
        let (blob, map) = blob_cache.get_or_insert_with(|| indexed_blob(text));
        if blob.is_empty() {
            continue;
        }
        for (s, e) in substring_hits(blob, &q) {
            if let Some(span) = map_byte_range(map, s, e) {
                spans.push(span);
            }
        }
    }
    merge_spans(spans)
}

/// 与 `to_pinyin_blob` 逐字节一致的 blob，外加「blob 字节区间 → 源字符下标」映射。
/// 映射表项为 `(blob_start, blob_end, char_index)`，字符区间用集合包含关系还原。
fn indexed_blob(text: &str) -> (String, Vec<(usize, usize, usize)>) {
    let mut full = String::new();
    let mut full_map: Vec<(usize, usize, usize)> = Vec::new();
    let mut initials = String::new();
    let mut init_map: Vec<(usize, usize, usize)> = Vec::new();
    for (i, py) in text.to_pinyin().enumerate() {
        if i >= MAX_SOURCE_CHARS {
            break;
        }
        let Some(py) = py else { continue };
        let at = full.len();
        full.push_str(py.plain());
        full_map.push((at, full.len(), i));
        let at = initials.len();
        initials.push_str(py.first_letter());
        init_map.push((at, initials.len(), i));
    }
    if full.is_empty() && initials.is_empty() {
        return (String::new(), Vec::new());
    }
    let offset = full.len();
    full.push_str(&initials);
    let mut map = full_map;
    map.extend(init_map.into_iter().map(|(a, b, i)| (a + offset, b + offset, i)));
    (full, map)
}

/// 所有出现位置（允许重叠，起点逐字节推进）。blob 恒为 ASCII，字节切片安全。
fn substring_hits(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = hay.get(from..).and_then(|rest| rest.find(needle)) {
        let s = from + rel;
        let e = s + needle.len();
        out.push((s, e));
        from = s + 1;
        if from >= hay.len() {
            break;
        }
    }
    out
}

/// 把 blob 字节区间 [s, e) 映射成源字符区间：凡与之相交的字符都算命中。
/// 部分音节（`pin` 落在「萍」的 `ping` 里）因此整字高亮，读起来才自然。
fn map_byte_range(
    map: &[(usize, usize, usize)],
    s: usize,
    e: usize,
) -> Option<(usize, usize)> {
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    for &(bs, be, ci) in map {
        if be > s && bs < e {
            lo = lo.min(ci);
            hi = hi.max(ci + 1);
        }
    }
    (hi > lo).then_some((lo, hi))
}

/// 原文子串命中区间（忽略大小写）。逐字符比较，避免 `to_lowercase` 改变长度时切错。
fn literal_span(text: &str, needle_lower: &str) -> Option<(usize, usize)> {
    let t: Vec<char> = text.chars().collect();
    let n: Vec<char> = needle_lower.chars().collect();
    if n.is_empty() || t.len() < n.len() {
        return None;
    }
    for start in 0..=t.len() - n.len() {
        let eq = t[start..start + n.len()]
            .iter()
            .zip(&n)
            .all(|(a, b)| a.eq_ignore_ascii_case(b) || a.to_lowercase().eq(b.to_lowercase()));
        if eq {
            return Some((start, start + n.len()));
        }
    }
    None
}

fn merge_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
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
    fn hit_spans_multi_token_and_multi_occurrence() {
        // 单 token 多字连拼
        assert_eq!(pinyin_hit_spans("萍姐，我是", "pingjie"), vec![(0, 2)]);
        // 首字母
        assert_eq!(pinyin_hit_spans("萍姐，我是", "pj"), vec![(0, 2)]);
        // 空格分词：每个 token 各自命中，相邻区间合并成连续段
        assert_eq!(pinyin_hit_spans("萍姐，我是", "ping jie"), vec![(0, 2)]);
        // 分词命中中间隔着非命中字时，保留为两个独立区间
        assert_eq!(pinyin_hit_spans("萍姐，我是", "ping shi"), vec![(0, 1), (4, 5)]);
        assert_eq!(pinyin_hit_span("萍姐，我是", "ping jie"), Some((0, 2)));
        assert_eq!(pinyin_hit_span("萍姐，我是", "萍 我"), Some((0, 4)));
        // 同一 token 命中多处（重复字）全部返回；相邻区间合并成一段
        assert_eq!(pinyin_hit_spans("你好你好", "nihao"), vec![(0, 4)]);
        assert_eq!(pinyin_hit_spans("你好-你好", "nihao"), vec![(0, 2), (3, 5)]);
        // 未命中
        assert!(pinyin_hit_spans("萍姐，我是", "xyz").is_empty());
        assert_eq!(pinyin_hit_span("萍姐，我是", "  "), None);
    }

    #[test]
    fn partial_syllable_queries_still_locate_chars() {
        // blob 是连写的，DB 用 LIKE '%q%' ⇒ 任意子串都算命中；高亮必须跟上，
        // 否则「搜到了却一个字也不亮」。以下每条旧实现都返回 None（逐字消费只认
        // 「完整音节 / 音节前缀」，且遇到「，」这类无拼音字符就断）。
        assert_eq!(pinyin_hit_spans("萍姐，我是", "pi"), vec![(0, 1)]);
        assert_eq!(pinyin_hit_spans("萍姐，我是", "pin"), vec![(0, 1)]);
        assert_eq!(pinyin_hit_spans("萍姐，我是", "pingj"), vec![(0, 2)]);
        assert_eq!(pinyin_hit_spans("萍姐，我是", "ingj"), vec![(0, 2)]);
        // 单 token 跨到隔着一个非汉字的下一个字（`jiew` = 姐+我）时，区间会
        // 连中间的「，」一起包进来——UI 只有一段连续高亮，包络是唯一可行的表达。
        assert_eq!(pinyin_hit_spans("萍姐，我是", "jiew"), vec![(1, 4)]);
        assert_eq!(pinyin_hit_spans("萍姐，我是", "jiewo"), vec![(1, 4)]);
        // 不完全音节 + 分词
        assert_eq!(pinyin_hit_spans("萍姐，我是", "pin wo"), vec![(0, 1), (3, 4)]);
        assert_eq!(pinyin_hit_span("萍姐，我是", "pin wo"), Some((0, 4)));
        // 「凭据」：pingj = 凭(ping) 的整音节 + 据(ju) 的首字母，正是用户报的形态
        assert_eq!(
            pinyin_hit_span("更新 Nacos 服务器地址和凭据，并将命名空间更改为 pig-test", "pingj"),
            Some((15, 17))
        );
    }

    #[test]
    fn indexed_blob_is_byte_identical_to_public_blob() {
        // 高亮用的带映射 blob 必须与检索侧入库的 blob 逐字节一致，
        // 否则两套判定又会漂移（这是本模块的核心不变量）。
        for text in [
            "萍姐，我是",
            "青松AI教育数字化平台.pdf",
            "报告_马春天_v2.pdf",
            "你好-你好",
            "纯 ASCII 无汉字",
            "",
        ] {
            assert_eq!(indexed_blob(text).0, to_pinyin_blob(text), "text={text}");
        }
    }

    #[test]
    fn hit_span_matches_blob_semantics() {
        // 高亮区间与检索 blob 的判定必须一致：blob 命中 ⇒ span 也能定位。
        // 否则会出现「搜到了但一行都不高亮」的割裂观感。
        for (text, q) in [
            ("萍姐，我是", "pingjie"),
            ("萍姐，我是", "pj"),
            ("萍姐，我是", "pin"),
            ("萍姐，我是", "pingj"),
            ("萍姐，我是", "ingj"),
            ("青松AI教育数字化平台.pdf", "qs"),
            ("青松AI教育数字化平台.pdf", "qingsong"),
            ("青松AI教育数字化平台.pdf", "songjia"),
            ("马春天.pdf", "machuntian"),
            ("部署 dev", "bushu"),
        ] {
            assert!(text_matches_query(text, q), "{text} 应命中 {q}");
            assert!(pinyin_hit_span(text, q).is_some(), "{text} 的 {q} 应有高亮区间");
        }
    }

    #[test]
    fn query_spaces_are_and_tokens() {
        assert!(text_matches_query("ai-edu-dataset", "ai edu"));
        assert!(text_matches_query("ai-edu-dataset", "ai  edu"));
        assert!(!text_matches_query("ai-edu-dataset", "ai xyz"));
        assert!(text_matches_query("foo", ""));
    }
}
