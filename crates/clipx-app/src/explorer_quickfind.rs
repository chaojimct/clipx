//! Explorer 内快速查找控制器（M4，移植 WPF ExplorerQuickFindController）。
//!
//! 有检索词时走与 FindX 主窗相同的一次关键词查询（管道 `pinyin: true`），
//! 再按当前文件夹切成本地/全盘两段；FindX 不可用才回退 Everything 三阶段。
//! 代际（gen）丢弃过期结果。结束会话时清钩线程快速标志（单一事实源）。

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clipx_core::pinyin::{pinyin_hit_span, text_matches_query};
use clipx_everything::ResultItem;
use clipx_everything::search::{build_parent_scoped_search, build_path_subtree_scoped_search};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::keyboard_hook::{self, KeyEvt};
use crate::win_popup;
use crate::{QfRow, QuickFindWindow};

pub const DEFAULT_HINT: &str = "↑↓ 选择 · ←→ 翻页 · Ctrl+N 快选 · Enter 定位 · Esc 关闭";
/// 查询 debounce（对齐 WPF：Everything 单查 <5ms，30ms 合并连按足够且无感知延迟）。
const QUERY_DEBOUNCE: Duration = Duration::from_millis(30);
/// 有检索词时一次 FindX/Everything 关键词查询（FindX GUI 同款，应 <50ms）。
const IPC_QF: Duration = Duration::from_millis(500);
/// 当前文件夹 / 路径树：Everything 正常 <5ms；超时立刻改用文件系统，避免卡 3 秒。
const IPC_FAST: Duration = Duration::from_millis(250);
/// 快路径失败后的全盘兜底，给 IPC 留足时间。
const IPC_GLOBAL: Duration = Duration::from_millis(3000);
/// ←→/PgUp/PgDn 翻页步长（对齐 WPF MoveSelectionPage pageSize=8）。
const PAGE_SIZE: i32 = 8;
/// 窗口总宽（含 16px 阴影圈），比 WPF 540 收一档，长文件名仍能看清。
const WIN_W: f32 = 440.0;
const QF_CHROME: f32 = 16.0;
const QF_UI: f32 = 121.0; // header 32 + 1 + search 56 + footer 32
const QF_ROW: f32 = 28.0;
const QF_MIN_H: f32 = 240.0;
const QF_MAX_H: f32 = 480.0;

/// 逻辑线程持有的行数据（纯数据，Send；Slint QfRow 在事件循环线程组装）。
#[derive(Clone, Debug, Default)]
pub struct QfRowSrc {
    pub full_path: String,
    pub name_pre: String,
    pub name_hit: String,
    pub name_post: String,
    pub rel: String,
    pub is_folder: bool,
    pub is_global: bool,
    pub index_label: String,
}

/// 查询线程产出的一次回显载荷。
#[derive(Clone, Debug, Default)]
pub struct QfView {
    pub rows: Vec<QfRowSrc>,
    pub count_label: String,
    pub hint_label: String,
}

pub struct QfState {
    /// 会话活动（对应 WPF _sessionActive；hook 侧已置位，事件到达本线程）。
    active: bool,
    /// 文件夹解析完成（对应 WPF _session；未完成期间字符仅累积不查询）。
    started: bool,
    frame: isize,
    folder: String,
    folder_display: String,
    typing: String,
    /// 查询代际：每次调度/结束会话自增，线程用它丢弃过期结果。
    gen: Arc<AtomicU64>,
    items: Vec<QfRowSrc>,
    selected: usize,
    count_label: String,
    hint_label: String,
    /// DirectOpen：对话框前台则导航对话框，否则 ShellExecute
    direct_open: bool,
}

impl Default for QfState {
    fn default() -> Self {
        Self {
            active: false,
            started: false,
            frame: 0,
            folder: String::new(),
            folder_display: String::new(),
            typing: String::new(),
            gen: Arc::new(AtomicU64::new(0)),
            items: Vec::new(),
            selected: 0,
            count_label: String::new(),
            hint_label: DEFAULT_HINT.to_string(),
            direct_open: false,
        }
    }
}

// ===================== 事件入口（逻辑线程调用） =====================

pub fn handle_key(
    k: KeyEvt,
    state: &mut QfState,
    max_results: u32,
    tx: &std::sync::mpsc::Sender<crate::logic::AppEvt>,
    weak: &slint::Weak<QuickFindWindow>,
    open_mode: &str,
) {
    state.direct_open = open_mode == "DirectOpen";
    match k {
        KeyEvt::QfStart { frame, desktop, ch } => {
            start(state, frame, desktop, ch, max_results, tx, weak)
        }
        KeyEvt::QfEnd | KeyEvt::QfEsc => end(state, weak),
        _ if !state.active => {}
        KeyEvt::QfChar(c) => {
            state.typing.push(c);
            state.hint_label = DEFAULT_HINT.to_string();
            push_ui(state, weak, false);
            if state.started {
                schedule_query(state, max_results, tx);
            }
        }
        KeyEvt::QfBackspace => {
            let had = state.typing.pop().is_some();
            if !had || state.typing.is_empty() {
                // 退格清空检索词 → 结束会话（对齐 WPF）
                end(state, weak);
            } else {
                push_ui(state, weak, false);
                if state.started {
                    schedule_query(state, max_results, tx);
                }
            }
        }
        KeyEvt::QfEnter => activate(state, state.selected, weak),
        KeyEvt::QfUp => move_sel(state, weak, -1),
        KeyEvt::QfDown => move_sel(state, weak, 1),
        KeyEvt::QfPage(d) => move_sel(state, weak, d.saturating_mul(PAGE_SIZE)),
        KeyEvt::QfHome => {
            state.selected = 0;
            push_ui(state, weak, false);
        }
        KeyEvt::QfEndKey => {
            state.selected = state.items.len().saturating_sub(1);
            push_ui(state, weak, false);
        }
        KeyEvt::QfQuickSelect(n) => activate(state, n as usize, weak),
        _ => {}
    }
}

