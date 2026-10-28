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
    /// 跟随鼠标（默认 true；设为 Explorer 则贴资源管理器窗口，见 push_ui）。
    pub follow_mouse: bool,
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
            follow_mouse: true,
        }
    }
}

// ===================== 插件壳（Step 2a）：只改接收者，不改逻辑 =====================
//
// QfState 与全部自由函数原样保留；新增薄壳 `QfPlugin` 持有状态，`logic.rs`
// 的调用点从"自由函数 + &mut state.qf"改为"插件方法"，行为逐行一致。
// 事件包络（`AppEvt::Qf*`）与 keyboard_hook 生产侧本次不动，分 2b 收敛。
// trait 化（`CapabilityPlugin` 注册表）同样在 2b；2a 的唯一目标是"改地址不改逻辑"。
pub struct QfPlugin {
    state: QfState,
}

impl Default for QfPlugin {
    fn default() -> Self {
        Self {
            state: QfState::default(),
        }
    }
}

impl crate::plugin::AppPlugin for QfPlugin {
    fn id(&self) -> &'static str {
        Self::ID
    }

    /// 包络分发入口：`logic.rs` 的 `AppEvt::PluginEvt` arm 唯一调用点。
    /// 四个分支即原四个 match arm 的整体搬家（含 `!visible` 防御与设置透传）。
    fn on_plugin_event(
        &mut self,
        payload: crate::plugin::PluginPayload,
        ctx: &crate::plugin::PluginCtx,
    ) {
        use crate::plugin::PluginPayload as P;
        match payload {
            P::QfKey(k) => {
                // 主弹窗可见期间钩子不会产生 Qf 事件，此处防御性忽略
                if !ctx.main_visible {
                    let max = ctx.settings.explorer_everything_quickfind_max_results;
                    self.handle_key(
                        k,
                        max,
                        ctx.evt_tx,
                        ctx.qf,
                        &ctx.settings.explorer_quickfind_open_mode,
                        &ctx.settings.explorer_quickfind_follow_mode,
                    );
                }
            }
            P::QfRowActivated(i) => {
                self.activate_index(
                    i.max(0) as usize,
                    ctx.qf,
                    &ctx.settings.explorer_quickfind_open_mode,
                );
            }
            P::QfFolderResolved { gen, folder } => {
                let max = ctx.settings.explorer_everything_quickfind_max_results;
                self.apply_folder(gen, folder, max, ctx.evt_tx, ctx.qf);
            }
            P::QfResults { gen, view } => {
                self.apply_results(gen, view, ctx.qf);
            }
        }
    }
}

impl QfPlugin {
    /// 插件路由键（生产点与分发点共用，改名只改这一处）。
    pub const ID: &'static str = "quickfind";

    pub fn handle_key(
        &mut self,
        k: KeyEvt,
        max_results: u32,
        tx: &std::sync::mpsc::Sender<crate::logic::AppEvt>,
        weak: &slint::Weak<QuickFindWindow>,
        open_mode: &str,
        follow_mode: &str,
    ) {
        handle_key(
            k,
            &mut self.state,
            max_results,
            tx,
            weak,
            open_mode,
            follow_mode,
        )
    }

    pub fn apply_folder(
        &mut self,
        gen: u64,
        folder: Option<String>,
        max_results: u32,
        tx: &std::sync::mpsc::Sender<crate::logic::AppEvt>,
        weak: &slint::Weak<QuickFindWindow>,
    ) {
        apply_folder(gen, folder, &mut self.state, max_results, tx, weak)
    }

    pub fn apply_results(
        &mut self,
        gen: u64,
        view: QfView,
        weak: &slint::Weak<QuickFindWindow>,
    ) {
        apply_results(gen, view, &mut self.state, weak)
    }

    pub fn activate_index(
        &mut self,
        idx: usize,
        weak: &slint::Weak<QuickFindWindow>,
        open_mode: &str,
    ) {
        activate_index(idx, &mut self.state, weak, open_mode)
    }

