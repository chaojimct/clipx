//! 文件夹跳转 Picker（M5d）：对齐 WPF `FileDialogJumpPickerWindow` + `FileJumpHotkey`。
//!
//! - 全局 `Ctrl+G`：有对话框 → 采集候选并弹列表；无对话框 → 收藏/最近/管理器列表，Enter 在资源管理器打开
//! - 对话框前台自动弹出（`filejump_auto_popup`，400ms 轮询，WPF 前台事件的轻量版）
//! - 自动跳转最佳路径（`filejump_auto_jump`，默认关）
//! - 采集与导航全在后台线程（TC 剪贴板借道 + dopusrt + 注入都阻塞），gen 丢弃过期结果
//! - 键盘复用主弹窗钩子通道（VISIBLE 共享位）：↑↓←→/PgUp/PgDn/Home/End 移动，
//!   Enter 跳转，Esc 清搜索/关闭，Del 移除收藏/最近，Menu/Ctrl+P 收藏切换

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

static FOLLOW_MOUSE: AtomicBool = AtomicBool::new(false);

pub fn set_follow_mouse(v: bool) {
    FOLLOW_MOUSE.store(v, Ordering::SeqCst);
}

use clipx_filejump::{Candidate, CandidateSource, DialogKind};
use slint::ComponentHandle;

use crate::logic::AppEvt;
use crate::settings::{self, Settings};

/// 内卡片宽；窗口总宽 = 此值 + 两侧阴影 16×2 = 400，比 WPF 500 / 剪贴板 452 更窄。
pub const FJ_W: f32 = 368.0;
const FJ_CHROME: f32 = 16.0;
const FJ_ROW_H: f32 = 36.0;
const FJ_UI_CHROME: f32 = 108.0; // header 40 + search 36 + footer 32
const FJ_MIN_H: f32 = 168.0;
const FJ_MAX_H: f32 = 500.0;
const PAGE: usize = 8;

#[derive(Default)]
pub struct FjState {
    pub visible: bool,
    pub dialog: isize,
    pub kind: Option<DialogKind>,
    /// 无对话框时的全局模式：Enter 在资源管理器打开而非导航。
    pub global_mode: bool,
    pub query: String,
    pub items: Vec<Candidate>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub first_visible: usize,
    pub gen: u64,
    pub pending_auto_jump: bool,
    pub navigating: bool,
    pub hint: String,
    pub shown_at: Option<Instant>,
    /// 跟随线程 kill event（HANDLE 存 isize，0=无）；hide 时 signal，线程负责 unhook + 关闭。
    pub follow_kill: isize,
    /// 兜底 tick 比较基准：对话框上次矩形（物理像素）。
    /// WinEvent 是实时主力；个别不发 LOCATIONCHANGE 的宿主靠 200ms tick 补。
    pub last_dlg_rect: Option<(i32, i32, i32, i32)>,
    /// Tab 仅收藏
    pub fav_only: bool,
    /// 二次 Ctrl+G 直跳
    pub last_hotkey_at: Option<Instant>,
    /// auto_sync：曾离开对话框
    pub left_dialog: bool,
    /// 对话框当前文件夹（采集线程回读，查询时把子文件夹插到其后面）。
    pub current_folder: Option<String>,
}

pub fn source_label(s: CandidateSource) -> &'static str {
    match s {
        CandidateSource::Manager => "管理器",
        CandidateSource::Favorite => "收藏",
        CandidateSource::Recent => "常用",
        CandidateSource::Last => "上次",
        CandidateSource::Everything => "everything",
        CandidateSource::Current => "当前",
    }
}

fn source_kind(s: CandidateSource) -> i32 {
    match s {
        CandidateSource::Manager => 0,
        CandidateSource::Favorite => 1,
        CandidateSource::Recent => 2,
        CandidateSource::Last => 3,
        CandidateSource::Everything => 4,
        CandidateSource::Current => 5,
    }
}

