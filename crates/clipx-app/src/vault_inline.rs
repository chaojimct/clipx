//! Vault 行内钥匙按钮的 UIA 探针（M2，Windows）。
//!
//! 策略（密码优先）：焦点落在密码框（`IsPassword`）→ 显示；焦点落在普通
//! Edit 且同窗存在密码框（用户名框）→ 显示；其余一律隐藏。Electron 树不全、
//! 提权窗口（UIPI）等拿不到时静默隐藏，绝不瞎弹（降级走 Ctrl+Alt+L 热键）。
//!
//! 开销：500ms 轮询 + 焦点未变时跳过重查（`CompareElements`），单次有效
//! 探测约毫秒级；主弹窗可见/自家前台/密码管理器前台时直接隐藏不碰 COM。
//! 隐私：只读控件类型与几何矩形，不读任何文本值、不记日志（窗口标题、
//! 键入内容永不进探针；密码值 UIA 本来也不给读）。

/// 钥匙按钮期望态（逻辑线程消费；`show=false` 时 x/y 无意义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaultKeyState {
    pub show: bool,
    pub x: i32,
    pub y: i32,
}

/// 后台轮询线程（500ms）：决策变化才回投 `AppEvt::VaultKey`。
/// 非 Windows 下直接返回（调用方无条件 spawn，平台差异不出调用点）。
pub fn spawn_poller(tx: std::sync::mpsc::Sender<crate::logic::AppEvt>) {
    #[cfg(not(windows))]
    {
        let _ = tx;
        return;
    }
    #[cfg(windows)]
    {
        let _ = std::thread::Builder::new()
            .name("clipx-vaultkey-watch".into())
            .spawn(move || poll_loop(tx));
    }
}

#[cfg(windows)]
fn poll_loop(tx: std::sync::mpsc::Sender<crate::logic::AppEvt>) {
    use std::time::Duration;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};

    let co = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let auto: Option<IUIAutomation> = unsafe {
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()
    };
    let mut last_sent: Option<VaultKeyState> = None;
    let mut mem = ProbeMem::default();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let next = probe_once(&auto, &mut mem);
        let changed = match (last_sent, next) {
            (None, Some(_)) => true,
            (Some(_), None) => true,
            (Some(a), Some(b)) => {
                a.show != b.show
                    || (b.show && (a.x - b.x).abs() + (a.y - b.y).abs() > 8)
            }
            (None, None) => false,
        };
        if changed {
            last_sent = next;
            let st = next.unwrap_or(VaultKeyState { show: false, x: 0, y: 0 });
            if tx
                .send(crate::logic::AppEvt::VaultKey {
                    show: st.show,
                    x: st.x,
                    y: st.y,
                })
                .is_err()
            {
                break;
            }
        }
    }
    if co.is_ok() {
        unsafe { CoUninitialize() };
    }
}

/// 单次探测：返回 Some(show) / None=隐藏。`mem` 记住上次元素+决策，
///
/// 焦点未变时只看矩形漂移（滚动跟随），不重跑附近密码框搜索；
/// 上次判隐藏的元素，下次直接延续隐藏（不误重放显示）。
#[cfg(windows)]
#[derive(Default)]
struct ProbeMem {
    el: Option<windows::Win32::UI::Accessibility::IUIAutomationElement>,
    show: bool,
    rect: Option<(i32, i32, i32, i32)>,
}