/// 会话启动：立即展示浮层（首字符回显），文件夹解析走后台线程。
fn start(
    state: &mut QfState,
    frame: isize,
    desktop: bool,
    ch: char,
    max_results: u32,
    tx: &std::sync::mpsc::Sender<crate::logic::AppEvt>,
    weak: &slint::Weak<QuickFindWindow>,
) {
    if state.active {
        return;
    }
    state.active = true;
    state.started = false;
    state.frame = frame;
    state.folder.clear();
    state.folder_display = "定位中…".to_string();
    state.typing.clear();
    state.typing.push(ch);
    state.items.clear();
    state.selected = 0;
    state.count_label.clear();
    state.hint_label = "正在定位当前文件夹…".to_string();
    let gen = bump_gen(state);
    push_ui(state, weak, true);

    if desktop {
        // 桌面目录解析无 COM 开销，内联完成（对齐 WPF）
        apply_folder(gen, shell_desktop_dir(), state, max_results, tx, weak);
    } else {
        let tx = tx.clone();
        let _ = std::thread::Builder::new()
            .name("clipx-qf-folder".into())
            .spawn(move || {
                let folder = shell_explorer_folder_path(frame);
                let _ = tx.send(crate::logic::AppEvt::QfFolderResolved { gen, folder });
            });
    }
}

/// 文件夹解析完成（gen 校验防过期）：设定检索根并调度查询。
pub fn apply_folder(
    gen: u64,
    folder: Option<String>,
    state: &mut QfState,
    max_results: u32,
    tx: &std::sync::mpsc::Sender<crate::logic::AppEvt>,
    weak: &slint::Weak<QuickFindWindow>,
) {
    if !state.active || state.gen.load(Ordering::SeqCst) != gen {
        return;
    }
    state.started = true;
    match folder.filter(|f| !f.trim().is_empty()) {
        Some(f) => {
            state.folder = f.trim().to_string();
            state.folder_display = state.folder.clone();
        }
        None => {
            // 非常规文件夹（读取失败/虚拟位置）→ 全盘检索
            state.folder.clear();
            state.folder_display = "全盘".to_string();
        }
    }
    push_ui(state, weak, false);
    schedule_query(state, max_results, tx);
}

/// 查询结果回显（gen 校验丢弃过期阶段结果；每次回显选中归零，对齐 WPF）。
pub fn apply_results(
    gen: u64,
    view: QfView,
    state: &mut QfState,
    weak: &slint::Weak<QuickFindWindow>,
) {
    if !state.active || state.gen.load(Ordering::SeqCst) != gen {
        return;
    }
    state.items = view.rows;
    state.selected = 0;
    state.count_label = view.count_label;
    state.hint_label = view.hint_label;
    push_ui(state, weak, false);
}

/// 列表点击（row-activated）：定位并选中该路径。
pub fn activate_index(
    idx: usize,
    state: &mut QfState,
    weak: &slint::Weak<QuickFindWindow>,
    open_mode: &str,
) {
    state.direct_open = open_mode == "DirectOpen";
    if state.active {
        activate(state, idx, weak);
    }
}

pub fn end_if_active(state: &mut QfState, weak: &slint::Weak<QuickFindWindow>) {
    if state.active {
        end(state, weak);
    }
}

fn move_sel(state: &mut QfState, weak: &slint::Weak<QuickFindWindow>, delta: i32) {
    let len = state.items.len();
    if len == 0 {
        return;
    }
    let next = (state.selected as i64 + delta as i64).clamp(0, len as i64 - 1);
    state.selected = next as usize;
    push_ui(state, weak, false);
}

