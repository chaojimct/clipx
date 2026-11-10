//! 纯函数：设置/填充/版本等用户可见文案与决策（无 I/O、无平台 API）。
//!
//! 放在独立模块是为了三平台单测都能跑，不必拉起 UI、bw 或真实窗口。

/// 密码库依赖边界（不涉及真实保险库内容，只决定给用户看什么提示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultBoundary {
    /// 本机没有可用的 `bw` 二进制。
    MissingCli,
    /// 已安装但会话锁定（设置页解锁是主路径）。
    Locked,
    /// `bw login` 需要二次验证，应用内无法代劳。
    Needs2fa,
    /// 人机验证，必须去网页或终端。
    NeedsCaptcha,
}

/// 设置页状态行：缺二进制时必须仍以「未检测」开头（`vault_dot` 按此前缀匹配）。
#[cfg(test)]
pub const VAULT_STATUS_MISSING: &str = "未检测到 bw（请先安装 Bitwarden CLI）";

pub fn vault_boundary_hint(b: VaultBoundary) -> &'static str {
    match b {
        VaultBoundary::MissingCli => {
            "未检测到 Bitwarden CLI：打开设置 → 密码库，用「关联拓展包」下载，或自行安装 bw 后再搜 `bw `。"
        }
        VaultBoundary::Locked => {
            "密码库已锁定：打开设置 → 密码库，在解锁区输入主密码后点解锁。若账号开启了二次验证，请先在终端运行 `bw login` 完成验证再回来。"
        }
        VaultBoundary::Needs2fa => {
            "需要二次验证：请去终端运行 `bw login` 完成验证，再回到设置页点解锁。应用内无法代替完成二次验证。"
        }
        VaultBoundary::NeedsCaptcha => {
            "需要人机验证：请到网页端或终端完成一次登录后再回来解锁。"
        }
    }
}

/// QQ / 变形登录框填充之后的决策：宁可停，也不要把密码发到未验证的焦点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MorphFillDecision {
    /// 账号和密码都进入了对应框。
    Complete,
    /// 只确认密码进入了已校验的密码框（账号可能本来就在）。
    PasswordOnly,
    /// 只写了用户名：禁止再走剪贴板回落，以免密码贴进错误位置。
    UsernameOnlyStop,
    /// 命中特殊窗门但框没认出来：同样禁止剪贴板回落。
    UnrecognizedStop,
    /// 不是这类特殊窗，交给后续 typemode / 剪贴板路径。
    Fallthrough,
}

pub fn decide_morph_fill(user_ok: bool, pw_ok: bool, gate: bool) -> MorphFillDecision {
    match (user_ok, pw_ok) {
        (true, true) => MorphFillDecision::Complete,
        (false, true) => MorphFillDecision::PasswordOnly,
        (true, false) => MorphFillDecision::UsernameOnlyStop,
        (false, false) if gate => MorphFillDecision::UnrecognizedStop,
        (false, false) => MorphFillDecision::Fallthrough,
    }
}

/// 给用户看的结果说明。`Complete` / `Fallthrough` 不在这里给结语（走原成功/后续路径）。
pub fn morph_fill_message(d: MorphFillDecision, name: &str) -> Option<String> {
    match d {
        MorphFillDecision::Complete | MorphFillDecision::Fallthrough => None,
        MorphFillDecision::PasswordOnly => Some(format!(
            "已为 {name} 填入密码（账号未改）。请目视确认焦点仍在密码框后再继续。"
        )),
        MorphFillDecision::UsernameOnlyStop => Some(
            "用户名已填入，密码未写入：为避免把密码发到错误位置，已停止自动填充。请点进密码框后按 Alt+2 只填密码。"
                .into(),
        ),
        MorphFillDecision::UnrecognizedStop => Some(
            "未识别到可用的密码框，已停止自动填充，以免把密码发到错误位置。请点进密码框后按 Alt+2 只填密码；账号框用 Alt+1。"
                .into(),
        ),
    }
}

/// 老版迁移卡片只在「关于」组展示。
pub fn legacy_card_visible(section_id: &str, detected: bool) -> bool {
    detected && section_id == "about"
}

/// OCR 精度引擎模型在本进程里的可见状态（不含下载进度数字，那走提示条）。
pub fn ocr_pack_status_line(
    feature_on: bool,
    models_ready: bool,
    need_restart: bool,
    last_error: Option<&str>,
) -> Option<(String, bool)> {
    if !feature_on {
        return Some((
            "当前构建未含精度引擎（--no-default-features 应急瘦身）。".into(),
            true,
        ));
    }
    if let Some(err) = last_error {
        let err = err.trim();
        if !err.is_empty() {
            return Some((
                format!("模型下载失败：{err}。当前仍使用系统 OCR，可在下方重试下载。"),
                true,
            ));
        }
    }
    if models_ready && need_restart {
        return Some((
            "模型已下载，请重启 clipx 后生效；重启前仍使用系统 OCR。".into(),
            false,
        ));
    }
    if models_ready {
        return None;
    }
    Some((
        "精度引擎模型未就绪，当前使用系统 OCR。下载完成后需重启 clipx 才会切换到精度引擎。".into(),
        false,
    ))
}

/// 拓展包状态短文案（「已下载」不要让人以为当下进程已经切到精度引擎）。
pub fn ocr_pack_state_label(models_ready: bool, need_restart: bool) -> String {
    if models_ready && need_restart {
        "已下载（待重启）".into()
    } else if models_ready {
        "已下载".into()
    } else {
        String::new() // 调用方保留「未下载（n/m）」计算
    }
}

pub fn is_prerelease(cargo: &str) -> bool {
    cargo.contains('-')
}

