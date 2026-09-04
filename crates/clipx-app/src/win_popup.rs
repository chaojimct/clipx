use slint::Window;

#[cfg(windows)]
use windows::Win32::Foundation::HWND;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_TOPMOST,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_SHOWNOACTIVATE,
    WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

/// 必须在 `Window::show` 之后调用：winit 的 show 会重置 EXSTYLE，
/// 不跟 SetWindowPos(SWP_FRAMECHANGED) 的话 TOOLWINDOW/TOPMOST 不会生效，
/// 任务栏就会留下一个「Slint Window」，窗口本身却看不见。
#[cfg(windows)]
pub fn apply_style(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let add = (WS_EX_NOACTIVATE.0
            | WS_EX_TOOLWINDOW.0
            | WS_EX_TOPMOST.0
            | WS_EX_LAYERED.0) as isize;
        let remove = WS_EX_APPWINDOW.0 as isize;
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (current | add) & !remove);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

#[allow(dead_code)]
/// 0.4~1.0；依赖 `WS_EX_LAYERED`（`apply_style` 已加上）。
/// 弹窗透明度改走 Slint `panel-opacity`，避免 LWA_ALPHA 毁掉圆角外的 per-pixel 透明。
#[cfg(windows)]
pub fn apply_opacity(window: &Window, opacity: f64) {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::UI::WindowsAndMessaging::{SetLayeredWindowAttributes, LWA_ALPHA};
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    let alpha = (opacity.clamp(0.4, 1.0) * 255.0).round() as u8;
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
    }
}

#[cfg(not(windows))]
pub fn apply_opacity(_window: &Window, _opacity: f64) {}

/// 按当前显示器 DPI 把逻辑尺寸写成物理像素。
/// 快查窗口启动时在主屏创建、弹出时挪到资源管理器屏幕：若仍按旧 scale 设 Logical，
/// 内容会按新 DPI 绘制而 HWND 还是旧大小，表现为放大后上方/右方被裁。
pub fn sync_logical_size(window: &Window, logical_w: f32, logical_h: f32) {
    let scale = window.scale_factor().max(1.0);
    window.set_size(slint::WindowSize::Physical(slint::PhysicalSize::new(
        (logical_w * scale).round() as u32,
        (logical_h * scale).round() as u32,
    )));
}

/// 弹窗定位：光标所在显示器工作区，右下偏移 12px，越界回缩（WPF 版 M1 简化：鼠标锚点）。
#[cfg(windows)]
pub fn position_near_cursor(window: &Window, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return;
        }
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
            return;
        }
        let scale = window.scale_factor();
        let w = (logical_w * scale) as i32;
        let h = (logical_h * scale) as i32;
        let work = mi.rcWork;
        let mut x = pt.x + 12;
        let mut y = pt.y + 12;
        if x + w > work.right {
            x = work.right - w;
        }
        if y + h > work.bottom {
            y = work.bottom - h;
        }
        if x < work.left {
            x = work.left;
        }
        if y < work.top {
            y = work.top;
        }
        window.set_position(slint::WindowPosition::Physical(
            slint::PhysicalPosition::new(x, y),
        ));
    }
}

/// 在逻辑线程解析锚点（物理像素，已含偏移）。UIA 在独立线程，超时不挡 UI。
#[cfg(windows)]
pub fn resolve_popup_anchor(mode: &str) -> (i32, i32) {
    if mode == "Caret" {
        if let Some((x, y)) = caret_screen_point() {
            return (x, y + 24);
        }
    }
    if let Some((x, y)) = cursor_screen_point() {
        return (x + 8, y + 20);
    }
    (0, 0)
}

#[cfg(not(windows))]
pub fn resolve_popup_anchor(_mode: &str) -> (i32, i32) {
    (0, 0)
}

/// 把已解析的物理锚点放到所在屏工作区内。
#[cfg(windows)]
pub fn position_at(window: &Window, logical_w: f32, logical_h: f32, x: i32, y: i32) {
    place_in_work(window, logical_w, logical_h, x, y);
}