/// 子串 / 拼音 / 空格分词 AND（路径或别名任一命中即可）。
pub fn filter_items(items: &[Candidate], query: &str) -> Vec<usize> {
    let q = query.trim();
    if q.is_empty() {
        return (0..items.len()).collect();
    }
    items
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            q.split_whitespace().all(|tok| {
                clipx_core::pinyin::text_matches_query(&c.path, tok)
                    || c.alias
                        .as_deref()
                        .is_some_and(|a| clipx_core::pinyin::text_matches_query(a, tok))
            })
        })
        .map(|(i, _)| i)
        .collect()
}

fn fav_candidates(favs: &[crate::settings::FolderFavorite]) -> Vec<Candidate> {
    favs.iter()
        .filter_map(|f| {
            let norm = clipx_filejump::collectors::normalize_path(f.path())?;
            if !std::path::Path::new(&norm).is_dir() {
                return None;
            }
            let alias = if f.phrase().is_empty() {
                None
            } else {
                Some(f.phrase().to_string())
            };
            Some(Candidate { path: norm, alias, source: CandidateSource::Favorite })
        })
        .collect()
}

fn recent_candidates(recent: &[String]) -> Vec<Candidate> {
    recent.iter()
        .filter_map(|p| {
            let norm = clipx_filejump::collectors::normalize_path(p)?;
            if !std::path::Path::new(&norm).is_dir() {
                return None;
            }
            Some(Candidate { path: norm, alias: None, source: CandidateSource::Recent })
        })
        .collect()
}

fn current_folder_candidate(dialog: isize) -> Option<Candidate> {
    if dialog == 0 {
        return None;
    }
    let raw = clipx_filejump::inject::read_current_folder(dialog).ok()?;
    let norm = clipx_filejump::collectors::normalize_path(&raw)?;
    if !Path::new(&norm).is_dir() {
        return None;
    }
    Some(Candidate {
        path: norm,
        alias: None,
        source: CandidateSource::Current,
    })
}

/// 当前文件夹下一层子目录（查询时插在「当前」后面、全盘 everything 前面）。
fn list_child_folders(folder: &str, query: &str, max: usize) -> Vec<Candidate> {
    let q = query.trim();
    if q.is_empty() || max == 0 {
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        if !clipx_core::pinyin::text_matches_query(&name, q) {
            continue;
        }
        let path = ent.path().to_string_lossy().replace('/', "\\");
        out.push(Candidate {
            path,
            alias: None,
            source: CandidateSource::Current,
        });
    }
    out.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    out.truncate(max);
    out
}

fn everything_folders(query: &str, max: usize) -> Vec<Candidate> {
    let q = query.trim();
    if q.is_empty() || max == 0 {
        return Vec::new();
    }
    let timeout = Duration::from_millis(3000);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let push_from = |res: clipx_everything::QueryResults,
                     seen: &mut std::collections::HashSet<String>,
                     out: &mut Vec<Candidate>| {
        for it in res.items {
            if !it.is_folder {
                continue;
            }
            if !Path::new(&it.full_path).is_dir() {
                continue;
            }
            if !seen.insert(it.full_path.to_lowercase()) {
                continue;
            }
            out.push(Candidate {
                path: it.full_path,
                alias: None,
                source: CandidateSource::Everything,
            });
            if out.len() >= max {
                break;
            }
        }
    };
    let expr = clipx_everything::search::build_folder_search(q);
    if let Ok(res) = clipx_everything::query(&expr, 50, timeout) {
        push_from(res, &mut seen, &mut out);
    }
    // findx 可能不认 folder: 修饰符：裸关键词再滤文件夹。
    if out.len() < max {
        if let Ok(res) = clipx_everything::query(q, 80, timeout) {
            push_from(res, &mut seen, &mut out);
        }
    }
    out
}

// ================= 后台采集 =================

pub struct CollectReq {
    pub gen: u64,
    pub query: String,
    pub favorites: Vec<crate::settings::FolderFavorite>,
    pub recent: Vec<String>,
    pub everything_enabled: bool,
    pub dialog: isize,
}

