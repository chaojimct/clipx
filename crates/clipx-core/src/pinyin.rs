use pinyin::{ToPinyin, ToPinyinMulti};

/// 拼音串源文本上限：blob 走 LIKE 子串匹配，限制体积保证按键级查询性能。
/// WPF 版上限 8192（内存线性扫描）；DB 场景 512 已覆盖绝大多数检索目标。
const MAX_SOURCE_CHARS: usize = 512;

/// 模糊归一（listary #4：z/zh、c/ch、s/sh、an/ang、en/eng、in/ing、ü/v）。
/// blob 侧按音节归一、查询侧按串归一（最长匹配左到右），两边同归一即模糊命中。
/// 代价是过度归并（如 san/shan/sang/shang 全归 "san"）——剪贴板检索宁可多召回，
/// 排序会把精确的顶上去（见 docs/search-scoring.md）。
fn canonical_syllable(py: &str) -> String {
    // ü 在 plain() 里原样保留（如 "lü"），统一归成 v（用户都打 lv）。
    let mut s = py.replace('ü', "v").replace('Ü', "v");
    for (long, short) in [("zh", "z"), ("ch", "c"), ("sh", "s")] {
        if let Some(rest) = s.strip_prefix(long) {
            s = format!("{short}{rest}");
            break;
        }
    }
    for (long, short) in [("ang", "an"), ("eng", "en"), ("ing", "in")] {
        if let Some(stem) = s.strip_suffix(long) {
            s = format!("{stem}{short}");
            break;
        }
    }
    s
}

