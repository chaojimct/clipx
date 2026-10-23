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

/// 诊断日志（复用 tsf_inject 同款 %TEMP%\clipx_inject.log）。
#[cfg(windows)]
fn flog(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("clipx_inject.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let ts = clipx_core::time::hms_now();
        let _ = writeln!(f, "[fj {ts}] {msg}");
    }
}

#[cfg(not(windows))]
fn flog(_msg: &str) {}

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
        let cls = crate::dialog::win::class_of(windows::Win32::Foundation::HWND(
            dialog_hwnd as *mut _,
        ));
        let is_32770 = cls == "#32770";
        flog(&format!(
            "nav: cls={cls} kind={kind:?} inject={allow_inject} 32770={is_32770} path={path}"
        ));
        if win::read_current_folder(dialog_hwnd)
            .map(|cur| paths_equal(&cur, &path))
            .unwrap_or(false)
        {
            flog("nav: already-at-target");
            return Ok(true);
        }
        if allow_inject && is_32770 && matches!(kind, DialogKind::System) {
            let inj = win::browse_object_inject(dialog_hwnd, &path);
            flog(&format!("nav: inject result={inj}"));
            if inj {
                return Ok(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(80));
            if win::read_current_folder(dialog_hwnd)
                .map(|cur| paths_equal(&cur, &path))
                .unwrap_or(false)
            {
                flog("nav: inject-readback-ok");
                return Ok(true);
            }
        }
        if matches!(kind, DialogKind::NotDialog) {
            flog("nav: skip (NotDialog)");
            return Ok(false);
        }
        flog("nav: keyboard-fallback");
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

/// 选文件对话框里**定位文件**：跳到其父文件夹 + 把文件名写进「文件名」框。
///
/// 文件名框文本变化会让列表视图自动选中同名项（系统打开/保存框的标准行为），
/// 用户随后按 Enter / 点「打开」即选中该文件。文件不存在则报错。
pub fn locate_file_in_dialog(
    dialog_hwnd: isize,
    kind: DialogKind,
    file_path: &str,
    allow_inject: bool,
) -> anyhow::Result<bool> {
    let p = std::path::Path::new(file_path.trim().trim_matches('"'));
    if !p.is_file() {
        anyhow::bail!("文件不存在: {file_path}");
    }
    let parent = p
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("无父目录: {file_path}"))?;
    // 1) 先像普通跳转一样落到父文件夹。
    let mut ok = navigate_to_folder(dialog_hwnd, kind, &parent, allow_inject)?;
    // 2) 文件名框写入文件名 → 列表选中该项。
    let file_name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !file_name.is_empty() {
        #[cfg(windows)]
        {
            let set = set_filename_in_dialog(dialog_hwnd, &file_name);
            flog(&format!(
                "locate: nav={ok} set_filename={set} name={file_name}"
            ));
            if set {
                ok = true;
            } else {
                // 没写进文件名框就别报成功——否则 UI 显示「已定位」但对话框毫无变化。
                ok = false;
            }
        }
        #[cfg(not(windows))]
        let _ = &file_name;
    }
    Ok(ok)
}

