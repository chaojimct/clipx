//! Explorer / Shell 交互（仅 Windows）：资源管理器窗口检测、路径采集、
//! 就地导航与选中。移植自 WPF FileManagerPathCollector（Explorer 部分）与
//! ExplorerQuickFindController.NavigateAndSelect。
//!
//! COM 调用要求所在线程已 CoInitializeEx；调用方自行保证。

#![cfg(windows)]

use windows::core::{BSTR, GUID, Interface};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VariantClear, VARIANT};
use windows::Win32::UI::Shell::{
    Folder, Folder2, IShellFolderViewDual, IShellWindows, IWebBrowser2, ILCreateFromPathW, ILFree,
    SHGetKnownFolderPath, SHOpenFolderAndSelectItems, FOLDERID_Desktop,
};
use windows::Win32::System::Com::{
    CoCreateInstance, IDispatch, CLSCTX_ALL,
};

const CLSID_SHELLWINDOWS: GUID = GUID::from_values(
    0x9CB9A6D5,
    0x5D8A,
    0x425F,
    [0xB3, 0xF1, 0xBF, 0x5E, 0x32, 0xB8, 0xC4, 0x0C],
);

/// SelectItem 标志（WPF：SELECT|DESELECTOTHERS|ENSUREVISIBLE|FOCUSED）。
const SVSI_SELECT_FLAGS: i32 = 0x1 | 0x4 | 0x8 | 0x10;

// ===================== 窗口检测（纯 Win32，钩子线程安全调用） =====================

pub fn class_name(hwnd: isize) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    unsafe {
        let h = HWND(hwnd as *mut _);
        if h.0.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 256];
        let n = GetClassNameW(h, &mut buf);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// 沿 GetParent 父链向上找资源管理器框架（CabinetWClass / ExploreWClass）。
/// 对齐 WPF TryFindExplorerCabinetFrame（EnumWindows 兜底未移植：前台窗口
/// 属 Explorer 进程时父链必命中，未命中即安全放行）。
pub fn find_cabinet_frame(hwnd: isize) -> Option<isize> {
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetParent, GA_ROOT, IsWindow};
    unsafe {
        let mut w = HWND(hwnd as *mut _);
        if w.0.is_null() || !IsWindow(Some(w)).as_bool() {
            return None;
        }
        for _ in 0..64 {
            let cls = class_name(w.0 as isize);
            if cls == "CabinetWClass" || cls == "ExploreWClass" {
                return Some(w.0 as isize);
            }
            let Ok(parent) = GetParent(w) else {
                break;
            };
            if parent.0.is_null() {
                break;
            }
            w = parent;
        }
        // Win11 XAML 岛等场景父链断开：退回 GA_ROOT 判一次
        let root = GetAncestor(w, GA_ROOT);
        let cls = class_name(root.0 as isize);
        if cls == "CabinetWClass" || cls == "ExploreWClass" {
            Some(root.0 as isize)
        } else {
            None
        }
    }
}

/// 前台是否桌面（Progman / WorkerW）。
pub fn is_desktop_hwnd(hwnd: isize) -> bool {
    let cls = class_name(hwnd);
    cls == "Progman" || cls == "WorkerW"
}

/// 焦点是否在可编辑控件上（地址栏/搜索框/重命名框）。仅 Win32 API，<0.1ms。
/// 对齐 WPF QuickCheckFocusNotEditBox（含 Win11 DirectUI focus=0 但 caret 非空场景）。
pub fn focus_is_edit_box(frame: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };
    unsafe {
        let h = HWND(frame as *mut _);
        let tid = GetWindowThreadProcessId(h, None);
        if tid == 0 {
            return false;
        }
        let mut gti = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(tid, &mut gti).is_err() {
            return false; // 取不到放行（WPF 同）
        }
        let null = |h: &HWND| h.0.is_null();
        if null(&gti.hwndFocus) {
            // focus 为 0 但存在文本 caret：正在编辑（重命名/地址栏）
            let has_caret = !null(&gti.hwndCaret)
                || (gti.rcCaret.right > gti.rcCaret.left && gti.rcCaret.bottom > gti.rcCaret.top);
            return has_caret;
        }
        let cls = class_name(gti.hwndFocus.0 as isize);
        if cls == "Edit" {
            return true;
        }
        if cls.contains("ComboBox") {
            return true;
        }
        if cls.to_lowercase().contains("richedit") {
            return true;
        }
        false
    }
}