/// 查询 token 的模糊归一：与 [`canonical_syllable`] 同一套映射，但查询串没有
/// 音节边界（如 `shange` 可能是 sh-an-ge），分歧点（ang/eng/ing 缩不缩）全部分支，
/// 最多 4 个变体（首 2 个分歧点全分支，其余强制归一；去重）。
/// 非 ASCII（中文）原样通过——字面匹配不受影响。
/// blob 侧是按音节确定归一的，查询侧用变体 OR 即全覆盖，无回归。
pub fn canonical_query_variants(token: &str) -> Vec<String> {
    // 第一步确定性：小写、ü→v、声母 zh/ch/sh 左到右（声母不可能歧义）。
    let lower = token.to_lowercase();
    let mut s = String::with_capacity(lower.len());
    let mut i = 0;
    let bytes = lower.as_bytes();
    while i < bytes.len() {
        let rest = &lower[i..];
        if let Some(ch) = rest.chars().next() {
            if ch == 'ü' || ch == 'Ü' {
                s.push('v');
                i += ch.len_utf8();
                continue;
            }
            if !ch.is_ascii_alphabetic() {
                s.push(ch);
                i += ch.len_utf8();
                continue;
            }
        }
        let mut hit = false;
        for (long, short) in [("zh", "z"), ("ch", "c"), ("sh", "s")] {
            if rest.starts_with(long) {
                s.push_str(short);
                i += long.len();
                hit = true;
                break;
            }
        }
        if !hit {
            s.push(bytes[i] as char);
            i += 1;
        }
    }
    // 第二步分歧点：ang/eng/ing 在串里出现的位置（非重叠左到右，只认字符边界）。
    let sb = s.as_bytes();
    let mut points: Vec<(usize, &str)> = Vec::new();
    let mut j = 0;
    while j < sb.len() {
        if s.is_char_boundary(j) && j + 3 <= sb.len() {
            let rest = &s[j..];
            let mut hit = false;
            for (long, short) in [("ang", "an"), ("eng", "en"), ("ing", "in")] {
                if rest.starts_with(long) {
                    points.push((j, short));
                    j += long.len();
                    hit = true;
                    break;
                }
            }
            if hit {
                continue;
            }
        }
        // 原样过一个字符。
        if sb[j] < 0x80 {
            j += 1;
        } else {
            j += s[j..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
    }
    if points.is_empty() {
        return vec![s];
    }
    // 变体 = 全归一 + 每个分歧点各一个"该点保持、其余归一"（k+1 个，封顶 4）。
    // 保持分支用来撞跨音节边界的巧合（如 blob "an"+"ge"="ange" 含字面 "ang"）。
    let mut out = vec![reduce_all_finals(&s)];
    for (pos, _) in points.iter().take(3) {
        // 该点保持 3 字母原样，前后段走全归一（保持"只留本点"）。
        let mut full = reduce_all_finals(&s[..*pos]);
        full.push_str(&s[*pos..*pos + 3]);
        full.push_str(&reduce_all_finals(&s[*pos + 3..]));
        out.push(full);
    }
    out.sort();
    out.dedup();
    out.truncate(4);
    out
}

/// 串内 ang/eng/ing 全归一（变体生成的收尾用；中文字符原样过）。
fn reduce_all_finals(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if s.is_char_boundary(i) && i + 3 <= b.len() {
            let rest = &s[i..];
            let mut hit = false;
            for (long, short) in [("ang", "an"), ("eng", "en"), ("ing", "in")] {
                if rest.starts_with(long) {
                    out.push_str(short);
                    i += long.len();
                    hit = true;
                    break;
                }
            }
            if hit {
                continue;
            }
        }
        if b[i] < 0x80 {
            out.push(b[i] as char);
            i += 1;
        } else {
            let ch = s[i..].chars().next().unwrap_or('\u{FFFD}');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 生成拼音检索 blob（对齐 WPF PinyinSearchIndex.BuildBlob）：
/// 主读音全拼连写 + 主读音首字母连写 + 次读音全拼连写 + 次读音首字母连写，
/// 全部走模糊归一，统一小写无分隔。
/// 查询侧用 LIKE '%q%' 子串匹配，因此支持任意起始位置（"nihao"/"shijie"/"nh"）。
/// 次读音示例（具体值以 `pinyin` crate 数据为准，见单测）：「重」主 zhong + 次 chong。
pub fn to_pinyin_blob(text: &str) -> String {
    build_blob(text).0
}

/// 主读音之外的次读音（plain, 首字母），去重去主。「还」→ [("huan","h"), ...]。
fn secondary_readings(ch: char, primary_plain: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(multi) = ch.to_pinyin_multi() {
        for py in multi.into_iter() {
            let plain = py.plain().to_string();
            if plain == primary_plain {
                continue;
            }
            let initial = py.first_letter().to_string();
            if !out.iter().any(|(p, _)| *p == plain) {
                out.push((plain, initial));
            }
        }
    }
    out
}

/// 与 `to_pinyin_blob` 逐字节一致的 blob，外加「blob 字节区间 → 源字符下标」映射。
/// 映射表项为 `(blob_start, blob_end, char_index)`，字符区间用集合包含关系还原。
fn indexed_blob(text: &str) -> (String, Vec<(usize, usize, usize)>) {
    build_blob(text)
}

/// blob 构建本体（`to_pinyin_blob` 与 `indexed_blob` 同源，防漂移）：
/// [主全拼][主首字母][次全拼][次首字母]，四段全走模糊归一。
/// 映射只记"哪段字节属于哪个源字符"，归一不改变分段归属。
fn build_blob(text: &str) -> (String, Vec<(usize, usize, usize)>) {
    let mut full = String::new();
    let mut full_map: Vec<(usize, usize, usize)> = Vec::new();
    let mut initials = String::new();
    let mut init_map: Vec<(usize, usize, usize)> = Vec::new();
    let mut sec_full = String::new();
    let mut sec_full_map: Vec<(usize, usize, usize)> = Vec::new();
    let mut sec_init = String::new();
    let mut sec_init_map: Vec<(usize, usize, usize)> = Vec::new();
    let mut any = false;
    for (i, ch) in text.chars().enumerate() {
        if i >= MAX_SOURCE_CHARS {
            break;
        }
        let Some(py) = ch.to_pinyin() else { continue };
        any = true;
        let plain = canonical_syllable(py.plain());
        let at = full.len();
        full.push_str(&plain);
        full_map.push((at, full.len(), i));
        let initial = canonical_syllable(py.first_letter());
        let at = initials.len();
        initials.push_str(&initial);
        init_map.push((at, initials.len(), i));
        for (sec_plain, sec_initial) in secondary_readings(ch, py.plain()) {
            let canon = canonical_syllable(&sec_plain);
            let at = sec_full.len();
            sec_full.push_str(&canon);
            sec_full_map.push((at, sec_full.len(), i));
            let canon_init = canonical_syllable(&sec_initial);
            // 首字母归一后可能为空（如 ü→v 不会，但防万一），空则不记。
            if !canon_init.is_empty() {
                let at = sec_init.len();
                sec_init.push_str(&canon_init);
                sec_init_map.push((at, sec_init.len(), i));
            }
        }
    }
    if !any {
        return (String::new(), Vec::new());
    }
    let mut blob = full;
    let mut map = full_map;
    let shift = |m: &mut Vec<(usize, usize, usize)>, off: usize| {
        for (a, b, _) in m.iter_mut() {
            *a += off;
            *b += off;
        }
    };
    let mut sec_map = sec_full_map;
    let mut sec_im = sec_init_map;
    let off1 = blob.len();
    blob.push_str(&initials);
    shift(&mut init_map, off1);
    let off2 = blob.len();
    blob.push_str(&sec_full);
    shift(&mut sec_map, off2);
    let off3 = blob.len();
    blob.push_str(&sec_init);
    shift(&mut sec_im, off3);
    map.extend(init_map);
    map.extend(sec_map);
    map.extend(sec_im);
    (blob, map)
}

/// 空格分词 AND：每段需为原文子串（忽略大小写）或拼音 blob 子串。
/// 字面走原文 token；拼音走模糊归一 token（与入库 blob 同归一）。
///
/// - `"qs"` 命中「青松…」（首字母）
/// - `"qingsong"` 命中「青松…」（全拼）
/// - `"ai edu"` 命中 `ai-edu-dataset`（两段都是原文子串）
/// - `"zuan"` 命中「转」（`zhuan` 归一；模糊）
/// - `"lv"` 命中「绿」（`lü` 归一；模糊）
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
        contains_ci(&lower, &t)
            || (!blob.is_empty()
                && canonical_query_variants(&t)
                    .iter()
                    .any(|cq| blob.contains(cq.as_str())))
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
        // 拼音走模糊归一变体（与入库 blob 同归一）：`zuan` 能定位「转」。
        // 变体取并（`shange`→{sane,sange} 全覆盖分词歧义）。
        let cqs = canonical_query_variants(&q);
        let (blob, map) = blob_cache.get_or_insert_with(|| indexed_blob(text));
        if blob.is_empty() {
            continue;
        }
        for cq in &cqs {
            for (s, e) in substring_hits(blob, cq) {
                if let Some(span) = map_byte_range(map, s, e) {
                    spans.push(span);
                }
            }
        }
    }
    merge_spans(spans)
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
        // 部(bus)署：主 shu→су "su" + 次 pou（`pinyin` crate 异读表数据）。
        let blob = to_pinyin_blob("部署 dev");
        assert_eq!(blob, "busubspoup");
    }

    #[test]
    fn supports_mid_string_starts() {
        let blob = to_pinyin_blob("你好世界");
        assert!(blob.contains("niha"));
        // 归一后 shijie→{sijie}：查原文串用变体。
        assert!(canonical_query_variants("shijie")
            .iter()
            .any(|v| blob.contains(v)));
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
    fn canonical_query_variants_cover_segmentation() {
        // 确定性：无分歧点即单变体。
        assert_eq!(canonical_query_variants("shi"), vec!["si"]);
        assert_eq!(canonical_query_variants("hello"), vec!["hello"]);
        assert_eq!(canonical_query_variants("青松"), vec!["青松"]);
        // 大小写 + ü。
        assert_eq!(canonical_query_variants("Zhuang"), vec!["zuan", "zuang"]);
        assert_eq!(canonical_query_variants("Lü"), vec!["lv"]);
        // 分歧点分支：`shange`→{sane,sange}，`changan`→{canan,cangan}。
        assert_eq!(
            canonical_query_variants("shange"),
            vec!["sane".to_string(), "sange".to_string()]
        );
        assert!(canonical_query_variants("changan").contains(&"canan".to_string()));
        // 4 变体封顶。
        assert!(canonical_query_variants("shanghang").len() <= 4);
    }

    #[test]
    fn secondary_readings_are_recallable() {
        // 次读音来自 `pinyin` crate 异读表（heteronym 特性，默认开启）。
        // 行 secondary han/hen、乐 yue、都 du、觉 jiao、长 chang——单字查询全召回。
        assert!(text_matches_query("银行", "hang"));
        assert!(text_matches_query("银行", "yinxin"));
        assert!(text_matches_query("音乐", "yue"));
        assert!(text_matches_query("首都", "du"));
        assert!(text_matches_query("睡觉", "jiao"));
        assert!(text_matches_query("长江", "can"));
        assert_eq!(pinyin_hit_span("银行", "hang"), Some((1, 2)));
        assert_eq!(pinyin_hit_span("音乐", "yue"), Some((1, 2)));
        // 已知边界：次读音存在独立节里，跨节的多字组合查不到
        //（如 "changjiang" 拼不出连续串；LIKE 子串架构的固有上限）。
        // 单字次读音永远可召回。
        assert!(!text_matches_query("长江", "changjiang"));
    }

    #[test]
    fn fuzzy_queries_recall_canonical() {
        // z/zh、s/sh、c/ch、ang/an、ü/v 互通（单音节；多音节歧义走变体 OR）。
        assert!(text_matches_query("张三", "zan"));
        assert!(text_matches_query("张三", "zhang"));
        assert!(text_matches_query("三个苹果", "sange"));
        assert!(text_matches_query("三个苹果", "shange"));
        assert!(text_matches_query("长江", "can"));
        assert!(text_matches_query("长江", "zan"));
        assert!(text_matches_query("绿茶", "lv"));
        assert_eq!(pinyin_hit_span("张三", "zan"), Some((0, 1)));
        assert_eq!(pinyin_hit_span("绿茶", "lv"), Some((0, 1)));
        // 精确串仍然精确（归一是双向一致的）。
        assert!(!text_matches_query("张三", "xyz"));
    }

    #[test]
    fn query_spaces_are_and_tokens() {
        assert!(text_matches_query("ai-edu-dataset", "ai edu"));
        assert!(text_matches_query("ai-edu-dataset", "ai  edu"));
        assert!(!text_matches_query("ai-edu-dataset", "ai xyz"));
        assert!(text_matches_query("foo", ""));
    }
}