/// Enter/快选/点击：结束会话 → 后台线程就地导航选中（M4-c）。
/// 导航含 Navigate 轮询等待（最长 ~500ms），不得占用逻辑线程。
fn activate(state: &mut QfState, idx: usize, weak: &slint::Weak<QuickFindWindow>) {
    let Some(path) = state.items.get(idx).map(|r| r.full_path.clone()) else {
        return;
    };
    let frame = state.frame;
    let direct = state.direct_open;
    end(state, weak);
    let _ = std::thread::Builder::new()
        .name("clipx-qf-nav".into())
        .spawn(move || {
            if direct {
                #[cfg(windows)]
                {
                    if crate::win_popup::foreground_is_file_dialog() {
                        shell_navigate_and_select(frame, &path);
                    } else {
                        shell_execute_path(&path);
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = (frame, path);
                }
            } else {
                shell_navigate_and_select(frame, &path);
            }
        });
}

fn end(state: &mut QfState, weak: &slint::Weak<QuickFindWindow>) {
    state.active = false;
    state.started = false;
    state.frame = 0;
    state.folder.clear();
    state.folder_display.clear();
    state.typing.clear();
    state.items.clear();
    state.selected = 0;
    state.count_label.clear();
    state.hint_label = DEFAULT_HINT.to_string();
    state.gen.fetch_add(1, Ordering::SeqCst);
    keyboard_hook::qf_clear_session();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().hide();
        }
    });
}

fn bump_gen(state: &mut QfState) -> u64 {
    state.gen.fetch_add(1, Ordering::SeqCst) + 1
}

// ===================== 查询调度（三阶段，对齐 WPF ScheduleQuery） =====================

fn schedule_query(
    state: &mut QfState,
    max_results: u32,
    tx: &std::sync::mpsc::Sender<crate::logic::AppEvt>,
) {
    let gen = bump_gen(state);
    let folder = state.folder.clone();
    let typing = state.typing.clone();
    let tx = tx.clone();
    let gen_shared = state.gen.clone();
    let _ = std::thread::Builder::new()
        .name("clipx-qf-query".into())
        .spawn(move || {
            // debounce：连按时合并为一次查询（Everything 单查 <5ms，30ms 无感知）
            std::thread::sleep(QUERY_DEBOUNCE);
            if gen_shared.load(Ordering::SeqCst) != gen {
                return;
            }
            run_query(gen, gen_shared, &folder, &typing, max_results, &tx);
        });
}

