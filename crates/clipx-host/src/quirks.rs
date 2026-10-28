//! 按应用 quirks 策略表（Phase C）。
//!
//! 原散落在 `win_popup.rs` / `paste.rs` 里的按应用 `if`（微信主进程、
//! 企业微信 Qt、Shell 宿主、终端类名/进程表、Electron 粘贴等待）收敛到此：
//! **身份识别 + 策略开关进表，几何数学不动**。以后新增按应用特判只加表行，
//! 不许在调用点散写 `exe.contains(...)`（冻结线，见 docs/ROADMAP.md 计划重对齐）。
//!
//! 全部纯字符串逻辑：零平台依赖，三平台可单测。

/// exe 文件名归一化：取 basename → 小写 → 去 `.exe` 后缀。
///
/// 曾用名 `fg_exe_stem` 的教训：实现从没去过后缀，按 stem 精确比较会误判，
/// 故归一化收敛到这一处（2026-09-21 微信特判坑）。
pub fn exe_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let lower = file.to_ascii_lowercase();
    lower
        .strip_suffix(".exe")
        .unwrap_or(&lower)
        .to_string()
}

/// 按应用策略开关（一次查表，多处复用，避免每次调 win32 取前台进程）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AppQuirks {
    /// 微信**主进程**（Weixin.exe / 旧版 WeChat.exe）：聊天输入区固定底部，可几何锚定。
    /// `WeChatAppEx.exe`（小程序/内置浏览器）必须排除——输入框不固定底部。
    pub wechat_main: bool,
    /// 企业微信（WXWork.exe，32 位）：Qt 自绘 + 无 TSF 文档，信标多数时候 miss，
    /// 与微信同源走几何锚定。
    pub wework: bool,
}

impl AppQuirks {
    pub fn for_exe(file_name: &str) -> Self {
        let stem = exe_stem(file_name);
        Self {
            wechat_main: stem == "weixin" || stem == "wechat",
            wework: stem == "wxwork",
        }
    }
}

/// Shell 宿主进程（开始菜单/搜索等全屏浮层）：弹窗若跟光标会被盖住（Win11 下
/// Shell 处更高 Z 带），检测到退到工作区左上固定位（对齐 WPF
/// `PositionPopupFixedShellWorkArea`）。入参为已归一化的 stem（大小写不敏感）。
pub fn is_dedicated_shell_host(stem: &str) -> bool {
    matches!(
        stem.to_ascii_lowercase().as_str(),
        "searchhost" | "startmenuexperiencehost" | "shellexperiencehost" | "shellhost"
    )
}

/// Chromium / Electron 宿主窗口类 token（WorkBuddy、VS Code、网页壳）。
/// 注意这是**窗口类名**维度，不是 exe 维度（`Weixin.exe` 的类名含 webview 也不算）。
pub fn is_chromium_token(s: &str) -> bool {
    let n = s.to_ascii_lowercase();
    n.contains("chrome_widgetwin") || n.contains("chrome_renderwidget")
}

/// 窗口类是否终端宿主（conhost / Windows Terminal / mintty 等）。
pub fn is_terminal_class_name(class: &str) -> bool {
    let c = class.to_ascii_lowercase();
    matches!(
        c.as_str(),
        "consolewindowclass"
            | "cascadia_hosting_window_class"
            | "pseudoconsolewindow"
            | "mintty"
            | "putty"
            | "kitty"
            | "alacritty"
    ) || c.contains("wezterm")
        || c.contains("xshell")
        || c.contains("mobaxterm")
        || c.contains("tabby")
}