#[cfg(not(windows))]
pub fn position_at(window: &Window, logical_w: f32, logical_h: f32, _x: i32, _y: i32) {
    position_near_cursor(window, logical_w, logical_h);
}

/// `Caret`：UIA → Win32 caret → 缓存 → 鼠标（对齐 WPF PositionPopup）。
#[cfg(windows)]
pub fn position_popup(window: &Window, logical_w: f32, logical_h: f32, mode: &str) {
    let (x, y) = resolve_popup_anchor(mode);
    position_at(window, logical_w, logical_h, x, y);
}

#[cfg(not(windows))]
pub fn position_popup(window: &Window, logical_w: f32, logical_h: f32, _mode: &str) {
    position_near_cursor(window, logical_w, logical_h);
}

/// 启动时预热 UIA，避免第一次从 Word/Chromium 呼出时冷启动超时落到鼠标。
pub fn warmup_caret_uia() {
    #[cfg(windows)]
    {
        let _ = std::thread::Builder::new()
            .name("clipx-uia-warmup".into())
            .spawn(|| {
                let _ = uia_caret_point();
            });
    }
}

#[cfg(windows)]
fn cursor_screen_point() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt).ok()?;
        Some((pt.x, pt.y))
    }
}

#[cfg(windows)]
struct CaretCache {
    hwnd: isize,
    x: i32,
    y: i32,
    at: std::time::Instant,
}

#[cfg(windows)]
static CARET_CACHE: std::sync::Mutex<Option<CaretCache>> = std::sync::Mutex::new(None);

#[cfg(windows)]
fn cache_caret(hwnd: isize, pt: (i32, i32)) {
    if let Ok(mut g) = CARET_CACHE.lock() {
        *g = Some(CaretCache {
            hwnd,
            x: pt.0,
            y: pt.1,
            at: std::time::Instant::now(),
        });
    }
}

#[cfg(windows)]
fn cached_caret(hwnd: isize) -> Option<(i32, i32)> {
    let g = CARET_CACHE.lock().ok()?;
    let c = g.as_ref()?;
    if c.hwnd != hwnd || c.at.elapsed() > std::time::Duration::from_secs(30) {
        return None;
    }
    Some((c.x, c.y))
}

#[cfg(windows)]
fn caret_screen_point() -> Option<(i32, i32)> {
    let fg = foreground_hwnd();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let _ = std::thread::Builder::new()
        .name("clipx-uia-caret".into())
        .spawn(move || {
            let pt = uia_caret_point();
            if let Some(p) = pt {
                cache_caret(fg, p);
            }
            let _ = tx.send(pt);
        });
    let uia = rx
        .recv_timeout(std::time::Duration::from_millis(220))
        .ok()
        .flatten();
    if let Some(p) = uia {
        return Some(p);
    }
    if let Some(p) = gui_thread_caret() {
        cache_caret(fg, p);
        return Some(p);
    }
    if let Some(p) = attached_caret() {
        cache_caret(fg, p);
        return Some(p);
    }
    cached_caret(fg)
}

