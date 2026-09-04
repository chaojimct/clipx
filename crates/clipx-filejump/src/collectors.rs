//! 路径采集（M5b）：一档 Explorer COM / TC / XY / DOpus，二档 UIA 白名单。
//!
//! 基线 `FileManagerPathCollector.cs`（WPF v1.9.8）。纯逻辑跨平台可测，
//! Win32 协议实现在 `win` 子模块。

use crate::models::{Candidate, CandidateSource};

/// 路径归一化：去首尾空白与引号，统一 `\` 分隔，压掉末尾多余分隔符（保盘符根）。
pub fn normalize_path(raw: &str) -> Option<String> {
    let mut s = raw.trim().trim_matches('"').trim().to_string();
    if s.is_empty() {
        return None;
    }
    s = s.replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    if s.is_empty() {
        return None;
    }
    Some(s)
}

/// 安装目录排除（WPF `PushRecentFileDialogFolder` 规则）：安装器自身目录不学习。
pub fn is_install_dir(path_lower: &str) -> bool {
    path_lower.contains("\\clipboardx\\tools") || path_lower.contains("\\clipboardx\\out")
}

/// 多源合并去重：大小写不敏感去重，保首见顺序。
pub fn merge_candidates(groups: Vec<Vec<Candidate>>) -> Vec<Candidate> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for g in groups {
        for c in g {
            let key = c.path.to_lowercase();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(c);
        }
    }
    out
}

/// 当前可跳转路径快照（M5b）：Z 序遍历顶层窗口，按类名分发各采集器。
/// `gate` 用于 TC/XY 的剪贴板借道（arm 防自采，对齐 WPF `ClipboardGate`）。
pub fn collect(gate: Option<&clipx_core::ClipboardGate>) -> Vec<Candidate> {
    #[cfg(windows)]
    {
        win::collect(gate)
    }
    #[cfg(not(windows))]
    {
        let _ = gate;
        Vec::new()
    }
}

/// 对话框在 Z 序中向下偏移 `delta` 个窗口后取管理器路径
///（WPF `TryGetZOrderLinkedFolder`，默认 delta=2：对话框→宿主→文件管理器）。
pub fn zorder_linked_folder(
    dialog_hwnd: isize,
    gate: Option<&clipx_core::ClipboardGate>,
    delta: usize,
) -> Option<String> {
    #[cfg(windows)]
    {
        win::zorder_linked_folder(dialog_hwnd, gate, delta)
    }
    #[cfg(not(windows))]
    {
        let _ = (dialog_hwnd, gate, delta);
        None
    }
}

#[cfg(windows)]
pub mod win {
    //! Win32 采集器实现（M5b）。
    //!
    //! 协议基线 `FileManagerPathCollector.cs`（WPF v1.9.8）：
    //! - TC：类名 `TTOTAL_CMD`，`SendMessage(1075, 2029/2030)` 借剪贴板取源/目标路径
    //! - XY：类名 `ThunderRT6FormDC`，`WM_COPYDATA(0x400001, "::copytext get('path', a);")`
    //! - DOpus：类名 `dopus.lister`，同目录 `dopusrt.exe /info <tmp> paths` 解析 XML
    //! - Explorer：`CabinetWClass/ExploreWClass`，COM `Shell.Application.Windows` 枚举，
    //!   失败回退子控件 Edit 文本扫描
    use std::path::PathBuf;

