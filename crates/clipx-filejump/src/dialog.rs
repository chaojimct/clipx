//! 对话框分类（M5a）：纯逻辑部分跨平台可测，Win32 取证在 `win` 子模块。
//!
//! 移植基线 `FileDialogJumpHelper.cs`（WPF v1.9.8）：
//! - 系统公共对话框：窗口类 `#32770` + 文件特征子控件（地址栏/文件名输入/
//!   Shell 视图）；纯 Static + Button 的消息框不是对话框
//! - WPS 套件（wps/et/wpp 进程）自带打开/另存为：非 #32770，标题匹配；
//!   WPS 进程内的 #32770 也可能是原生消息框，须排除
//! - Internet Download Manager 主界面：#32770 + Explorer 风格子控件，
//!   易误判，必须排除
//! - Qt 壳 + 极短本地化标题：仅 WPS 进程内补充匹配

/// 对话框分类结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    /// 系统公共对话框（#32770 + 文件特征）。
    System,
    /// WPS 自带打开/保存框（永不注入，走六重降级）。
    Wps,
    /// 自定义规则命中的对话框（M5c 后续，CustomDialogTab）。
    Custom,
    /// 不是文件对话框（消息框/主界面等）。
    NotDialog,
}

/// WPS 套件进程基名（小写，无扩展名）。
pub const WPS_PROCESSES: &[&str] = &["wps", "et", "wpp", "wpspdf"];

/// WPS 自定义打开/保存窗口标题片段（须与套件进程同时匹配）。
pub const WPS_DIALOG_TITLE_HINTS: &[&str] =
    &["打开", "另存为", "保存", "open", "save as", "save"];

/// IDMan 主界面特征：#32770 + Explorer 风格子控件，易误判，排除。
pub fn is_idman_main(class_name: &str, exe_base_lower: &str, title: &str) -> bool {
    class_name == "#32770"
        && (exe_base_lower == "idman" || exe_base_lower == "idm")
        && title.contains("Internet Download Manager")
}

/// 远程桌面连接框（mstsc `#32770`）：有计算机/用户名 Edit，不是打开/另存为。
pub fn is_rdp_connect_ui(class_name: &str, exe_base_lower: &str, title: &str) -> bool {
    if class_name != "#32770" {
        return false;
    }
    if !matches!(exe_base_lower, "mstsc" | "msrdc" | "rdclient" | "mstscax") {
        return false;
    }
    if is_file_dialog_title(title) {
        return false;
    }
    let t = title.to_lowercase();
    title.contains("远程桌面") || t.contains("remote desktop") || title.is_empty()
}

/// Sublime 等编辑器的保存确认框：#32770 + 标题含 save，但无文件特征。
/// 调用方须先做子控件特征检查；本函数只做标题侧的保守排除提示。
pub fn looks_like_save_confirm(title_lower: &str, has_file_features: bool) -> bool {
    !has_file_features
        && (title_lower.contains("save") || title_lower.contains("保存"))
}

/// #32770 子控件特征判定：地址栏/文件名输入/Shell 视图任一存在即为文件框；
/// 纯 Static + Button 的消息框返回 false。
pub fn has_file_dialog_features(
    has_address_bar: bool,
    has_filename_input: bool,
    has_shell_view: bool,
) -> bool {
    has_address_bar || has_filename_input || has_shell_view
}

/// 对齐 WPF `ClassifyFileDialog` 子控件启发式：必须是
/// DirectUI+工具栏+Edit、SysListView+工具栏+Edit，或 Shell 视图。
/// 禁止「只要有 Edit」——远程桌面连接框、登录框都会误伤。
pub fn classes_look_like_file_dialog(classes: &[String]) -> bool {
    let has = |n: &str| classes.iter().any(|c| c.eq_ignore_ascii_case(n));
    let has_sub = |n: &str| classes.iter().any(|c| c.contains(n));
    let direct = has_sub("DirectUIHWND");
    let list = has("SysListView32");
    let tb = has("ToolbarWindow32");
    let edit = has("Edit");
    (direct && tb && edit)
        || (list && tb && edit)
        || has_sub("SHELLDLL_DefView")
        || has_sub("ShellDefView")
}

/// WPS 进程判定（基名小写比较，无 .exe）。
pub fn is_wps_process(exe_base_lower: &str) -> bool {
    WPS_PROCESSES.contains(&exe_base_lower)
}