/// UIA TextPattern 选区 / 焦点控件矩形。空 ClassName 或铺满前台窗的矩形视为无效。
#[cfg(windows)]
fn uia_caret_point() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayDestroy, SafeArrayUnaccessData};
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationTextPattern, UIA_TextPatternId,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect};

    unsafe {
        let co = CoInitializeEx(None, COINIT_MULTITHREADED);
        let auto: IUIAutomation =
            match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(v) => v,
                Err(_) => {
                    if co.is_ok() {
                        CoUninitialize();
                    }
                    return None;
                }
            };
        let focused = match auto.GetFocusedElement() {
            Ok(el) => el,
            Err(_) => {
                if co.is_ok() {
                    CoUninitialize();
                }
                return None;
            }
        };
        let class_name = focused
            .CurrentClassName()
            .ok()
            .map(|b| b.to_string())
            .unwrap_or_default();

        let mut out = None;
        if let Ok(pattern) = focused.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
        {
            if let Ok(sel) = pattern.GetSelection() {
                if sel.Length().unwrap_or(0) > 0 {
                    if let Ok(range) = sel.GetElement(0) {
                        if let Ok(psa) = range.GetBoundingRectangles() {
                            if !psa.is_null() {
                                let mut data: *mut core::ffi::c_void = core::ptr::null_mut();
                                if SafeArrayAccessData(psa, &mut data).is_ok() && !data.is_null() {
                                    let nums = data as *const f64;
                                    let left = *nums;
                                    let top = *nums.add(1);
                                    let width = *nums.add(2);
                                    let height = *nums.add(3);
                                    let _ = SafeArrayUnaccessData(psa);
                                    if (left > 0.0 || top > 0.0) && width >= 0.0 && height >= 0.0 {
                                        if !class_name.is_empty() {
                                            out = Some((
                                                left.round() as i32,
                                                (top + height).round() as i32,
                                            ));
                                        }
                                    }
                                }
                                let _ = SafeArrayDestroy(psa);
                            }
                        }
                    }
                }
            }
        }

        if out.is_none() {
            if let Ok(rect) = focused.CurrentBoundingRectangle() {
                let w = (rect.right - rect.left).max(0);
                let h = (rect.bottom - rect.top).max(0);
                if w > 0 && h > 0 && !class_name.is_empty() {
                    let fg = GetForegroundWindow();
                    let mut fg_rc = RECT::default();
                    let window_level = if GetWindowRect(fg, &mut fg_rc).is_ok() {
                        let fg_area = (fg_rc.right - fg_rc.left).max(0) as f64
                            * (fg_rc.bottom - fg_rc.top).max(0) as f64;
                        let area = w as f64 * h as f64;
                        fg_area > 0.0 && area / fg_area >= 0.95
                    } else {
                        false
                    };
                    if !window_level {
                        out = Some((rect.left + 20, rect.bottom + 4));
                    }
                }
            }
        }

        if co.is_ok() {
            CoUninitialize();
        }
        out
    }
}

#[cfg(windows)]
fn gui_thread_caret() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return None;
        }
        let tid = GetWindowThreadProcessId(fg, None);
        if tid == 0 {
            return None;
        }
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(tid, &mut info).ok()?;
        if info.hwndCaret.0.is_null() {
            return None;
        }
        // Electron 常把 hwndCaret 填成主窗、rcCaret=(0,0,0,0)
        if info.rcCaret.right <= info.rcCaret.left || info.rcCaret.bottom <= info.rcCaret.top {
            return None;
        }
        let mut pt = POINT {
            x: info.rcCaret.left,
            y: info.rcCaret.bottom,
        };
        if !ClientToScreen(info.hwndCaret, &mut pt).as_bool() {
            return None;
        }
        if pt.x == 0 && pt.y == 0 {
            return None;
        }
        Some((pt.x, pt.y))
    }
}

#[cfg(windows)]
fn attached_caret() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCaretPos, GetForegroundWindow, GetWindowThreadProcessId,
    };
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return None;
        }
        let fg_tid = GetWindowThreadProcessId(fg, None);
        if fg_tid == 0 {
            return None;
        }
        let my_tid = GetCurrentThreadId();
        if !AttachThreadInput(my_tid, fg_tid, true).as_bool() {
            return None;
        }
        let focus = GetFocus();
        let mut caret = POINT::default();
        let ok = !focus.0.is_null() && GetCaretPos(&mut caret).is_ok() && (caret.x != 0 || caret.y != 0);
        let pt = if ok && ClientToScreen(focus, &mut caret).as_bool() {
            Some((caret.x, caret.y))
        } else {
            None
        };
        let _ = AttachThreadInput(my_tid, fg_tid, false);
        pt
    }
}