#[cfg(windows)]
fn probe_once(
    auto: &Option<windows::Win32::UI::Accessibility::IUIAutomation>,
    mem: &mut ProbeMem,
) -> Option<VaultKeyState> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    // 廉价门（不碰 COM）：主窗可见 / 无前台 / 自家 / 密码管理器前台 → 藏。
    if crate::keyboard_hook::is_visible() {
        *mem = ProbeMem::default();
        return None;
    }
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() {
        *mem = ProbeMem::default();
        return None;
    }
    let fg_isize = fg.0 as isize;
    if crate::win_popup::window_is_own_process(fg_isize) {
        *mem = ProbeMem::default();
        return None;
    }
    if crate::policy::is_password_manager_foreground() {
        *mem = ProbeMem::default();
        return None;
    }
    let auto = auto.as_ref()?;
    unsafe {
        let focused: windows::Win32::UI::Accessibility::IUIAutomationElement =
            auto.GetFocusedElement().ok()?;
        // 焦点未变：延续上次决策；显示中且矩形漂移>8px 才重算锚点（滚动跟随）。
        // 没漂移也返回同值显示态，调用方按"变化"去重吞掉，不重复投递。
        if let Some(prev) = mem.el.as_ref() {
            if auto.CompareElements(prev, &focused).unwrap_or_default().as_bool() {
                if !mem.show {
                    return None;
                }
                let rect = element_rect(&focused)?;
                mem.rect = Some(rect);
                return anchor_of(rect).map(|(x, y)| VaultKeyState { show: true, x, y });
            }
        }
        mem.el = Some(focused.clone());
        // 全量评估：文本类控件 + 非屏外 + 尺寸 sane。
        if !is_text_control(&focused) {
            mem.show = false;
            mem.rect = None;
            return None;
        }
        let rect = element_rect(&focused)?;
        mem.rect = Some(rect);
        // 变形单节点（quirk + 登录窗标题双重门，见 `morph_gate`）：焦点文本框
        // 即显示（两行都与填充相关；行判定在填充时按当前值做，不在这里猜）。
        if morph_gate() {
            mem.show = true;
            return anchor_of(rect).map(|(x, y)| VaultKeyState { show: true, x, y });
        }
        let is_pw = is_password_field(&focused);
        if !is_pw && find_password_element(auto, HWND(fg.0)).is_none() {
            mem.show = false;
            return None;
        }
        mem.show = true;
        anchor_of(rect).map(|(x, y)| VaultKeyState { show: true, x, y })
    }
}

/// 元素矩形（物理像素）+ sane 检查：空/屏外/过大（文档窗格）一律 None。
#[cfg(windows)]
unsafe fn element_rect(
    el: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::UI::Accessibility::UIA_IsOffscreenPropertyId;
    if let Ok(v) = el.GetCurrentPropertyValue(UIA_IsOffscreenPropertyId) {
        if i32::try_from(&v).unwrap_or(0) != 0 {
            return None;
        }
    }
    let r = el.CurrentBoundingRectangle().ok()?;
    let (l, t, w, h) = (r.left, r.top, r.right - r.left, r.bottom - r.top);
    if w < 24 || h < 10 || h > 400 {
        return None;
    }
    Some((l, t, w, h))
}