#[allow(clippy::too_many_lines)]
fn run_query(
    gen: u64,
    gen_shared: Arc<AtomicU64>,
    folder: &str,
    typing: &str,
    max_results: u32,
    tx: &std::sync::mpsc::Sender<crate::logic::AppEvt>,
) {
    let max = (max_results.max(1)) as usize;
    let typing_trim = typing.trim();
    let has_typing = !typing_trim.is_empty();
    let needle = has_typing.then_some(typing_trim);
    let stale = || gen_shared.load(Ordering::SeqCst) != gen;
    let post = |rows: Vec<QfRowSrc>, count: String, hint: String| {
        let _ = tx.send(crate::logic::AppEvt::QfResults {
            gen,
            view: QfView {
                rows,
                count_label: count,
                hint_label: hint,
            },
        });
    };
    let plain_list = |items: Vec<ResultItem>| {
        let rows = from_full_paths(items, folder, needle);
        if rows.is_empty() {
            (rows, String::new(), "无匹配项".to_string())
        } else {
            let count = format!("{} 项", rows.len());
            (rows, count, DEFAULT_HINT.to_string())
        }
    };

    // 全盘会话（文件夹解析失败回退）：一次关键词查询，不再串 parent:/path:。
    if folder.is_empty() {
        if !has_typing {
            post(Vec::new(), String::new(), DEFAULT_HINT.to_string());
            return;
        }
        match clipx_everything::query(typing_trim, max as u32, IPC_QF) {
            Ok(r) => {
                let (rows, count, hint) = plain_list(r.items);
                post(rows, count, hint);
            }
            Err(_) => post(Vec::new(), String::new(), "无匹配项".to_string()),
        }
        return;
    }

    let fs = list_folder_children(folder, typing, max);
    if stale() {
        return;
    }

    if !has_typing {
        let (rows, count, hint) = plain_list(fs);
        post(rows, count, hint);
        return;
    }

    // 快路径：与 FindX 主窗一样只打一次关键词（拼音在管道侧开启）。
    // 本地 FS 合并进当前文件夹段；成功则不再跑 parent: → path: → 全盘。
    match clipx_everything::query(typing_trim, global_ask(max), IPC_QF) {
        Ok(r) => {
            let local = merge_prefer_first(in_folder(r.items.clone(), folder), fs, max);
            let global = not_in_folder(r.items, folder);
            let (rows, n_global) = from_scoped_and_global(local, global, folder, needle, max);
            if rows.is_empty() {
                post(Vec::new(), String::new(), "无匹配项".to_string());
            } else {
                let n = rows.len();
                post(rows, qf_count_label(n, n_global), DEFAULT_HINT.to_string());
            }
            return;
        }
        Err(_) => {}
    }
    if stale() {
        return;
    }
    if !fs.is_empty() {
        let rows = from_full_paths(fs.clone(), folder, needle);
        post(
            rows,
            format!("{} 项", fs.len()),
            DEFAULT_HINT.to_string(),
        );
    }

    // ---------- 快路径失败：Everything parent: / path: / 全盘兜底 ----------
    let q1 = clipx_everything::query(
        &build_parent_scoped_search(folder, typing),
        max as u32,
        IPC_FAST,
    );
    if stale() {
        return;
    }
    if is_ipc_down(&q1) {
        let (ok, global) = match clipx_everything::query(typing_trim, global_ask(max), IPC_GLOBAL)
        {
            Ok(r) => (
                true,
                not_in_folder(filter_name_hits(r.items, needle), folder),
            ),
            Err(_) => (false, Vec::new()),
        };
        let (rows, n_global) = from_scoped_and_global(fs, global, folder, needle, max);
        if rows.is_empty() {
            if !ok {
                post(Vec::new(), String::new(), "无匹配项".to_string());
            }
        } else {
            let n = rows.len();
            post(rows, qf_count_label(n, n_global), DEFAULT_HINT.to_string());
        }
        return;
    }

    let ev1 = scoped_hits(items_of(&q1), folder, needle);
    let p1 = merge_prefer_first(fs, ev1, max);
    if !p1.is_empty() {
        let n = p1.len();
        let rows = from_full_paths(p1.clone(), folder, needle);
        post(rows, format!("{n} 项"), DEFAULT_HINT.to_string());
    }
    if stale() {
        return;
    }

    // ---------- 阶段 2：path: 树下任意深度 ----------
    let q2 = clipx_everything::query(
        &build_path_subtree_scoped_search(folder, typing),
        max as u32,
        IPC_FAST,
    );
    if stale() {
        return;
    }
    let p2 = scoped_hits(items_of(&q2), folder, needle);
    let ok_local = q1.is_ok() || q2.is_ok() || !p1.is_empty();
    let local = merge_prefer_first(p1, p2, max);
    if ok_local && !local.is_empty() {
        let n = local.len();
        let rows = from_full_paths(local.clone(), folder, needle);
        post(rows, format!("{n} 项"), DEFAULT_HINT.to_string());
    }
    if stale() {
        return;
    }

    // ---------- 阶段 3：全盘关键词（失败不覆盖已有本地结果） ----------
    let q3 = clipx_everything::query(typing_trim, global_ask(max), IPC_GLOBAL);
    if stale() {
        return;
    }
    let (q3_ok, global) = (
        q3.is_ok(),
        not_in_folder(filter_name_hits(items_of(&q3), needle), folder),
    );
    if stale() {
        return;
    }

    match (ok_local, q3_ok) {
        (false, false) => {
            post(Vec::new(), String::new(), "无匹配项".to_string());
        }
        (false, true) => {
            let (rows, _) = from_scoped_and_global(Vec::new(), global, folder, needle, max);
            if rows.is_empty() {
                post(Vec::new(), String::new(), "无匹配项".to_string());
            } else {
                let n = rows.len();
                post(rows, format!("{n} 项（全盘）"), DEFAULT_HINT.to_string());
            }
        }
        (true, false) => {
            let rows = from_full_paths(local, folder, needle);
            if rows.is_empty() {
                post(Vec::new(), String::new(), "无匹配项".to_string());
            } else {
                let n = rows.len();
                post(rows, format!("{n} 项"), DEFAULT_HINT.to_string());
            }
        }
        (true, true) => {
            let (rows, n_global) = from_scoped_and_global(local, global, folder, needle, max);
            if rows.is_empty() {
                post(Vec::new(), String::new(), "无匹配项".to_string());
            } else {
                let n = rows.len();
                post(rows, qf_count_label(n, n_global), DEFAULT_HINT.to_string());
            }
        }
    }
}

fn qf_count_label(total: usize, n_global: usize) -> String {
    if n_global > 0 {
        format!(
            "共 {} 项（当前路径 {} · 全盘 {}）",
            total,
            total - n_global,
            n_global
        )
    } else {
        format!("{total} 项")
    }
}

/// 当前文件夹一层（文件系统兜底）。Everything 服务在 session 0 / parent: 空结果时使用。
fn list_folder_children(folder: &str, typing: &str, max: usize) -> Vec<ResultItem> {
    let Ok(rd) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let kw = typing.trim();
    let mut items = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !text_matches_query(&name, kw) {
            continue;
        }
        let is_folder = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let full = ent.path().to_string_lossy().replace('/', "\\");
        items.push(ResultItem {
            full_path: full,
            file_name: name,
            is_folder,
            is_drive: false,
            name_hl: Vec::new(),
        });
    }
    items.sort_by(|a, b| {
        a.is_folder
            .cmp(&b.is_folder)
            .reverse()
            .then_with(|| a.file_name.to_lowercase().cmp(&b.file_name.to_lowercase()))
    });
    items.truncate(max);
    items
}

fn items_of(
    r: &Result<clipx_everything::QueryResults, clipx_everything::QueryError>,
) -> Vec<ResultItem> {
    r.as_ref().map(|x| x.items.clone()).unwrap_or_default()
}

fn is_ipc_down(r: &Result<clipx_everything::QueryResults, clipx_everything::QueryError>) -> bool {
    matches!(
        r,
        Err(clipx_everything::QueryError::NotRunning)
            | Err(clipx_everything::QueryError::SendFailed)
            | Err(clipx_everything::QueryError::Timeout)
            | Err(clipx_everything::QueryError::ReplyWindowFailed)
    )
}