pub fn spawn_collect(
    tx: Sender<AppEvt>,
    gate: clipx_core::ClipboardGate,
    req: CollectReq,
) {
    std::thread::Builder::new()
        .name("clipx-fj-collect".into())
        .spawn(move || {
            let current = current_folder_candidate(req.dialog);
            let children = current
                .as_ref()
                .map(|c| list_child_folders(&c.path, &req.query, 20))
                .unwrap_or_default();
            let mut groups = vec![
                current.into_iter().collect::<Vec<_>>(),
                children,
                clipx_filejump::collectors::collect(Some(&gate)),
                fav_candidates(&req.favorites),
                recent_candidates(&req.recent),
            ];
            if req.everything_enabled && !req.query.trim().is_empty() {
                groups.push(everything_folders(&req.query, 20));
            }
            let items = clipx_filejump::collectors::merge_candidates(groups);
            let _ = tx.send(AppEvt::FjCollected { gen: req.gen, items });
        })
        .ok();
}

/// 打字后：当前文件夹子目录 +（可选）Everything 全盘文件夹。
pub fn spawn_ev_query(
    tx: Sender<AppEvt>,
    gen: u64,
    query: String,
    current_folder: Option<String>,
    everything_enabled: bool,
) {
    std::thread::Builder::new()
        .name("clipx-fj-ev".into())
        .spawn(move || {
            if query.trim().is_empty() {
                return;
            }
            let mut items = current_folder
                .as_deref()
                .map(|f| list_child_folders(f, &query, 20))
                .unwrap_or_default();
            if everything_enabled {
                items.extend(everything_folders(&query, 20));
            }
            let _ = tx.send(AppEvt::FjEvResults { gen, items });
        })
        .ok();
}

// ================= 后台导航 =================

pub fn spawn_navigate(
    tx: Sender<AppEvt>,
    dialog: isize,
    kind: DialogKind,
    path: String,
    allow_inject: bool,
    gen: u64,
) {
    std::thread::Builder::new()
        .name("clipx-fj-nav".into())
        .spawn(move || {
            let ok = clipx_filejump::inject::navigate_to_folder(dialog, kind, &path, allow_inject)
                .unwrap_or(false);
            let _ = tx.send(AppEvt::FjNavigated { gen, path, ok });
        })
        .ok();
}

// ================= 前台轮询（自动弹出） =================

/// watcher 配置快照（设置保存时更新，轮询线程每轮读取）。
static WATCH_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static WATCH_AUTOPOPUP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static WATCH_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn watch_set(enabled: bool, auto_popup: bool, delay_ms: u64) {
    use std::sync::atomic::Ordering;
    WATCH_ENABLED.store(enabled, Ordering::SeqCst);
    WATCH_AUTOPOPUP.store(auto_popup, Ordering::SeqCst);
    WATCH_DELAY_MS.store(delay_ms, Ordering::SeqCst);
}

/// 对话框前台轮询：400ms 一次；同一 hwnd 只触发一次，切走后重置。
/// 主弹窗/QF 可见时（VISIBLE 共享位）跳过，避免抢键路由。
pub fn spawn_watcher(tx: Sender<AppEvt>) {
    std::thread::Builder::new()
        .name("clipx-fj-watch".into())
        .spawn(move || {
            use std::sync::atomic::Ordering;
            let mut last_fired: isize = 0;
            loop {
                std::thread::sleep(Duration::from_millis(400));
                if !WATCH_ENABLED.load(Ordering::SeqCst)
                    || !WATCH_AUTOPOPUP.load(Ordering::SeqCst)
                {
                    continue;
                }
                if crate::keyboard_hook::is_visible() {
                    continue;
                }
                #[cfg(windows)]
                let found = clipx_filejump::dialog::win::resolve_from_foreground();
                #[cfg(not(windows))]
                let found: Option<isize> = None;
                match found {
                    Some(d) if d != last_fired => {
                        last_fired = d;
                        // 延时内再确认一次前台仍是同一对话框（WPF ShowDelay 语义）。
                        let delay = WATCH_DELAY_MS.load(Ordering::SeqCst).max(80);
                        std::thread::sleep(Duration::from_millis(delay));
                        #[cfg(windows)]
                        let still = clipx_filejump::dialog::win::resolve_from_foreground() == Some(d);
                        #[cfg(not(windows))]
                        let still = false;
                        if still && !crate::keyboard_hook::is_visible() {
                            let _ = tx.send(AppEvt::FileJumpAuto(d));
                        }
                    }
                    None => {
                        last_fired = 0;
                    }
                    _ => {}
                }
            }
        })
        .ok();
}