#[cfg(windows)]
fn place_in_work(window: &Window, logical_w: f32, logical_h: f32, x: i32, y: i32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    unsafe {
        let pt = POINT { x, y };
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
            return;
        }
        let scale = window.scale_factor();
        let w = (logical_w * scale) as i32;
        let h = (logical_h * scale) as i32;
        let work = mi.rcWork;
        let mut x = x;
        let mut y = y;
        if x + w > work.right {
            x = work.right - w;
        }
        if y + h > work.bottom {
            y = work.bottom - h;
        }
        if x < work.left {
            x = work.left;
        }
        if y < work.top {
            y = work.top;
        }
        window.set_position(slint::WindowPosition::Physical(
            slint::PhysicalPosition::new(x, y),
        ));
    }
}

/// 预览加宽后把窗口拉回当前显示器工作区，不改锚点到光标。
#[cfg(windows)]
pub fn clamp_to_work_area(window: &Window, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_err() {
            return;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
            return;
        }
        let scale = window.scale_factor();
        let w = (logical_w * scale) as i32;
        let h = (logical_h * scale) as i32;
        let work = mi.rcWork;
        let mut x = rc.left;
        let mut y = rc.top;
        if x + w > work.right {
            x = work.right - w;
        }
        if y + h > work.bottom {
            y = work.bottom - h;
        }
        if x < work.left {
            x = work.left;
        }
        if y < work.top {
            y = work.top;
        }
        if x != rc.left || y != rc.top {
            window.set_position(slint::WindowPosition::Physical(
                slint::PhysicalPosition::new(x, y),
            ));
        }
    }
}

#[cfg(not(windows))]
pub fn clamp_to_work_area(_window: &Window, _logical_w: f32, _logical_h: f32) {}

/// 快速查找浮层定位：资源管理器右侧优先，放不下换左侧，再不行贴工作区右缘；
/// 垂直顶部对齐资源管理器（垂直居中偏移，对齐 WPF PositionNearExplorer）。
#[cfg(windows)]
pub fn position_near_explorer(window: &Window, frame: isize, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindow};

    unsafe {
        let h = HWND(frame as *mut _);
        if frame == 0 || !IsWindow(Some(h)).as_bool() {
            position_near_cursor(window, logical_w, logical_h);
            return;
        }
        let mut rc = RECT::default();
        if GetWindowRect(h, &mut rc).is_err() {
            position_near_cursor(window, logical_w, logical_h);
            return;
        }
        let monitor = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
            position_near_cursor(window, logical_w, logical_h);
            return;
        }
        let scale = window.scale_factor();
        let w = (logical_w * scale) as i32;
        let hgt = (logical_h * scale) as i32;
        let work = mi.rcWork;
        let mut x = if rc.right + w + 8 <= work.right {
            rc.right + 4
        } else if rc.left - w - 8 >= work.left {
            rc.left - w - 4
        } else {
            work.right - w - 16
        };
        let cy = (rc.top + rc.bottom) / 2;
        let mut y = rc.top.max(cy - hgt / 2);
        if x < work.left {
            x = work.left;
        }
        if y + hgt > work.bottom {
            y = work.bottom - hgt;
        }
        if y < work.top {
            y = work.top;
        }
        window.set_position(slint::WindowPosition::Physical(
            slint::PhysicalPosition::new(x, y),
        ));
    }
}

#[cfg(not(windows))]
pub fn position_near_hwnd_or_cursor(
    window: &Window,
    _anchor: isize,
    logical_w: f32,
    logical_h: f32,
) {
    position_near_cursor(window, logical_w, logical_h);
}

/// 对话框/窗口居中到光标所在显示器工作区（设置窗口用）。
#[cfg(windows)]
pub fn center_on_cursor_monitor(window: &Window, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return;
        }
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
            return;
        }
        let scale = window.scale_factor();
        let w = (logical_w * scale) as i32;
        let h = (logical_h * scale) as i32;
        let work = mi.rcWork;
        let x = work.left + ((work.right - work.left - w) / 2).max(0);
        let y = work.top + ((work.bottom - work.top - h) / 2).max(0);
        window.set_position(slint::WindowPosition::Physical(
            slint::PhysicalPosition::new(x, y),
        ));
    }
}