fn filter_name_hits(mut items: Vec<ResultItem>, needle: Option<&str>) -> Vec<ResultItem> {
    let Some(n) = needle.filter(|s| !s.is_empty()) else {
        return items;
    };
    items.retain(|it| text_matches_query(&it.file_name, n) || text_matches_query(&it.full_path, n));
    items
}

fn global_ask(max: usize) -> u32 {
    (max.saturating_mul(4)).clamp(64, 500) as u32
}

fn not_in_folder(items: Vec<ResultItem>, folder: &str) -> Vec<ResultItem> {
    if folder.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|it| {
            strip_base(&it.full_path, folder).is_none()
                && !it.full_path.eq_ignore_ascii_case(folder)
        })
        .collect()
}

fn in_folder(items: Vec<ResultItem>, folder: &str) -> Vec<ResultItem> {
    if folder.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|it| {
            strip_base(&it.full_path, folder).is_some() || it.full_path.eq_ignore_ascii_case(folder)
        })
        .collect()
}

/// 丢弃 IPC 错包带来的盘外噪音；文件名也必须含关键词。
fn scoped_hits(items: Vec<ResultItem>, folder: &str, needle: Option<&str>) -> Vec<ResultItem> {
    let mut items = filter_name_hits(items, needle);
    if folder.is_empty() {
        return items;
    }
    items.retain(|it| {
        strip_base(&it.full_path, folder).is_some() || it.full_path.eq_ignore_ascii_case(folder)
    });
    items
}

// ===================== 结果构造（对齐 WPF QuickFindResultItem） =====================

/// 保留 first 顺序，追加 second 中未出现的路径，容量封顶 max。
fn merge_prefer_first(first: Vec<ResultItem>, second: Vec<ResultItem>, max: usize) -> Vec<ResultItem> {
    let mut seen = HashSet::new();
    let mut merged = Vec::with_capacity(max.min(first.len() + second.len()));
    for it in first.into_iter().chain(second) {
        if seen.insert(it.full_path.to_lowercase()) {
            merged.push(it);
            if merged.len() >= max {
                break;
            }
        }
    }
    merged
}

/// 当前路径结果在前（深度+名称排序），全盘补充项在后（路径排序，标记 is_global）。
fn from_scoped_and_global(
    scoped: Vec<ResultItem>,
    global: Vec<ResultItem>,
    base: &str,
    needle: Option<&str>,
    max: usize,
) -> (Vec<QfRowSrc>, usize) {
    let mut seen = HashSet::new();
    let mut scoped_kept = Vec::new();
    for it in scoped {
        if seen.insert(it.full_path.to_lowercase()) {
            scoped_kept.push(it);
            if scoped_kept.len() >= max {
                break;
            }
        }
    }
    let mut global_kept = Vec::new();
    for it in global {
        if seen.insert(it.full_path.to_lowercase()) {
            global_kept.push(it);
        }
    }
    if !global_kept.is_empty() {
        let reserve = (max / 4).clamp(1, 40).min(global_kept.len()).min(max.saturating_sub(1));
        let local_cap = max.saturating_sub(reserve).max(1);
        scoped_kept.truncate(local_cap);
        global_kept.truncate(max.saturating_sub(scoped_kept.len()));
    }
    let n_global = global_kept.len();
    global_kept.sort_by(|a, b| a.full_path.to_lowercase().cmp(&b.full_path.to_lowercase()));

    let mut rows = from_full_paths(scoped_kept, base, needle);
    rows.extend(
        from_full_paths(global_kept, base, needle)
            .into_iter()
            .map(|mut r| {
                r.is_global = true;
                r
            }),
    );
    reindex_labels(&mut rows);
    (rows, n_global)
}

/// 相对路径深度优先（直属子项最前），同级按名称（对齐 WPF FromFullPaths 排序）。
fn from_full_paths(items: Vec<ResultItem>, base: &str, needle: Option<&str>) -> Vec<QfRowSrc> {
    struct Pending {
        item: ResultItem,
        name: String,
        rel: String,
    }
    let mut list: Vec<Pending> = items
        .into_iter()
        .map(|it| {
            let trimmed = it.full_path.trim_end_matches(['\\', '/']);
            let name = if it.file_name.is_empty() {
                file_name_of(trimmed).to_string()
            } else {
                it.file_name.clone()
            };
            let rel = strip_base(trimmed, base).unwrap_or(&name).to_string();
            Pending { item: it, name, rel }
        })
        .collect();
    list.sort_by(|a, b| {
        let da = a.rel.matches(['\\', '/']).count();
        let db = b.rel.matches(['\\', '/']).count();
        da.cmp(&db)
            .then_with(|| a.rel.to_lowercase().cmp(&b.rel.to_lowercase()))
    });
    let mut rows: Vec<QfRowSrc> = list
        .into_iter()
        .map(|p| {
            let (name_pre, name_hit, name_post) =
                split_name(&p.name, needle, &p.item.name_hl);
            // rel 与文件名相同（直属子项/非根下）时不重复展示（对齐 WPF）
            let rel = if !p.rel.is_empty() && !p.rel.eq_ignore_ascii_case(&p.name) {
                p.rel
            } else {
                String::new()
            };
            QfRowSrc {
                full_path: p.item.full_path,
                name_pre,
                name_hit,
                name_post,
                rel,
                is_folder: p.item.is_folder,
                is_global: false,
                index_label: String::new(),
            }
        })
        .collect();
    reindex_labels(&mut rows);
    rows
}