// ================= WinEvent 实时跟随（对齐 WPF dock follow hooks） =================
//
// WPF 方案：全局 WinEvent 钩子（OUTOFCONTEXT|SKIPOWNPROCESS，pid/tid=0）
// 监听 MOVESIZE 范围 + LOCATIONCHANGE，按 DockEventBelongsToOwner 过滤
// （事件窗与 owner 同 root 且是 root 本体或 owner 本体），命中即重算 dock；
// 另有 OWNER DESTROY 钩子关窗 + 500ms timer 兜底。
// Rust 侧：独立线程泵消息（OUTOFCONTEXT 回调落在本线程），kill event 退出。
// 我们钩 LOCATIONCHANGE + DESTROY（MOVE/SIZE 由 LOCATIONCHANGE 覆盖；
// 焦点窃取抑制不需要——我们从不抢焦点），200ms tick 保留作兜底。
#[cfg(windows)]
mod follow {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::Mutex;

    use windows::Win32::Foundation::{HANDLE, HWND, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, INFINITE};
    use windows::Win32::UI::Accessibility::{
        SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetAncestor, IsWindow, MsgWaitForMultipleObjects, PeekMessageW,
        TranslateMessage, GA_ROOT, MSG, PM_REMOVE, QS_ALLINPUT, EVENT_OBJECT_DESTROY,
        EVENT_OBJECT_LOCATIONCHANGE,
    };

    use crate::logic::AppEvt;

    // WINEVENT_OUTOFCONTEXT=0x0000 | WINEVENT_SKIPOWNPROCESS=0x0002
    //（windows 0.59 未导出具名常量，此处按 SDK 原值定义）。
    const FLAGS: u32 = 0x0000 | 0x0002;

    static TX: Mutex<Option<std::sync::mpsc::Sender<AppEvt>>> = Mutex::new(None);
    static DLG: AtomicIsize = AtomicIsize::new(0);

    fn belongs(hwnd: HWND, dlg: HWND) -> bool {
        unsafe {
            if !IsWindow(Some(dlg)).as_bool() {
                return false;
            }
            let owner_root = GetAncestor(dlg, GA_ROOT);
            if owner_root.0.is_null() {
                return false;
            }
            let event_root = GetAncestor(hwnd, GA_ROOT);
            if event_root.0.is_null() || event_root != owner_root {
                return false;
            }
            hwnd == owner_root || hwnd == dlg
        }
    }

    unsafe extern "system" fn callback(
        _hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        _id_obj: i32,
        _id_child: i32,
        _tid: u32,
        _time: u32,
    ) {
        let dlg_isize = DLG.load(Ordering::SeqCst);
        if dlg_isize == 0 || hwnd.0.is_null() {
            return;
        }
        let dlg = HWND(dlg_isize as *mut _);
        if event == EVENT_OBJECT_DESTROY {
            if hwnd == dlg {
                if let Some(tx) = TX.lock().unwrap().as_ref() {
                    let _ = tx.send(AppEvt::FjDialogGone);
                }
            }
            return;
        }
        if event == EVENT_OBJECT_LOCATIONCHANGE && belongs(hwnd, dlg) {
            if let Some(tx) = TX.lock().unwrap().as_ref() {
                let _ = tx.send(AppEvt::FjDialogMoved);
            }
        }
    }

