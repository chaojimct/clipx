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
        let is_pw = is_password(&focused);
        if !is_pw && !window_has_password(auto, HWND(fg.0)) {
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

/// 同窗是否存在密码框（用户名框判定用；找到第一个即停）。
#[cfg(windows)]
unsafe fn window_has_password(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
    fg: windows::Win32::Foundation::HWND,
) -> bool {
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{TreeScope_Descendants, UIA_IsPasswordPropertyId};
    let Ok(root) = auto.ElementFromHandle(fg) else {
        return false;
    };
    let Ok(cond) =
        auto.CreatePropertyCondition(UIA_IsPasswordPropertyId, &VARIANT::from(true))
    else {
        return false;
    };
    root.FindFirst(TreeScope_Descendants, &cond).is_ok()
}

/// 锚点：输入框右缘 +6px，纵向居中（按钮 30px 逻辑高）。
#[cfg(windows)]
fn anchor_of(rect: (i32, i32, i32, i32)) -> Option<(i32, i32)> {
    let (l, t, w, h) = rect;
    Some((l + w + 6, t + (h - 30).max(0) / 2))
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
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        TreeScope_Descendants, UIA_IsPasswordPropertyId,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    use windows::core::BSTR;

    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    let Ok(focused) = auto.GetFocusedElement() else {
        return false;
    };
    let want_user = !username.trim().is_empty();
    if want_user {
        // 焦点必须在用户名框（文本类、非密码）；在密码框/别处说明布局不符。
        if !is_text_control(&focused) || is_password(&focused) {
            return false;
        }
        let Ok(pat) = focused.GetCurrentPatternAs::<
            windows::Win32::UI::Accessibility::IUIAutomationValuePattern,
        >(windows::Win32::UI::Accessibility::UIA_ValuePatternId) else {
            return false;
        };
        if pat.SetValue(&BSTR::from(username.trim())).is_err() {
            return false;
        }
        // Tab 切到密码框（焦点切换 + 框架校验是异步的，多等一拍）。
        crate::paste::send_vk_tap(0x09);
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    // 密码目标：无用户名且焦点已在密码框就地填，否则找同窗密码框。
    let target = if !want_user && is_password(&focused) {
        focused
    } else {
        let Ok(root) = auto.ElementFromHandle(HWND(fg.0)) else {
            return false;
        };
        let Ok(cond) =
            auto.CreatePropertyCondition(UIA_IsPasswordPropertyId, &VARIANT::from(true))
        else {
            return false;
        };
        let Ok(el) = root.FindFirst(TreeScope_Descendants, &cond) else {
            return false;
        };
        el
    };
    let Ok(pat) = target.GetCurrentPatternAs::<
        windows::Win32::UI::Accessibility::IUIAutomationValuePattern,
    >(windows::Win32::UI::Accessibility::UIA_ValuePatternId) else {
        return false;
    };
    pat.SetValue(&BSTR::from(password.as_str())).is_ok()
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
