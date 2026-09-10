//! 注入调度（M5c）：复用 `../clipboard/native/ShellNavigate` DLL，宿主侧只做调度。
//!
//! 基线 `ShellDialogDeepNavigate.cs`（WPF v1.9.8）：
//! - 链路：`OpenProcess → 架构检测 → VirtualAllocEx → CreateRemoteThread(LoadLibrary)
//!   → 调导出函数 → WM_USER+7 取 IShellBrowser → BrowseObject`
//! - native 侧每次只做单次 BrowseObject（不可在宿主 UI 线程 Sleep），
//!   视图未就绪（busy/E_FAIL）由本模块在后台线程退避后重拉远程线程
//! - WPS 自定义框永不注入（走六重降级，见 `dialog::DialogKind::Wps`）
//! - COM 借用指针（CWM_GETISHELLBROWSER 返回值）不得 Release —— 硬约束

use crate::dialog::DialogKind;

/// 退避重试间隔（WPF 源码实测值；ROADMAP 旧值已按源码纠偏）。
pub const RETRY_DELAYS_MS: &[u64] = &[0, 150, 300, 500, 800, 1200];

/// 跳转策略：按对话框种类决定注入 vs 降级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigatePlan {
    /// #32770 系统框：注入优先，失败回退地址栏模拟。
    InjectThenFallback,
    /// WPS/非注入框：仅地址栏/键入模拟。
    SimulateOnly,
    /// 非对话框：不动作。
    Skip,
}

pub fn plan_for(kind: DialogKind) -> NavigatePlan {
    match kind {
        DialogKind::System => NavigatePlan::InjectThenFallback,
        DialogKind::Wps | DialogKind::Custom => NavigatePlan::SimulateOnly,
        DialogKind::NotDialog => NavigatePlan::Skip,
    }
}