    /// 启动跟随线程，返回 kill event 句柄（存 isize）；hide 时 `stop`。
    pub fn spawn(tx: std::sync::mpsc::Sender<AppEvt>, dialog: isize) -> isize {
        let kill = unsafe {
            match CreateEventW(None, true, false, None) {
                Ok(h) => h,
                Err(_) => return 0,
            }
        };
        // HANDLE（*mut c_void）不是 Send：传原始值，线程内重建。
        let kill_val = kill.0 as isize;
        *TX.lock().unwrap() = Some(tx);
        DLG.store(dialog, Ordering::SeqCst);
        std::thread::Builder::new()
            .name("clipx-fj-follow".into())
            .spawn(move || unsafe {
                let kill = HANDLE(kill_val as *mut _);
                let h1 = SetWinEventHook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    None,
                    Some(callback),
                    0,
                    0,
                    FLAGS,
                );
                let h2 = SetWinEventHook(
                    EVENT_OBJECT_DESTROY,
                    EVENT_OBJECT_DESTROY,
                    None,
                    Some(callback),
                    0,
                    0,
                    FLAGS,
                );
                loop {
                    let ret =
                        MsgWaitForMultipleObjects(Some(&[kill]), false, INFINITE, QS_ALLINPUT);
                    if ret == WAIT_OBJECT_0 {
                        break;
                    }
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                if !h1.0.is_null() {
                    let _ = UnhookWinEvent(h1);
                }
                if !h2.0.is_null() {
                    let _ = UnhookWinEvent(h2);
                }
                let _ = windows::Win32::Foundation::CloseHandle(kill);
            })
            .ok();
        kill_val
    }

    pub fn stop(kill_isize: isize) {
        if kill_isize == 0 {
            return;
        }
        unsafe {
            let _ = SetEvent(HANDLE(kill_isize as *mut _));
        }
    }
}

/// 启动 WinEvent 跟随线程（dialog=0 时不启动）；返回 kill 句柄。
pub fn follow_spawn(tx: Sender<AppEvt>, dialog: isize) -> isize {
    #[cfg(windows)]
    {
        if dialog == 0 {
            return 0;
        }
        follow::spawn(tx, dialog)
    }
    #[cfg(not(windows))]
    {
        let _ = (tx, dialog);
        0
    }
}

/// 停止跟随线程（hide 时调用；线程负责 unhook + 关闭句柄）。
pub fn follow_stop(kill: isize) {
    #[cfg(windows)]
    {
        follow::stop(kill);
    }
    #[cfg(not(windows))]
    {
        let _ = kill;
    }
}

// ================= 设置持久化（收藏/最近） =================

pub fn persist_lists(
    path: &Path,
    favs: &[crate::settings::FolderFavorite],
    recent: &[String],
    recent_max: usize,
) {
    let mut s: Settings = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    s.filejump_favorites = favs.to_vec();
    let mut r = recent.to_vec();
    r.truncate(recent_max.max(1));
    s.filejump_recent = r;
    let _ = settings::save(path, &s);
}

pub fn push_recent(path: &Path, recent: &mut Vec<String>, dir: &str, recent_max: usize) {
    recent.retain(|p| p.to_lowercase() != dir.to_lowercase());
    recent.insert(0, dir.to_string());
    recent.truncate(recent_max.max(1));
    let favs: Vec<crate::settings::FolderFavorite> = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .map(|s| s.filejump_favorites)
        .unwrap_or_default();
    persist_lists(path, &favs, recent, recent_max);
}

// ================= UI 推送 =================