    use clipx_core::ClipboardGate;
    use windows::Win32::Foundation::{HGLOBAL, HWND, LPARAM, WPARAM};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
        COPYDATASTRUCT,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, GetTopWindow, GetWindow, GetWindowTextW, IsWindow,
        IsWindowVisible, SendMessageTimeoutW, SendMessageW, GW_HWNDNEXT,
        SMTO_ABORTIFHUNG, WM_COPYDATA,
    };
    use crate::collectors::{is_install_dir, merge_candidates, normalize_path};
    use crate::models::{Candidate, CandidateSource};

    const TC_MSG: u32 = 1075;
    const TC_SRC: usize = 2029;
    const TC_TRG: usize = 2030;
    const XY_COPYDATA_ID: usize = 0x400001;
    const XY_SCRIPT: &str = "::copytext get('path', a);";

    fn labeled(label: &str, path: String) -> Candidate {
        Candidate {
            path,
            alias: Some(label.to_string()),
            source: CandidateSource::Manager,
        }
    }

    // ================= 顶层窗口 / 进程取证 =================

    pub fn top_level_zorder() -> Vec<isize> {
        unsafe {
            let mut out = Vec::new();
            let Ok(mut hwnd) = GetTopWindow(None) else {
                return out;
            };
            loop {
                if hwnd.0.is_null() {
                    break;
                }
                if IsWindow(Some(hwnd)).as_bool() && IsWindowVisible(hwnd).as_bool() {
                    out.push(hwnd.0 as isize);
                }
                match GetWindow(hwnd, GW_HWNDNEXT) {
                    Ok(next) => hwnd = next,
                    Err(_) => break,
                }
            }
            out
        }
    }

    pub fn class_of(hwnd_isize: isize) -> String {
        crate::dialog::win::class_of(HWND(hwnd_isize as *mut _))
    }

    /// 进程映像全路径（DOpus 找 dopusrt.exe 用）。
    pub fn exe_path_of(hwnd_isize: isize) -> Option<String> {
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let hwnd = HWND(hwnd_isize as *mut _);
            let mut pid = 0u32;
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                hwnd,
                Some(&mut pid),
            );
            if pid == 0 {
                return None;
            }
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                h,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            let _ = windows::Win32::Foundation::CloseHandle(h);
            if ok.is_err() {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        }
    }

    // ================= 剪贴板借道（TC/XY） =================

    fn clip_get_text() -> Option<String> {
        unsafe {
            for _ in 0..10 {
                if OpenClipboard(None).is_ok() {
                    let _ = CloseClipboard();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if OpenClipboard(None).is_err() {
                return None;
            }
            let text = (|| {
                let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
                if h.0.is_null() {
                    return None;
                }
                let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
                if p.is_null() {
                    return None;
                }
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
                let _ = GlobalUnlock(HGLOBAL(h.0));
                Some(s)
            })();
            let _ = CloseClipboard();
            text
        }
    }

    fn clip_set_text(s: &str) {
        unsafe {
            if OpenClipboard(None).is_err() {
                return;
            }
            let _ = EmptyClipboard();
            let wide: Vec<u16> = s.encode_utf16().chain([0]).collect();
            let bytes = wide.len() * 2;
            if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
                let p = GlobalLock(h) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                    let _ = GlobalUnlock(h);
                    use windows::Win32::Foundation::HANDLE;
                    if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0))).is_err() {
                        let _ = windows::Win32::Foundation::GlobalFree(Some(h));
                    }
                }
            }
            let _ = CloseClipboard();
        }
    }

    fn clip_clear() {
        unsafe {
            if OpenClipboard(None).is_ok() {
                let _ = EmptyClipboard();
                let _ = CloseClipboard();
            }
        }
    }

    // ================= Total Commander =================

    fn tc_path(hwnd: HWND, cmd: usize, gate: Option<&ClipboardGate>) -> Option<String> {
        if let Some(g) = gate {
            g.arm();
        }
        let backup = clip_get_text();
        clip_clear();
        unsafe {
            SendMessageW(hwnd, TC_MSG, Some(WPARAM(cmd)), Some(LPARAM(0)));
        }
        std::thread::sleep(std::time::Duration::from_millis(90));
        let got = clip_get_text().unwrap_or_default().trim().to_string();
        // 恢复用户剪贴板（WPF 同行为；恢复本身也会触发监听，gate 窗口内被抑制）
        if let Some(g) = gate {
            g.arm();
        }
        match backup {
            Some(b) => clip_set_text(&b),
            None => clip_clear(),
        }
        let norm = normalize_path(&got)?;
        if !std::path::Path::new(&norm).is_dir() {
            return None;
        }
        Some(norm)
    }

    // ================= XYplorer =================

    fn xy_path(hwnd: HWND, gate: Option<&ClipboardGate>) -> Option<String> {
        if let Some(g) = gate {
            g.arm();
        }
        clip_clear();
        let wide: Vec<u16> = XY_SCRIPT.encode_utf16().collect();
        unsafe {
            let cds = COPYDATASTRUCT {
                dwData: XY_COPYDATA_ID,
                cbData: (wide.len() * 2) as u32,
                lpData: wide.as_ptr() as *mut _,
            };
            SendMessageTimeoutW(
                hwnd,
                WM_COPYDATA,
                WPARAM(0),
                LPARAM(&cds as *const _ as isize),
                SMTO_ABORTIFHUNG,
                2000,
                None,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        let got = clip_get_text().unwrap_or_default().trim().to_string();
        let norm = normalize_path(&got)?;
        if !std::path::Path::new(&norm).is_dir() {
            return None;
        }
        Some(norm)
    }

    // ================= Directory Opus =================

    /// dopusrt 输出 XML 解析（纯逻辑，`lister="<hwnd>"...tab_state="1|2"...><path>DIR</path>`）。
    pub fn parse_dopus_paths(xml: &str, hwnd_ids: &[String]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (state, label) in [("1", "Directory Opus (活动)"), ("2", "Directory Opus (被动)")] {
            'id: for id in hwnd_ids {
                let mut search = 0usize;
                while let Some(li) = xml[search..].find(&format!("lister=\"{id}\"")) {
                    let base = search + li;
                    let seg = &xml[base..];
                    let seg_end = seg.find("</lister>").map(|i| base + i).unwrap_or(xml.len());
                    let seg = &xml[base..seg_end];
                    let mut p = 0usize;
                    while let Some(pi) = seg[p..].find("<path") {
                        let pb = p + pi;
                        let pe = seg[pb..].find('>').map(|i| pb + i).unwrap_or(seg.len());
                        let tag_end = pe.min(seg.len().saturating_sub(1));
                        let tag = &seg[pb..=tag_end];
                        let close = seg[pe..].find("</path>").map(|i| pe + i);
                        if tag.contains(&format!("tab_state=\"{state}\"")) {
                            if let Some(c) = close {
                                let val = seg[pe + 1..c].trim().to_string();
                                if !val.is_empty() && std::path::Path::new(&val).is_dir() {
                                    out.push((label.to_string(), val));
                                }
                                break 'id;
                            }
                        }
                        p = pe + 1;
                        if p >= seg.len() {
                            break;
                        }
                    }
                    search = base + 1;
                    if search >= xml.len() {
                        break;
                    }
                }
            }
        }
        out
    }

    fn dopus_paths_for(hwnd_isize: isize) -> Vec<Candidate> {
        let exe = match exe_path_of(hwnd_isize) {
            Some(e) => e,
            None => return Vec::new(),
        };
        let dir = match std::path::Path::new(&exe).parent() {
            Some(d) => d.to_path_buf(),
            None => return Vec::new(),
        };
        let rt = dir.join("dopusrt.exe");
        if !rt.is_file() {
            return Vec::new();
        }
        let tmp: PathBuf = std::env::temp_dir().join(format!(
            "clipx-dopus-{}-{}.xml",
            std::process::id(),
            exe.len()
        ));
        let mut child = match std::process::Command::new(&rt)
            .args(["/info", &tmp.to_string_lossy(), "paths"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        // 最长等 5s（WPF WaitForExit(5000)）。
        let mut waited = 0u32;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if waited >= 50 => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                Ok(None) => {
                    waited += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
        let xml = std::fs::read_to_string(&tmp).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp);
        if xml.is_empty() {
            return Vec::new();
        }
        let ids = [hwnd_isize.to_string(), (hwnd_isize as u32).to_string()];
        parse_dopus_paths(&xml, &ids)
            .into_iter()
            .map(|(label, p)| labeled(&label, p))
            .collect()
    }

    // ================= Explorer（COM 主 + Edit 回退） =================

    /// 针对某个资源管理器框架窗口取当前文件夹。
    /// 与 FileJump 同一套 COM（`LocationURL` + STA），再按 HWND 打分 / 标题 / 地址栏 Edit 对齐到该窗口。
    pub fn explorer_path_for_frame(frame: isize) -> Option<String> {
        if frame == 0 {
            return None;
        }
        let wins = explorer_windows();
        let mut best_score = i32::MIN;
        let mut best_path: Option<String> = None;
        for (hwnd, path) in &wins {
            let score = explorer_com_match_score(frame, *hwnd);
            if score < 0 || score < best_score {
                continue;
            }
            if score > best_score {
                best_score = score;
                best_path = Some(path.clone());
            } else if score == best_score {
                if best_path.as_ref().map(|p| path.len() > p.len()).unwrap_or(true) {
                    best_path = Some(path.clone());
                }
            }
        }
        if best_score >= 1 {
            if let Some(p) = best_path {
                return Some(p);
            }
        }
        if let Some(p) = explorer_edit_fallback(frame) {
            return Some(p);
        }
        let title = frame_title(frame);
        if !title.is_empty() {
            for (_, path) in &wins {
                let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
                if !name.is_empty()
                    && (title.eq_ignore_ascii_case(name) || title.contains(name))
                {
                    return Some(path.clone());
                }
            }
        }
        if let Some(p) = best_path {
            return Some(p);
        }
        if wins.len() == 1 {
            return Some(wins[0].1.clone());
        }
        None
    }

    fn frame_title(frame: isize) -> String {
        unsafe {
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(HWND(frame as *mut _), &mut buf);
            if n <= 0 {
                return String::new();
            }
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }

    /// 对齐 WPF `ExplorerComMatchScore`：4 全等 … 0 同顶层，-1 无关。
    fn explorer_com_match_score(frame: isize, shell_hwnd: isize) -> i32 {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetAncestor, GetParent, IsChild, GA_ROOT,
        };
        if frame == 0 || shell_hwnd == 0 {
            return -1;
        }
        if frame == shell_hwnd {
            return 4;
        }
        unsafe {
            let f = HWND(frame as *mut _);
            let h = HWND(shell_hwnd as *mut _);
            if IsChild(f, h).as_bool() {
                return 3;
            }
            let mut w = h;
            for _ in 0..64 {
                let Ok(p) = GetParent(w) else { break };
                if p.0.is_null() {
                    break;
                }
                if p.0 as isize == frame {
                    return 2;
                }
                w = p;
            }
            if GetAncestor(h, GA_ROOT).0 as isize == frame {
                return 1;
            }
            let fr = GetAncestor(f, GA_ROOT).0 as isize;
            let sr = GetAncestor(h, GA_ROOT).0 as isize;
            if fr != 0 && fr == sr {
                return 0;
            }
        }
        -1
    }

    fn explorer_windows() -> Vec<(isize, String)> {
        unsafe {
            use windows::Win32::System::Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL,
                COINIT_APARTMENTTHREADED,
            };
            use windows::Win32::System::Variant::{VARIANT, VARIANT_0_0, VT_I4};
            use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};
            use windows::core::Interface;

            struct Uninit;
            impl Drop for Uninit {
                fn drop(&mut self) {
                    unsafe { CoUninitialize() };
                }
            }
            if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
                return Vec::new();
            }
            let _g = Uninit;
            let Ok(wins) = CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL)
            else {
                return Vec::new();
            };
            let Ok(count) = wins.Count() else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for i in 0..count {
                let mut v = VARIANT::default();
                {
                    let inner: &mut VARIANT_0_0 = &mut *v.Anonymous.Anonymous;
                    inner.vt = VT_I4;
                    inner.Anonymous.lVal = i;
                }
                let Ok(disp) = wins.Item(&v) else { continue };
                let Ok(wb) = disp.cast::<IWebBrowser2>() else { continue };
                let hwnd = wb.HWND().ok().map(|h| h.0 as isize).unwrap_or(0);
                let Ok(url) = wb.LocationURL() else { continue };
                if let Some(p) = url_to_path(&url.to_string()) {
                    if std::path::Path::new(&p).is_dir() {
                        out.push((hwnd, p));
                    }
                }
            }
            out
        }
    }

    /// COM 枚举 `Shell.Application.Windows` 的全部 Explorer 路径。
    /// 借用指针全部即时释放（RAII），失败返回空（调用方走 Edit 回退）。
    pub fn explorer_all_paths() -> Vec<String> {
        let mut out = Vec::new();
        for (_, p) in explorer_windows() {
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    /// `file:///C:/x` → `C:\x`（百分号解码 + `/`→`\`）。
    pub fn url_to_path(url: &str) -> Option<String> {
        let rest = url.strip_prefix("file:///")?;
        // shell: GUID / 库视图无路径，跳过
        if rest.starts_with("shell:") || rest.contains("::") {
            return None;
        }
        let bytes = rest.as_bytes();
        let mut dec = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    dec.push(h << 4 | l);
                    i += 3;
                    continue;
                }
            }
            dec.push(bytes[i]);
            i += 1;
        }
        let s = String::from_utf8(dec).ok()?;
        let s = s.replace('/', "\\");
        normalize_path(&s)
    }

    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    struct EditAcc {
        best: String,
    }

    unsafe extern "system" fn edit_proc(hwnd: HWND, lp: LPARAM) -> windows::Win32::Foundation::BOOL {
        use windows::Win32::Foundation::BOOL;
        let acc = &mut *(lp.0 as *mut EditAcc);
        let mut buf = [0u16; 1024];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n > 0 {
            let t = String::from_utf16_lossy(&buf[..n as usize]);
            if let Some(p) = normalize_path(&t) {
                if p.len() > acc.best.len() && std::path::Path::new(&p).is_dir() {
                    acc.best = p;
                }
            }
        }
        BOOL(1)
    }

    /// Edit 回退：读 Explorer 框子控件文本里最长的现存目录（WPF UIA 回退的轻量版）。
    pub fn explorer_edit_fallback(frame_isize: isize) -> Option<String> {
        unsafe {
            let mut acc = EditAcc { best: String::new() };
            let _ = EnumChildWindows(
                Some(HWND(frame_isize as *mut _)),
                Some(edit_proc),
                LPARAM(&mut acc as *mut _ as isize),
            );
            if acc.best.is_empty() {
                None
            } else {
                Some(acc.best)
            }
        }
    }

    // ================= 总装 =================

    pub fn collect(gate: Option<&ClipboardGate>) -> Vec<Candidate> {
        let tops = top_level_zorder();
        let mut groups: Vec<Vec<Candidate>> = Vec::new();
        let mut explorer_frames: Vec<isize> = Vec::new();
        let mut opus_done = false;

        for h in &tops {
            match class_of(*h).as_str() {
                "TTOTAL_CMD" => {
                    let hwnd = HWND(*h as *mut _);
                    let mut v = Vec::new();
                    if let Some(p) = tc_path(hwnd, TC_SRC, gate) {
                        v.push(labeled("Total Commander (源)", p));
                    }
                    if let Some(p) = tc_path(hwnd, TC_TRG, gate) {
                        v.push(labeled("Total Commander (目标)", p));
                    }
                    if !v.is_empty() {
                        groups.push(v);
                    }
                }
                "ThunderRT6FormDC" => {
                    if let Some(p) = xy_path(HWND(*h as *mut _), gate) {
                        groups.push(vec![labeled("XYplorer", p)]);
                    }
                }
                "dopus.lister" if !opus_done => {
                    opus_done = true;
                    let v = dopus_paths_for(*h);
                    if !v.is_empty() {
                        groups.push(v);
                    }
                }
                "CabinetWClass" | "ExploreWClass" => explorer_frames.push(*h),
                _ => {}
            }
            if groups.len() >= 12 {
                break;
            }
        }
        if !explorer_frames.is_empty() {
            // COM 一次枚举全部窗口路径（WPF 的 15s 缓存此处省略：单次枚举 <40ms 量级）。
            let mut v: Vec<Candidate> = explorer_all_paths()
                .into_iter()
                .filter(|p| !is_install_dir(&p.to_lowercase()))
                .map(|p| labeled("资源管理器", p))
                .collect();
            if v.is_empty() {
                // COM 不可用时逐窗 Edit 回退。
                for f in explorer_frames {
                    if let Some(p) = explorer_edit_fallback(f) {
                        v.push(labeled("资源管理器", p));
                    }
                }
            }
            if !v.is_empty() {
                groups.push(v);
            }
        }
        merge_candidates(groups)
    }

    pub fn zorder_linked_folder(
        dialog_hwnd: isize,
        gate: Option<&ClipboardGate>,
        delta: usize,
    ) -> Option<String> {
        let tops = top_level_zorder();
        let idx = tops.iter().position(|h| *h == dialog_hwnd)?;
        let target = *tops.get(idx + delta.max(1))?;
        match class_of(target).as_str() {
            "TTOTAL_CMD" => tc_path(HWND(target as *mut _), TC_SRC, gate),
            "ThunderRT6FormDC" => xy_path(HWND(target as *mut _), gate),
            "CabinetWClass" | "ExploreWClass" => {
                let all = explorer_all_paths();
                all.into_iter().next().or_else(|| explorer_edit_fallback(target))
            }
            _ => None,
        }
    }
}