#[cfg(not(windows))]
pub fn center_on_cursor_monitor(_window: &Window, _logical_w: f32, _logical_h: f32) {}

/// 对话框矩形（物理像素，左/上/右/下），窗口已销毁或取不到返回 None。
#[cfg(windows)]
pub fn dialog_rect(anchor: isize) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindow};

    if anchor == 0 {
        return None;
    }
    unsafe {
        let h = HWND(anchor as *mut _);
        if !IsWindow(Some(h)).as_bool() {
            return None;
        }
        let mut rc = RECT::default();
        if GetWindowRect(h, &mut rc).is_err() {
            return None;
        }
        Some((rc.left, rc.top, rc.right, rc.bottom))
    }
}

#[cfg(not(windows))]
pub fn dialog_rect(_anchor: isize) -> Option<(i32, i32, i32, i32)> {
    None
}

/// 对话框是否还活着（销毁即 false，Picker 应跟着关闭）。
#[cfg(windows)]
pub fn dialog_alive(anchor: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;
    if anchor == 0 {
        return false;
    }
    unsafe { IsWindow(Some(HWND(anchor as *mut _))).as_bool() }
}

#[cfg(not(windows))]
pub fn dialog_alive(_anchor: isize) -> bool {
    false
}

/// FileJump Picker 停靠定位（对齐 WPF `FileJumpPickerDockPlacement`）：
/// 右 → 左 → 底部居中 → 工作区夹紧（物理像素，gap=4，y 与框顶对齐）。
/// 显示器取对话框中心所在屏；anchor 无效时跟光标。
#[cfg(windows)]
pub fn position_dock_dialog(window: &Window, anchor: isize, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    if let Some((l, t, r, b)) = dialog_rect(anchor) {
        unsafe {
            let center = POINT {
                x: (l + r) / 2,
                y: (t + b) / 2,
            };
            let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
                let scale = window.scale_factor();
                let w = (logical_w * scale) as i32;
                let hgt = (logical_h * scale) as i32;
                let work = mi.rcWork;
                let (x, y) = clipx_filejump::dock::dock_position(
                    (l, t, r, b),
                    w,
                    hgt,
                    (work.left, work.top, work.right, work.bottom),
                );
                window.set_position(slint::WindowPosition::Physical(
                    slint::PhysicalPosition::new(x, y),
                ));
                return;
            }
        }
    }
    position_near_cursor(window, logical_w, logical_h);
}

#[cfg(not(windows))]
pub fn position_dock_dialog(window: &Window, _anchor: isize, logical_w: f32, logical_h: f32) {
    position_near_cursor(window, logical_w, logical_h);
}

/// Picker 重算停靠（对话框移动/缩放跟随用，对齐 WPF `TryRealtimeDockFollow`）：
/// 用 Picker 当前物理尺寸重跑 dock 规则，位置不变则跳过；
/// 对话框已死返回 false（调用方关闭 Picker）。
/// SetWindowPos 不抢焦点不改层级（NOZORDER|NOACTIVATE|NOSIZE）。
#[cfg(windows)]
pub fn redock_picker(anchor: isize) -> bool {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };

    let hwnd = crate::mouse_hook::FJ_HWND.load(std::sync::atomic::Ordering::SeqCst);
    if hwnd == 0 {
        return true;
    }
    let Some((l, t, r, b)) = dialog_rect(anchor) else {
        return false;
    };
    unsafe {
        let h = HWND(hwnd as *mut _);
        let mut rc = RECT::default();
        if GetWindowRect(h, &mut rc).is_err() {
            return true;
        }
        let pw = rc.right - rc.left;
        let ph = rc.bottom - rc.top;
        if pw <= 0 || ph <= 0 {
            return true;
        }
        let center = POINT {
            x: (l + r) / 2,
            y: (t + b) / 2,
        };
        let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
            return true;
        }
        let work = mi.rcWork;
        let (x, y) = clipx_filejump::dock::dock_position(
            (l, t, r, b),
            pw,
            ph,
            (work.left, work.top, work.right, work.bottom),
        );
        if x == rc.left && y == rc.top {
            return true;
        }
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        true
    }
}

