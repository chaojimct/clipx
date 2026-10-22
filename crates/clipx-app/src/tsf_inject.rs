//! 路线乙：按需注入 —— 在目标进程内自主测量 caret（Windows）。
//!
//! 与路线甲（注册成 TSF TIP profile）的本质区别：**全程不碰输入法**。
//! 不调用任何 profile API，不销毁/重建用户的 TIP 实例，不动 keyboard
//! openclose compartment ⇒ 微信输入法的中/英态（存在它自己实例内存里）
//! 原样保留；也不往 HKLM 注册，Win+Space 输入法列表里不会多出一项。
//!
//! 流程（按需 + 钩子短驻留，平衡"零常驻"与"首次慢"）：
//! 1. 取当前焦点窗口线程（Electron 的 TSF 上下文挂在浏览器 UI 线程上）。
//! 2. `SetWindowsHookExW(WH_GETMESSAGE, …, target_tid)` 把信标 DLL 注入该进程；
//!    钩子回调 `ClipxCaretHookProc` 在**宿主 UI 线程内**跑一次只读测量。
//! 3. `PostThreadMessageW` 唤醒目标线程，等共享内存里出现本 pid 的新结果。
//! 4. 钩子**驻留 `HOOK_GRACE_MS`**（默认 5s）：同一应用连续呼出不重复装钩，
//!    首次注入的 DLL 加载成本只付一次；过期或换目标进程时自动摘除。
//!
//! 失败一律静默返回 None，由 `win_popup` 的老链路兜底。

use clipx_core::tsf_caret as proto;
use std::sync::Mutex;
use std::time::Instant;

/// 钩子驻留时长（毫秒）：同一目标线程在此窗口内重复测量不再装钩。
const HOOK_GRACE_MS: u64 = 5_000;
/// 等目标线程应答的总超时（毫秒）——**首次装钩**（DLL 要进目标进程）。
const WAIT_TIMEOUT_FRESH_MS: u64 = 400;
/// 等目标线程应答的总超时（毫秒）——钩子已驻留（热路径）。
const WAIT_TIMEOUT_WARM_MS: u64 = 80;
/// 轮询间隔（毫秒）。
const POLL_MS: u64 = 4;
/// 结果新鲜度上限（毫秒），与 tsf_caret 侧保持一致。
const MAX_AGE_MS: u64 = 1500;

/// 驻留钩子缓存：同一 UI 线程短时间内的重复测量直接复用，省掉重复装钩。
struct HookEntry {
    hook: windows::Win32::UI::WindowsAndMessaging::HHOOK,
    tid: u32,
    pid: u32,
    installed_at: Instant,
}

// HHOOK 是内核句柄（非指针），跨线程持有没有数据竞争。
unsafe impl Send for HookEntry {}
unsafe impl Sync for HookEntry {}

impl Drop for HookEntry {
    fn drop(&mut self) {
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx;
            let _ = UnhookWindowsHookEx(self.hook);
        }
    }
}

static HOOK_CACHE: Mutex<Option<HookEntry>> = Mutex::new(None);

/// 注入模式下的测量入口。
///
/// 返回 `(CaretRect, tag)`；拿不到就 `None`，由调用方回落老链路。
pub fn measure_foreground_caret() -> Option<(proto::CaretRect, u32)> {
    let (target_tid, target_pid) = focus_thread()?;
    measure_thread_caret(target_tid, target_pid)
}

/// 对指定 pid 的 UI 线程做一次注入测量（自检用，不依赖抢焦点）。
///
/// 与 `measure_foreground_caret` 同一条链路，只是目标线程由调用方指定。
pub fn measure_pid_caret(target_pid: u32) -> Option<(proto::CaretRect, u32)> {
    if target_pid == 0 || target_pid == std::process::id() {
        return None;
    }
    let target_tid = ui_thread_of(target_pid)?;
    measure_thread_caret(target_tid, target_pid)
}