/// 跳转入口（M5c）：`TryNavigateToFolder` 全链路。
/// - 已在目标 → true（不动作）；目录不存在 → false
/// - #32770：注入优先（`ClipboardX_RemoteNavigate`），80ms 后回读校验，失败走键盘回退
/// - WPS/自定义：永不注入，直接键盘回退（Alt+D → Ctrl+L 链）
/// - `allow_inject=false`（杀软拦截场景）跳过注入，对齐 WPF `EnableShellNavigateInject`
pub fn navigate_to_folder(
    dialog_hwnd: isize,
    kind: DialogKind,
    folder: &str,
    allow_inject: bool,
) -> anyhow::Result<bool> {
    let path = normalize_for_nav(folder)?;
    #[cfg(windows)]
    {
        if win::read_current_folder(dialog_hwnd)
            .map(|cur| paths_equal(&cur, &path))
            .unwrap_or(false)
        {
            return Ok(true);
        }
        let is_32770 = crate::dialog::win::class_of(windows::Win32::Foundation::HWND(
            dialog_hwnd as *mut _,
        )) == "#32770";
        if allow_inject && is_32770 && matches!(kind, DialogKind::System) {
            if win::browse_object_inject(dialog_hwnd, &path) {
                return Ok(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(80));
            if win::read_current_folder(dialog_hwnd)
                .map(|cur| paths_equal(&cur, &path))
                .unwrap_or(false)
            {
                return Ok(true);
            }
        }
        if matches!(kind, DialogKind::NotDialog) {
            return Ok(false);
        }
        Ok(win::navigate_keyboard(dialog_hwnd, kind, &path))
    }
    #[cfg(not(windows))]
    {
        let _ = (dialog_hwnd, kind, path, allow_inject);
        Ok(false)
    }
}

/// 回读对话框当前文件夹（注入读优先，失败返回 Err，调用方可降级）。
pub fn read_current_folder(dialog_hwnd: isize) -> anyhow::Result<String> {
    #[cfg(windows)]
    {
        win::read_current_folder(dialog_hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = dialog_hwnd;
        anyhow::bail!("非 Windows 不支持")
    }
}

/// 导航路径规范化：全路径 + 联接点/符号链接解析（WPF `NormalizeFolderPathForNavigation`），
/// 减轻 BrowseObject 与地址栏在 Shell/云目录上的失败率。
pub fn normalize_for_nav(folder: &str) -> anyhow::Result<String> {
    let p = std::path::Path::new(folder.trim().trim_matches('"'));
    if !p.is_dir() {
        anyhow::bail!("目录不存在: {folder}");
    }
    let full = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = full.to_string_lossy().to_string();
    // canonicalize 的 \\?\ 前缀会让地址栏失败，去掉它。
    Ok(s.strip_prefix("\\\\?\\").unwrap_or(&s).to_string())
}

/// 宽松路径相等（大小写不敏感 + 末尾分隔符归一，WPF `PathsLooselyEqual`）。
pub fn paths_equal(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches(['\\', '/']).to_lowercase();
    !a.is_empty() && !b.is_empty() && norm(a) == norm(b)
}

#[cfg(windows)]
pub mod win {
    //! Win32 注入与键盘回退实现（M5c）。
    //!
    //! 链路（WPF `ShellDialogDeepNavigate`）：
    //! `OpenProcess → 架构检测 → VirtualAllocEx → CreateRemoteThread(LoadLibraryW)
    //!  → 取导出函数 → 传 payload → WM_USER+7 取 IShellBrowser → BrowseObject`。
    //! native 侧单次调用不 Sleep；busy(0x800700AA)/E_FAIL(0x80004005) 由本模块
    //! 按 [`super::RETRY_DELAYS_MS`] 在后台线程重试（宿主 UI 线程绝不 Sleep）。
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
    use windows::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W,
        TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
    };
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
    use windows::Win32::System::Memory::{
        VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
    };
    use windows::Win32::System::Threading::{
        CreateRemoteThread, GetExitCodeThread, IsWow64Process, OpenProcess,
        WaitForSingleObject, PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION,
        PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        VK_CONTROL, VK_LMENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::{IsWindow, SetForegroundWindow};
    use windows::core::{HSTRING, PCSTR};

    use crate::dialog::DialogKind;

    pub const EXPORT_NAV: &str = "ClipboardX_RemoteNavigate";
    pub const EXPORT_READ: &str = "ClipboardX_RemoteReadCurrentFolder";
    const EXIT_BUSY: u32 = 0x800700AA;
    const EXIT_E_FAIL: u32 = 0x80004005;

    #[repr(C)]
    struct Payload64 {
        hwnd: u64,
        path: [u16; 520],
    }

    #[repr(C)]
    struct Payload32 {
        hwnd: u32,
        path: [u16; 520],
    }

    fn dll_for_target(target_is64: bool) -> &'static str {
        if target_is64 {
            "ClipboardXShellNavigate.dll"
        } else {
            "ClipboardXShellNavigate32.dll"
        }
    }

    fn dll_full_path(name: &str) -> Option<std::path::PathBuf> {
        let mut cands = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                cands.push(dir.join(name));
            }
        }
        let native = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../clipboard/native/ShellNavigate/bin");
        if name.contains("32") {
            cands.push(native.join("Win32").join("Release").join(name));
        } else {
            cands.push(native.join("x64").join("Release").join(name));
        }
        cands.into_iter().find(|p| p.is_file())
    }

    fn target_is_64(hproc: HANDLE) -> anyhow::Result<bool> {
        unsafe {
            let mut wow64 = windows::Win32::Foundation::BOOL(0);
            IsWow64Process(hproc, &mut wow64)?;
            // 本注入器为 64 位：目标 wow64 → 32 位，否则 64 位。
            Ok(!wow64.as_bool())
        }
    }

    fn remote_module_base(pid: u32, dll_name: &str) -> Option<usize> {
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid).ok()?;
            let mut me = MODULEENTRY32W {
                dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
                ..Default::default()
            };
            let lower = dll_name.to_lowercase();
            if Module32FirstW(snap, &mut me).is_err() {
                let _ = CloseHandle(snap);
                return None;
            }
            loop {
                let end = me.szModule.iter().position(|c| *c == 0).unwrap_or(256);
                let name = String::from_utf16_lossy(&me.szModule[..end]);
                if name.to_lowercase() == lower {
                    let _ = CloseHandle(snap);
                    return Some(me.modBaseAddr as usize);
                }
                if Module32NextW(snap, &mut me).is_err() {
                    break;
                }
            }
            let _ = CloseHandle(snap);
            None
        }
    }

    /// 同架构：本地 LoadLibrary + GetProcAddress 求 RVA，远端基址 + RVA。
    fn remote_export_same_arch(
        dll_path: &std::path::Path,
        dll_name: &str,
        pid: u32,
        export: &str,
    ) -> anyhow::Result<usize> {
        unsafe {
            let hs_path = HSTRING::from(dll_path.to_string_lossy().as_ref());
            let local = LoadLibraryW(&hs_path)?;
            let hs_name = HSTRING::from(dll_name);
            let local_base = GetModuleHandleW(&hs_name)?.0 as usize;
            let name_nul = format!("{export}\0");
            let Some(local_fn) = GetProcAddress(local, PCSTR::from_raw(name_nul.as_ptr())) else {
                anyhow::bail!("本地导出缺失: {export}");
            };
            let rva = (local_fn as usize) - local_base;
            let remote_base = remote_module_base(pid, dll_name)
                .ok_or_else(|| anyhow::anyhow!("远端模块未加载"))?;
            Ok(remote_base + rva)
        }
    }

    /// 跨架构（64→32）：读远端 PE 导出表定位（WPF `FindExportInRemoteModule`），
    /// 含 x86 `__stdcall` 修饰名 `_Name@4` 回退。
    fn remote_export_cross_arch(
        hproc: HANDLE,
        remote_base: usize,
        export: &str,
    ) -> anyhow::Result<usize> {
        unsafe {
            let read_at = |rva: usize, n: usize| -> anyhow::Result<Vec<u8>> {
                let mut b = vec![0u8; n];
                let mut r = 0usize;
                ReadProcessMemory(
                    hproc,
                    (remote_base + rva) as *const _,
                    b.as_mut_ptr() as *mut _,
                    n,
                    Some(&mut r),
                )?;
                Ok(b)
            };
            let head = read_at(0, 0x1000)?;
            let e_lfanew = u32::from_le_bytes(head[0x3C..0x40].try_into().unwrap()) as usize;
            let magic =
                u16::from_le_bytes(head[e_lfanew + 0x18..e_lfanew + 0x1A].try_into().unwrap());
            let exp_off = if magic == 0x20B { e_lfanew + 0x88 } else { e_lfanew + 0x78 };
            let exp_rva =
                u32::from_le_bytes(head[exp_off..exp_off + 4].try_into().unwrap()) as usize;
            let exp = read_at(exp_rva, 40)?;
            let num_names = u32::from_le_bytes(exp[0x18..0x1C].try_into().unwrap()) as usize;
            let addr_names = u32::from_le_bytes(exp[0x20..0x24].try_into().unwrap()) as usize;
            let addr_ord = u32::from_le_bytes(exp[0x24..0x28].try_into().unwrap()) as usize;
            let addr_funcs = u32::from_le_bytes(exp[0x1C..0x20].try_into().unwrap()) as usize;
            let names = read_at(addr_names, num_names * 4)?;
            let ords = read_at(addr_ord, num_names * 2)?;
            for i in 0..num_names {
                let nrva =
                    u32::from_le_bytes(names[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
                let nb = read_at(nrva, export.len() + 16)?;
                let end = nb.iter().position(|b| *b == 0).unwrap_or(nb.len());
                let name = String::from_utf8_lossy(&nb[..end]).to_string();
                if name == export || name == format!("_{export}@4") {
                    let ord =
                        u16::from_le_bytes(ords[i * 2..i * 2 + 2].try_into().unwrap()) as usize;
                    let fb = read_at(addr_funcs + ord * 4, 4)?;
                    let frva = u32::from_le_bytes(fb[..4].try_into().unwrap()) as usize;
                    return Ok(remote_base + frva);
                }
            }
            anyhow::bail!("远端导出未找到: {export}")
        }
    }

    struct Proc(HANDLE);
    impl Drop for Proc {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// 打开目标进程 + 架构判定 + DLL 路径（32→64 直接拒绝）。
    fn open_target(dialog: HWND) -> anyhow::Result<(Proc, u32, bool, std::path::PathBuf, String)> {
        unsafe {
            let mut pid = 0u32;
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                dialog,
                Some(&mut pid),
            );
            if pid == 0 {
                anyhow::bail!("pid=0");
            }
            let access = PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_READ
                | PROCESS_VM_WRITE;
            let h = OpenProcess(access, false, pid)?;
            let is64 = target_is_64(h)?;
            if !is64 && cfg!(target_pointer_width = "32") {
                // 32 位注入器 → 64 位目标不支持（需 Heaven's Gate）。
                anyhow::bail!("32→64 注入不支持");
            }
            let name = dll_for_target(is64).to_string();
            let full = dll_full_path(&name)
                .ok_or_else(|| anyhow::anyhow!("缺失 DLL: {name}（与 clipx.exe 同目录）"))?;
            Ok((Proc(h), pid, is64, full, name))
        }
    }

    type ThreadStart = unsafe extern "system" fn(*mut std::ffi::c_void) -> u32;

    /// 远端调用一次：写 payload → CreateRemoteThread → 等待 → 读回（读模式）。
    fn remote_call(
        dialog_isize: isize,
        path_or_empty: &str,
        export: &str,
        want_readback: bool,
    ) -> anyhow::Result<(u32, String)> {
        unsafe {
            let dialog = HWND(dialog_isize as *mut _);
            if !IsWindow(Some(dialog)).as_bool() {
                anyhow::bail!("窗口无效");
            }
            let (proc, pid, is64, dll_full, dll_name) = open_target(dialog)?;
            let hproc = proc.0;
            // 确保远端已加载 DLL（LoadLibraryW 远线程；已加载时引用计数+1，无害）。
            let k32 = GetModuleHandleW(windows::core::w!("kernel32.dll"))?;
            let Some(load) = GetProcAddress(k32, PCSTR::from_raw(b"LoadLibraryW\0".as_ptr()))
            else {
                anyhow::bail!("LoadLibraryW 缺失");
            };
            let wpath: Vec<u16> = dll_full
                .to_string_lossy()
                .encode_utf16()
                .chain([0])
                .collect();
            let rp = VirtualAllocEx(
                hproc,
                None,
                wpath.len() * 2,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            if rp.is_null() {
                anyhow::bail!("VirtualAllocEx 失败");
            }
            let mut written = 0usize;
            WriteProcessMemory(
                hproc,
                rp,
                wpath.as_ptr() as *const _,
                wpath.len() * 2,
                Some(&mut written),
            )?;
            let start: ThreadStart = std::mem::transmute(load as usize);
            let mut _tid = 0u32;
            let th = CreateRemoteThread(hproc, None, 0, Some(start), Some(rp), 0, Some(&mut _tid))?;
            WaitForSingleObject(th, 10000);
            let _ = CloseHandle(th);
            VirtualFreeEx(hproc, rp, 0, MEM_RELEASE)?;

            // 导出地址。
            let remote_base = remote_module_base(pid, &dll_name)
                .ok_or_else(|| anyhow::anyhow!("远端模块未找到"))?;
            let self64 = cfg!(target_pointer_width = "64");
            let rfn = if self64 == is64 {
                remote_export_same_arch(&dll_full, &dll_name, pid, export)?
            } else if self64 && !is64 {
                remote_export_cross_arch(hproc, remote_base, export)?
            } else {
                anyhow::bail!("32→64 注入不支持");
            };

            // payload。
            let payload_bytes: Vec<u8> = if is64 {
                let p = Payload64 {
                    hwnd: dialog_isize as u64,
                    path: super::encode_path_w(path_or_empty).unwrap_or([0u16; 520]),
                };
                std::slice::from_raw_parts(
                    &p as *const _ as *const u8,
                    std::mem::size_of::<Payload64>(),
                )
                .to_vec()
            } else {
                let p = Payload32 {
                    hwnd: dialog_isize as u32,
                    path: super::encode_path_w(path_or_empty).unwrap_or([0u16; 520]),
                };
                std::slice::from_raw_parts(
                    &p as *const _ as *const u8,
                    std::mem::size_of::<Payload32>(),
                )
                .to_vec()
            };
            let rp2 = VirtualAllocEx(
                hproc,
                None,
                payload_bytes.len(),
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            if rp2.is_null() {
                anyhow::bail!("VirtualAllocEx payload 失败");
            }
            let mut w2 = 0usize;
            WriteProcessMemory(
                hproc,
                rp2,
                payload_bytes.as_ptr() as *const _,
                payload_bytes.len(),
                Some(&mut w2),
            )?;

            // 退避重试：busy/E_FAIL 重拉远线程（DLL 常驻远端）。
            let start2: ThreadStart = std::mem::transmute(rfn);
            let mut last: u32 = EXIT_E_FAIL;
            for (i, delay) in super::RETRY_DELAYS_MS.iter().enumerate() {
                if i > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(*delay));
                }
                let mut tid2 = 0u32;
                let Ok(th2) =
                    CreateRemoteThread(hproc, None, 0, Some(start2), Some(rp2), 0, Some(&mut tid2))
                else {
                    continue;
                };
                WaitForSingleObject(th2, 10000);
                let mut code = 0u32;
                let _ = GetExitCodeThread(th2, &mut code);
                let _ = CloseHandle(th2);
                last = code;
                if code != EXIT_BUSY && code != EXIT_E_FAIL {
                    break;
                }
            }
            let mut back = String::new();
            if want_readback && last == 0 {
                let mut rb = vec![0u8; payload_bytes.len()];
                let mut r = 0usize;
                if ReadProcessMemory(hproc, rp2, rb.as_mut_ptr() as *mut _, rb.len(), Some(&mut r))
                    .is_ok()
                {
                    let off = if is64 { 8 } else { 4 };
                    let cells = (rb.len() - off) / 2;
                    let mut wlen = cells.min(520);
                    for i in 0..cells.min(520) {
                        if rb[off + i * 2] == 0 && rb[off + i * 2 + 1] == 0 {
                            wlen = i;
                            break;
                        }
                    }
                    let w16: Vec<u16> = (0..wlen)
                        .map(|i| {
                            u16::from_le_bytes([rb[off + i * 2], rb[off + i * 2 + 1]])
                        })
                        .collect();
                    back = String::from_utf16_lossy(&w16);
                }
            }
            VirtualFreeEx(hproc, rp2, 0, MEM_RELEASE)?;
            Ok((last, back))
        }
    }

    /// 注入式导航（单次结果；调用方负责 80ms 后回读校验 + 键盘回退）。
    pub fn browse_object_inject(dialog_isize: isize, folder: &str) -> bool {
        if !std::path::Path::new(folder).is_dir() {
            return false;
        }
        matches!(remote_call(dialog_isize, folder, EXPORT_NAV, false), Ok((0, _)))
    }

    /// 注入式回读当前文件夹。
    pub fn read_current_folder(dialog_isize: isize) -> anyhow::Result<String> {
        match remote_call(dialog_isize, "", EXPORT_READ, true) {
            Ok((0, back)) if !back.is_empty() => Ok(back),
            Ok((code, _)) => anyhow::bail!("回读失败 exit={code:#X}"),
            Err(e) => Err(e),
        }
    }

    // ================= 键盘回退 =================

    fn key(vk: u16, up: bool) {
        unsafe {
            let inp = INPUT {
                r#type: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(vk),
                        wScan: 0,
                        dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[inp], std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn type_text_unicode(s: &str) {
        unsafe {
            let mut inputs: Vec<INPUT> = Vec::with_capacity(s.len() * 2);
            for c in s.encode_utf16() {
                for up in [false, true] {
                    inputs.push(INPUT {
                        r#type: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                                wScan: c,
                                dwFlags: if up {
                                    KEYEVENTF_KEYUP | KEYEVENTF_UNICODE
                                } else {
                                    KEYEVENTF_UNICODE
                                },
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    });
                }
            }
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn press_enter() {
        key(0x0D, false);
        std::thread::sleep(std::time::Duration::from_millis(30));
        key(0x0D, true);
    }

    fn ctrl_key(vk: u16) {
        key(VK_CONTROL.0, false);
        key(vk, false);
        key(vk, true);
        key(VK_CONTROL.0, true);
    }

    /// 地址栏式导航：聚焦对话框 → Alt+D → 全选 → 键入路径 → Enter → 校验。
    /// WPS 链：Alt+D 失败时追加 Ctrl+L（WPF `TryNavigateWpsCustom` 简化版）。
    pub fn navigate_keyboard(dialog_isize: isize, kind: DialogKind, folder: &str) -> bool {
        let dialog = HWND(dialog_isize as *mut _);
        unsafe {
            if !IsWindow(Some(dialog)).as_bool() {
                return false;
            }
            let _ = SetForegroundWindow(dialog);
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
        // Alt+D。
        key(VK_LMENU.0, false);
        key(0x44, false);
        key(0x44, true);
        key(VK_LMENU.0, true);
        std::thread::sleep(std::time::Duration::from_millis(120));
        ctrl_key(0x41); // Ctrl+A 全选覆盖旧地址
        std::thread::sleep(std::time::Duration::from_millis(40));
        type_text_unicode(folder);
        std::thread::sleep(std::time::Duration::from_millis(60));
        press_enter();
        std::thread::sleep(std::time::Duration::from_millis(200));
        if super::paths_equal(
            &read_current_folder(dialog_isize).unwrap_or_default(),
            folder,
        ) {
            return true;
        }
        // WPS/顽固框：Ctrl+L 再试一次。
        if matches!(kind, DialogKind::Wps | DialogKind::Custom) {
            ctrl_key(0x4C);
            std::thread::sleep(std::time::Duration::from_millis(120));
            ctrl_key(0x41);
            type_text_unicode(folder);
            std::thread::sleep(std::time::Duration::from_millis(60));
            press_enter();
            std::thread::sleep(std::time::Duration::from_millis(200));
            return super::paths_equal(
                &read_current_folder(dialog_isize).unwrap_or_default(),
                folder,
            );
        }
        false
    }

    /// 无对话框时在资源管理器中打开路径（全局 Ctrl+G 回退行为）。
    pub fn open_in_explorer(folder: &str) -> bool {
        std::process::Command::new("explorer")
            .arg(folder)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    }
}

/// payload 路径槽编码（UTF-16，520 留 1 个 0 位）。
#[cfg_attr(not(windows), allow(dead_code))]
fn encode_path_w(s: &str) -> Option<[u16; 520]> {
    let w: Vec<u16> = s.encode_utf16().collect();
    if w.len() >= 520 {
        return None;
    }
    let mut arr = [0u16; 520];
    arr[..w.len()].copy_from_slice(&w);
    Some(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_matches_wpf_rules() {
        assert_eq!(plan_for(DialogKind::System), NavigatePlan::InjectThenFallback);
        assert_eq!(plan_for(DialogKind::Wps), NavigatePlan::SimulateOnly);
        assert_eq!(plan_for(DialogKind::NotDialog), NavigatePlan::Skip);
    }

    #[test]
    fn retry_delays_match_source() {
        assert_eq!(RETRY_DELAYS_MS, &[0, 150, 300, 500, 800, 1200]);
    }

    #[test]
    fn normalize_strips_unc_prefix() {
        // 不存在的目录直接 Err。
        assert!(normalize_for_nav("C:\\definitely\\not\\here\\zzz").is_err());
        // 现存目录：temp 可解析。
        let tmp = std::env::temp_dir().to_string_lossy().to_string();
        let out = normalize_for_nav(&tmp).unwrap();
        assert!(!out.starts_with("\\\\?\\"));
    }

    #[test]
    fn paths_equal_loose() {
        assert!(paths_equal("C:\\A\\", "c:\\a"));
        assert!(!paths_equal("C:\\A", "C:\\B"));
        assert!(!paths_equal("", "C:\\B"));
    }

    #[test]
    fn payload_encodes_utf16() {
        let a = encode_path_w("C:\\测试").unwrap();
        assert_eq!(a[0], 'C' as u16);
        assert!(encode_path_w(&"x".repeat(600)).is_none());
    }
}