/// 前 9 行标注序号（Ctrl+1..9 快选提示）。
fn reindex_labels(rows: &mut [QfRowSrc]) {
    for (i, r) in rows.iter_mut().enumerate() {
        r.index_label = if i < 9 {
            (i + 1).to_string()
        } else {
            String::new()
        };
    }
}

fn file_name_of(path: &str) -> &str {
    match path.rfind(['\\', '/']) {
        Some(pos) => &path[pos + 1..],
        None => path,
    }
}

/// path 位于 base 之下时返回相对部分（大小写不敏感；ASCII 路径足够）。
/// 对齐 WPF：等价于 trimmed.StartsWith(base + "\") 剥离，其余回退 None。
fn strip_base<'a>(path: &'a str, base: &str) -> Option<&'a str> {
    let p = path.trim_end_matches(['\\', '/']);
    let b = base.trim_end_matches(['\\', '/']);
    if b.is_empty() || p.len() <= b.len() {
        return None;
    }
    if let Some(prefix) = p.get(..b.len()) {
        if prefix.eq_ignore_ascii_case(b) {
            let rest = &p[b.len()..];
            if let Some(stripped) = rest.strip_prefix('\\').or_else(|| rest.strip_prefix('/')) {
                return Some(stripped);
            }
        }
    }
    None
}

fn split_name(
    text: &str,
    needle: Option<&str>,
    hl: &[(u32, u32)],
) -> (String, String, String) {
    if let Some((s, e)) = covering_hl(hl, text) {
        return chars_split(text, s, e);
    }
    split_hit(text, needle)
}

fn covering_hl(hl: &[(u32, u32)], text: &str) -> Option<(usize, usize)> {
    let n = text.chars().count();
    let mut start = u32::MAX;
    let mut end = 0u32;
    let mut any = false;
    for &(a, b) in hl {
        if b > a && (a as usize) < n {
            any = true;
            start = start.min(a);
            end = end.max(b);
        }
    }
    if any {
        Some((start as usize, (end as usize).min(n)))
    } else {
        None
    }
}

fn chars_split(text: &str, start: usize, end: usize) -> (String, String, String) {
    let t: Vec<char> = text.chars().collect();
    let s = start.min(t.len());
    let e = end.min(t.len()).max(s);
    (
        t[..s].iter().collect(),
        t[s..e].iter().collect(),
        t[e..].iter().collect(),
    )
}

/// 大小写不敏感的首次命中切分（无命中/空 needle → 整段入 pre）。
/// 含空格时先试整句，再试第一段；再不行按拼音/首字母标出对应汉字。
fn split_hit(text: &str, needle: Option<&str>) -> (String, String, String) {
    let whole = || (text.to_string(), String::new(), String::new());
    let Some(n) = needle.filter(|n| !n.is_empty()) else {
        return whole();
    };
    if let Some(hit) = split_hit_literal(text, n) {
        return hit;
    }
    if n.chars().any(char::is_whitespace) {
        if let Some(first) = n.split_whitespace().next() {
            if let Some(hit) = split_hit_literal(text, first) {
                return hit;
            }
        }
    }
    if let Some((s, e)) = pinyin_hit_span(text, n) {
        return chars_split(text, s, e);
    }
    whole()
}

fn split_hit_literal(text: &str, n: &str) -> Option<(String, String, String)> {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = n.chars().collect();
    if p.is_empty() || t.len() < p.len() {
        return None;
    }
    let eq_ci = |a: char, b: char| {
        a.eq_ignore_ascii_case(&b) || a.to_lowercase().eq(b.to_lowercase())
    };
    for start in 0..=t.len() - p.len() {
        if t[start..start + p.len()]
            .iter()
            .zip(&p)
            .all(|(a, b)| eq_ci(*a, *b))
        {
            return Some((
                t[..start].iter().collect(),
                t[start..start + p.len()].iter().collect(),
                t[start + p.len()..].iter().collect(),
            ));
        }
    }
    None
}

// ===================== UI 推送 =====================

struct QfUiBundle {
    rows: Vec<QfRowSrc>,
    selected: i32,
    folder_label: String,
    typing_text: String,
    count_label: String,
    hint_label: String,
    show: bool,
    frame: isize,
    win_h: f32,
}