/// 关于页版本行：预发布标识要写明不是正式发版。
pub fn version_label(cargo: &str) -> String {
    if is_prerelease(cargo) {
        format!("{cargo}（开发版，非正式发版）")
    } else {
        cargo.to_string()
    }
}

/// Inno Setup `VersionInfoVersion` 只能是 x.y.z.w 数字。`0.10.23-dev` → `0.10.23.0`。
#[cfg(test)]
pub fn file_version_quad(cargo: &str) -> String {
    let core = cargo.split('-').next().unwrap_or(cargo);
    let mut parts: Vec<&str> = core.split('.').collect();
    while parts.len() < 3 {
        parts.push("0");
    }
    format!("{}.{}.{}.0", parts[0], parts[1], parts[2])
}

/// Inno `AppVersion` / 输出文件名可用：非空、ASCII、无空白和路径非法字符。
#[cfg(test)]
pub fn inno_app_version_usable(s: &str) -> bool {
    !s.is_empty()
        && s.is_ascii()
        && !s.chars().any(|c| {
            c.is_whitespace() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_hints_are_actionable_and_dont_bypass_2fa() {
        let two = vault_boundary_hint(VaultBoundary::Needs2fa);
        assert!(two.contains("终端"));
        assert!(two.contains("bw login"));
        assert!(two.contains("无法代替"));
        assert!(!two.contains("跳过"));
        assert!(!two.contains("关闭二次验证"));
        let miss = vault_boundary_hint(VaultBoundary::MissingCli);
        assert!(miss.contains("设置"));
        assert!(miss.contains("拓展包") || miss.contains("安装"));
        let locked = vault_boundary_hint(VaultBoundary::Locked);
        assert!(locked.contains("设置"));
        assert!(locked.contains("解锁"));
        assert!(VAULT_STATUS_MISSING.starts_with("未检测"));
    }

    #[test]
    fn morph_fill_stops_instead_of_clipboard_fallback() {
        assert_eq!(
            decide_morph_fill(true, true, true),
            MorphFillDecision::Complete
        );
        assert_eq!(
            decide_morph_fill(false, true, true),
            MorphFillDecision::PasswordOnly
        );
        assert_eq!(
            decide_morph_fill(true, false, true),
            MorphFillDecision::UsernameOnlyStop
        );
        assert_eq!(
            decide_morph_fill(false, false, true),
            MorphFillDecision::UnrecognizedStop
        );
        assert_eq!(
            decide_morph_fill(false, false, false),
            MorphFillDecision::Fallthrough
        );
        let stop = morph_fill_message(MorphFillDecision::UsernameOnlyStop, "x").unwrap();
        assert!(stop.contains("Alt+2"));
        assert!(stop.contains("停止"));
        let lost = morph_fill_message(MorphFillDecision::UnrecognizedStop, "x").unwrap();
        assert!(lost.contains("错误位置"));
        assert!(morph_fill_message(MorphFillDecision::Complete, "n").is_none());
        let pw = morph_fill_message(MorphFillDecision::PasswordOnly, "mail").unwrap();
        assert!(pw.contains("mail"));
        assert!(pw.contains("账号未改"));
    }

    #[test]
    fn legacy_card_only_on_about() {
        assert!(!legacy_card_visible("advanced", true));
        assert!(!legacy_card_visible("vault", true));
        assert!(!legacy_card_visible("about", false));
        assert!(legacy_card_visible("about", true));
        assert!(!legacy_card_visible("", true));
    }

    #[test]
    fn ocr_status_explains_restart_and_fallback() {
        let (t, warn) = ocr_pack_status_line(true, false, false, None).unwrap();
        assert!(t.contains("系统 OCR"));
        assert!(t.contains("重启"));
        assert!(!warn);
        let (t, warn) = ocr_pack_status_line(true, true, true, None).unwrap();
        assert!(t.contains("请重启"));
        assert!(!warn);
        assert!(ocr_pack_status_line(true, true, false, None).is_none());
        let (t, warn) = ocr_pack_status_line(true, false, false, Some("网络中断")).unwrap();
        assert!(warn);
        assert!(t.contains("网络中断"));
        assert!(t.contains("系统 OCR"));
        let (t, warn) = ocr_pack_status_line(false, false, false, None).unwrap();
        assert!(warn);
        assert!(t.contains("未含精度引擎"));
        assert_eq!(ocr_pack_state_label(true, true), "已下载（待重启）");
        assert_eq!(ocr_pack_state_label(true, false), "已下载");
        assert_eq!(ocr_pack_state_label(false, false), "");
    }

    #[test]
    fn version_label_marks_prerelease_and_inno_quad_is_numeric() {
        assert_eq!(
            version_label("0.10.23-dev"),
            "0.10.23-dev（开发版，非正式发版）"
        );
        assert_eq!(version_label("0.10.23"), "0.10.23");
        assert_eq!(file_version_quad("0.10.23-dev"), "0.10.23.0");
        assert_eq!(file_version_quad("0.10.23"), "0.10.23.0");
        assert!(inno_app_version_usable("0.10.23-dev"));
        assert!(inno_app_version_usable("0.10.23"));
        assert!(!inno_app_version_usable("0.10.23 dev"));
        assert!(!inno_app_version_usable(r"0.10\23"));
        let v = env!("CARGO_PKG_VERSION");
        assert!(inno_app_version_usable(v), "workspace 版本不能用于 Inno：{v}");
        let quad = file_version_quad(v);
        assert!(
            quad.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
            "VersionInfoVersion 必须是数字四段：{quad}"
        );
        if is_prerelease(v) {
            assert!(version_label(v).contains("开发版"));
        }
    }
}