#[cfg(not(windows))]
pub fn redock_picker(_anchor: isize) -> bool {
    true
}

/// Picker 手动拖拽：逻辑位移（逻辑像素，内部乘 scale）。
#[cfg(windows)]
pub fn drag_picker_by(window: &Window, dx_logical: f32, dy_logical: f32) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE,
    };

    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_err() {
            return;
        }
        let scale = window.scale_factor();
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rc.left + (dx_logical * scale) as i32,
            rc.top + (dy_logical * scale) as i32,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

#[cfg(not(windows))]
pub fn drag_picker_by(_window: &Window, _dx: f32, _dy: f32) {}

#[cfg(windows)]
pub fn store_hwnd(window: &Window) {
    if let Some(h) = hwnd_isize(window) {
        crate::mouse_hook::POPUP_HWND.store(h, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(windows)]
pub fn foreground_hwnd() -> isize {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow().0 as isize }
}

#[cfg(windows)]
pub fn foreground_is_file_dialog() -> bool {
    let h = foreground_hwnd();
    h != 0
        && clipx_filejump::dialog::win::classify_hwnd(h)
            .map(|k| k != clipx_filejump::DialogKind::NotDialog)
            .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn foreground_is_file_dialog() -> bool {
    false
}

/// 粘贴前把前台抢回呼出时的目标窗（对齐 WPF SetForegroundWindowAggressive）。
#[cfg(windows)]
pub fn restore_foreground(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsWindow, SetForegroundWindow,
    };

    if hwnd == 0 {
        return;
    }
    let target = HWND(hwnd as *mut _);
    unsafe {
        if !IsWindow(Some(target)).as_bool() {
            return;
        }
        let fg = GetForegroundWindow();
        if fg == target {
            return;
        }
        let cur = GetCurrentThreadId();
        let mut fg_pid = 0u32;
        let fg_tid = if fg.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, Some(&mut fg_pid))
        };
        let mut target_pid = 0u32;
        let target_tid = GetWindowThreadProcessId(target, Some(&mut target_pid));
        let attach_fg = fg_tid != 0 && fg_tid != cur;
        let attach_target = target_tid != 0 && target_tid != cur && target_tid != fg_tid;
        if attach_fg {
            let _ = AttachThreadInput(cur, fg_tid, true);
        }
        if attach_target {
            let _ = AttachThreadInput(cur, target_tid, true);
        }
        let _ = SetForegroundWindow(target);
        if attach_target {
            let _ = AttachThreadInput(cur, target_tid, false);
        }
        if attach_fg {
            let _ = AttachThreadInput(cur, fg_tid, false);
        }
    }
}

/// 空闲时归还工作集给 OS（经典 SetProcessWorkingSetSize(-1,-1) 惯用法）。
/// 峰值操作（4K 预览解码等）后防止 WS 虚高；下次访问 soft fault 廉价拉回。
#[cfg(windows)]
pub fn trim_working_set() {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    unsafe {
        let h = GetCurrentProcess();
        let _ = SetProcessWorkingSetSize(h, usize::MAX, usize::MAX);
    }
}

#[cfg(not(windows))]
pub fn trim_working_set() {}

#[cfg(windows)]
fn hwnd_isize(window: &Window) -> Option<isize> {
    hwnd_of(window).map(|h| h.0 as isize)
}

#[cfg(windows)]
fn hwnd_of(window: &Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().window_handle() {
        Ok(handle) => match handle.as_raw() {
            RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut _)),
            _ => None,
        },
        Err(_) => None,
    }
}

#[cfg(not(windows))]
pub fn apply_style(_window: &Window) {}

#[cfg(not(windows))]
pub fn position_near_cursor(_window: &Window, _logical_w: f32, _logical_h: f32) {}

#[cfg(not(windows))]
pub fn store_hwnd(_window: &Window) {}

#[cfg(not(windows))]
pub fn foreground_hwnd() -> isize {
    0
}
