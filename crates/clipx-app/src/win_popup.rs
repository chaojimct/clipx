use slint::Window;

#[cfg(windows)]
use windows::Win32::Foundation::HWND;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST,
};

#[cfg(windows)]
pub fn apply_style(window: &Window) {
    if let Some(hwnd) = hwnd_of(window) {
        unsafe {
            let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let flags = (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0) as isize;
            let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, current | flags);
        }
    }
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