/// 进程名是否终端（对齐 WPF `PasteTargetHeuristics`，并补 SSH 客户端）。
///
/// **不要**把 Cursor / VS Code 这类 Electron 编辑器加进来（v0.10.9 收回
/// `9ae9c09` 的顺手扩大）：它们的集成终端画在主窗口里（没有独立 HWND），
/// 按进程名判等于把编辑器/对话输入框一起判成终端——后果是被强行改用
/// Shift+Insert、文本被去 CR。Windows 下集成终端的复制粘贴就是 Ctrl+C / Ctrl+V
/// （只有 Linux 才是 Ctrl+Shift+V），加它们既没必要也有害。
pub fn is_terminal_process_name(s: &str) -> bool {
    let n = exe_stem(s);
    matches!(
        n.as_str(),
        "cmd"
            | "powershell"
            | "pwsh"
            | "windowsterminal"
            | "wt"
            | "openconsole"
            | "conhost"
            | "mintty"
            | "bash"
            | "wsl"
            | "wslhost"
            | "wezterm-gui"
            | "wezterm"
            | "putty"
            | "kitty"
            | "tabby"
            | "alacritty"
            | "xshell"
            | "xshell5"
            | "xshell6"
            | "xshell7"
            | "mobaxterm"
    )
}

/// Electron 系（opencode Desktop / VS Code / Cursor）读剪贴板走异步 IPC，
/// 写完立刻 Ctrl+V 会撞上旧内容/空内容；多等一帧。原生应用无感，补全场景更不敏感。
/// （原 `logic.rs` 行内粘贴路径的裸 `60`，收编为命名常量。）
pub const ELECTRON_PASTE_SETTLE_MS: u64 = 60;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_normalizes() {
        assert_eq!(exe_stem(r"C:\Windows\System32\conhost.exe"), "conhost");
        assert_eq!(exe_stem("WindowsTerminal.exe"), "windowsterminal");
        assert_eq!(exe_stem("pwsh"), "pwsh");
        assert_eq!(exe_stem("/usr/bin/nvim"), "nvim");
    }

    #[test]
    fn wechat_main_matches_only_main_process() {
        assert!(AppQuirks::for_exe("Weixin.exe").wechat_main);
        assert!(AppQuirks::for_exe("WeChat.exe").wechat_main);
        // 小程序/内置浏览器必须排除：输入框不固定底部。
        assert!(!AppQuirks::for_exe("WeChatAppEx.exe").wechat_main);
        assert!(!AppQuirks::for_exe("WorkBuddy.exe").wechat_main);
    }

    #[test]
    fn wework_matches() {
        let q = AppQuirks::for_exe("WXWork.exe");
        assert!(q.wework);
        assert!(!q.wechat_main);
    }

    #[test]
    fn shell_hosts() {
        assert!(is_dedicated_shell_host("StartMenuExperienceHost"));
        assert!(is_dedicated_shell_host("SearchHost"));
        assert!(is_dedicated_shell_host("ShellExperienceHost"));
        assert!(is_dedicated_shell_host("ShellHost"));
        assert!(is_dedicated_shell_host("searchhost")); // 大小写不敏感
        assert!(!is_dedicated_shell_host("explorer"));
        assert!(!is_dedicated_shell_host("WorkBuddy"));
        assert!(!is_dedicated_shell_host(""));
    }

    #[test]
    fn terminal_tables_keep_v0109_lessons() {
        assert!(is_terminal_class_name("CASCADIA_HOSTING_WINDOW_CLASS"));
        assert!(is_terminal_class_name("ConsoleWindowClass"));
        assert!(!is_terminal_class_name("Chrome_WidgetWin_1"));
        assert!(is_terminal_process_name("WindowsTerminal.exe"));
        assert!(is_terminal_process_name("pwsh"));
        assert!(is_terminal_process_name(r"C:\Windows\System32\conhost.exe"));
        assert!(is_terminal_process_name("wezterm-gui"));
        assert!(!is_terminal_process_name("WorkBuddy.exe"));
        assert!(!is_terminal_process_name("msedge"));
        // Electron 编辑器永不算终端（v0.10.9 收回）。
        assert!(!is_terminal_process_name("Cursor"));
        assert!(!is_terminal_process_name(
            r"C:\Program Files\cursor\Cursor.exe"
        ));
        assert!(!is_terminal_process_name("Code.exe"));
        assert!(!is_terminal_process_name("Code - Insiders.exe"));
    }

    #[test]
    fn chromium_token_is_class_dimension() {
        assert!(is_chromium_token("Chrome_WidgetWin_1"));
        assert!(is_chromium_token("Chrome_RenderWidgetHostHWND"));
        assert!(!is_chromium_token("Weixin.exe"));
        assert!(!is_chromium_token("WorkBuddy.exe"));
    }
}