#[allow(clippy::too_many_arguments)]
pub fn push_fj_ui(
    weak: &slint::Weak<crate::FileJumpWindow>,
    st: &FjState,
    show: bool,
    anchor_dialog: isize,
) {
    // FjRow is plain data (Send); ModelRc is built on the event-loop thread.
    // 主行永远是路径（管理器标签/收藏别名只进次行），对齐 WPF PathLine 语义。
    let rows: Vec<crate::FjRow> = st
        .filtered
        .iter()
        .enumerate()
        .map(|(i, &idx)| {
            let c = &st.items[idx];
            let label = source_label(c.source);
            let sub: String = match c.source {
                CandidateSource::Favorite => match c.alias.as_deref() {
                    Some(a) if !a.is_empty() => format!("{a}  · 收藏"),
                    _ => label.to_string(),
                },
                _ => match c.alias.as_deref() {
                    Some(a) if !a.is_empty() => format!("{a} · {label}"),
                    _ => label.to_string(),
                },
            };
            crate::FjRow {
                alias: c.path.clone().into(),
                path: sub.into(),
                source: label.into(),
                index_label: {
                    let rel = i as i32 - st.first_visible as i32 + 1;
                    if (1..=9).contains(&rel) {
                        rel.to_string().into()
                    } else {
                        "".into()
                    }
                },
                source_kind: source_kind(c.source),
            }
        })
        .collect();
    let n = st.filtered.len();
    let title: slint::SharedString = if st.global_mode {
        "跳转到文件夹（全局）".into()
    } else {
        "跳转到文件夹".into()
    };
    let shown = n.min(PAGE).max(1) as f32;
    let h = (FJ_UI_CHROME + shown * FJ_ROW_H).clamp(FJ_MIN_H, FJ_MAX_H);
    let selected = st.selected as i32;
    let first_visible = st.first_visible as i32;
    let search: slint::SharedString = st.query.clone().into();
    let count: slint::SharedString = format!("{} 个", n).into();
    let hint: slint::SharedString = if st.navigating {
        "正在跳转…".into()
    } else if st.hint.is_empty() {
        "↑↓ 选择 · ←→ 翻页 · Ctrl+N 跳转 · Enter 跳转 · Del 移除收藏/最近 · Esc 关闭".into()
    } else {
        st.hint.clone().into()
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        use slint::{ModelRc, VecModel};
        let Some(ui) = weak.upgrade() else { return };
        if show {
            crate::settings_win::paint_theme_handle(ui.global::<crate::Theme>());
        }
        ui.set_title_label(title);
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_selected_index(selected);
        ui.set_search_text(search);
        ui.set_count_label(count);
        ui.set_hint_label(hint);
        ui.set_first_visible(first_visible);
        let win_w = FJ_W + FJ_CHROME * 2.0;
        let win_h = h + FJ_CHROME * 2.0;
        ui.window().set_size(slint::WindowSize::Logical(slint::LogicalSize::new(win_w, win_h)));
        if show {
            #[cfg(windows)]
            if FOLLOW_MOUSE.load(Ordering::SeqCst) || anchor_dialog == 0 {
                crate::win_popup::position_near_cursor(&ui.window(), win_w, win_h);
            } else {
                crate::win_popup::position_dock_dialog(&ui.window(), anchor_dialog, win_w, win_h);
            }
            #[cfg(not(windows))]
            crate::win_popup::position_near_cursor(&ui.window(), win_w, win_h);
            let _ = ui.window().show();
            #[cfg(windows)]
            {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                if let Ok(hh) = ui.window().window_handle().window_handle() {
                    if let RawWindowHandle::Win32(w) = hh.as_raw() {
                        crate::mouse_hook::FJ_HWND
                            .store(w.hwnd.get() as isize, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
        }
    });
}

pub fn hide_fj_ui(weak: &slint::Weak<crate::FileJumpWindow>) {
    #[cfg(windows)]
    crate::mouse_hook::FJ_HWND.store(0, std::sync::atomic::Ordering::SeqCst);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().hide();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_child_folders_skips_files_and_matches_name() {
        let dir = std::env::temp_dir().join(format!("clipx-fj-child-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dept-data")).unwrap();
        std::fs::create_dir_all(dir.join("readme")).unwrap();
        std::fs::write(dir.join("file.txt"), b"x").unwrap();
        let hits = list_child_folders(&dir.to_string_lossy(), "dept", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.to_lowercase().replace('/', "\\").ends_with("dept-data"));
        assert_eq!(hits[0].source, CandidateSource::Current);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