/// 是否文本输入类（Edit / Document；ComboBox 下拉不算，避免误挂）。
#[cfg(windows)]
unsafe fn is_text_control(
    el: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> bool {
    use windows::Win32::UI::Accessibility::{
        UIA_ControlTypePropertyId, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
    };
    let ct = el
        .GetCurrentPropertyValue(UIA_ControlTypePropertyId)
        .ok()
        .and_then(|v| i32::try_from(&v).ok())
        .unwrap_or(0);
    ct == UIA_EditControlTypeId.0 as i32 || ct == UIA_DocumentControlTypeId.0 as i32
}

/// 是否密码框（`IsPassword` 标准语义；读失败按非密码处理）。
#[cfg(windows)]
unsafe fn is_password(el: &windows::Win32::UI::Accessibility::IUIAutomationElement) -> bool {
    use windows::Win32::UI::Accessibility::UIA_IsPasswordPropertyId;
    el.GetCurrentPropertyValue(UIA_IsPasswordPropertyId)
        .ok()
        .and_then(|v| i32::try_from(&v).ok())
        .unwrap_or(0)
        != 0
}

/// 密码框名关键词（`IsPassword` 不报的自绘框兜底：QQ 登录这类）。
/// 全小写子串比对；`pass` 兜住 passcode/passwort 等变体，误伤面是输入框名，
/// 窄到可接受（只判登录上下文，不全局生效）。
#[cfg(windows)]
const PW_NAME_KEYWORDS: [&str; 5] = ["密码", "password", "passwd", "pwd", "pass"];

/// 用户名框名关键词。
#[cfg(windows)]
const ACCT_NAME_KEYWORDS: [&str; 12] = [
    "账号", "用户名", "用户", "account", "username", "login", "mail", "email", "邮箱",
    "手机", "phone", "qid",
];

/// 元素可访问名（小写；失败即空串）。
/// 只取 Name（占位/标签），绝不读 Value（内容）——探针与判定同守这条线。
#[cfg(windows)]
unsafe fn element_name(
    el: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> String {
    el.CurrentName()
        .map(|h| h.to_string().to_lowercase())
        .unwrap_or_default()
}

#[cfg(windows)]
fn name_has(lower_name: &str, kws: &[&str]) -> bool {
    !lower_name.is_empty() && kws.iter().any(|k| lower_name.contains(k))
}

/// 是否密码框：标准语义优先，名关键词兜底（自绘/字体掩码框）。
#[cfg(windows)]
unsafe fn is_password_field(
    el: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> bool {
    is_password(el) || name_has(&element_name(el), &PW_NAME_KEYWORDS)
}

/// 同窗找输入框：名关键词命中 + 可见矩形（FindAll 上限 80，名先行、矩形只验命中者）。
/// 返回第一个命中的元素（调用方区分用户/密码两套关键词）。
#[cfg(windows)]
unsafe fn find_field_by_name(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
    fg: windows::Win32::Foundation::HWND,
    kws: &[&str],
) -> Option<windows::Win32::UI::Accessibility::IUIAutomationElement> {
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_EditControlTypeId,
    };
    let root = auto.ElementFromHandle(fg).ok()?;
    let cond = auto
        .CreatePropertyCondition(
            UIA_ControlTypePropertyId,
            &VARIANT::from(UIA_EditControlTypeId.0 as i32),
        )
        .ok()?;
    let found = root.FindAll(TreeScope_Descendants, &cond).ok()?;
    let n = found.Length().unwrap_or(0).min(80);
    let mut i = 0;
    while i < n {
        if let Ok(el) = found.GetElement(i) {
            if name_has(&element_name(&el), kws) && element_rect(&el).is_some() {
                return Some(el);
            }
        }
        i += 1;
    }
    None
}

/// 同窗密码框：可见 `IsPassword` 优先，名关键词兜底；都没有返回 None。
/// 注意：自绘登录框（如 QQ）的密码框可能两样都没有，此时返回 None——
/// 调用方用“账号框坐标系”兜底（见 `is_other_login_field`）。
#[cfg(windows)]
unsafe fn find_password_element(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
    fg: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::UI::Accessibility::IUIAutomationElement> {
    if let Some(el) = visible_password_element(auto, fg) {
        return Some(el);
    }
    find_field_by_name(auto, fg, &PW_NAME_KEYWORDS)
}

/// 同窗第一个**可见**密码框（SetValue 目标用）：隐藏/屏外/退化矩形节点
/// SetValue 报 S_OK 但无效果，必须过滤；都没有返回 None。
#[cfg(windows)]
unsafe fn visible_password_element(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
    fg: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::UI::Accessibility::IUIAutomationElement> {
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{TreeScope_Descendants, UIA_IsPasswordPropertyId};
    let root = auto.ElementFromHandle(fg).ok()?;
    let cond =
        auto.CreatePropertyCondition(UIA_IsPasswordPropertyId, &VARIANT::from(true)).ok()?;
    let found = root.FindAll(TreeScope_Descendants, &cond).ok()?;
    let n = found.Length().unwrap_or(0);
    let mut i = 0;
    while i < n {
        if let Ok(el) = found.GetElement(i) {
            // element_rect 自带屏外/尺寸 sane 过滤；密码框不需要文本类判定。
            if element_rect(&el).is_some() {
                return Some(el);
            }
        }
        i += 1;
    }
    None
}

/// 两元素是否同一节点（账号坐标系的身份判定：焦点框 vs 树里找到的账号框）。
#[cfg(windows)]
unsafe fn same_element(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
    a: &windows::Win32::UI::Accessibility::IUIAutomationElement,
    b: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> bool {
    auto.CompareElements(a, b).unwrap_or_default().as_bool()
}

/// 焦点是否为“账号框之外的另一个登录框”（QQ 登录密码框形态）：
/// 树里能找到账号框，焦点是文本类、不是账号框自己、且树里没有可识别的密码框。
/// 满足即把焦点框当密码框用（它的 IsPassword/Name 全是错的，只能靠排除法）。
#[cfg(windows)]
unsafe fn is_other_login_field(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
    fg: windows::Win32::Foundation::HWND,
    focused: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> bool {
    if !is_text_control(focused) || is_password_field(focused) {
        return false;
    }
    let Some(acct) = find_field_by_name(auto, fg, &ACCT_NAME_KEYWORDS) else {
        return false;
    };
    if same_element(auto, focused, &acct) {
        return false;
    }
    find_password_element(auto, fg).is_none()
}

/// 诊断快照（`--vault-probe`）：前台上下文 + 焦点元素 + 附近密码框。
/// 只读元数据（类型/标记/几何），不读任何文本值、不碰剪贴板、不改状态。
/// 输出留在用户终端，不写日志文件。
#[cfg(windows)]
pub fn probe_report() -> String {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let mut out = Vec::new();
    let fg = unsafe { GetForegroundWindow() };
    out.push(format!("fg_exe={}", crate::win_popup::foreground_exe_base()));
    out.push(format!("fg_title={}", truncate40(&crate::win_popup::foreground_title())));
    out.push(format!("fg_own={}", crate::win_popup::window_is_own_process(fg.0 as isize)));
    out.push(format!(
        "fg_pw_manager={}",
        crate::policy::is_password_manager_foreground()
    ));
    out.push(format!("popup_visible={}", crate::keyboard_hook::is_visible()));
    unsafe {
        let co = CoInitializeEx(None, COINIT_MULTITHREADED);
        let auto: IUIAutomation =
            match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(v) => v,
                Err(e) => {
                    if co.is_ok() {
                        CoUninitialize();
                    }
                    out.push(format!("uia=ERR {e}"));
                    return out.join("\n");
                }
            };
        match auto.GetFocusedElement() {
            Ok(el) => {
                out.push(format!("focus_text={}", is_text_control(&el)));
                out.push(format!("focus_password={}", is_password_field(&el)));
                out.push(format!("focus_name={}", truncate24(&element_name(&el))));
                match el.CurrentBoundingRectangle() {
                    Ok(r) => out.push(format!(
                        "focus_rect={},{},{}x{}",
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top
                    )),
                    Err(e) => out.push(format!("focus_rect=ERR {e}")),
                }
            }
            Err(e) => out.push(format!("focus=ERR {e}")),
        }
        if !fg.0.is_null() {
            out.push(format!(
                "window_has_password={}",
                find_password_element(&auto, HWND(fg.0)).is_some()
            ));
            // 账号坐标系（QQ 黑户形态定位用）：树里账号框+焦点身份。
            let acct = find_field_by_name(&auto, HWND(fg.0), &ACCT_NAME_KEYWORDS);
            out.push(format!("tree_acct_found={}", acct.is_some()));
            if let Some(a) = acct.as_ref() {
                match a.CurrentBoundingRectangle() {
                    Ok(r) => out.push(format!(
                        "tree_acct_rect={},{},{}x{}",
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top
                    )),
                    Err(e) => out.push(format!("tree_acct_rect=ERR {e}")),
                }
            }
            match (auto.GetFocusedElement(), acct.as_ref()) {
                (Ok(focused), Some(a)) => out.push(format!(
                    "focus_is_acct={}",
                    same_element(&auto, &focused, a)
                )),
                _ => out.push("focus_is_acct=?".to_string()),
            }
        }
        if co.is_ok() {
            CoUninitialize();
        }
    }
    out.join("\n")
}

/// 探针输出截断（定位够用，不搬运长文本）。
#[cfg(windows)]
fn truncate24(s: &str) -> String {
    let t: String = s.chars().take(24).collect();
    if s.chars().count() > 24 {
        format!("{t}…")
    } else {
        t
    }
}

/// 探针输出截断（窗口标题只看前 40 字：定位够用，不搬运长文本）。
#[cfg(windows)]
fn truncate40(s: &str) -> String {
    let t: String = s.chars().take(40).collect();
    if s.chars().count() > 40 {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(not(windows))]
pub fn probe_report() -> String {
    "vault-probe: 当前平台不支持".to_string()
}

/// 变形单节点登录框填充（QQ `morph_login` quirk；判据见 quirks.rs）。
/// 身份识别无解，改用“读当前值 + 矩形跟踪”协议。双重门（失窃级事故防线）：
/// ① 前台标题必须为登录窗（`QQ`；聊天窗标题是联系人名）；
/// ② 树里必须有账号框（登录表单才有；聊天窗没有）。
/// 行判定只看当前值（名全是错的，不看）：
/// - 值 == 用户名 → 账号行：原地重写（幂等）→ Tab 一次 → 矩形变了且仍是
///   文本框即收养为密码框 → 填密码。
/// - 否则（空/圆点/他值）→ 密码行：就地填密码；用户名找后台账号节点，
///   找到了顺手填，找不到跳过（QQ 一般已记住账号）。
/// 返回 (user_ok, pw_ok)；读到的值只做比较即 drop，不记日志。
#[cfg(windows)]
pub fn morph_fill(username: &str, password: &zeroize::Zeroizing<String>) -> (bool, bool) {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    unsafe {
        let co = CoInitializeEx(None, COINIT_MULTITHREADED);
        let auto: IUIAutomation =
            match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(v) => v,
                Err(_) => {
                    if co.is_ok() {
                        CoUninitialize();
                    }
                    return (false, false);
                }
            };
        let r = morph_fill_with(&auto, username, password);
        if co.is_ok() {
            CoUninitialize();
        }
        r
    }
}

#[cfg(not(windows))]
pub fn morph_fill(_username: &str, _password: &zeroize::Zeroizing<String>) -> (bool, bool) {
    (false, false)
}

/// morph 双重门：quirk 表 + 登录窗标题（聊天窗标题是联系人名，进去了就是
/// 把密码填进聊天框，必须拦）。
#[cfg(windows)]
fn morph_gate() -> bool {
    if !clipx_host::quirks::AppQuirks::for_exe(&crate::win_popup::foreground_exe_base())
        .morph_login
    {
        return false;
    }
    crate::win_popup::foreground_title().trim().eq_ignore_ascii_case("qq")
}

#[cfg(windows)]
unsafe fn morph_fill_with(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
    username: &str,
    password: &zeroize::Zeroizing<String>,
) -> (bool, bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    if !morph_gate() {
        return (false, false);
    }
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return (false, false);
    }
    let Ok(focused) = auto.GetFocusedElement() else {
        return (false, false);
    };
    if !is_text_control(&focused) {
        return (false, false);
    }
    // 门②：树里必须有账号框（登录表单）。
    if find_field_by_name(auto, HWND(fg.0), &ACCT_NAME_KEYWORDS).is_none() {
        return (false, false);
    }
    let Ok(pat) = focused.GetCurrentPatternAs::<
        windows::Win32::UI::Accessibility::IUIAutomationValuePattern,
    >(windows::Win32::UI::Accessibility::UIA_ValuePatternId) else {
        // 不可读 → 协议无从谈起，回落（调用方走剪贴板序列）。
        return (false, false);
    };
    let cur = pat.CurrentValue().map(|b| b.to_string()).unwrap_or_default();
    let want_user = !username.trim().is_empty();
    if want_user && cur.trim() == username.trim() {
        // 账号行：原地重写用户名（幂等，顺手规范空格）→ Tab 一次收养密码框。
        if !set_verified(&focused, username.trim()) {
            return (false, false);
        }
        let old_rect = element_rect(&focused);
        crate::paste::send_vk_tap(0x09);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let Ok(el) = auto.GetFocusedElement() else {
            return (true, false);
        };
        if !is_text_control(&el) {
            return (true, false);
        }
        let moved = match (old_rect, element_rect(&el)) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        };
        if !moved {
            return (true, false);
        }
        return (true, set_password_verified(&el, password.as_str()));
    }
    // 密码行（空/圆点/他值）：就地填密码；用户名找后台账号节点顺手填。
    if !set_password_verified(&focused, password.as_str()) {
        return (false, false);
    }
    let mut user_ok = false;
    if want_user {
        if let Some(acct) = find_field_by_name(auto, HWND(fg.0), &ACCT_NAME_KEYWORDS) {
            // 同一变形节点（与焦点是同一个）就跳过——往密码框里写用户名是事故。
            if !same_element(auto, &focused, &acct) {
                user_ok = set_verified(&acct, username.trim());
            }
        }
    }
    (user_ok, true)
}

/// 锚点：输入框右缘 +6px，纵向居中（按钮 30px 逻辑高）。
#[cfg(windows)]
fn anchor_of(rect: (i32, i32, i32, i32)) -> Option<(i32, i32)> {
    let (l, t, w, h) = rect;
    Some((l + w + 6, t + (h - 30).max(0) / 2))
}

/// 当前焦点是否在密码框（单次 COM 查询；失败一律 false）。
/// 给“Tab 直到密码框”循环用（QQ 登录这类 Tab 停靠点多：下拉箭头/链接/复选框）。
#[cfg(windows)]
pub fn focus_is_password_now() -> bool {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    unsafe {
        let co = CoInitializeEx(None, COINIT_MULTITHREADED);
        let auto: IUIAutomation =
            match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(v) => v,
                Err(_) => {
                    if co.is_ok() {
                        CoUninitialize();
                    }
                    return false;
                }
            };
        let r = auto
            .GetFocusedElement()
            .map(|el| is_password(&el))
            .unwrap_or(false);
        if co.is_ok() {
            CoUninitialize();
        }
        r
    }
}

#[cfg(not(windows))]
pub fn focus_is_password_now() -> bool {
    false
}

/// SetValue 直填（M3）：用户名→焦点框，Tab，密码→密码框，全程不碰剪贴板。
///
/// 任一步失败即 false，调用方回落剪贴板序列（此时剪贴板还没动过，回落干净）。
/// 用户名为空时：焦点是密码框就地填密码，否则 false（布局不符，回落序列）。
/// 密码写完不读回校验（密码值 UIA 不给读，系统安全设计），调用方只 toast。
/// 非 Windows 恒 false。
pub fn try_setvalue_fill(username: &str, password: &zeroize::Zeroizing<String>) -> bool {
    #[cfg(not(windows))]
    {
        let _ = (username, password);
        return false;
    }
    #[cfg(windows)]
    {
        setvalue_fill_inner(username, password)
    }
}

#[cfg(windows)]
fn setvalue_fill_inner(username: &str, password: &zeroize::Zeroizing<String>) -> bool {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    unsafe {
        let co = CoInitializeEx(None, COINIT_MULTITHREADED);
        let auto: IUIAutomation =
            match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(v) => v,
                Err(_) => {
                    if co.is_ok() {
                        CoUninitialize();
                    }
                    return false;
                }
            };
        let r = setvalue_fill_with(&auto, username, password);
        if co.is_ok() {
            CoUninitialize();
        }
        r
    }
}

#[cfg(windows)]
unsafe fn setvalue_fill_with(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
    username: &str,
    password: &zeroize::Zeroizing<String>,
) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    let Ok(focused) = auto.GetFocusedElement() else {
        return false;
    };
    let want_user = !username.trim().is_empty();
    // 焦点框判定：标准语义/名关键词是密码框，或它是账号框之外的另一个框
    //（QQ 黑户形态：IsPassword/Name 全错，只能靠账号坐标系排除法）。
    let focused_pw = is_password_field(&focused)
        || is_other_login_field(auto, HWND(fg.0), &focused);
    // 用户名先填（焦点不动）：焦点合适就地用，否则按名找账号框；
    // 有用户名却两处都拿不到→ false（回落剪贴板序列，不乱填）。
    if want_user {
        let user_el = if !focused_pw && is_text_control(&focused) {
            focused.clone()
        } else {
            let Some(el) = find_field_by_name(auto, HWND(fg.0), &ACCT_NAME_KEYWORDS) else {
                return false;
            };
            el
        };
        if !set_verified(&user_el, username.trim()) {
            return false;
        }
    }
    // 密码目标三级：焦点框 → 树（`IsPassword`/名关键词）→ Tab 一次收养。
    // 收养：用户名刚落进焦点账号框，Tab 一次后焦点变了且是文本框即收养
    //（懒树/黑户形态；复选框/链接/按钮非文本类，天然过滤）。
    let pw_el = if focused_pw {
        focused.clone()
    } else if let Some(el) = find_password_element(auto, HWND(fg.0)) {
        el
    } else if want_user {
        crate::paste::send_vk_tap(0x09);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let Ok(el) = auto.GetFocusedElement() else {
            return false;
        };
        if !is_text_control(&el) || same_element(auto, &el, &focused) {
            return false;
        }
        el
    } else {
        return false;
    };
    // 密码：SetValue；可读回来的框（字体掩码类）必须对上，对不上即 false；
    // 真密码框不可读，写成功即信任。
    set_password_verified(&pw_el, password.as_str())
}

