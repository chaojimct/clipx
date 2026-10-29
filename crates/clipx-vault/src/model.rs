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
}