/// 收藏/最近候选构造（Picker 用，与采集器无关的纯逻辑）。
pub fn favorites(paths: &[String]) -> Vec<Candidate> {
    paths
        .iter()
        .filter_map(|p| normalize_path(p))
        .map(|p| Candidate {
            path: p,
            alias: None,
            source: CandidateSource::Favorite,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_and_unifies() {
        assert_eq!(normalize_path("  \"C:/a/b/\"  ").as_deref(), Some("C:\\a\\b"));
        assert_eq!(normalize_path("C:\\").as_deref(), Some("C:\\"));
        assert_eq!(normalize_path("   "), None);
    }

    #[test]
    fn merge_dedups_case_insensitive() {
        let a = Candidate::manager("C:\\A");
        let b = Candidate::manager("c:\\a");
        let c = Candidate::manager("D:\\B");
        let out = merge_candidates(vec![vec![a, b, c]]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, "C:\\A");
    }

    #[cfg(windows)]
    #[test]
    fn url_to_path_decodes() {
        let f = super::win::url_to_path;
        assert_eq!(f("file:///C:/a%20b").as_deref(), Some("C:\\a b"));
        assert_eq!(f("file:///C:/Windows").as_deref(), Some("C:\\Windows"));
        assert_eq!(f("shell:::{1234}"), None);
    }

    #[cfg(windows)]
    #[test]
    fn dopus_xml_parses_active_passive() {
        // 用现存目录（temp）保证 is_dir 门槛通过。
        let dir = std::env::temp_dir().to_string_lossy().replace('/', "\\");
        let xml = format!(
            "<lister hwnd=\"1\" lister=\"1234\"><path tab_state=\"1\">{dir}</path>\
             <path tab_state=\"2\">{dir}</path></lister>"
        );
        let out = super::win::parse_dopus_paths(&xml, &["1234".to_string()]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "Directory Opus (活动)");
        assert_eq!(out[1].0, "Directory Opus (被动)");
    }
}