/// SetValue + 回读验证（用户名非秘密，可读）：框架受控组件（React/Electron）
///
/// 对程序化 SetValue 常报 S_OK 但不吃值（不触发 input 事件）。读回来对不上
/// 就返回 false，调用方回落剪贴板序列。
#[cfg(windows)]
unsafe fn set_verified(
    el: &windows::Win32::UI::Accessibility::IUIAutomationElement,
    text: &str,
) -> bool {
    use windows::core::BSTR;
    let Ok(pat) = el.GetCurrentPatternAs::<
        windows::Win32::UI::Accessibility::IUIAutomationValuePattern,
    >(windows::Win32::UI::Accessibility::UIA_ValuePatternId) else {
        return false;
    };
    if pat.SetValue(&BSTR::from(text)).is_err() {
        return false;
    }
    std::thread::sleep(std::time::Duration::from_millis(120));
    let back = pat.CurrentValue().map(|b| b.to_string()).unwrap_or_default();
    back.trim() == text.trim()
}

/// 密码 SetValue：可读回来的框必须对上（字体掩码类）；真密码框不可读即信任。
/// 读回来的短暂持有只做比较，函数返回即 drop（调用方秘密本就内存持有）。
#[cfg(windows)]
unsafe fn set_password_verified(
    el: &windows::Win32::UI::Accessibility::IUIAutomationElement,
    password: &str,
) -> bool {
    use windows::core::BSTR;
    let Ok(pat) = el.GetCurrentPatternAs::<
        windows::Win32::UI::Accessibility::IUIAutomationValuePattern,
    >(windows::Win32::UI::Accessibility::UIA_ValuePatternId) else {
        return false;
    };
    if pat.SetValue(&BSTR::from(password)).is_err() {
        return false;
    }
    match pat.CurrentValue() {
        Ok(v) => v.to_string() == password,
        // 不可读 = 真密码框语义，写成功即信任（读回验证无从谈起）。
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_is_right_edge_plus_gap() {
        // 纯几何：与平台无关，三平台可跑。
        #[cfg(windows)]
        {
            assert_eq!(anchor_of((100, 200, 300, 24)), Some((406, 200)));
            assert_eq!(anchor_of((0, 0, 100, 60)), Some((106, 15)));
        }
        #[cfg(not(windows))]
        {
            let st = VaultKeyState { show: true, x: 1, y: 2 };
            assert_eq!(st, VaultKeyState { show: true, x: 1, y: 2 });
        }
    }
}