/// WPS 自定义框标题判定（大小写不敏感，调用方先转小写）。
pub fn is_wps_dialog_title(title_lower: &str) -> bool {
    WPS_DIALOG_TITLE_HINTS
        .iter()
        .any(|h| title_lower.contains(&h.to_lowercase()))
}

/// 顶层分类（纯逻辑，可单测；Win32 取证由调用方传入）。
pub fn classify_dialog(
    class_name: &str,
    exe_base_lower: &str,
    title: &str,
    has_file_features: bool,
) -> DialogKind {
    // WPS 优先：类名判断之前识别（WPS 不用系统公共 #32770）。
    if is_wps_process(exe_base_lower) {
        // WPS 进程内的 #32770 可能是原生消息框（保存确认/错误提示）。
        if class_name == "#32770" && !has_file_features {
            return DialogKind::NotDialog;
        }
        let title_lower = title.to_lowercase();
        if class_name != "#32770" && is_wps_dialog_title(&title_lower) {
            return DialogKind::Wps;
        }
        if class_name == "#32770" && has_file_features {
            // WPS 进程内弹出的浏览对话框：仍按 #32770 单独尝试注入。
            return DialogKind::System;
        }
        return DialogKind::NotDialog;
    }
    // IDMan 主界面 / 远程桌面连接框排除。
    if is_idman_main(class_name, exe_base_lower, title) {
        return DialogKind::NotDialog;
    }
    if is_rdp_connect_ui(class_name, exe_base_lower, title) {
        return DialogKind::NotDialog;
    }
    if class_name == "#32770" {
        if has_file_features {
            return DialogKind::System;
        }
        return DialogKind::NotDialog;
    }
    DialogKind::NotDialog
}

/// 通用文件对话框标题判定（WPF `IsFileDialogTitle`）。
pub fn is_file_dialog_title(title: &str) -> bool {
    if title.is_empty() || is_known_non_dialog_title(title) {
        return false;
    }
    let t = title.to_lowercase();
    title.contains("打开")
        || title.contains("另存")
        || title.contains("保存")
        || t.contains("open file")
        || t.contains("open folder")
        || t == "open"
        || t.contains("save as")
        || t == "save"
        || t.contains("browse")
}

/// 已知非对话框标题（WPF `IsKnownNonFileDialogTitle`：Sublime 保存确认等）。
pub fn is_known_non_dialog_title(title: &str) -> bool {
    let t = title.trim().to_lowercase();
    if t.is_empty() {
        return false;
    }
    ["save changes", "unsaved changes", "do you want to save", "confirm save"]
        .iter()
        .any(|h| t.contains(h))
        || ["保存更改", "是否保存", "保存修改", "未保存"]
            .iter()
            .any(|h| title.contains(h))
}

/// WPS 自定义框标题全规则（WPF `IsWpsSuiteFileDialog` 标题侧）。
pub fn is_wps_suite_dialog_title(title: &str) -> bool {
    ["打开文件", "打开文档", "打开工作簿", "打开演示", "另存文件", "保存文件"]
        .iter()
        .any(|h| title.contains(h))
        || ["另存为", "保存", "打开"].iter().any(|h| title == *h)
        || title.starts_with("打开(")
        || title.starts_with("另存为(")
}

