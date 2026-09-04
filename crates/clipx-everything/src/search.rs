//! Everything 检索表达式构造（纯函数，行为移植自 WPF ExplorerQuickFindController）。

/// 规范化当前文件夹路径，供 `parent:` / `path:` 使用。
///
/// WPF 两个历史坑（ExplorerQuickFindController.NormalizeFolderForEverything 注记）：
/// 1. 把 `C:\` 收成 `C:` 会让 `parent:C:` 与正确的 `parent:C:\` 不一致 → 盘符根搜索 0 条；
///    盘符根固定为 `X:\`。
/// 2. 引号内路径末尾反斜杠 `\"` 会被 Everything 当成转义引号 → 非根路径去掉末尾分隔符，
///    盘符根保留 `X:\`（无引号、无歧义）。
pub fn normalize_folder_for_everything(folder: &str) -> String {
    let t = folder.trim();
    if t.is_empty() {
        return String::new();
    }
    let full = t.replace('/', "\\");
    let b = full.as_bytes();
    // 盘符路径（含 "C:" 裸盘符 → 按根处理；Explorer 不会产出该形态，防御性兜底）
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let drive = b[0].to_ascii_uppercase() as char;
        let rest = full[2..].trim_matches('\\');
        if rest.is_empty() {
            return format!("{drive}:\\");
        }
        return format!("{drive}:\\{rest}");
    }
    // UNC 或其它：仅去末尾分隔符
    full.trim_end_matches('\\').to_string()
}

/// 当前文件夹**一层**子项 + 可选关键词（`parent:`）。
pub fn build_parent_scoped_search(folder: &str, typing: &str) -> String {
    let f = normalize_folder_for_everything(folder);
    let kw = typing.trim();
    if f.is_empty() {
        return kw.to_string();
    }
    let token = format!("parent:{}", quote_path_token(&f));
    if kw.is_empty() {
        token
    } else {
        format!("{token} {kw}")
    }
}

/// 当前路径**树下**任意深度 + 可选关键词（`path:` 匹配完整路径前缀）。
pub fn build_path_subtree_scoped_search(folder: &str, typing: &str) -> String {
    let f = normalize_folder_for_everything(folder);
    let kw = typing.trim();
    if f.is_empty() {
        return kw.to_string();
    }
    let token = format!("path:{}", quote_path_token(&f));
    if kw.is_empty() {
        token
    } else {
        format!("{token} {kw}")
    }
}

/// 全文件夹检索（`folder:"…"`，对齐 WPF `EverythingIpc.TryQueryFolderPaths`：
/// 关键词内引号转义、超长截断 1800 字符、空输入返回空串由调用方短路）。
pub fn build_folder_search(user_typing: &str) -> String {
    let q: String = user_typing.trim().chars().take(1800).collect();
    let escaped = q.replace('"', "\\\"");
    format!("folder:\"{escaped}\"")
}

/// 路径含空格时用双引号包住；末尾不应有反斜杠（避免 `\"` 被解释为转义引号）。
fn quote_path_token(path: &str) -> String {
    if path.contains(' ') {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_drive_root_forms() {
        assert_eq!(normalize_folder_for_everything("C:\\"), "C:\\");
        assert_eq!(normalize_folder_for_everything("c:\\"), "C:\\");
        assert_eq!(normalize_folder_for_everything("C:"), "C:\\");
        assert_eq!(normalize_folder_for_everything("d:  "), "D:\\");
    }

    #[test]
    fn normalize_strips_trailing_separators() {
        assert_eq!(normalize_folder_for_everything("C:\\foo\\"), "C:\\foo");
        assert_eq!(normalize_folder_for_everything("C:\\foo\\bar"), "C:\\foo\\bar");
        assert_eq!(normalize_folder_for_everything("C:/foo/bar/"), "C:\\foo\\bar");
    }

    #[test]
    fn normalize_unc_and_empty() {
        assert_eq!(normalize_folder_for_everything("\\\\srv\\share\\"), "\\\\srv\\share");
        assert_eq!(normalize_folder_for_everything("  "), "");
        assert_eq!(normalize_folder_for_everything(""), "");
    }

    #[test]
    fn parent_search_quotes_paths_with_spaces() {
        assert_eq!(build_parent_scoped_search("C:\\foo bar\\", "txt"), "parent:\"C:\\foo bar\" txt");
        assert_eq!(build_parent_scoped_search("C:\\foo", ""), "parent:C:\\foo");
        assert_eq!(build_parent_scoped_search("", "kw"), "kw");
        assert_eq!(build_parent_scoped_search("C:\\", "readme"), "parent:C:\\ readme");
    }

    #[test]
    fn path_search_uses_path_prefix_token() {
        assert_eq!(build_path_subtree_scoped_search("C:\\foo\\", "a b"), "path:C:\\foo a b");
        assert_eq!(build_path_subtree_scoped_search("", ""), "");
    }

    #[test]
    fn folder_search_escapes_inner_quotes() {
        assert_eq!(build_folder_search("a\"b"), "folder:\"a\\\"b\"");
        assert_eq!(build_folder_search("  简单  "), "folder:\"简单\"");
    }
}
