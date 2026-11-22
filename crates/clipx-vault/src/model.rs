//! Vault 条目模型：只带检索与展示所需的元数据，**永不携带秘密字段**。
//!
//! password / totp 永远走 [`crate::Vault::password`] / [`crate::Vault::totp`]
//! 现取现用（`Zeroizing<String>`），不进本结构、不进搜索索引、不进日志。

/// 一条 vault 登录条目（`bw list items` 单行映射）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultItem {
    /// `bw` 条目 id（传给 `bw get` 的唯一凭证，不展示）。
    pub id: String,
    /// 条目名（列表主行）。
    pub name: String,
    /// 用户名（列表副行；非秘密，Bitwarden 官方 UI 同样展示）。
    pub username: String,
    /// 是否含 TOTP（只记有无，不记种子）。
    pub has_totp: bool,
    /// 关联 URI 首个（匹配显示用）。
    pub uri: String,
}

impl VaultItem {
    /// 列表副行：`user · uri`，空段自动省略。
    pub fn subtitle(&self) -> String {
        match (self.username.is_empty(), self.uri.is_empty()) {
            (true, true) => String::new(),
            (false, true) => self.username.clone(),
            (true, false) => self.uri.clone(),
            (false, false) => format!("{} · {}", self.username, self.uri),
        }
    }

    /// 前台匹配分（Quick Access 置顶用）：3 强相关 / 2 弱相关 / 0 无信号。
    /// 只做大小写不敏感子串比对，不分词不碰网络；纯函数，单测锁定。
    /// `exe` 为归一化 stem（小写无后缀），`title` 为前台窗口标题；两空即 0。
    pub fn match_score(&self, exe: &str, title: &str) -> u8 {
        self.match_score_with(exe, title, &[])
    }

    /// 带自定义匹配词的版本（设置 `vault_match_words`，空格/逗号分隔）：
    /// 任一词命中标题/进程名/条目名/用户名/URI-host 即强匹配（3 分）。
    /// 解决特定页面总匹配不上的问题（中英文名对不上时手动补一词）。
    pub fn match_score_with(&self, exe: &str, title: &str, extra: &[String]) -> u8 {
        let base = self.match_score_base(exe, title);
        if base >= 3 {
            return 3;
        }
        let exe = exe.trim().to_lowercase();
        let title = title.trim().to_lowercase();
        if exe.is_empty() && title.is_empty() {
            return base;
        }
        let name = self.name.to_lowercase();
        let user = self.username.to_lowercase();
        let host = uri_host(&self.uri);
        for w in extra {
            let w = w.trim().to_lowercase();
            if w.len() <= 1 {
                continue;
            }
            if title.contains(&w)
                || exe.contains(&w)
                || (!name.is_empty() && name.contains(&w))
                || (!user.is_empty() && user.contains(&w))
                || (!host.is_empty() && host.contains(&w))
            {
                return 3;
            }
        }
        base
    }

    fn match_score_base(&self, exe: &str, title: &str) -> u8 {
        let exe = exe.trim().to_lowercase();
        let title = title.trim().to_lowercase();
        if exe.is_empty() && title.is_empty() {
            return 0;
        }
        let name = self.name.to_lowercase();
        let host = uri_host(&self.uri);
        // 强相关：名与 exe 互含，或完整 host 落在标题里。
        if !name.is_empty()
            && !exe.is_empty()
            && (name.contains(&exe) || exe.contains(&name))
        {
            return 3;
        }
        if !host.is_empty() && title.contains(&host) {
            return 3;
        }
        // 弱相关：host 核心词（首 label）命中 exe/标题，或名落在标题里。
        let core = host_core(&host);
        if !core.is_empty() && (exe.contains(&core) || title.contains(&core)) {
            return 2;
        }
        if !name.is_empty() && title.contains(&name) {
            return 2;
        }
        0
    }
}

/// 自定义匹配词切分（设置原文 → 小写词表）：逗号/空白分隔，空段丢弃。
/// 纯函数，三平台单测。
pub fn split_match_words(s: &str) -> Vec<String> {
    s.split([',', '，', ' ', '\t', '\n', '、'])
        .map(|w| w.trim().to_lowercase())
        .filter(|w| w.len() > 1)
        .collect()
}
/// URI 取 host（去 scheme/路径/端口，转小写；畸形即空串）。
fn uri_host(uri: &str) -> String {
    let s = uri.trim().to_lowercase();
    let s = s.split("://").last().unwrap_or("");
    let s = s.split(['/', '?', '#']).next().unwrap_or("");
    let s = s.split('@').last().unwrap_or("");
    let s = s.split(':').next().unwrap_or("");
    s.trim().trim_end_matches('.').to_string()
}

/// host 核心词：首 label，长度 ≤2 视为无意义（如 `co.uk` 的 `co`）。
fn host_core(host: &str) -> String {
    let core = host.split('.').next().unwrap_or("").to_string();
    if core.len() > 2 { core } else { String::new() }
}

