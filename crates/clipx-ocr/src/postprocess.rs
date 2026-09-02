//! OCR 文本后处理（WPF 版 OcrTextPostProcessor 的 Rust 移植）：
//! Windows OCR 引擎按词返回，词间拼接需要 CJK 感知的空格策略——
//! 拉丁词之间保留空格，CJK 字符之间去掉引擎误插的空格。

pub fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{3040}'..='\u{309F}'
        | '\u{30A0}'..='\u{30FF}'
        | '\u{AC00}'..='\u{D7AF}'
        | '\u{3130}'..='\u{318F}')
}

fn uses_latin_word_spacing(c: char) -> bool {
    if (c as u32) <= 0x7F {
        c.is_ascii_alphanumeric()
    } else {
        c.is_alphabetic() && !is_cjk_char(c)
    }
}

fn should_insert_space_between_words(previous_word: &str, next_word: &str) -> bool {
    let (Some(a), Some(b)) = (previous_word.chars().last(), next_word.chars().next()) else {
        return false;
    };
    let a_cjk = is_cjk_char(a);
    let b_cjk = is_cjk_char(b);
    if a_cjk && b_cjk {
        return false;
    }
    if uses_latin_word_spacing(a) && uses_latin_word_spacing(b) {
        return true;
    }
    a_cjk != b_cjk
}

/// 按行组织 OCR 词 → 拼接文本；全空白返回 None。
pub fn format_result(lines: &[Vec<String>]) -> Option<String> {
    let mut sb = String::new();
    for line in lines {
        if !sb.is_empty() {
            sb.push('\n');
        }
        let mut last_word: Option<&str> = None;
        for word in line {
            if word.is_empty() {
                continue;
            }
            if let Some(prev) = last_word {
                if !sb.is_empty()
                    && !sb.ends_with('\n')
                    && should_insert_space_between_words(prev, word)
                {
                    sb.push(' ');
                }
            }
            sb.push_str(word);
            last_word = Some(word.as_str());
        }
    }
    let built = normalize(&sb);
    if built.trim().is_empty() {
        None
    } else {
        Some(built)
    }
}

/// 统一换行符、删除 CJK 字符之间的空白（含全角空格）、去首尾空白。
pub fn normalize(text: &str) -> String {
    let unified = text.replace("\r\n", "\n").replace('\r', "\n");
    strip_inter_cjk_spaces(&unified).trim().to_string()
}

fn strip_inter_cjk_spaces(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let is_blank = |c: char| c == ' ' || c == '\t' || c == '\u{3000}';
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if is_blank(chars[i]) {
            let mut j = i;
            while j < chars.len() && is_blank(chars[j]) {
                j += 1;
            }
            let prev_cjk = i > 0 && is_cjk_char(chars[i - 1]);
            let next_cjk = j < chars.len() && is_cjk_char(chars[j]);
            if !(prev_cjk && next_cjk) {
                out.extend(&chars[i..j]);
            }
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_words_get_spaces() {
        let lines = vec![vec!["hello".into(), "world".into()]];
        assert_eq!(format_result(&lines).as_deref(), Some("hello world"));
    }

    #[test]
    fn cjk_chars_lose_spaces() {
        let lines = vec![vec!["你".into(), "好".into(), "世".into(), "界".into()]];
        assert_eq!(format_result(&lines).as_deref(), Some("你好世界"));
    }

    #[test]
    fn mixed_cjk_latin_gets_space() {
        let lines = vec![vec!["部署".into(), "dev".into(), "环境".into()]];
        assert_eq!(format_result(&lines).as_deref(), Some("部署 dev 环境"));
    }

    #[test]
    fn multi_line_joins_with_newline() {
        let lines = vec![vec!["第一行".into()], vec!["second".into(), "line".into()]];
        assert_eq!(
            format_result(&lines).as_deref(),
            Some("第一行\nsecond line")
        );
    }

    #[test]
    fn normalize_strips_inter_cjk_fullwidth_space() {
        assert_eq!(normalize("你 好\u{3000}世 界 x"), "你好世界 x");
        // CJK 与拉丁之间的空格保留
        assert_eq!(normalize("你好 world"), "你好 world");
    }

    #[test]
    fn empty_result_is_none() {
        assert!(format_result(&[]).is_none());
        assert!(format_result(&[vec![]]).is_none());
        assert!(format_result(&[vec![" ".into()]]).is_none());
    }
}
