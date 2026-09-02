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
}