/// 把文件名写进「文件名」输入框（选文件对话框）。供上层在 UIA 选中失败时兜底。
pub fn set_filename_in_dialog(dialog_hwnd: isize, file_name: &str) -> bool {
    #[cfg(windows)]
    {
        win::set_filename_edit(dialog_hwnd, file_name)
    }
    #[cfg(not(windows))]
    {
        let _ = (dialog_hwnd, file_name);
        false
    }
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
    use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM};
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
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, FindWindowExW, GetClassNameW, GetDlgItem, GetParent, IsWindow,
        IsWindowVisible, SetForegroundWindow, SetWindowTextW,
    };
    use windows::core::{w, HSTRING, PCSTR, PCWSTR};

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
        remote_base: usize,
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
            // 诊断：进程名 + 位数，定位「到底注的是谁」。
            let pname = std::path::Path::new(&exe_name_of(pid))
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "?".into());
            super::flog(&format!(
                "open_target: pid={pid} name={pname} is64={is64}"
            ));
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

    /// 进程主模块路径（QueryFullProcessImageNameW）。
    fn exe_name_of(pid: u32) -> String {
        unsafe {
            use windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return "?".into();
            };
            let mut buf = [0u16; 1024];
            let mut sz = buf.len() as u32;
            use windows::core::PWSTR;
            use windows::Win32::System::Threading::PROCESS_NAME_FORMAT;
            let ok = windows::Win32::System::Threading::QueryFullProcessImageNameW(
                h,
                PROCESS_NAME_FORMAT(0),
                PWSTR(buf.as_mut_ptr()),
                &mut sz,
            )
            .is_ok();
            let _ = CloseHandle(h);
            if ok {
                String::from_utf16_lossy(&buf[..sz as usize])
            } else {
                "?".into()
            }
        }
    }

    type ThreadStart = unsafe extern "system" fn(*mut std::ffi::c_void) -> u32;

    /// `LoadLibraryW` 在**目标进程**里的可执行地址。
    ///
    /// - x64 → x64：本进程 kernel32 的地址直接可用。
    /// - x64 → x86（企业微信 `WXWork.exe` 就是 32 位宿主）：本进程拿到的是
    ///   **64 位 kernel32** 的地址。把这个地址当远线程入口投进 32 位进程，
    ///   线程会跳到无效地址、立刻结束，`GetExitCodeThread` 读回 0。
    ///   这正是日志里 `LoadLibraryW wait=WAIT_EVENT(0) ret=0x0` 后紧接
    ///   `browse_object_inject: err=LoadLibraryW 远端失败 ret=0` 的成因。
    ///
    ///   正确做法：目标进程里 32 位 `kernel32` 的基址 + `SysWOW64\kernel32.dll`
    ///   里 `LoadLibraryW` 的导出 RVA。
    fn loadlibrary_in_target(hproc: HANDLE, is64: bool) -> anyhow::Result<usize> {
        unsafe {
            if is64 {
                let k32 = GetModuleHandleW(windows::core::w!("kernel32.dll"))?;
                let f = GetProcAddress(k32, PCSTR::from_raw(b"LoadLibraryW\0".as_ptr()))
                    .ok_or_else(|| anyhow::anyhow!("LoadLibraryW 缺失"))?;
                return Ok(f as usize);
            }
        }
        let base = wow64_kernel32_base(hproc)
            .ok_or_else(|| anyhow::anyhow!("目标进程里找不到 32 位 kernel32"))?;
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let path = std::path::Path::new(&root).join("SysWOW64").join("kernel32.dll");
        let rva = pe_export_rva(&path, "LoadLibraryW")
            .ok_or_else(|| anyhow::anyhow!("解析 SysWOW64/kernel32.dll 导出失败"))?;
        super::flog(&format!(
            "loadlib32: base={base:#X} rva={rva:#X} addr={:#X}",
            base + rva
        ));
        Ok(base + rva)
    }

    /// WOW64 进程里 32 位 `kernel32.dll` 的基址。
    ///
    /// 64 位枚举器只能用 `LIST_MODULES_32BIT` 才看得到 32 位模块
    /// （默认 filter 会回 32 位目标的 32 位模块，但显式指定更稳）。
    /// 返回的是 32 位地址空间里的基址（低地址），可直接给远线程。
    fn wow64_kernel32_base(hproc: HANDLE) -> Option<usize> {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::System::ProcessStatus::{
            EnumProcessModulesEx, GetModuleBaseNameW, LIST_MODULES_32BIT,
        };
        unsafe {
            let mut mods = [HMODULE::default(); 512];
            let mut needed = 0u32;
            let cb = (mods.len() * std::mem::size_of::<HMODULE>()) as u32;
            EnumProcessModulesEx(
                hproc,
                mods.as_mut_ptr(),
                cb,
                &mut needed,
                LIST_MODULES_32BIT,
            )
            .ok()?;
            let sz = std::mem::size_of::<HMODULE>();
            let n = ((needed as usize) / sz).min(mods.len()).max(1);
            for m in mods.iter().take(n) {
                if m.is_invalid() {
                    continue;
                }
                let mut buf = [0u16; 260];
                let len = GetModuleBaseNameW(hproc, Some(*m), &mut buf);
                if len == 0 {
                    continue;
                }
                let name = String::from_utf16_lossy(&buf[..len as usize]).to_ascii_lowercase();
                if name == "kernel32.dll" {
                    return Some(m.0 as usize);
                }
            }
            None
        }
    }

    /// 从 PE 文件的导出表取函数 RVA（转发导出返回 None，避免把数据区当地址调）。
    ///
    /// 只用来读 `SysWOW64\kernel32.dll` —— 本进程加载不了 32 位 DLL，
    /// 只能靠静态解析拿「同一个 Windows 版本里一致」的 RVA。
    fn pe_export_rva(path: &std::path::Path, func: &str) -> Option<usize> {
        let d = std::fs::read(path).ok()?;
        let at_u16 = |o: usize| -> Option<u16> {
            Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
        };
        let at_u32 = |o: usize| -> Option<u32> {
            Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
        };
        let e_lfanew = at_u32(0x3C)? as usize;
        if d.get(e_lfanew..e_lfanew + 4)? != b"PE\0\0" {
            return None;
        }
        let coff = e_lfanew + 4;
        let num_sec = at_u16(coff + 2)? as usize;
        let opt_size = at_u16(coff + 16)? as usize;
        let opt = coff + 20;
        let magic = at_u16(opt)?;
        let dd = opt + if magic == 0x10B { 96 } else { 112 };
        let exp_rva = at_u32(dd)? as usize;
        let exp_size = at_u32(dd + 4)? as usize;
        if exp_rva == 0 {
            return None;
        }
        let sec = opt + opt_size;
        let rva2off = |rva: usize| -> Option<usize> {
            for i in 0..num_sec {
                let s = sec + i * 40;
                let vsize = at_u32(s + 8)? as usize;
                let va = at_u32(s + 12)? as usize;
                let rsize = at_u32(s + 16)? as usize;
                let raw = at_u32(s + 20)? as usize;
                let span = vsize.max(rsize);
                if rva >= va && rva < va + span {
                    return Some(raw + (rva - va));
                }
            }
            None
        };
        let e = rva2off(exp_rva)?;
        let n_names = at_u32(e + 24)? as usize;
        let addr_fns = at_u32(e + 28)? as usize;
        let addr_names = at_u32(e + 32)? as usize;
        let addr_ords = at_u32(e + 36)? as usize;
        let names = rva2off(addr_names)?;
        let ords = rva2off(addr_ords)?;
        let fns = rva2off(addr_fns)?;
        for i in 0..n_names {
            let nrva = at_u32(names + i * 4)? as usize;
            let no = rva2off(nrva)?;
            let end = no + d.get(no..)?.iter().position(|&b| b == 0)?;
            if &d[no..end] != func.as_bytes() {
                continue;
            }
            let ord = at_u16(ords + i * 2)? as usize;
            let frva = at_u32(fns + ord * 4)? as usize;
            // 转发导出：RVA 落在导出表自身范围内，指向的是 "DLL.Func" 字符串。
            if frva >= exp_rva && frva < exp_rva + exp_size {
                return None;
            }
            return Some(frva);
        }
        None
    }

    /// 单测入口（`pe_export_rva` 是模块私有）。
    #[cfg(test)]
    pub(crate) fn pe_export_rva_for_test(
        path: &std::path::Path,
        func: &str,
    ) -> Option<usize> {
        pe_export_rva(path, func)
    }

    /// 诊断入口：把导航 DLL 远程加载进指定 pid，返回 `(目标是否 64 位, 远线程退出码)`。
    /// 退出码非 0 = `LoadLibraryW` 成功。用来验证 x64→x86 地址换算没走错，
    /// 不需要打开任何文件对话框。
    pub fn remote_load_dll_probe(pid: u32) -> anyhow::Result<(bool, u32)> {        unsafe {
            let access = PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_READ
                | PROCESS_VM_WRITE;
            let h = OpenProcess(access, false, pid)?;
            let is64 = target_is_64(h)?;
            let name = dll_for_target(is64);
            let full =
                dll_full_path(name).ok_or_else(|| anyhow::anyhow!("缺失 DLL: {name}"))?;
            let addr = loadlibrary_in_target(h, is64)?;
            let wpath: Vec<u16> = full.to_string_lossy().encode_utf16().chain([0]).collect();
            let rp = VirtualAllocEx(
                h,
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
                h,
                rp,
                wpath.as_ptr() as *const _,
                wpath.len() * 2,
                Some(&mut written),
            )?;
            let start: ThreadStart = std::mem::transmute(addr);
            let mut tid = 0u32;
            let th = CreateRemoteThread(h, None, 0, Some(start), Some(rp), 0, Some(&mut tid))?;
            WaitForSingleObject(th, 10000);
            let mut code = 0u32;
            let _ = GetExitCodeThread(th, &mut code);
            let _ = CloseHandle(th);
            let _ = VirtualFreeEx(h, rp, 0, MEM_RELEASE);
            let _ = CloseHandle(h);
            Ok((is64, code))
        }
    }

    /// 信标 DLL 名字（按目标位数）。`pub(super)` 仅为单测可见。
    pub(super) fn beacon_dll_name(target_is64: bool) -> &'static str {
        if target_is64 {
            "clipx_caret_beacon.dll"
        } else {
            "clipx_caret_beacon32.dll"
        }
    }

    /// 信标 DLL 路径：exe 同目录（安装态）→ 仓库 target/release（开发态）→
    /// i686 构建目录（32 位开发态）。
    fn beacon_dll_path(name: &str) -> Option<std::path::PathBuf> {
        let mut cands = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                cands.push(dir.join(name));
            }
        }
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        if name.contains("32") {
            cands.push(root.join("target/i686-pc-windows-msvc/release").join(name));
            cands.push(root.join("target/release").join(name));
        } else {
            cands.push(root.join("target/release").join(name));
        }
        cands.into_iter().find(|p| p.is_file())
    }

    /// 把**光标信标 DLL** 注入目标进程，并远程调用 `ClipxBeaconStart`，
    /// 让它在宿主进程内给所有线程自装 `WH_GETMESSAGE` 钩子。
    ///
    /// 为什么需要这条路：`SetWindowsHookEx` 只能注入**同位数** DLL，x64 的
    /// clipx.exe 装不进 32 位宿主（企业微信 / WPS / 腾讯会议…）。注入 x86 信标
    /// 后由它自己装钩子，位数天然一致。
    ///
    /// 返回：已挂上的钩子数（`0` = 失败）。
    pub fn inject_caret_beacon(pid: u32) -> anyhow::Result<u32> {
        unsafe {
            let access = PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_READ
                | PROCESS_VM_WRITE;
            let h = OpenProcess(access, false, pid)?;
            if h.is_invalid() {
                anyhow::bail!("OpenProcess({pid}) 失败");
            }
            let result = (|| -> anyhow::Result<u32> {
                let is64 = target_is_64(h)?;
                let name = beacon_dll_name(is64);
                let full = beacon_dll_path(name)
                    .ok_or_else(|| anyhow::anyhow!("缺失信标 DLL: {name}"))?;
                super::flog(&format!(
                    "inject_beacon: pid={pid} is64={is64} dll={}",
                    full.display()
                ));

                // ① 远端加载 DLL（地址按目标架构取，见 loadlibrary_in_target）。
                let load_addr = loadlibrary_in_target(h, is64)?;
                let wpath: Vec<u16> =
                    full.to_string_lossy().encode_utf16().chain([0]).collect();
                let rp = VirtualAllocEx(
                    h,
                    None,
                    wpath.len() * 2,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                );
                if rp.is_null() {
                    anyhow::bail!("VirtualAllocEx 失败");
                }
                let mut written = 0usize;
                let write_res = WriteProcessMemory(
                    h,
                    rp,
                    wpath.as_ptr() as *const _,
                    wpath.len() * 2,
                    Some(&mut written),
                );
                let cleanup = |rp: *mut std::ffi::c_void| {
                    let _ = VirtualFreeEx(h, rp, 0, MEM_RELEASE);
                };
                if write_res.is_err() {
                    cleanup(rp);
                    anyhow::bail!("WriteProcessMemory 失败");
                }
                let start: ThreadStart = std::mem::transmute(load_addr);
                let mut tid = 0u32;
                let th = match CreateRemoteThread(h, None, 0, Some(start), Some(rp), 0, Some(&mut tid)) {
                    Ok(t) => t,
                    Err(e) => {
                        cleanup(rp);
                        anyhow::bail!("CreateRemoteThread(LoadLibrary) 失败: {e}");
                    }
                };
                WaitForSingleObject(th, 10000);
                let mut load_ret = 0u32;
                let _ = GetExitCodeThread(th, &mut load_ret);
                let _ = CloseHandle(th);
                cleanup(rp);
                if load_ret == 0 {
                    anyhow::bail!("信标 DLL 远端加载失败（LoadLibraryW ret=0）");
                }
                super::flog(&format!("inject_beacon: loaded ret={load_ret:#X}"));

                // ② 定位远端模块基址（LoadLibrary 退出码在 64 位下被截断成 u32，只能查模块表）。
                let mut remote_base = 0usize;
                for attempt in 0..10 {
                    if let Some(b) = remote_module_base(pid, name) {
                        remote_base = b;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    if attempt == 9 {
                        anyhow::bail!("远端信标模块未找到（已重试 10 次）");
                    }
                }

                // ③ 解析 `ClipxBeaconStart` 地址并按目标架构调用。
                let self64 = cfg!(target_pointer_width = "64");
                let rfn = if self64 == is64 {
                    remote_export_same_arch(&full, name, remote_base, "ClipxBeaconStart")?
                } else if self64 && !is64 {
                    remote_export_cross_arch(h, remote_base, "ClipxBeaconStart")?
                } else {
                    anyhow::bail!("32→64 注入不支持");
                };
                super::flog(&format!(
                    "inject_beacon: base={remote_base:#X} fn={rfn:#X}"
                ));

                let start2: ThreadStart = std::mem::transmute(rfn);
                let mut tid2 = 0u32;
                let th2 = CreateRemoteThread(h, None, 0, Some(start2), None, 0, Some(&mut tid2))?;
                WaitForSingleObject(th2, 10000);
                let mut hooks = 0u32;
                let _ = GetExitCodeThread(th2, &mut hooks);
                let _ = CloseHandle(th2);
                super::flog(&format!("inject_beacon: ClipxBeaconStart hooks={hooks}"));
                Ok(hooks)
            })();
            let _ = CloseHandle(h);
            result
        }
    }

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
            // 注意：地址必须按目标架构取 —— 32 位目标用本进程 64 位 kernel32 的
            // 地址会静默返回 0（企业微信上传框跳不过去的根因）。
            let load_addr = loadlibrary_in_target(hproc, is64)?;
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
            let start: ThreadStart = std::mem::transmute(load_addr);
            let mut _tid = 0u32;
            let th = CreateRemoteThread(hproc, None, 0, Some(start), Some(rp), 0, Some(&mut _tid))?;
            let wait = WaitForSingleObject(th, 10000);
            let mut load_ret = 0u32;
            let _ = GetExitCodeThread(th, &mut load_ret);
            let _ = CloseHandle(th);
            VirtualFreeEx(hproc, rp, 0, MEM_RELEASE)?;
            super::flog(&format!(
                "remote_call: LoadLibraryW wait={wait:?} ret={load_ret:#X} dll={}",
                dll_full.display()
            ));
            if load_ret == 0 {
                anyhow::bail!("LoadLibraryW 远端失败 ret=0");
            }

            // 导出地址。模块列表可能滞后，重试查找。
            let mut remote_base = 0usize;
            for attempt in 0..10 {
                if let Some(b) = remote_module_base(pid, &dll_name) {
                    remote_base = b;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                if attempt == 9 {
                    anyhow::bail!("远端模块未找到（已重试10次）");
                }
            }
            let self64 = cfg!(target_pointer_width = "64");
            let rfn = if self64 == is64 {
                remote_export_same_arch(&dll_full, &dll_name, remote_base, export)?
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
            super::flog("browse_object_inject: not-a-dir");
            return false;
        }
        match remote_call(dialog_isize, folder, EXPORT_NAV, false) {
            Ok((0, _)) => true,
            Ok((code, _)) => {
                super::flog(&format!("browse_object_inject: exit={code:#X}"));
                false
            }
            Err(e) => {
                super::flog(&format!("browse_object_inject: err={e}"));
                false
            }
        }
    }

    /// 注入式回读当前文件夹。
    pub fn read_current_folder(dialog_isize: isize) -> anyhow::Result<String> {
        match remote_call(dialog_isize, "", EXPORT_READ, true) {
            Ok((0, back)) if !back.is_empty() => Ok(back),
            Ok((code, _)) => anyhow::bail!("回读失败 exit={code:#X}"),
            Err(e) => Err(e),
        }
    }

    /// 把「文件名」输入框设为 `file_name`（列表视图会随之选中同名项）。
    ///
    /// #32770 打开/保存框的文件名控件：优先按标准 ID 取（cmb13=0x047C 的
    /// Edit 子控 / edt1=0x0480），找不到再按类名找第一个可见 ComboBox/Edit
    /// （排除地址栏——它在 ToolbarWindow32 之下）。
    pub fn set_filename_edit(dialog_isize: isize, file_name: &str) -> bool {
        let dialog = HWND(dialog_isize as *mut _);
        unsafe {
            if !IsWindow(Some(dialog)).as_bool() {
                return false;
            }
            // 1) 标准控件 ID：edt1(0x0480) / cmb13(0x047C)。
            let mut tried = Vec::new();
            for id in [0x0480i32, 0x047C] {
                match GetDlgItem(Some(dialog), id) {
                    Ok(ctrl) if !ctrl.0.is_null() => {
                        tried.push(format!("id={id:#X}:{}", set_text_deep(ctrl, file_name)));
                    }
                    _ => tried.push(format!("id={id:#X}:no-ctrl")),
                }
            }
            // 2) 类名兜底：找第一个不在工具栏里的 ComboBox/Edit。
            let hit = find_filename_like_edit(dialog);
            let fb = hit.is_some_and(|h| set_text_deep(h, file_name));
            super::flog(&format!(
                "set_filename_edit: {tried:?} fallback={fb} found={} name={file_name}",
                hit.is_some()
            ));
            fb
        }
    }

    /// 控件设文本：ComboBox 取其 Edit 子控件，其余直接 SetWindowTextW。
    fn set_text_deep(ctrl: HWND, text: &str) -> bool {
        unsafe {
            let mut cls = [0u16; 64];
            let n = GetClassNameW(ctrl, &mut cls);
            let cls = String::from_utf16_lossy(&cls[..n as usize]);
            let target = if cls == "ComboBox" {
                FindWindowExW(Some(ctrl), None, w!("Edit"), w!(""))
                    .unwrap_or(ctrl)
            } else {
                ctrl
            };
            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            SetWindowTextW(target, PCWSTR(wide.as_ptr())).is_ok()
        }
    }

    /// 枚举子控件找「文件名」样式输入框（ComboBox 或 Edit，
    /// 但地址栏的 ComboBox/Edit 挂在 ToolbarWindow32 之下，须排除）。
    fn find_filename_like_edit(dialog: HWND) -> Option<HWND> {
        struct Acc {
            hit: isize,
            dialog: isize,
        }
        unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
            let acc = &mut *(lp.0 as *mut Acc);
            if acc.hit != 0 {
                return BOOL(1);
            }
            let mut cls = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut cls);
            let cls = String::from_utf16_lossy(&cls[..n as usize]);
            if cls != "ComboBox" && cls != "Edit" {
                return BOOL(1);
            }
            // 跳过工具栏里的（地址栏）。
            let mut parent = GetParent(hwnd).unwrap_or(HWND::default());
            let mut in_toolbar = false;
            for _ in 0..8 {
                if parent.0.is_null() {
                    break;
                }
                let mut pcls = [0u16; 64];
                let pn = GetClassNameW(parent, &mut pcls);
                let pcls = String::from_utf16_lossy(&pcls[..pn as usize]);
                if pcls.contains("Toolbar") || pcls == "ReBarWindow32" {
                    in_toolbar = true;
                    break;
                }
                if parent.0 as isize == acc.dialog {
                    break;
                }
                parent = GetParent(parent).unwrap_or(HWND::default());
            }
            if !in_toolbar && IsWindowVisible(hwnd).as_bool() {
                acc.hit = hwnd.0 as isize;
                return BOOL(0); // 找到即停
            }
            BOOL(1)
        }
        let mut acc = Acc {
            hit: 0,
            dialog: dialog.0 as isize,
        };
        unsafe {
            let _ = EnumChildWindows(Some(dialog), Some(cb), LPARAM(&mut acc as *mut _ as isize));
        }
        (acc.hit != 0).then(|| HWND(acc.hit as *mut _))
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

    /// 信标 DLL 必须按目标位数选：32 位宿主加载 x64 信标会失败，
    /// 反之亦然（文件本身就叫 ...32.dll，安装包与 CI 都按这个名字分发）。
    #[test]
    #[cfg(windows)]
    fn beacon_dll_name_follows_target_bits() {
        assert_eq!(win::beacon_dll_name(true), "clipx_caret_beacon.dll");
        assert_eq!(win::beacon_dll_name(false), "clipx_caret_beacon32.dll");
        // 两个名字必须不同，否则 build_beacon.ps1 的改名归集会静默覆盖。
        assert_ne!(win::beacon_dll_name(true), win::beacon_dll_name(false));
    }

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

    /// SysWOW64 kernel32 的导出解析必须能拿到 LoadLibraryW 的合法 RVA。
    /// 这是 x64→x86 注入的唯一地址来源，解析错了就等于注入全废。
    #[cfg(windows)]
    #[test]
    fn syswow64_loadlibraryw_rva_parses() {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let p = std::path::Path::new(&root)
            .join("SysWOW64")
            .join("kernel32.dll");
        assert!(p.is_file(), "SysWOW64/kernel32.dll 不存在: {}", p.display());
        let rva = super::win::pe_export_rva_for_test(&p, "LoadLibraryW")
            .unwrap_or_else(|| panic!("解析 LoadLibraryW 导出失败: {}", p.display()));
        // 映像内偏移，且不会落在文件头。Win10/11 上实际约 0x1Dxxx 量级。
        assert!(rva > 0x1000 && rva < 0x200000, "可疑 RVA: {rva:#X}");
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