#[cfg(windows)]
pub mod win {
    //! Win32 取证：HWND → 类名/进程名/标题/子控件特征 → [`super::DialogKind`]。
    //!
    //! 对齐 WPF `ClassifyFileDialog` + `ResolveFileDialogHwndFromWindowOrAncestor`：
    //! - WPS 先于类名识别；IDMan 主界面（无 owner + 非文件标题）排除
    //! - 子控件整树收集类名：DirectUIHWND+Toolbar+Edit / SysListView32+Toolbar+Edit /
    //!   ShellDefView；标题像对话框也算（通用处理走地址栏）
    use anyhow::Result;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, GetAncestor, GetClassNameW, GetForegroundWindow, GetLastActivePopup,
        GetParent, GetWindow, GetWindowTextW, IsWindow, GA_ROOT, GW_OWNER,
    };

    use super::{is_file_dialog_title, is_idman_main, is_known_non_dialog_title, is_wps_process,
        is_wps_suite_dialog_title, DialogKind};

    pub fn class_of(hwnd: HWND) -> String {
        unsafe {
            let mut buf = [0u16; 256];
            let n = GetClassNameW(hwnd, &mut buf);
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }

    pub fn class_of_pub(hwnd_isize: isize) -> Option<String> {
        Some(class_of(HWND(hwnd_isize as *mut _)))
    }

    pub fn title_of_pub(hwnd_isize: isize) -> Option<String> {
        Some(text_of(HWND(hwnd_isize as *mut _)))
    }

    pub fn text_of(hwnd: HWND) -> String {
        unsafe {
            let mut buf = [0u16; 1024];
            let n = GetWindowTextW(hwnd, &mut buf);
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }

    /// 进程主模块基名小写（无扩展名），失败返回空串。
    pub fn exe_base_of(hwnd: HWND) -> String {
        unsafe {
            let mut pid = 0u32;
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                hwnd,
                Some(&mut pid),
            );
            if pid == 0 {
                return String::new();
            }
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return String::new();
            };
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                h,
                windows::Win32::System::Threading::PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            windows::Win32::Foundation::CloseHandle(h).ok();
            if ok.is_err() {
                return String::new();
            }
            let full = String::from_utf16_lossy(&buf[..len as usize]);
            let base = full.rsplit(['\\', '/']).next().unwrap_or("");
            let base = base.strip_suffix(".exe").or_else(|| base.strip_suffix(".EXE")).unwrap_or(base);
            base.to_lowercase()
        }
    }

    struct ClassAcc {
        classes: Vec<String>,
        count: usize,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lp: LPARAM) -> windows::Win32::Foundation::BOOL {
        let acc = &mut *(lp.0 as *mut ClassAcc);
        if acc.count >= 4000 {
            return windows::Win32::Foundation::BOOL(0);
        }
        acc.count += 1;
        let mut buf = [0u16; 128];
        let n = GetClassNameW(hwnd, &mut buf);
        acc.classes.push(String::from_utf16_lossy(&buf[..n as usize]));
        windows::Win32::Foundation::BOOL(1)
    }

    fn descendant_classes(hwnd: HWND) -> Vec<String> {
        unsafe {
            let mut acc = ClassAcc {
                classes: Vec::new(),
                count: 0,
            };
            let _ = EnumChildWindows(Some(hwnd), Some(enum_proc), LPARAM(&mut acc as *mut _ as isize));
            acc.classes
        }
    }

    pub fn classify_hwnd(hwnd_isize: isize) -> Result<DialogKind> {
        let hwnd = HWND(hwnd_isize as *mut _);
        unsafe {
            if hwnd.0.is_null() || !IsWindow(Some(hwnd)).as_bool() {
                anyhow::bail!("无效窗口");
            }
        }
        let title = text_of(hwnd);
        if is_known_non_dialog_title(&title) {
            return Ok(DialogKind::NotDialog);
        }
        // WPS 先于类名识别。
        let exe = exe_base_of(hwnd);
        if is_wps_process(&exe) {
            let class = class_of(hwnd);
            if class == "#32770" {
                // WPS 进程内 #32770：有文件特征=浏览对话框（走注入），否则消息框。
                let classes = descendant_classes(hwnd);
                let has = has_file_features(&classes);
                return Ok(if has { DialogKind::System } else { DialogKind::NotDialog });
            }
            if is_wps_suite_dialog_title(&title) {
                return Ok(DialogKind::Wps);
            }
            // Qt5 自绘：空标题 + 够大的可见启用窗口 + 非辅助类时 opacity 启发式。
            // Rust 侧保守处理：空标题非 #32770 一律不判，避免误伤主窗口。
            return Ok(DialogKind::NotDialog);
        }
        let class = class_of(hwnd);
        if class != "#32770" {
            return Ok(DialogKind::NotDialog);
        }
        // IDMan 主界面排除：无 owner + 标题不像文件框。
        unsafe {
            let has_owner = GetWindow(hwnd, GW_OWNER).is_ok();
            if exe == "idman" && !has_owner && !is_file_dialog_title(&title) {
                return Ok(DialogKind::NotDialog);
            }
        }
        if is_idman_main(&class, &exe, &title) {
            return Ok(DialogKind::NotDialog);
        }
        if super::is_rdp_connect_ui(&class, &exe, &title) {
            return Ok(DialogKind::NotDialog);
        }
        let classes = descendant_classes(hwnd);
        if super::classes_look_like_file_dialog(&classes) || is_file_dialog_title(&title) {
            return Ok(DialogKind::System);
        }
        if crate::custom::runtime_hit(&class, &exe, &title) {
            return Ok(DialogKind::Custom);
        }
        Ok(DialogKind::NotDialog)
    }

    fn has_file_features(classes: &[String]) -> bool {
        super::classes_look_like_file_dialog(classes)
    }

    /// 前台窗口向上溯（父链 64 级 + LastActivePopup，微信等模态框场景）找对话框。
    pub fn resolve_from_foreground() -> Option<isize> {
        unsafe {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                return None;
            }
            if std::process::id() == {
                let mut pid = 0u32;
                windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(fg, Some(&mut pid));
                pid
            } {
                return None;
            }
            let mut h = fg;
            for _ in 0..64 {
                if h.0.is_null() || !IsWindow(Some(h)).as_bool() {
                    break;
                }
                if let Ok(DialogKind::System | DialogKind::Wps | DialogKind::Custom) =
                    classify_hwnd(h.0 as isize)
                {
                    return Some(h.0 as isize);
                }
                h = GetParent(h).unwrap_or(HWND(std::ptr::null_mut()));
            }
            // 模态框常挂在 GetLastActivePopup 上（微信等）。
            let root = GetAncestor(fg, GA_ROOT);
            for owner in [fg, root] {
                if owner.0.is_null() {
                    continue;
                }
                let pop = GetLastActivePopup(owner);
                if pop.0.is_null() || pop == owner {
                    continue;
                }
                if let Ok(DialogKind::System | DialogKind::Wps | DialogKind::Custom) =
                    classify_hwnd(pop.0 as isize)
                {
                    return Some(pop.0 as isize);
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_dialog_needs_features() {
        assert_eq!(
            classify_dialog("#32770", "notepad", "另存为", true),
            DialogKind::System
        );
        assert_eq!(
            classify_dialog("#32770", "notepad", "提示", false),
            DialogKind::NotDialog
        );
    }

    #[test]
    fn wps_custom_dialog() {
        assert_eq!(
            classify_dialog("Qt5QWindowIcon", "wps", "打开", false),
            DialogKind::Wps
        );
        // WPS 进程内 #32770 消息框排除。
        assert_eq!(
            classify_dialog("#32770", "et", "提示", false),
            DialogKind::NotDialog
        );
        // WPS 进程内弹出的浏览对话框仍走 System（注入）。
        assert_eq!(
            classify_dialog("#32770", "wps", "浏览", true),
            DialogKind::System
        );
    }

    #[test]
    fn idman_excluded() {
        assert_eq!(
            classify_dialog(
                "#32770",
                "idman",
                "Internet Download Manager 6.42",
                true
            ),
            DialogKind::NotDialog
        );
    }

    #[test]
    fn mstsc_connect_not_file_dialog() {
        assert_eq!(
            classify_dialog("#32770", "mstsc", "远程桌面连接", true),
            DialogKind::NotDialog
        );
        assert_eq!(
            classify_dialog("#32770", "mstsc", "Remote Desktop Connection", false),
            DialogKind::NotDialog
        );
        assert!(is_rdp_connect_ui(
            "#32770",
            "mstsc",
            "远程桌面连接"
        ));
        assert!(!is_rdp_connect_ui("#32770", "mstsc", "另存为"));
        assert!(!classes_look_like_file_dialog(&[
            "Edit".into(),
            "Button".into(),
            "Static".into(),
            "ComboBox".into(),
        ]));
        assert!(classes_look_like_file_dialog(&[
            "DirectUIHWND".into(),
            "ToolbarWindow32".into(),
            "Edit".into(),
        ]));
    }

    #[test]
    fn non_32770_not_dialog() {
        assert_eq!(
            classify_dialog("CabinetWClass", "explorer", "下载", true),
            DialogKind::NotDialog
        );
    }

    #[test]
    fn title_heuristics_match_wpf() {
        assert!(is_file_dialog_title("另存为"));
        assert!(is_file_dialog_title("Open File..."));
        assert!(!is_file_dialog_title("Internet Download Manager 6.42"));
        assert!(is_known_non_dialog_title("保存更改"));
        assert!(is_known_non_dialog_title("Save Changes?"));
        assert!(!is_file_dialog_title("保存更改"));
        assert!(is_wps_suite_dialog_title("打开文件"));
        assert!(is_wps_suite_dialog_title("另存为"));
        assert!(!is_wps_suite_dialog_title("WPS 文字"));
    }
}
