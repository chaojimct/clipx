//! 最小钩子实验（32 位）：在**自己的主线程**上装 `WH_GETMESSAGE`，
//! 用 `PostThreadMessage` 制造消息，统计回调次数。
//!
//! 目的：分清「钩子用法有问题」还是「从远程注入的 DLL 里装钩子不生效」。
//!
//! ```text
//! cargo build -p clipx-tsf --target i686-pc-windows-msvc --features win32-examples --example hooktest32
//! target\i686-pc-windows-msvc\debug\examples\hooktest32.exe
//! ```

#![cfg(windows)]

use std::sync::atomic::{AtomicU32, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, GetMessageW, PostThreadMessageW,
    RegisterClassW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, MSG, WH_GETMESSAGE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROY, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

static HITS: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let n = HITS.fetch_add(1, Ordering::Relaxed);
        if n < 3 {
            println!("[hook] hit #{n} code={code}");
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_DESTROY {
        unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn run_case(label: &str, use_hinst: bool) {
    unsafe {
        let hmod = GetModuleHandleW(None).expect("GetModuleHandleW");
        let hinst = HINSTANCE(hmod.0);
        let h: HHOOK = match SetWindowsHookExW(
            WH_GETMESSAGE,
            Some(hook_proc),
            if use_hinst { Some(hinst) } else { None },
            GetCurrentThreadId(),
        ) {
            Ok(h) => h,
            Err(e) => {
                println!("[{label}] SetWindowsHookEx 失败: {e}");
                return;
            }
        };
        println!("[{label}] 装钩子 OK hhk={:?} hinst={use_hinst}", h.0);

        for _ in 0..3 {
            let _ = PostThreadMessageW(GetCurrentThreadId(), 0x0400, WPARAM(0), LPARAM(0));
        }
        let mut msg = MSG::default();
        let mut pumped = 0;
        while pumped < 3 {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                break;
            }
            pumped += 1;
        }
        println!(
            "[{label}] 泵了 {pumped} 条消息, hook 命中 {}",
            HITS.load(Ordering::Relaxed)
        );
        let _ = UnhookWindowsHookEx(h);
        HITS.store(0, Ordering::Relaxed);
    }
}

fn main() {
    unsafe {
        let hmod = GetModuleHandleW(None).expect("GetModuleHandleW");
        let hinst = HINSTANCE(hmod.0);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: w!("hooktest32"),
            ..Default::default()
        };
        assert_ne!(RegisterClassW(&wc), 0, "RegisterClassW");
        let _hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("hooktest32"),
            w!("hooktest32"),
            WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 | WS_VISIBLE.0),
            0,
            0,
            300,
            200,
            None,
            None,
            Some(hinst),
            None,
        )
        .expect("CreateWindowExW");
        println!("pid={}", std::process::id());

        run_case("hMod=NULL", false);
        run_case("hMod=EXE", true);
    }
}