/// `bw list items` 数组解析（字段缺失即跳过该条，不整体失败）。
pub fn parse_list(json: &str) -> Vec<VaultItem> {
    let mut out = Vec::new();
    let Ok(arr) = serde_json::from_str::<serde_json::Value>(json) else {
        return out;
    };
    let Some(items) = arr.as_array() else {
        return out;
    };
    for it in items {
        let Some(id) = it.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let name = it
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("(no name)")
            .to_string();
        let login = it.get("login");
        let username = login
            .and_then(|l| l.get("username"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let has_totp = login
            .and_then(|l| l.get("totp"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        let uri = login
            .and_then(|l| l.get("uris"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|u| u.get("uri"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(VaultItem {
            id: id.to_string(),
            name,
            username,
            has_totp,
            uri,
        });
    }
    out
}

/// `bw status` 解析：`(server_url, user_email, status)`。
/// status ∈ unlocked | locked | unauthenticated（未知值原样返回）。
pub fn parse_status(json: &str) -> Option<(String, String, String)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let server = v
        .get("serverUrl")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let email = v
        .get("userEmail")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let status = v.get("status").and_then(|x| x.as_str())?.to_string();
    Some((server, email, status))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST_FIXTURE: &str = r#"[
        {"object":"item","id":"uuid-1","name":"github","login":{"username":"alice","password":"x","totp":"y","uris":[{"uri":"https://github.com"}]}},
        {"object":"item","id":"uuid-2","name":"weird","login":{"username":null,"uris":[]}},
        {"object":"item","id":"","name":"noid"},
        {"object":"item","name":"missing-id"},
        {"object":"item","id":"uuid-3","name":"note-only","notes":"hi"}
    ]"#;

    #[test]
    fn parse_list_skips_bad_rows_keeps_good() {
        let items = parse_list(LIST_FIXTURE);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "uuid-1");
        assert_eq!(items[0].username, "alice");
        assert!(items[0].has_totp);
        assert_eq!(items[0].uri, "https://github.com");
        assert_eq!(items[0].subtitle(), "alice · https://github.com");
        // password/totp 种子绝不能进结构（类型层面就没有字段，编译期保证）。
        let dbg = format!("{:?}", items[0]);
        assert!(!dbg.contains('x'), "Debug 不得泄露秘密占位");
        assert_eq!(items[1].username, "");
        assert_eq!(items[1].subtitle(), "");
        assert!(!items[2].has_totp);
    }

    #[test]
    fn parse_list_rejects_non_array() {
        assert!(parse_list("{}").is_empty());
        assert!(parse_list("not json").is_empty());
    }

    #[test]
    fn parse_status_fields() {
        let (server, email, status) = parse_status(
            r#"{"serverUrl":"https://vault.bitwarden.com","userEmail":"a@b.c","status":"locked"}"#,
        )
        .unwrap();
        assert_eq!(server, "https://vault.bitwarden.com");
        assert_eq!(email, "a@b.c");
        assert_eq!(status, "locked");
    }

    fn scored(name: &str, uri: &str) -> VaultItem {
        VaultItem {
            id: "x".into(),
            name: name.into(),
            username: String::new(),
            has_totp: false,
            uri: uri.into(),
        }
    }

    #[test]
    fn match_score_tiers() {
        let gh = scored("github", "https://github.com");
        // 强相关：名与 exe 互含。
        assert_eq!(gh.match_score("github", ""), 3);
        assert_eq!(gh.match_score("github-desktop", "x"), 3);
        // 弱相关：host 核心词落在标题里。
        assert_eq!(gh.match_score("chrome", "GitHub - Google Chrome"), 2);
        // 无信号：空上下文 / 风马牛不相及。
        assert_eq!(gh.match_score("", ""), 0);
        assert_eq!(gh.match_score("notepad", "untitled"), 0);
    }

    #[test]
    fn match_score_name_in_title_and_cjk() {
        let wx = scored("微信", "");
        assert_eq!(wx.match_score("weixin", "微信"), 2);
        assert_eq!(wx.match_score("weixin", "chat"), 0);
        // 畸形 URI 不 panic，只给低分。
        let bad = scored("note", "not a uri at all");
        assert_eq!(bad.match_score("chrome", "Google"), 0);
        // 短 host 核心词（co.uk 的 co）不算信号。
        let uk = scored("bank", "https://bank.co.uk/login");
        assert_eq!(uk.match_score("chrome", "Bank - Chrome"), 2);
        assert_eq!(uk.match_score("co", ""), 0);
    }

    #[test]
    fn uri_host_parsing() {
        assert_eq!(uri_host("https://github.com/a?b#c"), "github.com");
        assert_eq!(uri_host("http://10.0.0.5:8080/x"), "10.0.0.5");
        assert_eq!(uri_host("  HTTP://Example.COM/ "), "example.com");
        assert_eq!(uri_host(""), "");
        assert_eq!(host_core("github.com"), "github");
        assert_eq!(host_core("co"), "");
    }

    #[test]
    fn split_match_words_splits_and_drops_noise() {
        assert_eq!(
            split_match_words("mockplus 阿里云,RAM，  a  "),
            vec!["mockplus", "阿里云", "ram"]
        );
        assert!(split_match_words("  , ， ").is_empty());
        // 单字无信号（误伤面太大），切分时直接丢掉。
        assert!(split_match_words("a").is_empty());
    }

    #[test]
    fn match_score_with_custom_words() {
        // 中文名 vs 英文标题：默认 0 分，补词后直达 3 分。
        let it = scored("阿里云", "https://account.aliyun.com/login");
        assert_eq!(it.match_score("msedge", "RAM 登录"), 0);
        let words = split_match_words("aliyun RAM");
        assert_eq!(it.match_score_with("msedge", "RAM 登录", &words), 3);
        // 命中条目名/用户名/URI 维度同样强匹配。
        let mut gh = scored("github", "https://github.com");
        gh.username = "dev01@example.com".into();
        assert_eq!(
            gh.match_score_with("chrome", "无关标题", &split_match_words("github")),
            3
        );
        assert_eq!(
            gh.match_score_with("chrome", "无关标题", &split_match_words("example")),
            3
        );
        // 空上下文不加分（防全库误弹）。
        assert_eq!(it.match_score_with("", "", &words), 0);
        // 无额外词退化为默认口径。
        assert_eq!(
            it.match_score_with("msedge", "RAM 登录", &[]),
            it.match_score("msedge", "RAM 登录")
        );
    }
}