    pub fn end_if_active(&mut self, weak: &slint::Weak<QuickFindWindow>) {
        end_if_active(&mut self.state, weak)
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
    follow_mode: &str,
) {
    state.direct_open = open_mode == "DirectOpen";
    state.follow_mouse = follow_mode != "Explorer";
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
                let _ = tx.send(crate::logic::AppEvt::PluginEvt {
                    plugin: QfPlugin::ID,
                    payload: crate::plugin::PluginPayload::QfFolderResolved { gen, folder },
                });
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

/// Enter/快选/点击：结束会话 → 后台线程导航选中（M4-c）。
/// 导航含 Navigate 轮询等待（最长 ~500ms），不得占用逻辑线程。
/// 分流：当前路径段（`is_global == false`）就地复用当前窗口导航+选中；
/// 全盘段则新开/复用目标窗口（`SHOpenFolderAndSelectItems`），不动当前窗口。
fn activate(state: &mut QfState, idx: usize, weak: &slint::Weak<QuickFindWindow>) {
    let Some(row) = state.items.get(idx) else {
        return;
    };
    let path = row.full_path.clone();
    let is_global = row.is_global;
    let folder = state.folder.clone();
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
            } else if !is_global && !folder.is_empty() {
                // 本地命中：资源管理器窗口就地导航+选中；桌面检索根走
                // DefView 就地选中图标（桌面不在 ShellWindows 枚举里，
                // COM 永远匹配不上，之前恒掉新窗口兜底）。
                #[cfg(windows)]
                {
                    if is_desktop_folder(&folder) {
                        let ok = shell_desktop_select(&path);
                        crate::win_popup::write_data_log(
                            "qf_desk.log",
                            &format!("activate folder={folder} target={path} desk_ok={ok}"),
                        );
                        if !ok {
                            shell_reveal(&path);
                        }
                    } else {
                        shell_navigate_and_select(frame, &path);
                    }
                }
                #[cfg(not(windows))]
                {
                    shell_navigate_and_select(frame, &path);
                }
            } else {
                // 全盘命中/无检索根：不动当前窗口，另起窗口定位。
                shell_reveal(&path);
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
        let _ = tx.send(crate::logic::AppEvt::PluginEvt {
            plugin: QfPlugin::ID,
            payload: crate::plugin::PluginPayload::QfResults {
                gen,
                view: QfView {
                    rows,
                    count_label: count,
                    hint_label: hint,
                },
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
    // FS 直属子项优先（精确/浅层命中必须先保住，引擎排序深层在前时
    // 若把引擎放前面，`merge_prefer_first` 截断会直接丢掉 `C:\...\dev`
    // 这类直属精确匹配——与兜底分支 `merge_prefer_first(fs, ev1)` 同口径）。
    match clipx_everything::query(typing_trim, global_ask(max), IPC_QF) {
        Ok(r) => {
            let local = merge_prefer_first(fs, in_folder(r.items.clone(), folder), max);
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
        // 精确名匹配最前（`dev` 查询时直属 `dev` 文件夹不被字母序淹没），
        // 再文件夹优先、同级按名称。
        exact_rank(a, kw)
            .cmp(&exact_rank(b, kw))
            .then_with(|| {
                a.is_folder
                    .cmp(&b.is_folder)
                    .reverse()
                    .then_with(|| a.file_name.to_lowercase().cmp(&b.file_name.to_lowercase()))
            })
    });
    items.truncate(max);
    items
}

/// 文件名与检索词精确相等（大小写不敏感）则为 0，否则为 1，用于排序提权。
fn exact_rank(item: &ResultItem, kw: &str) -> u8 {
    let q = kw.trim();
    if q.is_empty() {
        return 1;
    }
    let name = if item.file_name.is_empty() {
        file_name_of(item.full_path.trim_end_matches(['\\', '/']))
    } else {
        item.file_name.as_str()
    };
    if name.eq_ignore_ascii_case(q) {
        0
    } else {
        1
    }
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
        }
    }
    // 截断前先按（精确名匹配 → 相对深度 → 名称）排序：引擎返回多为深层
    // 在前，直接截断会丢掉直属 `dev` 这类浅层精确命中。排序后再截，
    // 保证精确/浅层先保住（与最终 `from_full_paths` 展示顺序同口径）。
    let kw = needle.unwrap_or("");
    scoped_kept.sort_by(|a, b| {
        exact_rank(a, kw)
            .cmp(&exact_rank(b, kw))
            .then_with(|| rel_depth(&a.full_path, base).cmp(&rel_depth(&b.full_path, base)))
            .then_with(|| a.full_path.to_lowercase().cmp(&b.full_path.to_lowercase()))
    });
    scoped_kept.truncate(max);
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
        // 精确名匹配最前，再深度、再名称（与截断侧 `scoped_kept` 排序同口径，
        // 否则截住的顺序和展示的顺序会打架）。
        let kw = needle.unwrap_or("");
        let ea = if a.name.eq_ignore_ascii_case(kw.trim()) { 0u8 } else { 1u8 };
        let eb = if b.name.eq_ignore_ascii_case(kw.trim()) { 0u8 } else { 1u8 };
        let da = a.rel.matches(['\\', '/']).count();
        let db = b.rel.matches(['\\', '/']).count();
        ea.cmp(&eb)
            .then_with(|| da.cmp(&db))
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

/// 相对深度（直属子项=0）：路径即 base 本身也算 0；盘外噪音回 `usize::MAX` 沉底。
fn rel_depth(path: &str, base: &str) -> usize {
    if path.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(base.trim_end_matches(['\\', '/'])) {
        return 0;
    }
    match strip_base(path, base) {
        Some(rel) => rel.matches(['\\', '/']).count(),
        None => usize::MAX,
    }
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
    follow_mouse: bool,
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
        follow_mouse: state.follow_mouse,
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        if bundle.show {
            crate::settings_win::paint_theme_handle(ui.global::<crate::Theme>());
        }
        // 定位二选一用的标量先拷出来：下面 rows 会把 bundle 部分移动，
        // 闭包再借整个 bundle 会撞「部分移动后借用」。
        let follow_mouse = bundle.follow_mouse;
        let frame = bundle.frame;
        let place_h = bundle.win_h;
        let rows: Vec<QfRow> = bundle.rows
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
        // 定位二选一：跟鼠标（默认）或贴资源管理器（旧行为，可配）。
        // position_near_explorer 自带 frame==0 回退到鼠标，Mouse 分支直接走鼠标。
        let place = |w: &slint::Window| {
            if follow_mouse {
                win_popup::position_near_cursor(w, WIN_W, place_h);
            } else {
                win_popup::position_near_explorer(w, frame, WIN_W, place_h);
            }
        };
        if bundle.show {
            // 顺序要紧：先在**隐藏态**把高度抖小 1px，再 show（首帧即重建缓冲、全量重绘），
            // 下一帧再恢复目标高度。详见 win_popup::force_full_repaint_before_show 注释。
            win_popup::force_full_repaint_before_show(win, WIN_W, bundle.win_h);
            let _ = win.show();
            place(win);
            win_popup::apply_style(win);
            win_popup::sync_logical_size(win, WIN_W, bundle.win_h);
            win_popup::restore_size_after_show(&weak, WIN_W, bundle.win_h);
            place(win);
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
fn is_desktop_folder(folder: &str) -> bool {
    match shell_desktop_dir() {
        Some(d) => {
            let a = folder.trim_end_matches(['\\', '/']);
            let b = d.trim_end_matches(['\\', '/']);
            !b.is_empty() && a.eq_ignore_ascii_case(b)
        }
        None => false,
    }
}

/// 桌面就地选中（DefView），失败由调用方回退 reveal。
#[cfg(windows)]
fn shell_desktop_select(target: &str) -> bool {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    unsafe {
        let co = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let ok = crate::explorer_shell::select_on_desktop(target);
        if co.is_ok() {
            CoUninitialize();
        }
        ok
    }
}

/// 全盘命中定位：不动当前窗口，复用/新开目标窗口并选中。
#[cfg(windows)]
fn shell_reveal(target: &str) -> bool {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    unsafe {
        let co = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let ok = crate::explorer_shell::reveal_in_explorer(target);
        if co.is_ok() {
            CoUninitialize();
        }
        ok
    }
}

#[cfg(not(windows))]
fn shell_reveal(_target: &str) -> bool {
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

    #[test]
    fn exact_shallow_survives_deep_engine_flood() {
        // 回归：`C:\Users\chaoj` 下搜 `dev`，引擎先回 9 个深层命中，
        // 直属 `dev` 在合并输入里垫底，也必须在截断后存活并置顶。
        let folder = r"C:\Users\chaoj";
        let mut scoped = Vec::new();
        for i in 0..9 {
            scoped.push(item(
                &format!(r"C:\Users\chaoj\other{i}\dev-cache-{i}.tmp"),
                &format!("dev-cache-{i}.tmp"),
                false,
            ));
        }
        scoped.push(item(r"C:\Users\chaoj\dev", "dev", true));
        let (rows, _) = from_scoped_and_global(scoped, Vec::new(), folder, Some("dev"), 10);
        assert!(
            rows.iter().any(|r| r.full_path.eq_ignore_ascii_case(r"C:\Users\chaoj\dev")),
            "直属精确匹配不应被深层结果挤掉"
        );
        assert_eq!(
            rows[0].full_path.to_lowercase(),
            r"c:\users\chaoj\dev",
            "精确匹配应置顶"
        );
    }

    #[test]
    fn exact_shallow_survives_global_reserve_truncation() {
        // 本地 10 项 + 全盘 1 项，max=10 时本地要让出槽位；
        // 精确浅层仍不能被让位逻辑挤掉。
        let folder = r"C:\f";
        let mut scoped = vec![item(r"C:\f\dev", "dev", true)];
        for i in 0..9 {
            scoped.push(item(
                &format!(r"C:\f\sub{i}\dev-{i}.txt"),
                &format!("dev-{i}.txt"),
                false,
            ));
        }
        // 故意把精确项放最后，模拟引擎排序深层在前。
        scoped.rotate_left(1);
        let global = vec![item(r"C:\z\dev-global.txt", "dev-global.txt", false)];
        let (rows, n_global) = from_scoped_and_global(scoped, global, folder, Some("dev"), 10);
        assert!(n_global > 0);
        assert!(
            rows.iter().any(|r| r.full_path.eq_ignore_ascii_case(r"C:\f\dev")),
            "预留全盘槽位时也不能丢精确直属项"
        );
    }
}

/// 插件壳语义锁（Step 2a）：代际丢弃 + 会话清理 + 壳委托。死 weak + 真 channel，
/// 不碰 UI、不碰 Everything（`push_ui` 空 weak 直接返回，`invoke_from_event_loop`
/// 在单测里返回 Err 即忽略，`qf_clear_session` 只是原子位）。
#[cfg(test)]
mod plugin_shell_tests {
    use super::*;
    use crate::plugin::AppPlugin;
    use std::sync::atomic::Ordering;

    fn dead_weak() -> slint::Weak<QuickFindWindow> {
        slint::Weak::default()
    }

    fn row(path: &str) -> QfRowSrc {
        QfRowSrc {
            full_path: path.to_string(),
            ..Default::default()
        }
    }

    fn view(paths: &[&str]) -> QfView {
        QfView {
            rows: paths.iter().map(|p| row(p)).collect(),
            count_label: format!("{} 项", paths.len()),
            hint_label: DEFAULT_HINT.to_string(),
        }
    }

    #[test]
    fn plugin_id_is_quickfind() {
        assert_eq!(QfPlugin::default().id(), "quickfind");
    }

    #[test]
    fn stale_gen_results_ignored() {
        let mut p = QfPlugin::default();
        p.state.active = true;
        p.state.gen.store(5, Ordering::SeqCst);
        p.apply_results(4, view(&[r"C:\f\a.txt"]), &dead_weak());
        assert!(p.state.items.is_empty(), "过期代际必须丢弃");
    }

    #[test]
    fn current_gen_results_accepted_and_selection_reset() {
        let mut p = QfPlugin::default();
        p.state.active = true;
        p.state.gen.store(5, Ordering::SeqCst);
        p.state.selected = 3;
        p.apply_results(5, view(&[r"C:\f\a.txt", r"C:\f\b.txt"]), &dead_weak());
        assert_eq!(p.state.items.len(), 2);
        assert_eq!(p.state.selected, 0, "每次回显选中归零（对齐 WPF）");
        assert_eq!(p.state.count_label, "2 项");
    }

    #[test]
    fn stale_gen_folder_ignored() {
        let (tx, _rx) = std::sync::mpsc::channel::<crate::logic::AppEvt>();
        let mut p = QfPlugin::default();
        p.state.active = true;
        p.state.gen.store(5, Ordering::SeqCst);
        p.apply_folder(4, Some(r"C:\f".to_string()), 10, &tx, &dead_weak());
        assert!(!p.state.started, "过期代际不得启动会话");
        assert!(p.state.folder.is_empty());
    }

    #[test]
    fn fresh_folder_starts_session() {
        let (tx, _rx) = std::sync::mpsc::channel::<crate::logic::AppEvt>();
        let mut p = QfPlugin::default();
        p.state.active = true;
        p.state.gen.store(5, Ordering::SeqCst);
        // 注意：此调用会起一个 30ms debounce 的查询线程（gen 已对齐则继续跑
        // Everything 查询；测试只断言同步部分，线程 cit 发送失败即静默丢弃）。
        // 为避免 flaky，这里用空 typing + 全盘 folder("")：run_query 在无 typing
        // 时直接 post 空结果，不碰 Everything。
        p.apply_folder(5, Some(String::new()), 10, &tx, &dead_weak());
        assert!(p.state.started);
        assert_eq!(p.state.folder_display, "全盘");
    }

    #[test]
    fn end_clears_session_and_bumps_gen() {
        let mut p = QfPlugin::default();
        p.state.active = true;
        p.state.started = true;
        p.state.typing = "de".to_string();
        p.state.items = vec![row(r"C:\f\a.txt")];
        let g0 = p.state.gen.load(Ordering::SeqCst);
        p.end_if_active(&dead_weak());
        assert!(!p.state.active);
        assert!(!p.state.started);
        assert!(p.state.typing.is_empty());
        assert!(p.state.items.is_empty());
        assert_eq!(p.state.selected, 0);
        assert!(p.state.gen.load(Ordering::SeqCst) > g0, "end 必须推高代际");
        // 非活动时再调是空操作（不重复 bump）。
        let g1 = p.state.gen.load(Ordering::SeqCst);
        p.end_if_active(&dead_weak());
        assert_eq!(p.state.gen.load(Ordering::SeqCst), g1);
    }

    fn test_ctx<'a>(
        tx: &'a std::sync::mpsc::Sender<crate::logic::AppEvt>,
        weak: &'a slint::Weak<QuickFindWindow>,
        settings: &'a crate::settings::Settings,
        main_visible: bool,
    ) -> crate::plugin::PluginCtx<'a> {
        crate::plugin::PluginCtx {
            settings,
            evt_tx: tx,
            qf: weak,
            main_visible,
        }
    }

    #[test]
    fn dispatch_qfkey_guarded_by_main_visible() {
        use crate::plugin::PluginPayload as P;
        let settings = crate::settings::Settings::default();
        let (tx, _rx) = std::sync::mpsc::channel::<crate::logic::AppEvt>();
        let weak = dead_weak();
        let mut p = QfPlugin::default();
        p.state.active = true;
        // 主窗可见 → 防御性忽略，typing 不动。
        p.on_plugin_event(
            P::QfKey(KeyEvt::QfChar('a')),
            &test_ctx(&tx, &weak, &settings, true),
        );
        assert!(p.state.typing.is_empty());
        // 主窗隐藏 → 正常累积（未 started，不调度查询线程，确定性）。
        p.on_plugin_event(
            P::QfKey(KeyEvt::QfChar('a')),
            &test_ctx(&tx, &weak, &settings, false),
        );
        assert_eq!(p.state.typing, "a");
    }

    #[test]
    fn dispatch_row_activated_inactive_is_noop() {
        use crate::plugin::PluginPayload as P;
        let settings = crate::settings::Settings::default();
        let (tx, _rx) = std::sync::mpsc::channel::<crate::logic::AppEvt>();
        let weak = dead_weak();
        let mut p = QfPlugin::default();
        p.on_plugin_event(P::QfRowActivated(0), &test_ctx(&tx, &weak, &settings, false));
        assert!(!p.state.active);
    }
}