fn measure_thread_caret(target_tid: u32, target_pid: u32) -> Option<(proto::CaretRect, u32)> {
    if target_pid == std::process::id() {
        return None; // 自己不注入
    }
    let base = crate::tsf_caret::shm_init()?;
    let start_tick = now_tick();
    let _seq = crate::tsf_caret::write_request(base)?;

    // 复用驻留钩子，或装新的。
    let warm = ensure_hook(target_tid, target_pid)?;
    let wait_ms = if warm {
        WAIT_TIMEOUT_WARM_MS
    } else {
        WAIT_TIMEOUT_FRESH_MS
    };

    // 唤醒目标线程，让 WH_GETMESSAGE 立刻回调。
    unsafe {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
        let _ = PostThreadMessageW(target_tid, 0 /*WM_NULL*/, WPARAM(0), LPARAM(0));
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    while std::time::Instant::now() < deadline {
        if let Some(c) = crate::tsf_caret::scan_results(base, target_pid) {
            // 结果必须是本次请求之后测出来的（tick 单调同源），且尺寸合理。
            if c.tick >= start_tick && proto::is_fresh(&c, now_tick(), MAX_AGE_MS) && c.sane() {
                return Some((c, target_pid));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
    }
    None
}

/// 确保目标线程上有可用的钩子。返回 `true` 表示复用了驻留钩子（热路径）。
fn ensure_hook(target_tid: u32, target_pid: u32) -> Option<bool> {
    let mut cache = HOOK_CACHE.lock().ok()?;

    // 命中：同 tid 且未过期 → 直接复用。
    if let Some(entry) = cache.as_ref() {
        if entry.tid == target_tid
            && entry.installed_at.elapsed().as_millis() as u64 <= HOOK_GRACE_MS
        {
            return Some(true);
        }
    }

    // 未命中：先摘旧的（换目标或过期），再装新的。
    *cache = None;
    let hook = install_hook(target_tid)?;
    *cache = Some(HookEntry {
        hook,
        tid: target_tid,
        pid: target_pid,
        installed_at: Instant::now(),
    });
    Some(false)
}

/// 找出指定进程里「拥有可见顶层窗口」的 UI 线程 id。
fn ui_thread_of(target_pid: u32) -> Option<u32> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
    };

    struct Ctx {
        pid: u32,
        tid: u32,
    }
    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == ctx.pid && tid != 0 {
            ctx.tid = tid;
            return BOOL(0); // 找到即停
        }
        BOOL(1)
    }
    let mut ctx = Ctx { pid: target_pid, tid: 0 };
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    if ctx.tid == 0 {
        None
    } else {
        Some(ctx.tid)
    }
}

/// 取「当前真正持有键盘焦点」的窗口线程。
///
/// Electron 的 TSF 上下文挂在**浏览器 UI 线程**（顶层窗口线程）上，
/// 所以这里优先用 `GetGUIThreadInfo` 的焦点窗，回落到前台窗口。
fn focus_thread() -> Option<(u32, u32)> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };
    unsafe {
        let mut gti = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(0, &mut gti).is_ok() && !gti.hwndFocus.is_invalid() {
            let mut pid = 0u32;
            let tid = GetWindowThreadProcessId(gti.hwndFocus, Some(&mut pid));
            if tid != 0 && pid != 0 {
                return Some((tid, pid));
            }
        }
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(fg, Some(&mut pid));
        if tid == 0 || pid == 0 {
            return None;
        }
        Some((tid, pid))
    }
}

/// 把信标 DLL 挂到目标线程上（`WH_GETMESSAGE`）。
///
/// 返回 `HHOOK`，由调用方（`HookEntry`）负责摘除。
fn install_hook(target_tid: u32) -> Option<windows::Win32::UI::WindowsAndMessaging::HHOOK> {
    use windows::Win32::Foundation::{FARPROC, HINSTANCE};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowsHookExW, WH_GETMESSAGE};

    let dll_path = beacon_dll_path()?;
    let wide: Vec<u16> = dll_path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let hmod = LoadLibraryW(windows::core::PCWSTR(wide.as_ptr())).ok()?;
        let proc: FARPROC =
            GetProcAddress(hmod, windows::core::s!("ClipxCaretHookProc"));
        let Some(proc) = proc else {
            return None;
        };
        // FARPROC → HOOKPROC：两者都是 extern "system" fn，签名不同但 ABI 相同。
        let hook_proc: windows::Win32::UI::WindowsAndMessaging::HOOKPROC =
            Some(std::mem::transmute::<_, unsafe extern "system" fn(_, _, _) -> _>(proc));

        let hhk = SetWindowsHookExW(WH_GETMESSAGE, hook_proc, Some(HINSTANCE(hmod.0)), target_tid)
            .ok()?;
        Some(hhk)
    }
}

/// 信标 DLL 路径：优先 clipx.exe 同目录（安装态），回落仓库 target/release（开发态）。
fn beacon_dll_path() -> Option<std::path::PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidates = [
        exe_dir.join("clipx_caret_beacon.dll"),
        exe_dir.join("../../clipx_caret_beacon.dll"),
        exe_dir.join("../clipx_caret_beacon.dll"),
    ];
    for c in candidates {
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

/// 自检用：把解析到的信标 DLL 路径打出来。
pub fn debug_dll_path() -> Option<std::path::PathBuf> {
    beacon_dll_path()
}

fn now_tick() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount64;
    unsafe { GetTickCount64() }
}