fn push_ui(state: &QfState, weak: &slint::Weak<QuickFindWindow>, show: bool) {
    let n = (state.items.len().clamp(1, 10)) as f32;
    let win_h = (QF_UI + n * QF_ROW + QF_CHROME * 2.0).clamp(QF_MIN_H, QF_MAX_H);
    let bundle = QfUiBundle {
        rows: state.items.clone(),
        selected: state.selected as i32,
        folder_label: state.folder_display.clone(),
        typing_text: if state.typing.is_empty() {
            " ".to_string()
        } else {
            state.typing.clone()
        },
        count_label: state.count_label.clone(),
        hint_label: state.hint_label.clone(),
        show,
        frame: state.frame,
        win_h,
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        if bundle.show {
            crate::settings_win::paint_theme_handle(ui.global::<crate::Theme>());
        }
        let rows: Vec<QfRow> = bundle
            .rows
            .into_iter()
            .map(|r| QfRow {
                full_path: r.full_path.into(),
                name_pre: r.name_pre.into(),
                name_hit: r.name_hit.into(),
                name_post: r.name_post.into(),
                rel: r.rel.into(),
                is_folder: r.is_folder,
                is_global: r.is_global,
                index_label: r.index_label.into(),
            })
            .collect();
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_selected_index(bundle.selected);
        ui.set_folder_label(bundle.folder_label.into());
        ui.set_typing_text(bundle.typing_text.into());
        ui.set_count_label(bundle.count_label.into());
        ui.set_hint_label(bundle.hint_label.into());
        let win = ui.window();
        if bundle.show {
            let _ = win.show();
            win_popup::position_near_explorer(win, bundle.frame, WIN_W, bundle.win_h);
            win_popup::apply_style(win);
            win_popup::sync_logical_size(win, WIN_W, bundle.win_h);
            win_popup::position_near_explorer(win, bundle.frame, WIN_W, bundle.win_h);
        } else {
            win_popup::sync_logical_size(win, WIN_W, bundle.win_h);
        }
    });
}

// ===================== Shell 平台垫片（非 Windows 编译完整性） =====================

#[cfg(windows)]
fn shell_desktop_dir() -> Option<String> {
    crate::explorer_shell::desktop_dir()
}

#[cfg(not(windows))]
fn shell_desktop_dir() -> Option<String> {
    None
}

#[cfg(windows)]
fn shell_explorer_folder_path(frame: isize) -> Option<String> {
    crate::explorer_shell::explorer_folder_path(frame)
}

#[cfg(not(windows))]
fn shell_explorer_folder_path(_frame: isize) -> Option<String> {
    None
}

#[cfg(windows)]
fn shell_navigate_and_select(frame: isize, target: &str) -> bool {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    unsafe {
        let co = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let ok = crate::explorer_shell::navigate_and_select(frame, target);
        if co.is_ok() {
            CoUninitialize();
        }
        ok
    }
}

#[cfg(not(windows))]
fn shell_navigate_and_select(_frame: isize, _target: &str) -> bool {
    false
}