/// 光标是否点在对话框内的可编辑控件上（另存为文件名框等）。
pub fn cursor_hits_dialog_edit(dialog: isize) -> bool {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetCursorPos, GetParent, WindowFromPoint, GA_ROOT,
    };
    if dialog == 0 {
        return false;
    }
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return false;
        }
        let hwnd = WindowFromPoint(pt);
        if hwnd.0.is_null() {
            return false;
        }
        let mut w = hwnd;
        let mut under = false;
        for _ in 0..64 {
            if w.0 as isize == dialog {
                under = true;
                break;
            }
            match GetParent(w) {
                Ok(p) if !p.0.is_null() => w = p,
                _ => break,
            }
        }
        if !under && GetAncestor(hwnd, GA_ROOT).0 as isize != dialog {
            return false;
        }
        let cls = class_name(hwnd.0 as isize);
        cls == "Edit" || cls.contains("ComboBox") || cls.to_lowercase().contains("richedit")
    }
}

// ===================== 路径采集（Shell COM，须在 CoInit 线程） =====================

/// 桌面目录（桌面打字场景的检索根）。
pub fn desktop_dir() -> Option<String> {
    unsafe {
        let pw = SHGetKnownFolderPath(&FOLDERID_Desktop, Default::default(), None).ok()?;
        let s = pw.to_string().ok()?;
        CoTaskMemFree(Some(pw.0 as *const core::ffi::c_void));
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// 匹配资源管理器框架窗口，读其当前文件夹路径。
/// 直接走 FileJump 同一套采集（STA + LocationURL + 地址栏），不要另起一套 COM。
pub fn explorer_folder_path(frame: isize) -> Option<String> {
    clipx_filejump::collectors::win::explorer_path_for_frame(frame)
}

fn read_browser_path(browser: &IWebBrowser2) -> Option<String> {
    unsafe {
        if let Ok(url) = browser.LocationURL() {
            if let Some(p) = clipx_filejump::collectors::win::url_to_path(&url.to_string()) {
                if std::path::Path::new(&p).is_dir() {
                    return Some(p);
                }
            }
        }
        let doc = browser.Document().ok()?;
        let view: IShellFolderViewDual = doc.cast().ok()?;
        let folder: Folder = view.Folder().ok()?;
        let folder2: Folder2 = folder.cast().ok()?;
        let item = folder2.Self_().ok()?;
        let path = item.Path().ok()?;
        let s = path.to_string();
        if s.is_empty() || !std::path::Path::new(&s).is_dir() {
            None
        } else {
            Some(s)
        }
    }
}

/// 对齐 WPF ExplorerFrameMatches：相等 / 子窗口 / GA_ROOT / 父链包含。
fn frame_matches(frame: isize, hwnd: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetParent, IsChild, GA_ROOT,
    };
    if frame == 0 || hwnd == 0 {
        return false;
    }
    unsafe {
        let f = HWND(frame as *mut _);
        let h = HWND(hwnd as *mut _);
        if frame == hwnd {
            return true;
        }
        if IsChild(f, h).as_bool() {
            return true;
        }
        if GetAncestor(h, GA_ROOT).0 as isize == frame {
            return true;
        }
        let mut w = h;
        loop {
            let Ok(p) = GetParent(w) else {
                return false;
            };
            if p.0.is_null() {
                return false;
            }
            if p.0 as isize == frame {
                return true;
            }
            w = p;
        }
    }
}

// ===================== 就地导航 + 选中（M4-c） =====================

/// 在资源管理器中就地导航到目标所在文件夹并选中目标。
/// Shell COM 优先 → 失败回退 SHOpenFolderAndSelectItems。
pub fn navigate_and_select(frame: isize, target_full_path: &str) -> bool {
    let Some((dir, name)) = split_parent(target_full_path) else {
        return false;
    };
    if try_com_navigate_select(frame, &dir, &name) {
        return true;
    }
    reveal_in_explorer(target_full_path)
}

fn split_parent(path: &str) -> Option<(String, String)> {
    let p = path.trim_end_matches(['\\', '/']);
    let pos = p.rfind(['\\', '/'])?;
    let dir = &p[..pos];
    let name = &p[pos + 1..];
    if dir.is_empty() || name.is_empty() {
        return None;
    }
    Some((dir.to_string(), name.to_string()))
}

fn try_com_navigate_select(frame: isize, target_dir: &str, target_name: &str) -> bool {
    unsafe {
        let Ok(windows) =
            CoCreateInstance::<_, IShellWindows>(&CLSID_SHELLWINDOWS, None, CLSCTX_ALL)
        else {
            return false;
        };
        let Ok(count) = windows.Count() else {
            return false;
        };
        for i in 0..count {
            let idx = variant_i4(i);
            let Ok(dispatch) = windows.Item(&idx) else { continue };
            let Ok(browser) = dispatch.cast::<IWebBrowser2>() else { continue };
            let Ok(h) = browser.HWND() else { continue };
            if !frame_matches(frame, h.0) {
                continue;
            }

            // 当前路径不同则就地导航，轮询等导航完成（SSD 通常 <100ms）
            let cur = read_browser_path(&browser).unwrap_or_default();
            if !path_eq(&cur, target_dir) {
                if browser
                    .Navigate(&BSTR::from(target_dir), None, None, None, None)
                    .is_err()
                {
                    continue;
                }
                let mut nav_ok = false;
                for _ in 0..10 {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    if let Some(after) = read_browser_path(&browser) {
                        if path_eq(&after, target_dir) {
                            nav_ok = true;
                            break;
                        }
                    } else {
                        break;
                    }
                }
                if !nav_ok {
                    continue;
                }
            }

            // 选中目标（ParseName → SelectItem，SVSI 组合标志）
            let Ok(doc) = browser.Document() else { continue };
            let Ok(view) = doc.cast::<IShellFolderViewDual>() else { continue };
            let Ok(folder) = view.Folder() else { continue };
            let Ok(item) = folder.ParseName(&BSTR::from(target_name)) else { continue };
            let Ok(disp) = item.cast::<IDispatch>() else { continue };
            let mut v = variant_dispatch(disp);
            let selected = view.SelectItem(&v, SVSI_SELECT_FLAGS).is_ok();
            let _ = VariantClear(&mut v);
            if selected {
                return true;
            }
        }
        false
    }
}

fn path_eq(a: &str, b: &str) -> bool {
    let na = normalize_path_for_cmp(a);
    let nb = normalize_path_for_cmp(b);
    na == nb
}

/// 比较用规范化：/ → \、去尾分隔符、小写（Windows 路径大小写不敏感）。
fn normalize_path_for_cmp(p: &str) -> String {
    p.trim()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// 回退：复用/新开资源管理器窗口并选中目标。
fn reveal_in_explorer(full_path: &str) -> bool {
    unsafe {
        let pidl = ILCreateFromPathW(&windows::core::HSTRING::from(full_path));
        if pidl.is_null() {
            return false;
        }
        let ok = SHOpenFolderAndSelectItems(pidl, None, 0).is_ok();
        ILFree(Some(pidl as *const _));
        ok
    }
}

// ===================== VARIANT 构造（windows 0.59 无高层封装） =====================

fn variant_i4(v: i32) -> VARIANT {
    use windows::Win32::System::Variant::VT_I4;
    let mut var = VARIANT::default();
    unsafe {
        // VARIANT_0.Anonymous 是 ManuallyDrop<VARIANT_0_0>，写入需显式解引用
        (*var.Anonymous.Anonymous).vt = VT_I4;
        (*var.Anonymous.Anonymous).Anonymous.lVal = v;
    }
    var
}

fn variant_dispatch(d: IDispatch) -> VARIANT {
    use windows::Win32::System::Variant::VT_DISPATCH;
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_DISPATCH;
        (*var.Anonymous.Anonymous).Anonymous.pdispVal =
            core::mem::ManuallyDrop::new(Some(d));
    }
    var
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_parent_paths() {
        assert_eq!(
            split_parent(r"C:\foo\bar.txt"),
            Some((r"C:\foo".into(), "bar.txt".into()))
        );
        assert_eq!(split_parent(r"C:\bar.txt"), Some(("C:".into(), "bar.txt".into())));
        assert_eq!(split_parent(r"C:\"), None);
        assert_eq!(split_parent("bar.txt"), None);
    }

    #[test]
    fn path_cmp_ignores_case_and_slashes() {
        assert!(path_eq(r"C:\Foo\Bar", "c:/foo/bar/"));
        assert!(!path_eq(r"C:\Foo", r"C:\Foo2"));
    }
}