#[cfg(windows)]
fn shell_execute_path(path: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::Foundation::HWND;
    unsafe {
        let _ = ShellExecuteW(
            Some(HWND::default()),
            windows::core::w!("open"),
            &HSTRING::from(path),
            None,
            None,
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
}

#[cfg(not(windows))]
fn shell_execute_path(_path: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, name: &str, folder: bool) -> ResultItem {
        ResultItem {
            full_path: path.to_string(),
            file_name: name.to_string(),
            is_folder: folder,
            is_drive: false,
            name_hl: Vec::new(),
        }
    }

    #[test]
    fn split_hit_case_insensitive_first() {
        assert_eq!(
            split_hit("Readme.txt", Some("read")),
            ("".into(), "Read".into(), "me.txt".into())
        );
        assert_eq!(
            split_hit("报告v2.md", Some("v2")),
            ("报告".into(), "v2".into(), ".md".into())
        );
        assert_eq!(
            split_hit("abc", Some("zz")),
            ("abc".into(), "".into(), "".into())
        );
        assert_eq!(
            split_hit("abc", None),
            ("abc".into(), "".into(), "".into())
        );
        assert_eq!(
            split_hit("ai-edu-dataset", Some("ai edu")),
            ("".into(), "ai".into(), "-edu-dataset".into())
        );
        assert_eq!(
            split_hit("马春天.pdf", Some("machuntian")),
            ("".into(), "马春天".into(), ".pdf".into())
        );
        assert_eq!(
            split_hit("马春天.pdf", Some("mct")),
            ("".into(), "马春天".into(), ".pdf".into())
        );
        assert_eq!(
            split_hit("报告_马春天_v2.pdf", Some("machuntian")),
            ("报告_".into(), "马春天".into(), "_v2.pdf".into())
        );
        assert_eq!(
            split_name("马春天.pdf", Some("xx"), &[(0, 3)]),
            ("".into(), "马春天".into(), ".pdf".into())
        );
    }

    #[test]
    fn strip_base_variants() {
        assert_eq!(strip_base(r"C:\foo\bar\a.txt", r"C:\foo\bar"), Some("a.txt"));
        assert_eq!(strip_base(r"c:\FOO\BAR\sub\x", r"C:\foo\bar"), Some(r"sub\x"));
        assert_eq!(strip_base(r"C:\other\a", r"C:\foo"), None);
        // 路径即 base 本身（含尾分隔符防御性归一）：rel 回退为文件名
        assert_eq!(strip_base(r"C:\foo", r"C:\foo"), None);
        assert_eq!(strip_base(r"C:\foo\", r"C:\foo"), None);
        assert_eq!(strip_base("x", ""), None);
    }

    #[test]
    fn merge_prefers_first_and_dedupes() {
        let a = vec![item("C:\\a", "a", false), item("C:\\b", "b", true)];
        let b = vec![
            item("c:\\B", "B", true),
            item("C:\\c", "c", false),
        ];
        let m = merge_prefer_first(a, b, 10);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].full_path, "C:\\a");
        assert_eq!(m[1].full_path, "C:\\b");
        assert_eq!(m[2].full_path, "C:\\c");
    }

    #[test]
    fn from_full_paths_sorts_by_depth_then_name() {
        let items = vec![
            item(r"C:\f\nested\deep\z.txt", "z.txt", false),
            item(r"C:\f\a.txt", "a.txt", false),
            item(r"C:\f\nested\b.txt", "b.txt", false),
        ];
        let rows = from_full_paths(items, r"C:\f", Some("txt"));
        assert_eq!(rows[0].name_pre, "a.");
        assert_eq!(rows[0].name_hit, "txt");
        assert!(rows[0].rel.is_empty(), "直属子项 rel 与名称相同不展示");
        assert_eq!(rows[1].rel, r"nested\b.txt");
        assert_eq!(rows[2].rel, r"nested\deep\z.txt");
        assert_eq!(rows[0].index_label, "1");
    }

    #[test]
    fn list_folder_children_filters_and_sorts() {
        let dir = std::env::temp_dir().join(format!(
            "clipx-qf-fs-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Readme.txt"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("other.md"), b"y").unwrap();
        let rows = list_folder_children(&dir.to_string_lossy(), "read", 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_name, "Readme.txt");
        let all = list_folder_children(&dir.to_string_lossy(), "", 10);
        assert_eq!(all.len(), 3);
        assert!(all[0].is_folder, "文件夹排前");
        std::fs::create_dir_all(dir.join("青松资料")).unwrap();
        let py = list_folder_children(&dir.to_string_lossy(), "qs", 10);
        assert!(py.iter().any(|r| r.file_name == "青松资料"), "拼音首字母");
        let spaced = list_folder_children(&dir.to_string_lossy(), "read xyz", 10);
        assert!(spaced.is_empty());
        std::fs::write(dir.join("ai-edu-notes.txt"), b"z").unwrap();
        let and_tokens = list_folder_children(&dir.to_string_lossy(), "ai edu", 10);
        assert_eq!(and_tokens.len(), 1);
        assert_eq!(and_tokens[0].file_name, "ai-edu-notes.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoped_hits_drops_off_folder_and_name_miss() {
        let folder = r"C:\Users\chaoj\Desktop";
        let items = vec![
            item(r"C:\Users\chaoj\Desktop\ai-edu-dataset", "ai-edu-dataset", true),
            item(r"C:\Windows\ai.dll", "ai.dll", false),
            item(r"C:\Users\chaoj\Desktop\readme.txt", "readme.txt", false),
        ];
        let kept = scoped_hits(items, folder, Some("ai"));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].file_name, "ai-edu-dataset");
    }

    #[test]
    fn scoped_hits_keeps_pinyin_chinese_filename() {
        let folder = r"C:\Users\chaoj\Desktop";
        let items = vec![
            item(
                r"C:\Users\chaoj\Desktop\马春天.pdf",
                "马春天.pdf",
                false,
            ),
            item(r"C:\Windows\machuntian.txt", "machuntian.txt", false),
        ];
        let kept = scoped_hits(items, folder, Some("machuntian"));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].file_name, "马春天.pdf");
    }

    #[test]
    fn scoped_and_global_marks_and_orders() {
        let scoped = vec![item(r"C:\f\a.txt", "a.txt", false)];
        let global = vec![
            item(r"C:\z\y.txt", "y.txt", false),
            item(r"C:\b\x.txt", "x.txt", false),
        ];
        let (rows, n_global) = from_scoped_and_global(scoped, global, r"C:\f", None, 10);
        assert_eq!(n_global, 2);
        assert_eq!(rows.len(), 3);
        assert!(!rows[0].is_global);
        assert_eq!(rows[1].full_path, r"C:\b\x.txt");
        assert!(rows[1].is_global && rows[2].is_global);
        assert_eq!(rows[1].index_label, "2");
        assert_eq!(rows[2].index_label, "3");
    }

    #[test]
    fn scoped_and_global_reserves_slots_when_local_fills_max() {
        let scoped: Vec<_> = (0..10)
            .map(|i| item(&format!(r"C:\f\{i}.txt"), &format!("{i}.txt"), false))
            .collect();
        let global = vec![item(r"C:\z\hit.txt", "hit.txt", false)];
        let (rows, n_global) = from_scoped_and_global(scoped, global, r"C:\f", None, 10);
        assert!(n_global > 0, "本地填满时仍应保留全盘槽位");
        assert!(rows.iter().any(|r| r.is_global && r.full_path.ends_with("hit.txt")));
        assert_eq!(rows.len(), 10);
    }
}
