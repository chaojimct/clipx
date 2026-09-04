//! 逻辑线程：唯一持有弹窗状态（查询/过滤/列表/选中项/预览/可见性），
//! 所有输入源（键盘钩子/鼠标钩子/热键/托盘/处理器/OCR）经 AppEvt 汇入此线程，
//! UI 更新统一经 invoke_from_event_loop 回主线程（channel 模式，全平台约定）。

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use clipboard_rs::ClipboardContext;
use clipx_core::pinyin::to_pinyin_blob;
use clipx_core::{now_ms, time::time_ago, ClipboardGate, EntryKind, EntryMeta};
use clipx_store::Store;
use slint::{ComponentHandle, LogicalSize, ModelRc, SharedString, VecModel, WindowSize};

use crate::keyboard_hook::{self, KeyEvt};
use crate::settings::{QuickPaste, Settings};
use crate::{mouse_hook, paste, win_popup, PopupWindow, RowData};

const PREVIEW_W: f32 = 440.0;
const WIN_MIN_H: f32 = 200.0;
const HEADER_H: f32 = 44.0;
const SEARCH_H: f32 = 40.0;
const FOOTER_H: f32 = 36.0;
/// 圆角卡片外圈留白（阴影），对齐 WPF MainBorder Margin=16。
const POPUP_CHROME: f32 = 16.0;
const ROW_H: f32 = 40.0;
const QUERY_MAX_CHARS: usize = 64;

/// 预览大图解码上限（长边）：4K 图先等比缩小再进 UI，
/// 控制软件渲染下的解码内存峰值（原图 bytes 随即释放）。
const PREVIEW_MAX_DIM: u32 = 1600;

/// 跨线程像素载荷：slint::Image 非 Send，逻辑线程只产原始 RGBA，
/// SharedPixelBuffer/Image 在事件循环线程上组装。
#[derive(Clone, Default)]
struct ImageData {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

#[derive(Debug)]
pub enum AppEvt {
    Key(KeyEvt),
    Toggle,
    Hide,
    ListChanged,
    /// OCR 完成（或新图片入队后处理完）：刷新列表与预览中的 OCR 文本
    OcrDone,
    RowClicked(i32),
    RowDoubleClicked(i32),
    FilterCycle,
    /// 右键某行 / Menu 键：打开上下文菜单
    MenuRequest(i32),
    /// 菜单动作执行
    MenuAction(MenuAction),
    /// 点击菜单外关闭
    MenuClose,
    // ===== 快速查找（M4，Explorer 内 Everything 检索）=====
    /// 钩子快速查找事件（QfStart/会话内按键/会话结束）
    QfKey(KeyEvt),
    /// 快速查找浮层列表点击
    QfRowActivated(i32),
    /// 文件夹解析完成（后台 Shell COM 线程回投）
    QfFolderResolved {
        gen: u64,
        folder: Option<String>,
    },
    /// 查询阶段结果回投（gen 过期则丢弃）
    QfResults {
        gen: u64,
        view: crate::explorer_quickfind::QfView,
    },
    // ===== 文件夹跳转（M5d）=====
    /// 全局 Ctrl+G / 托盘：切换 Picker（有框采集跳转，无框全局模式）
    FileJumpToggle,
    /// 对话框前台自动弹出（watcher 线程）
    FileJumpAuto(isize),
    /// Picker 列表点击
    FjRowActivated(i32),
    /// 后台采集完成（gen 过期则丢弃）
    FjCollected {
        gen: u64,
        items: Vec<clipx_filejump::Candidate>,
    },
    /// Everything 文件夹补充完成（gen 过期则丢弃）
    FjEvResults {
        gen: u64,
        items: Vec<clipx_filejump::Candidate>,
    },
    /// 后台导航完成
    FjNavigated {
        gen: u64,
        path: String,
        ok: bool,
    },
    /// Picker header 手动拖拽（逻辑像素位移）
    FjDragged(f32, f32),
    // ===== 设置窗口（Phase A）=====
    OpenSettings,
    TrayAutostart,
    /// 托盘：暂停 / 继续采集
    TrayPause,
    /// 托盘：清空历史（两段确认）
    TrayClear,
    SettingBool(String),
    SettingCycle(String),
    SettingRecord(i32),
    SettingText(String, String),
    SettingSave,
    SettingCancel,
    SettingClear,
    SettingPage(i32),
    /// 后台进程枚举完成（设置「排除应用」）
    SettingProcs(Vec<String>),
    ExclAdd,
    ExclDel(i32),
    RuleDel(i32),
    CustomDel(i32),
    CustomImport,
    CustomExport,
    /// 批量模式循环切换（Phase B 实现）
    BatchCycle,
    /// 标题栏图钉：钉住弹窗
    PinWindow,
    /// 标题栏拖拽
    PopupDragged(f32, f32),
    /// 中键预览
    MiddlePreview(i32),
    /// 托盘：探测文件对话框向导
    TrayProbe,
    /// 托盘：关于 / 检查更新 / 导入导出
    TrayAbout,
    TrayUpdate,
    TrayExport,
    TrayImport,
    /// 后台更新检查结果
    UpdateAvailable(String),
    /// WinEvent：对话框移动/缩放（实时跟随，重算 dock）
    FjDialogMoved,
    /// WinEvent：对话框销毁（Picker 跟着关闭）
    FjDialogGone,
}

/// 右键上下文菜单动作
#[derive(Debug, Clone, Copy)]
pub enum MenuAction {
    Copy,
    Pin,
    Delete,
    /// 加成短语 / 编辑短语
    Phrase,
    Edit,
    OcrPaste,
    PasteAsFile,
    PasteAsJson,
    SaveImage,
    CopyPath,
    FilterSource,
}

pub struct LogicDeps {
    pub store: Store,
    pub settings: Settings,
    pub gate: ClipboardGate,
    /// 事件回投通道（快速查找/文件跳转的后台线程经此发回结果）
    pub evt_tx: std::sync::mpsc::Sender<AppEvt>,
    /// 快速查找浮层
    pub qf: slint::Weak<crate::QuickFindWindow>,
    /// 文件夹跳转浮层（M5d）
    pub fj: slint::Weak<crate::FileJumpWindow>,
    /// 设置窗口（Phase A）
    pub settings_win: slint::Weak<crate::SettingsWindow>,
    /// 托盘图标（标签/tooltip 刷新；无托盘时为空）
    pub tray: Option<slint::Weak<crate::TrayIcon>>,
    /// 热键热更新通道（设置保存后重注册）
    pub hotkey_tx: std::sync::mpsc::Sender<crate::HotkeySet>,
    /// 设置文件路径（FileJump 收藏/最近写回用）
    pub settings_path: std::path::PathBuf,
}

struct State {
    query: String,
    filter: Option<EntryKind>,
    items: Vec<EntryMeta>,
    selected: usize,
    visible: bool,
    preview_open: bool,
    preview: Option<PreviewData>,
    /// entry_id → 行内缩略图像素（仅解码一次；图片条数受 max_image_items 上限约束）
    thumb_cache: HashMap<i64, ImageData>,
    /// 弹窗隐藏时刻（空闲 trim 的计时锚点）
    hidden_at: Option<std::time::Instant>,
    /// 最近一次显示时刻（失焦关闭宽限期，避免 TOPMOST 刚 show 就误关）
    shown_at: Option<std::time::Instant>,
    /// 右键上下文菜单：是否打开 / 作用于哪一行
    menu_open: bool,
    menu_index: i32,
    /// Menu 键路径：本次 push 需在 Slint 侧按索引计算菜单位置（右键路径不需要）
    menu_keyboard_pending: bool,
    last_click_idx: Option<usize>,
    last_click_at: Option<std::time::Instant>,
    last_paste_at: Option<std::time::Instant>,
    /// FIFO/LIFO 队列（entry id，队首在 [0]）
    batch_queue: Vec<i64>,
    /// 面板主键+Tab：只显示快捷短语
    phrase_only: bool,
    /// 加成/编辑短语浮层
    phrase_edit: Option<PhraseEdit>,
    /// 编辑文本侧栏
    text_edit: Option<TextEdit>,
    /// 弹窗钉住（粘贴/点外不关；与条目置顶分开）
    window_pinned: bool,
    /// 多选锚点（含 selected）
    sel_anchor: usize,
    /// 当前列表首个可见行（WPF `_firstVisibleIndex`；序号 1–9 相对此行）
    first_visible: usize,
    /// Del 二次确认
    pending_delete: Option<i64>,
    /// 来源应用筛选
    source_filter: Option<String>,
    /// 托盘清空两段确认的武装时刻
    clear_armed_at: Option<std::time::Instant>,
    /// 快速查找会话状态（M4）
    qf: crate::explorer_quickfind::QfState,
    /// 文件夹跳转 Picker 状态（M5d）
    fj: crate::filejump::FjState,
    /// 生效中的设置（唯一真相源；设置窗口保存后更新此处）
    settings: Settings,
    /// 状态栏/托盘一次性提示（关于、导出、更新）
    notice: String,
    /// 设置窗口草稿态（Phase A）
    settings_win: crate::settings_win::WinState,
    #[cfg(windows)]
    foreground_at_show: isize,
}

impl State {
    fn new(deps: &LogicDeps) -> Self {
        Self {
            query: String::new(),
            filter: None,
            items: Vec::new(),
            selected: 0,
            visible: false,
            preview_open: false,
            preview: None,
            thumb_cache: HashMap::new(),
            // 启动即进入空闲计时：弹窗从未弹出的会话（迁移 OCR 回填突发后）
            // 也要周期性 trim，避免分配器滞留的工作集虚高
            hidden_at: Some(std::time::Instant::now()),
            shown_at: None,
            menu_open: false,
            menu_index: -1,
            menu_keyboard_pending: false,
            last_click_idx: None,
            last_click_at: None,
            last_paste_at: None,
            batch_queue: Vec::new(),
            phrase_only: false,
            phrase_edit: None,
            text_edit: None,
            window_pinned: false,
            sel_anchor: 0,
            first_visible: 0,
            pending_delete: None,
            source_filter: None,
            notice: String::new(),
            clear_armed_at: None,
            qf: crate::explorer_quickfind::QfState::default(),
            fj: crate::filejump::FjState::default(),
            settings: deps.settings.clone(),
            settings_win: crate::settings_win::WinState::default(),
            #[cfg(windows)]
            foreground_at_show: 0,
        }
    }
}

struct PreviewData {
    has_image: bool,
    image: ImageData,
    text: String,
    info: String,
}

struct PhraseEdit {
    content: String,
    buffer: String,
}

struct TextEdit {
    id: i64,
    buffer: String,
}

pub fn spawn(
    deps: LogicDeps,
    evt_rx: Receiver<AppEvt>,
    weak: slint::Weak<PopupWindow>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("clipx-logic".into())
        .spawn(move || {
            let mut state = State::new(&deps);
            let clipboard = ClipboardContext::new().ok();
            loop {
                match evt_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(evt) => handle(evt, &mut state, &deps, &weak, clipboard.as_ref()),
                    Err(RecvTimeoutError::Timeout) => check_foreground(&mut state, &deps, &weak),
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("启动逻辑线程失败: {e}"))
}

fn handle(
    evt: AppEvt,
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
) {
    match evt {
        AppEvt::Toggle => {
            if state.fj.visible {
                fj_hide(state, deps);
            }
            if state.visible {
                hide_popup(state, weak);
            } else {
                // 主弹窗弹出时终结快速查找会话（钩子键路由切回弹窗优先）
                crate::explorer_quickfind::end_if_active(&mut state.qf, &deps.qf);
                show_popup(state, deps, weak);
            }
        }
        AppEvt::Hide => {
            if state.visible && state.settings.hide_on_click_outside && !state.window_pinned {
                hide_popup(state, weak);
            }
            if state.fj.visible {
                // 停靠模式粘性：点对话框/外部不关闭（对齐 WPF sticky：
                // “贴靠、不因点文件窗/失焦而关”），否则拖框起手的第一下
                // 鼠标按下就把窗关了，跟随永无机会跑。
                // 关闭途径：Esc / Ctrl+G / 托盘 / 对话框销毁。
                // 全局模式（无对话框）点外关闭。
                if state.fj.global_mode || state.fj.dialog == 0 {
                    fj_hide(state, deps);
                }
            }
        }
        AppEvt::ListChanged => {
            if state.settings.batch_mode != "Off" {
                batch_enqueue_latest(state, deps, clipboard);
            }
            if state.visible {
                refresh_thumbs(state, deps);
                refresh(state, deps, weak, false);
            }
        }
        AppEvt::OcrDone => {
            if state.visible {
                reload_preview_if_open(state, deps);
                refresh(state, deps, weak, false);
            }
        }
        AppEvt::RowClicked(i) => {
            if state.visible {
                let idx = (i.max(0) as usize).min(state.items.len().saturating_sub(1));
                // Slint ListView 里 TouchArea.double-clicked 经常丢；用两次单击间隔判定
                let is_double = state.last_click_idx == Some(idx)
                    && state
                        .last_click_at
                        .map(|t| t.elapsed() < Duration::from_millis(400))
                        .unwrap_or(false);
                state.selected = idx;
                state.last_click_idx = Some(idx);
                state.last_click_at = Some(std::time::Instant::now());
                if !state.settings.paste_double_click || is_double {
                    activate_item(state, deps, weak, clipboard, idx);
                } else {
                    reload_preview_if_open(state, deps);
                    push_ui(state, weak);
                }
            }
        }
        AppEvt::RowDoubleClicked(i) => {
            if state.visible {
                activate_item(state, deps, weak, clipboard, i.max(0) as usize);
            }
        }
        AppEvt::FilterCycle => {
            if state.visible {
                state.phrase_only = false;
                if state.source_filter.take().is_some() {
                    refresh(state, deps, weak, true);
                } else {
                    state.filter = cycle_filter(state.filter);
                    refresh(state, deps, weak, true);
                }
            }
        }
        AppEvt::MenuRequest(i) => {
            if state.visible {
                let idx = (i.max(0) as usize).min(state.items.len().saturating_sub(1));
                state.selected = idx;
                state.menu_open = true;
                state.menu_index = idx as i32;
                state.menu_keyboard_pending = false;
                reload_preview_if_open(state, deps);
                push_ui(state, weak);
            }
        }
        AppEvt::MenuClose => {
            if state.menu_open {
                state.menu_open = false;
                push_ui(state, weak);
            }
        }
        AppEvt::MenuAction(action) => {
            if state.visible {
                menu_action(action, state, deps, weak, clipboard);
            }
        }
        AppEvt::Key(k) => {
            if matches!(k, KeyEvt::WinV) {
                if state.fj.visible {
                    fj_hide(state, deps);
                }
                if state.visible {
                    hide_popup(state, weak);
                } else {
                    crate::explorer_quickfind::end_if_active(&mut state.qf, &deps.qf);
                    show_popup(state, deps, weak);
                }
                return;
            }
            if matches!(k, KeyEvt::BatchAdvance) {
                batch_advance(state, deps, weak, clipboard);
                return;
            }
            // 设置窗口录制优先（主弹窗此时必隐藏）。
            if let KeyEvt::RecordVk(vk) = k {
                if state.settings_win.open {
                    crate::settings_win::handle_record_vk(&mut state.settings_win, vk);
                    crate::settings_win::patch_recording(&deps.settings_win, &state.settings_win);
                }
                return;
            }
            // FileJump 可见时键路由归 Picker（主弹窗此时必隐藏，互斥）
            if state.fj.visible {
                fj_key(k, state, deps);
                return;
            }
            if state.visible {
                handle_key(k, state, deps, weak, clipboard);
            }
        }
        // ===== 设置窗口（Phase A）=====
        AppEvt::OpenSettings => {
            if state.visible {
                hide_popup(state, weak);
            }
            if state.fj.visible {
                fj_hide(state, deps);
            }
            crate::explorer_quickfind::end_if_active(&mut state.qf, &deps.qf);
            crate::settings_win::open(
                &mut state.settings_win,
                &state.settings,
                &deps.settings_path,
                &deps.settings_win,
            );
            crate::settings_win::request_procs_if_needed(
                &mut state.settings_win,
                2,
                &deps.evt_tx,
            );
        }
        AppEvt::TrayAutostart => {
            state.settings.run_at_startup = !state.settings.run_at_startup;
            let ok = crate::autostart::set(
                state.settings.run_at_startup,
                state.settings.run_as_admin,
            );
            if !ok {
                state.settings.run_at_startup = !state.settings.run_at_startup;
            }
            let _ = crate::settings::save(&deps.settings_path, &state.settings);
            refresh_tray(state, deps);
        }
        AppEvt::TrayPause => {
            let paused = crate::policy::toggle_capture_paused();
            if paused && state.visible {
                // 暂停不影响已打开的列表，只挡后续入库
            }
            refresh_tray(state, deps);
        }
        AppEvt::TrayClear => {
            tray_clear_flow(state, deps, weak);
        }
        AppEvt::SettingBool(name) => {
            if state.settings_win.open {
                crate::settings_win::handle_bool(&mut state.settings_win, &name);
                // 开关已在 Slint 侧翻转；仅「自动弹出」会影响跟随行显隐。
                if name == "autopopup" {
                    crate::settings_win::patch_autopopup(
                        &deps.settings_win,
                        &state.settings_win,
                    );
                }
            }
        }
        AppEvt::SettingCycle(name) => {
            if state.settings_win.open {
                crate::settings_win::handle_cycle(&mut state.settings_win, &name, weak);
                crate::settings_win::patch_cycle(&deps.settings_win, &state.settings_win, &name);
            }
        }
        AppEvt::SettingRecord(slot) => {
            if state.settings_win.open {
                crate::settings_win::handle_record(&mut state.settings_win, slot);
                crate::settings_win::patch_recording(&deps.settings_win, &state.settings_win);
            }
        }
        AppEvt::SettingPage(page) => {
            if state.settings_win.open {
                crate::settings_win::request_procs_if_needed(
                    &mut state.settings_win,
                    page,
                    &deps.evt_tx,
                );
            }
        }
        AppEvt::SettingProcs(names) => {
            if state.settings_win.open {
                crate::settings_win::apply_procs(&mut state.settings_win, names);
                crate::settings_win::push_proc_list(&deps.settings_win, &state.settings_win);
            }
        }
        AppEvt::SettingText(field, text) => {
            if state.settings_win.open {
                crate::settings_win::handle_text(&mut state.settings_win, &field, text);
                // 文本框/滑条不整窗重推；短语列表变更才刷新模型。
                if field == "phrase-add"
                    || field == "phrase-del"
                    || field == "psel"
                    || field == "ptrig"
                    || field == "pbody"
                {
                    crate::settings_win::push(&deps.settings_win, &state.settings_win);
                }
            }
        }
        AppEvt::SettingSave => {
            if state.settings_win.open
                && crate::settings_win::handle_save(&mut state.settings_win)
                && apply_settings(state, deps, weak)
            {
                state.settings_win.open = false;
                crate::settings_win::hide(&deps.settings_win);
            }
            crate::settings_win::push(&deps.settings_win, &state.settings_win);
        }
        AppEvt::SettingCancel => {
            if state.settings_win.open {
                // 主题预览回滚（WPF 语义），其余 pending 直接丢弃。
                crate::settings_win::apply_theme(&state.settings.theme, weak);
                state.settings_win.open = false;
                crate::settings_win::hide(&deps.settings_win);
            }
        }
        AppEvt::SettingClear => {
            if state.settings_win.open {
                settings_clear_flow(state, deps);
            }
        }
        AppEvt::ExclAdd => {
            if state.settings_win.open {
                settings_excl_add(state);
                crate::settings_win::push(&deps.settings_win, &state.settings_win);
            }
        }
        AppEvt::ExclDel(i) => {
            if state.settings_win.open {
                settings_excl_del(state, i);
                crate::settings_win::push(&deps.settings_win, &state.settings_win);
            }
        }
        AppEvt::RuleDel(i) => {
            if state.settings_win.open {
                let idx = i.max(0) as usize;
                if idx < state.settings_win.draft_passthrough_len() {
                    state.settings_win.draft_rule_remove(idx);
                    crate::settings_win::push(&deps.settings_win, &state.settings_win);
                }
            }
        }
        AppEvt::CustomDel(i) => {
            if state.settings_win.open {
                settings_custom_del(state, i);
                crate::settings_win::push(&deps.settings_win, &state.settings_win);
            }
        }
        AppEvt::CustomImport => {
            if state.settings_win.open {
                settings_custom_import(state);
                crate::settings_win::push(&deps.settings_win, &state.settings_win);
            }
        }
        AppEvt::CustomExport => {
            if state.settings_win.open {
                settings_custom_export(state);
                crate::settings_win::push(&deps.settings_win, &state.settings_win);
            }
        }
        AppEvt::BatchCycle => {
            cycle_batch_mode(state, deps, weak);
        }
        AppEvt::PinWindow => {
            state.window_pinned = !state.window_pinned;
            if state.visible {
                push_ui(state, weak);
            }
        }
        AppEvt::PopupDragged(dx, dy) => {
            let weak_ui = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_ui.upgrade() {
                    crate::win_popup::drag_picker_by(&ui.window(), dx, dy);
                }
            });
        }
        AppEvt::MiddlePreview(i) => {
            if i >= 0 {
                state.selected = i as usize;
                if !state.preview_open {
                    toggle_preview(state, deps, weak);
                } else {
                    reload_preview_if_open(state, deps);
                    push_ui(state, weak);
                }
            }
        }
        AppEvt::TrayProbe => tray_probe_dialog(state, deps),
        AppEvt::TrayAbout => {
            notify(
                state,
                deps,
                format!("clipx {} · 对齐并超越 WPF 1.9.8", env!("CARGO_PKG_VERSION")),
            );
        }
        AppEvt::TrayUpdate => check_updates_now(state, deps),
        AppEvt::TrayExport => export_history(state, deps),
        AppEvt::TrayImport => import_history(state, deps, weak),
        // ===== 快速查找（M4）=====
        AppEvt::QfKey(k) => {
            // 主弹窗可见期间钩子不会产生 Qf 事件，此处防御性忽略
            if !state.visible {
                let max = state.settings.explorer_everything_quickfind_max_results;
                crate::explorer_quickfind::handle_key(
                    k,
                    &mut state.qf,
                    max,
                    &deps.evt_tx,
                    &deps.qf,
                    &state.settings.explorer_quickfind_open_mode,
                );
            }
        }
        AppEvt::QfRowActivated(i) => {
            crate::explorer_quickfind::activate_index(
                i.max(0) as usize,
                &mut state.qf,
                &deps.qf,
                &state.settings.explorer_quickfind_open_mode,
            );
        }
        AppEvt::QfFolderResolved { gen, folder } => {
            let max = state.settings.explorer_everything_quickfind_max_results;
            crate::explorer_quickfind::apply_folder(
                gen,
                folder,
                &mut state.qf,
                max,
                &deps.evt_tx,
                &deps.qf,
            );
        }
        AppEvt::QfResults { gen, view } => {
            crate::explorer_quickfind::apply_results(gen, view, &mut state.qf, &deps.qf);
        }
        // ===== 文件夹跳转（M5d）=====
        AppEvt::FileJumpToggle => {
            if !state.settings.filejump_enabled {
                return;
            }
            if state.fj.visible {
                let within = state
                    .fj
                    .last_hotkey_at
                    .map(|t| t.elapsed() < Duration::from_millis(state.settings.filejump_show_delay_ms.max(400)))
                    .unwrap_or(false);
                if within && !state.fj.filtered.is_empty() {
                    fj_activate(state, deps);
                } else {
                    fj_hide(state, deps);
                }
                return;
            }
            if state.visible {
                hide_popup(state, weak);
            }
            crate::explorer_quickfind::end_if_active(&mut state.qf, &deps.qf);
            #[cfg(windows)]
            let found = clipx_filejump::dialog::win::resolve_from_foreground();
            #[cfg(not(windows))]
            let found: Option<isize> = None;
            fj_show(state, deps, found, false);
            state.fj.last_hotkey_at = Some(std::time::Instant::now());
        }
        AppEvt::FileJumpAuto(d) => {
            if !state.settings.filejump_enabled || state.fj.visible || state.visible {
                return;
            }
            crate::explorer_quickfind::end_if_active(&mut state.qf, &deps.qf);
            fj_show(state, deps, Some(d), state.settings.filejump_auto_jump);
        }
        AppEvt::FjRowActivated(i) => {
            if state.fj.visible {
                let idx = i.max(0) as usize;
                if idx < state.fj.filtered.len() {
                    state.fj.selected = idx;
                    fj_activate(state, deps);
                }
            }
        }
        AppEvt::FjCollected { gen, items } => {
            if !state.fj.visible || state.fj.gen != gen {
                return;
            }
            state.fj.current_folder = items
                .iter()
                .find(|c| c.source == clipx_filejump::CandidateSource::Current)
                .map(|c| c.path.clone());
            state.fj.items = items;
            fj_apply_filter(state);
            state.fj.selected = 0;
            state.fj.hint.clear();
            // 自动跳转：首个候选直接跳（WPF FileJumpAutoOnFirstClick）。
            if state.fj.pending_auto_jump {
                state.fj.pending_auto_jump = false;
                if !state.fj.filtered.is_empty() && !state.fj.global_mode {
                    fj_activate(state, deps);
                    return;
                }
            }
            crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
        }
        AppEvt::FjEvResults { gen, items } => {
            if !state.fj.visible || state.fj.gen != gen {
                return;
            }
            let sel_path = state
                .fj
                .filtered
                .get(state.fj.selected)
                .and_then(|&i| state.fj.items.get(i))
                .map(|c| c.path.clone());
            let keep = state.fj.current_folder.as_deref();
            state.fj.items.retain(|c| {
                if c.source == clipx_filejump::CandidateSource::Everything {
                    return false;
                }
                if c.source != clipx_filejump::CandidateSource::Current {
                    return true;
                }
                keep.map(|k| k.eq_ignore_ascii_case(&c.path)).unwrap_or(false)
            });
            let mut seen: std::collections::HashSet<String> = state
                .fj
                .items
                .iter()
                .map(|c| c.path.to_lowercase())
                .collect();
            let mut children = Vec::new();
            let mut ev = Vec::new();
            for c in items {
                if !seen.insert(c.path.to_lowercase()) {
                    continue;
                }
                match c.source {
                    clipx_filejump::CandidateSource::Current => children.push(c),
                    clipx_filejump::CandidateSource::Everything => ev.push(c),
                    _ => {}
                }
            }
            let insert_at = state
                .fj
                .items
                .iter()
                .position(|c| {
                    c.source == clipx_filejump::CandidateSource::Current
                        && keep.map(|k| k.eq_ignore_ascii_case(&c.path)).unwrap_or(false)
                })
                .map(|i| i + 1)
                .unwrap_or(0);
            state.fj.items.splice(insert_at..insert_at, children);
            state.fj.items.extend(ev);
            fj_apply_filter(state);
            // 选中跟随原条目。
            if let Some(p) = sel_path {
                if let Some(pos) = state
                    .fj
                    .filtered
                    .iter()
                    .position(|&i| state.fj.items.get(i).map(|c| c.path == p).unwrap_or(false))
                {
                    state.fj.selected = pos;
                } else {
                    state.fj.selected = 0;
                }
            }
            crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
        }
        AppEvt::FjDragged(dx, dy) => {
            if state.fj.visible {
                let fj = deps.fj.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = fj.upgrade() {
                        crate::win_popup::drag_picker_by(&ui.window(), dx, dy);
                    }
                });
            }
        }
        AppEvt::FjDialogMoved => {
            // WinEvent 实时跟随：重算 dock（位置不变内部跳过）。
            // 对齐 WPF：手动拖拽的偏移在下一次 owner 移动时被 dock 重算覆盖。
            if state.fj.visible && state.fj.dialog != 0 {
                state.fj.last_dlg_rect = crate::win_popup::dialog_rect(state.fj.dialog);
                if !crate::win_popup::redock_picker(state.fj.dialog) {
                    fj_hide(state, deps);
                }
            }
        }
        AppEvt::FjDialogGone => {
            if state.fj.visible {
                fj_hide(state, deps);
            }
        }
        AppEvt::FjNavigated { gen, path, ok } => {
            if !state.fj.visible || state.fj.gen != gen {
                return;
            }
            state.fj.navigating = false;
            if ok {
                // 跳转成功保留弹窗（对齐 WPF“单击条目只导航不关闭”），可继续跳。
                let mut recent: Vec<String> = state.settings.filejump_recent.clone();
                crate::filejump::push_recent(
                    &deps.settings_path,
                    &mut recent,
                    &path,
                    state.settings.filejump_recent_max,
                );
                let n = state
                    .settings
                    .filejump_confirm_counts
                    .entry(path.clone())
                    .or_insert(0);
                *n += 1;
                let cnt = *n;
                if cnt >= state.settings.filejump_auto_add_min {
                    let already = state
                        .settings
                        .filejump_favorites
                        .iter()
                        .any(|f| f.path().eq_ignore_ascii_case(&path));
                    if !already {
                        state
                            .settings
                            .filejump_favorites
                            .push(crate::settings::FolderFavorite::full(String::new(), path.clone()));
                    }
                }
                let _ = crate::settings::save(&deps.settings_path, &state.settings);
                state.fj.hint = format!("已跳转：{path}");
                crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
            } else {
                state.fj.hint = "跳转失败（注入与键盘回退均未命中），换一条试试".to_string();
                crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
            }
        }
        AppEvt::UpdateAvailable(tag) => {
            if state.settings.last_update_tag.as_deref() == Some(tag.as_str()) {
                return;
            }
            state.settings.last_update_tag = Some(tag.clone());
            let _ = crate::settings::save(&deps.settings_path, &state.settings);
            notify(state, deps, format!("发现新版本 {tag}"));
        }
    }
}

fn handle_key(
    k: KeyEvt,
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
) {
    if state.text_edit.is_some() {
        handle_text_edit_key(k, state, deps, weak);
        return;
    }
    if state.phrase_edit.is_some() {
        handle_phrase_edit_key(k, state, deps, weak);
        return;
    }
    // 菜单打开期间：任意键（含 Esc）关闭菜单并吞掉，不穿透到背后列表
    // （菜单目标行可能与键盘选中行错位，穿透操作会作用在错误条目上）
    if state.menu_open {
        if matches!(k, KeyEvt::AltTap) {
            state.menu_open = false;
            push_ui(state, weak);
            return;
        }
        state.menu_open = false;
        push_ui(state, weak);
        return;
    }
    match k {
        KeyEvt::Esc => {
            if state.preview_open {
                state.preview_open = false;
                state.preview = None;
                push_ui(state, weak);
            } else if state.pending_delete.is_some() {
                state.pending_delete = None;
                push_ui(state, weak);
            } else if !state.query.is_empty() {
                state.query.clear();
                refresh(state, deps, weak, true);
            } else {
                hide_popup(state, weak);
            }
        }
        KeyEvt::Enter => paste_selection(state, deps, weak, clipboard, false),
        KeyEvt::CtrlEnter => paste_selection(state, deps, weak, clipboard, true),
        KeyEvt::ShiftEnter => paste_ocr(state, deps, weak, clipboard, state.selected),
        KeyEvt::AltTap => {
            if state.settings.batch_mode != "Off" && !state.batch_queue.is_empty() {
                batch_flush_all(state, deps, weak, clipboard);
            } else if !state.items.is_empty() {
                state.menu_open = true;
                state.menu_index = state.selected as i32;
                state.menu_keyboard_pending = true;
                push_ui(state, weak);
            }
        }
        KeyEvt::BatchAdvance => batch_advance(state, deps, weak, clipboard),
        KeyEvt::Space => toggle_preview(state, deps, weak),
        KeyEvt::PinToggle => {
            if let Some(meta) = state.items.get(state.selected).cloned() {
                if is_phrase_id(meta.id) {
                    return;
                }
                let _ = deps.store.toggle_pin(meta.id);
                // 置顶条目浮动到顶部：选中跟随原条目而非原索引
                refresh(state, deps, weak, false);
                state.selected = state
                    .items
                    .iter()
                    .position(|m| m.id == meta.id)
                    .unwrap_or(state.selected);
                ensure_selection_visible(state);
                reload_preview_if_open(state, deps);
                push_ui(state, weak);
            }
        }
        KeyEvt::Menu => {
            if !state.items.is_empty() {
                state.menu_open = true;
                state.menu_index = state.selected as i32;
                state.menu_keyboard_pending = true;
                push_ui(state, weak);
            }
        }
        KeyEvt::Char(c) => {
            if state.query.chars().count() < QUERY_MAX_CHARS {
                state.query.push(c);
                refresh(state, deps, weak, true);
            }
        }
        KeyEvt::Digit(n) => {
            // WPF：裸数字进搜索；快贴只走主键+1..9（QuickNum）
            handle_key(
                KeyEvt::Char((b'0' + n) as char),
                state,
                deps,
                weak,
                clipboard,
            );
        }
        // 面板主键+数字：按当前可见窗口的显示序号快贴（WPF PasteByIndex）
        KeyEvt::QuickNum(n) => {
            if let Some(idx) = index_of_display(n, state.first_visible, state.items.len()) {
                activate_item(state, deps, weak, clipboard, idx);
            }
        }
        KeyEvt::QuickTab => {
            state.phrase_only = !state.phrase_only;
            if state.phrase_only {
                state.filter = None;
            }
            refresh(state, deps, weak, true);
        }
        KeyEvt::Tab => {
            state.phrase_only = false;
            if state.source_filter.take().is_some() {
                refresh(state, deps, weak, true);
            } else {
                state.filter = cycle_filter(state.filter);
                refresh(state, deps, weak, true);
            }
        }
        KeyEvt::Backspace => {
            if state.query.pop().is_some() {
                refresh(state, deps, weak, true);
            }
        }
        KeyEvt::Delete => {
            if let Some(meta) = state.items.get(state.selected).cloned() {
                if state.pending_delete == Some(meta.id) {
                    delete_item(state, deps, meta.id);
                    state.pending_delete = None;
                    refresh_thumbs(state, deps);
                    refresh(state, deps, weak, false);
                    reload_preview_if_open(state, deps);
                } else {
                    state.pending_delete = Some(meta.id);
                    push_ui(state, weak);
                }
            }
        }
        KeyEvt::Up => move_selection(state, deps, weak, -1, false),
        KeyEvt::Down => move_selection(state, deps, weak, 1, false),
        KeyEvt::ShiftUp => move_selection(state, deps, weak, -1, true),
        KeyEvt::ShiftDown => move_selection(state, deps, weak, 1, true),
        KeyEvt::PgUp | KeyEvt::Left => scroll_page(state, deps, weak, -1),
        KeyEvt::PgDn | KeyEvt::Right => scroll_page(state, deps, weak, 1),
        KeyEvt::Home => {
            state.selected = 0;
            state.sel_anchor = 0;
            state.first_visible = 0;
            state.pending_delete = None;
            reload_preview_if_open(state, deps);
            push_ui(state, weak);
        }
        KeyEvt::End => {
            state.selected = state.items.len().saturating_sub(1);
            state.sel_anchor = state.selected;
            ensure_selection_visible(state);
            state.pending_delete = None;
            reload_preview_if_open(state, deps);
            push_ui(state, weak);
        }
        // Qf 事件经 AppEvt::QfKey 独立路由（见 main.rs 转发器），不会进入此分支
        _ => {}
    }
}

fn move_selection(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    delta: i32,
    expand: bool,
) {
    let len = state.items.len();
    if len == 0 {
        return;
    }
    let cur = state.selected as i64;
    let next = (cur + delta as i64).clamp(0, len as i64 - 1);
    state.selected = next as usize;
    if !expand {
        state.sel_anchor = state.selected;
    }
    ensure_selection_visible(state);
    state.pending_delete = None;
    reload_preview_if_open(state, deps);
    push_ui(state, weak);
}

/// Space 切换预览（WPF 版行为）：打开时加载当前选中项，
/// 切换选中项后预览内容跟随。
fn toggle_preview(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    if state.preview_open {
        state.preview_open = false;
        state.preview = None;
    } else {
        if state.items.is_empty() {
            return;
        }
        state.preview = Some(load_preview(
            &state.items[state.selected],
            &deps.store,
            &state.settings,
        ));
        state.preview_open = true;
    }
    push_ui(state, weak);
}

fn reload_preview_if_open(state: &mut State, deps: &LogicDeps) {
    if state.preview_open {
        state.preview = state
            .items
            .get(state.selected)
            .map(|m| load_preview(m, &deps.store, &state.settings));
    }
}

fn load_preview(meta: &EntryMeta, store: &Store, settings: &Settings) -> PreviewData {
    if let Some(idx) = phrase_index_of(meta.id) {
        let content = settings
            .phrases
            .get(idx)
            .map(|p| p.content.clone())
            .unwrap_or_default();
        let n = content.chars().count();
        return PreviewData {
            has_image: false,
            image: ImageData::default(),
            text: content,
            info: format!("快捷短语 · {n} 字"),
        };
    }
    match meta.kind {
        EntryKind::Text => {
            let full = store.get_text(meta.id).unwrap_or_default();
            let n = full.chars().count();
            PreviewData {
                has_image: false,
                image: ImageData::default(),
                text: full,
                info: format!("文本 · {n} 字"),
            }
        }
        EntryKind::Image => {
            let ocr = store.get_ocr(meta.id);
            let ocr_state = ocr.as_ref().map(|o| o.state).unwrap_or(0);
            let ocr_text = ocr.and_then(|o| o.text).unwrap_or_default();
            let (image, info) = match store.get_image(meta.id) {
                Some(row) => {
                    // 懒加载 + 限尺寸解码；row（原图 bytes）在本函数结束即释放
                    let img = decode_image_limited(&row.blob, PREVIEW_MAX_DIM);
                    (img, format!("图片 {}×{}", row.w, row.h))
                }
                None => (ImageData::default(), "图片".to_string()),
            };
            let info = match ocr_state {
                2 if ocr_text.trim().is_empty() => format!("{info} · OCR：未识别到文字"),
                2 => format!("{info} · OCR 文本"),
                3 => format!("{info} · OCR 失败"),
                _ => format!("{info} · OCR 进行中…"),
            };
            PreviewData {
                has_image: true,
                image,
                text: ocr_text,
                info,
            }
        }
        EntryKind::Files => {
            let paths = store.get_text(meta.id).unwrap_or_default();
            let n = paths.lines().count();
            PreviewData {
                has_image: false,
                image: ImageData::default(),
                text: paths,
                info: format!("文件 · {n} 项"),
            }
        }
        EntryKind::RichText => {
            let full = store.get_text(meta.id).unwrap_or_default();
            let n = full.chars().count();
            PreviewData {
                has_image: false,
                image: ImageData::default(),
                text: full,
                info: format!("富文本 · {n} 字 · 粘贴还原格式"),
            }
        }
    }
}

/// PNG bytes → 原始 RGBA（长边超限时等比缩小，解码内存上限可控）。
fn decode_image_limited(png: &[u8], max_dim: u32) -> ImageData {
    let Ok(img) = image::load_from_memory(png) else {
        return ImageData::default();
    };
    let img = if img.width().max(img.height()) > max_dim {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    ImageData {
        rgba: rgba.into_raw(),
        w,
        h,
    }
}

/// 事件循环线程上调用：RGBA → slint::Image。
fn to_slint_image(d: ImageData) -> slint::Image {
    if d.w == 0 || d.h == 0 {
        return slint::Image::default();
    }
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(d.w, d.h);
    let dst = buf.make_mut_slice();
    for (o, s) in dst.iter_mut().zip(d.rgba.chunks_exact(4)) {
        *o = slint::Rgba8Pixel {
            r: s[0],
            g: s[1],
            b: s[2],
            a: s[3],
        };
    }
    slint::Image::from_rgba8(buf)
}

/// 增量刷新行内缩略图缓存：一次批量查询，仅解码缓存未命中的条目。
fn refresh_thumbs(state: &mut State, deps: &LogicDeps) {
    for (id, t) in deps.store.list_thumbs(500) {
        state
            .thumb_cache
            .entry(id)
            .or_insert_with(|| decode_image_limited(&t.blob, 256));
    }
}

fn show_popup(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    state.query.clear();
    state.filter = None;
    state.phrase_only = false;
    state.phrase_edit = None;
    state.preview_open = false;
    state.preview = None;
    state.menu_open = false;
    state.menu_index = -1;
    state.hidden_at = None;
    state.shown_at = Some(std::time::Instant::now());
    refresh_thumbs(state, deps);
    state.items = merge_list_items(state, deps);
    state.selected = 0;
    state.sel_anchor = 0;
    state.first_visible = 0;
    state.pending_delete = None;
    state.visible = true;
    keyboard_hook::set_visible(true);
    #[cfg(windows)]
    {
        state.foreground_at_show = win_popup::foreground_hwnd();
    }
    let mut bundle = ui_bundle(state);
    bundle.show = true;
    bundle.reposition = true;
    let (ax, ay) = win_popup::resolve_popup_anchor(&state.settings.popup_position);
    bundle.anchor_x = ax;
    bundle.anchor_y = ay;
    invoke_ui(weak, bundle);
}

fn hide_popup(state: &mut State, weak: &slint::Weak<PopupWindow>) {
    state.visible = false;
    state.preview_open = false;
    state.preview = None;
    state.menu_open = false;
    state.menu_index = -1;
    state.phrase_edit = None;
    state.hidden_at = Some(std::time::Instant::now());
    state.shown_at = None;
    keyboard_hook::set_visible(false);
    #[cfg(windows)]
    mouse_hook::POPUP_HWND.store(0, std::sync::atomic::Ordering::SeqCst);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().hide();
        }
    });
}

fn refresh(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    reset_selection: bool,
) {
    state.items = merge_list_items(state, deps);
    if reset_selection {
        state.selected = 0;
        state.first_visible = 0;
        state.sel_anchor = 0;
    } else {
        state.selected = state.selected.min(state.items.len().saturating_sub(1));
        ensure_selection_visible(state);
    }
    push_ui(state, weak);
}

/// 按类型回写剪贴板（Gate 防自采在 arm 后由 monitor 吸收）。
/// 返回 false = 条目数据缺失（图片/文件读库失败），调用方不应继续。
fn write_entry_clipboard(
    meta: &EntryMeta,
    deps: &LogicDeps,
    clipboard: Option<&ClipboardContext>,
    settings: &Settings,
) -> bool {
    let Some(ctx) = clipboard else { return false };
    if let Some(idx) = phrase_index_of(meta.id) {
        let Some(text) = settings.phrases.get(idx).map(|p| p.content.as_str()) else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        deps.gate.arm();
        return paste::write_text(ctx, text).is_ok();
    }
    match meta.kind {
        EntryKind::Image => {
            let Some(img) = deps.store.get_image(meta.id) else {
                return false;
            };
            deps.gate.arm();
            paste::write_image(ctx, &img.blob).is_ok()
        }
        EntryKind::Files => {
            let Some(paths) = deps.store.get_files(meta.id) else {
                return false;
            };
            deps.gate.arm();
            paste::write_files(ctx, &paths).is_ok()
        }
        EntryKind::RichText => {
            // 投影 + HTML 同时写回；html 缺失时退化为纯文本
            if let Some(html) = deps.store.get_html(meta.id) {
                let text = deps.store.get_text(meta.id).unwrap_or_default();
                deps.gate.arm();
                return paste::write_rich_text(ctx, &text, &html).is_ok();
            }
            let Some(text) = deps.store.get_text(meta.id) else {
                return false;
            };
            deps.gate.arm();
            paste::write_text(ctx, &text).is_ok()
        }
        EntryKind::Text => {
            let Some(text) = deps.store.get_text(meta.id) else {
                return false;
            };
            deps.gate.arm();
            paste::write_text(ctx, &text).is_ok()
        }
    }
}

fn activate_item(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
) {
    if state.settings.batch_mode != "Off" {
        batch_enqueue(state, deps, weak, clipboard, idx);
    } else {
        do_paste(state, deps, weak, clipboard, idx);
    }
}

fn do_paste(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
) {
    if state
        .last_paste_at
        .map(|t| t.elapsed() < Duration::from_millis(350))
        .unwrap_or(false)
    {
        return;
    }
    let Some(meta) = state.items.get(idx) else {
        return;
    };
    if !write_entry_clipboard(meta, deps, clipboard, &state.settings) {
        return;
    }

    state.last_paste_at = Some(std::time::Instant::now());
    let target = {
        #[cfg(windows)]
        {
            state.foreground_at_show
        }
        #[cfg(not(windows))]
        {
            0
        }
    };
    let pinned = state.window_pinned;
    if !pinned {
        hide_popup(state, weak);
    }
    if state.settings.paste_simulate {
        // hide 走 UI 线程，先让出一轮再抢回呼出时的目标窗，否则 Ctrl+V 打到空处
        std::thread::sleep(Duration::from_millis(80));
        #[cfg(windows)]
        win_popup::restore_foreground(target);
        std::thread::sleep(Duration::from_millis(30));
        let mode = paste::paste_mode_for_target(target, &state.settings.paste_mode);
        paste::send_paste(mode);
    }
}

fn cycle_batch_mode(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    state.settings.batch_mode = match state.settings.batch_mode.as_str() {
        "Off" => "Lifo".to_string(),
        "Lifo" => "Fifo".to_string(),
        _ => "Off".to_string(),
    };
    if state.settings.batch_mode == "Off" {
        state.batch_queue.clear();
    }
    let _ = crate::settings::save(&deps.settings_path, &state.settings);
    sync_batch_watch(state);
    refresh_tray(state, deps);
    if state.visible {
        push_ui(state, weak);
    }
}

fn batch_enqueue(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
) {
    let Some(meta) = state.items.get(idx).cloned() else {
        return;
    };
    state.batch_queue.retain(|id| *id != meta.id);
    if state.settings.batch_mode == "Fifo" {
        state.batch_queue.push(meta.id);
    } else {
        state.batch_queue.insert(0, meta.id);
    }
    if let Some(head) = state.batch_queue.first().copied() {
        if let Some(meta) = meta_by_id(state, deps, head) {
            let _ = write_entry_clipboard(&meta, deps, clipboard, &state.settings);
        }
    }
    sync_batch_watch(state);
    push_ui(state, weak);
}

fn batch_advance(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
) {
    if state.settings.batch_mode == "Off" || state.batch_queue.is_empty() {
        return;
    }
    state.batch_queue.remove(0);
    if state.batch_queue.is_empty() {
        if state.settings.batch_auto_off_when_empty {
            state.settings.batch_mode = "Off".to_string();
            let _ = crate::settings::save(&deps.settings_path, &state.settings);
        }
        sync_batch_watch(state);
        if state.visible {
            push_ui(state, weak);
        }
        return;
    }
    let head = state.batch_queue[0];
    if let Some(meta) = meta_by_id(state, deps, head) {
        let _ = write_entry_clipboard(&meta, deps, clipboard, &state.settings);
    }
    sync_batch_watch(state);
    if state.visible {
        push_ui(state, weak);
    }
}

fn sync_batch_watch(state: &State) {
    crate::keyboard_hook::set_batch_watch(
        state.settings.batch_mode != "Off" && !state.batch_queue.is_empty(),
    );
}

fn meta_by_id(state: &State, deps: &LogicDeps, id: i64) -> Option<EntryMeta> {
    state
        .items
        .iter()
        .find(|m| m.id == id)
        .cloned()
        .or_else(|| {
            deps.store
                .search("", None, list_limit(&state.settings))
                .into_iter()
                .find(|m| m.id == id)
        })
}

fn list_limit(s: &Settings) -> i64 {
    s.max_items.clamp(10, 2000)
}

fn popup_w(s: &Settings) -> f32 {
    s.popup_width as f32
}

fn page_step(s: &State) -> i32 {
    s.settings.popup_page_items.clamp(1, 50) as i32
}

/// 当前列表可视行数（用于翻页夹紧首行）。
fn visible_rows(s: &State) -> usize {
    let search = !s.query.is_empty();
    let max_h = s.settings.popup_max_height as f32;
    let rh = row_h(&s.settings);
    let chrome = HEADER_H + FOOTER_H + if search { SEARCH_H } else { 0.0 };
    let list_h = (max_h - chrome)
        .min(s.items.len() as f32 * rh)
        .max(0.0);
    (list_h / rh).floor().max(1.0) as usize
}

fn clamp_first_visible(state: &mut State) {
    let vis = visible_rows(state);
    let max_first = state.items.len().saturating_sub(vis);
    if state.first_visible > max_first {
        state.first_visible = max_first;
    }
}

fn ensure_selection_visible(state: &mut State) {
    if state.items.is_empty() {
        state.first_visible = 0;
        return;
    }
    let vis = visible_rows(state).max(1);
    if state.selected < state.first_visible {
        state.first_visible = state.selected;
    } else if state.selected >= state.first_visible + vis {
        state.first_visible = state.selected + 1 - vis;
    }
    clamp_first_visible(state);
}

/// WPF ScrollPage：滚动窗口并保持选中项在窗口内的相对位置，序号随首行重算。
fn scroll_page(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    direction: i32,
) {
    let n = state.items.len();
    if n == 0 {
        return;
    }
    let vis = visible_rows(state).max(1);
    let rel = state.selected.saturating_sub(state.first_visible);
    let max_first = n.saturating_sub(vis);
    let new_first = (state.first_visible as i32 + direction * page_step(state))
        .clamp(0, max_first as i32) as usize;
    state.first_visible = new_first;
    state.selected = (new_first + rel).min(n - 1);
    state.sel_anchor = state.selected;
    state.pending_delete = None;
    reload_preview_if_open(state, deps);
    push_ui(state, weak);
}

/// 可见窗口内第 1–9 项的显示序号（相对 `first_visible`）。
fn visible_index_label(i: usize, first_visible: usize) -> String {
    let rel = i as i32 - first_visible as i32 + 1;
    if (1..=9).contains(&rel) {
        rel.to_string()
    } else {
        String::new()
    }
}

fn index_of_display(n: u8, first_visible: usize, len: usize) -> Option<usize> {
    if n < 1 || n > 9 || len == 0 {
        return None;
    }
    let idx = first_visible + (n as usize - 1);
    (idx < len).then_some(idx)
}

/// 右键菜单动作（menu_index 行；菜单已在 UI 侧关闭，这里只管状态与数据）
fn menu_action(
    action: MenuAction,
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
) {
    let idx = state.menu_index.max(0) as usize;
    state.menu_open = false;
    let Some(meta) = state.items.get(idx).cloned() else {
        push_ui(state, weak);
        return;
    };
    match action {
        // 复制：只写剪贴板（不模拟 Ctrl+V），写完收起弹窗
        MenuAction::Copy => {
            if write_entry_clipboard(&meta, deps, clipboard, &state.settings) {
                hide_popup(state, weak);
            } else {
                push_ui(state, weak);
            }
        }
        MenuAction::Pin => {
            if is_phrase_id(meta.id) {
                push_ui(state, weak);
                return;
            }
            let _ = deps.store.toggle_pin(meta.id);
            refresh(state, deps, weak, false);
            // 置顶浮动后选中跟随原条目
            state.selected = state
                .items
                .iter()
                .position(|m| m.id == meta.id)
                .unwrap_or(state.selected);
            ensure_selection_visible(state);
            reload_preview_if_open(state, deps);
            push_ui(state, weak);
        }
        MenuAction::Delete => {
            delete_item(state, deps, meta.id);
            refresh_thumbs(state, deps);
            refresh(state, deps, weak, false);
            reload_preview_if_open(state, deps);
        }
        MenuAction::Phrase => {
            begin_phrase_edit(state, deps, &meta);
            push_ui(state, weak);
        }
        MenuAction::Edit => begin_text_edit(state, deps, &meta, weak),
        MenuAction::OcrPaste => paste_ocr(state, deps, weak, clipboard, idx),
        MenuAction::PasteAsFile => paste_as_file(state, deps, weak, clipboard, idx, false),
        MenuAction::PasteAsJson => paste_as_file(state, deps, weak, clipboard, idx, true),
        MenuAction::SaveImage => save_image_to_temp(state, deps, &meta, clipboard),
        MenuAction::CopyPath => copy_entry_path(state, deps, &meta, clipboard, weak),
        MenuAction::FilterSource => {
            if !meta.source_app.is_empty() {
                state.source_filter = Some(meta.source_app.clone());
                refresh(state, deps, weak, true);
            } else {
                push_ui(state, weak);
            }
        }
    }
}

fn check_foreground(
    state: &mut State,
    deps: &LogicDeps,
    _weak: &slint::Weak<PopupWindow>,
) {
    // 空闲 trim：隐藏满 5 秒归还一次工作集，之后每满 5 秒重复
    // （burst/预览等峰值操作后 WS 不虚高；trim 后 soft fault 廉价拉回）
    if !state.visible {
        let idle_5s = state
            .hidden_at
            .map(|t| t.elapsed() >= Duration::from_secs(5))
            .unwrap_or(false);
        if idle_5s {
            win_popup::trim_working_set();
            state.hidden_at = Some(std::time::Instant::now());
        }
    }
    // FileJump 贴框随动：对话框移动/缩放时 Picker 平移相同差量；
    // 对话框已销毁则 Picker 跟着关闭（200ms 粒度，与 trim 同 tick）。
    fj_follow_tick(state, deps);
}

/// Picker 跟随兜底 tick（WPF 500ms timer 的轻量版，200ms 粒度）：
/// WinEvent 是实时主力；个别不发 LOCATIONCHANGE 的宿主靠这里补。
/// 对话框已死则关闭 Picker。
fn fj_follow_tick(state: &mut State, deps: &LogicDeps) {
    if !state.fj.visible || state.fj.dialog == 0 {
        return;
    }
    if !crate::win_popup::dialog_alive(state.fj.dialog) {
        fj_hide(state, deps);
        return;
    }
    let Some(rect) = crate::win_popup::dialog_rect(state.fj.dialog) else {
        return;
    };
    if state.fj.last_dlg_rect != Some(rect) {
        state.fj.last_dlg_rect = Some(rect);
        if crate::win_popup::redock_picker(state.fj.dialog) {
        } else {
            fj_hide(state, deps);
            return;
        }
    }
    if state.settings.filejump_auto_sync && state.fj.dialog != 0 {
        let fg = crate::win_popup::foreground_hwnd();
        if fg != state.fj.dialog {
            state.fj.left_dialog = true;
        } else if state.fj.left_dialog {
            state.fj.left_dialog = false;
            if let Some(path) = clipx_filejump::collectors::zorder_linked_folder(
                state.fj.dialog,
                Some(&deps.gate),
                2,
            ) {
                let kind = state.fj.kind.unwrap_or(clipx_filejump::DialogKind::System);
                state.fj.gen += 1;
                let gen = state.fj.gen;
                crate::filejump::spawn_navigate(
                    deps.evt_tx.clone(),
                    state.fj.dialog,
                    kind,
                    path,
                    state.settings.filejump_shell_inject,
                    gen,
                );
            }
            // 切回已开对话框：刷新候选
            let req = crate::filejump::CollectReq {
                gen: {
                    state.fj.gen += 1;
                    state.fj.gen
                },
                query: state.fj.query.clone(),
                favorites: state.settings.filejump_favorites.clone(),
                recent: state.settings.filejump_recent.clone(),
                everything_enabled: state.settings.filejump_everything_search,
                dialog: state.fj.dialog,
            };
            crate::filejump::spawn_collect(deps.evt_tx.clone(), deps.gate.clone(), req);
        }
    }
}
// 注意：弹窗是 WS_EX_NOACTIVATE，本来就不抢前台；不用前台变化关窗，
// 否则 show() 引起的任务栏/IME 抢焦会被当成失焦。外部点击由 mouse_hook 关，Esc 由键盘钩子关。

/// 逻辑线程产出的行数据（纯数据，Send）；RowData（含 slint::Image）
/// 在事件循环线程上组装。
struct RowSource {
    id: i32,
    kind: i32,
    icon: &'static str,
    index_label: String,
    preview: String,
    hit_pre: String,
    hit: String,
    hit_post: String,
    sub: String,
    time_ago: String,
    thumb: ImageData,
    in_range: bool,
    pending_delete: bool,
}

struct UiBundle {
    rows: Vec<RowSource>,
    selected: i32,
    search_active: bool,
    search_text: SharedString,
    search_count: SharedString,
    filter_label: SharedString,
    item_count: SharedString,
    width: f32,
    height: f32,
    show: bool,
    preview_active: bool,
    preview_has_image: bool,
    preview_image: ImageData,
    preview_text: SharedString,
    preview_info: SharedString,
    menu_visible: bool,
    menu_index: i32,
    menu_pinned: bool,
    menu_is_phrase: bool,
    menu_can_phrase: bool,
    menu_can_edit: bool,
    menu_can_ocr: bool,
    menu_can_file: bool,
    menu_position_keyboard: bool,
    list_width: f32,
    batch_label: SharedString,
    footer_hint: SharedString,
    /// 仅首次 show 时定位；后续刷新只 clamp，避免每次重算贴到鼠标。
    reposition: bool,
    anchor_x: i32,
    anchor_y: i32,
    opacity: f64,
    phrase_edit_open: bool,
    phrase_edit_preview: SharedString,
    phrase_edit_buffer: SharedString,
    text_edit_open: bool,
    text_edit_buffer: SharedString,
    row_height_px: f32,
    window_pinned: bool,
    first_visible: i32,
}

fn ui_bundle(state: &mut State) -> UiBundle {
    clamp_first_visible(state);
    let (preview_active, preview_has_image, preview_image, preview_text, preview_info) =
        match state.preview.as_ref() {
            Some(p) => (
                state.preview_open,
                p.has_image,
                p.image.clone(),
                p.text.clone().into(),
                p.info.clone().into(),
            ),
            None => (
                false,
                false,
                ImageData::default(),
                SharedString::new(),
                SharedString::new(),
            ),
        };
    let menu_index = if state.menu_open {
        state.menu_index
    } else {
        -1
    };
    let menu_meta = state.items.get(menu_index.max(0) as usize);
    let (phrase_preview, phrase_buf) = match state.phrase_edit.as_ref() {
        Some(e) => (truncate_preview(&e.content, 2), e.buffer.clone()),
        None => (String::new(), String::new()),
    };
    let (lo, hi) = sel_range(state);
    let bundle = UiBundle {
        rows: build_rows(
            &state.items,
            &state.thumb_cache,
            &state.batch_queue,
            &state.settings,
            &state.query,
            lo,
            hi,
            state.pending_delete,
            state.first_visible,
        ),
        selected: state.selected as i32,
        search_active: !state.query.is_empty(),
        search_text: state.query.clone().into(),
        search_count: format!("{} 条结果", state.items.len()).into(),
        filter_label: filter_label(state).into(),
        item_count: if state.items.is_empty() {
            SharedString::default()
        } else {
            format!("({})", state.items.len()).into()
        },
        width: if preview_active {
            popup_w(&state.settings) + PREVIEW_W + POPUP_CHROME * 2.0
        } else {
            popup_w(&state.settings) + POPUP_CHROME * 2.0
        },
        height: window_height(
            state.items.len(),
            !state.query.is_empty(),
            state.settings.popup_max_height as f32,
            row_h(&state.settings),
        ) + POPUP_CHROME * 2.0,
        show: false,
        preview_active,
        preview_has_image,
        preview_image,
        preview_text,
        preview_info,
        menu_visible: state.menu_open,
        menu_index,
        menu_pinned: menu_meta.map(|m| m.pinned).unwrap_or(false),
        menu_is_phrase: menu_meta.map(|m| is_phrase_id(m.id)).unwrap_or(false),
        menu_can_phrase: menu_meta
            .map(|m| !is_phrase_id(m.id) && matches!(m.kind, EntryKind::Text | EntryKind::RichText))
            .unwrap_or(false),
        menu_can_edit: menu_meta
            .map(|m| !is_phrase_id(m.id) && matches!(m.kind, EntryKind::Text | EntryKind::RichText))
            .unwrap_or(false),
        menu_can_ocr: menu_meta
            .map(|m| m.kind == EntryKind::Image)
            .unwrap_or(false),
        menu_can_file: menu_meta.map(|m| !is_phrase_id(m.id)).unwrap_or(false),
        menu_position_keyboard: state.menu_keyboard_pending,
        list_width: popup_w(&state.settings).max(260.0),
        batch_label: batch_label(state).into(),
        footer_hint: footer_hint(state).into(),
        reposition: false,
        anchor_x: 0,
        anchor_y: 0,
        opacity: state.settings.popup_opacity,
        phrase_edit_open: state.phrase_edit.is_some(),
        phrase_edit_preview: phrase_preview.into(),
        phrase_edit_buffer: phrase_buf.into(),
        text_edit_open: state.text_edit.is_some(),
        text_edit_buffer: state
            .text_edit
            .as_ref()
            .map(|e| e.buffer.clone())
            .unwrap_or_default()
            .into(),
        row_height_px: row_h(&state.settings),
        window_pinned: state.window_pinned,
        first_visible: state.first_visible as i32,
    };
    state.menu_keyboard_pending = false;
    bundle
}

fn push_ui(state: &mut State, weak: &slint::Weak<PopupWindow>) {
    invoke_ui(weak, ui_bundle(state));
}

fn invoke_ui(weak: &slint::Weak<PopupWindow>, bundle: UiBundle) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        let rows: Vec<RowData> = bundle
            .rows
            .into_iter()
            .map(|r| RowData {
                id: r.id,
                kind: r.kind,
                icon: r.icon.into(),
                index_label: r.index_label.into(),
                preview: r.preview.into(),
                hit_pre: r.hit_pre.into(),
                hit: r.hit.into(),
                hit_post: r.hit_post.into(),
                sub: r.sub.into(),
                time_ago: r.time_ago.into(),
                thumb: to_slint_image(r.thumb),
                in_range: r.in_range,
                pending_delete: r.pending_delete,
            })
            .collect();
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_selected_index(bundle.selected);
        ui.set_search_active(bundle.search_active);
        ui.set_search_text(bundle.search_text);
        ui.set_search_count(bundle.search_count);
        ui.set_type_filter_label(bundle.filter_label.into());
        ui.set_item_count(bundle.item_count);
        ui.set_batch_label(bundle.batch_label);
        ui.set_footer_hint(bundle.footer_hint);
        ui.set_list_width_px(bundle.list_width);
        ui.set_preview_active(bundle.preview_active);
        ui.set_preview_has_image(bundle.preview_has_image);
        ui.set_preview_image(to_slint_image(bundle.preview_image));
        ui.set_preview_text(bundle.preview_text);
        ui.set_preview_info(bundle.preview_info);
        ui.set_menu_visible(bundle.menu_visible);
        ui.set_menu_index(bundle.menu_index);
        ui.set_menu_pinned(bundle.menu_pinned);
        ui.set_menu_is_phrase(bundle.menu_is_phrase);
        ui.set_menu_can_phrase(bundle.menu_can_phrase);
        ui.set_menu_can_edit(bundle.menu_can_edit);
        ui.set_menu_can_ocr(bundle.menu_can_ocr);
        ui.set_menu_can_file(bundle.menu_can_file);
        ui.set_phrase_edit_open(bundle.phrase_edit_open);
        ui.set_phrase_edit_preview(bundle.phrase_edit_preview);
        ui.set_phrase_edit_buffer(bundle.phrase_edit_buffer);
        ui.set_text_edit_open(bundle.text_edit_open);
        ui.set_text_edit_buffer(bundle.text_edit_buffer);
        ui.set_window_pinned(bundle.window_pinned);
        ui.set_row_height_px(bundle.row_height_px);
        ui.set_panel_opacity(bundle.opacity.clamp(0.4, 1.0) as f32);
        ui.set_first_visible(bundle.first_visible);
        if bundle.menu_position_keyboard && bundle.menu_index >= 0 {
            ui.invoke_menu_position_at(bundle.menu_index);
        }
        let win = ui.window();
        let w = bundle.width;
        win.set_size(WindowSize::Logical(LogicalSize::new(w, bundle.height)));
        if bundle.show {
            let _ = win.show();
            win.set_size(WindowSize::Logical(LogicalSize::new(w, bundle.height)));
            if bundle.reposition {
                win_popup::position_at(win, w, bundle.height, bundle.anchor_x, bundle.anchor_y);
                win_popup::apply_style(win);
                win_popup::store_hwnd(win);
            }
        } else {
            win_popup::clamp_to_work_area(win, w, bundle.height);
        }
    });
}

fn build_rows(
    items: &[EntryMeta],
    thumbs: &HashMap<i64, ImageData>,
    queue: &[i64],
    settings: &Settings,
    query: &str,
    lo: usize,
    hi: usize,
    pending: Option<i64>,
    first_visible: usize,
) -> Vec<RowSource> {
    let now = now_ms();
    items
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let qpos = queue.iter().position(|id| *id == m.id);
            let index_label = visible_index_label(i, first_visible);
            let mut sub = kind_sub(m, settings);
            if qpos.is_some() {
                if !sub.is_empty() {
                    sub.push_str(" · ");
                }
                sub.push_str("队列");
            }
            let preview = truncate_preview(&m.preview, settings.preview_max_lines);
            let (hit_pre, hit, hit_post) = split_hit(&preview, query);
            RowSource {
                id: m.id as i32,
                kind: if is_phrase_id(m.id) {
                    4
                } else {
                    m.kind.as_i64() as i32
                },
                icon: if is_phrase_id(m.id) {
                    "⚡"
                } else {
                    kind_icon(m.kind)
                },
                index_label,
                preview,
                hit_pre,
                hit,
                hit_post,
                sub,
                time_ago: if is_phrase_id(m.id) {
                    String::new()
                } else {
                    time_ago(m.created_ms, now)
                },
                thumb: thumbs.get(&m.id).cloned().unwrap_or_default(),
                in_range: i >= lo && i <= hi && lo != hi,
                pending_delete: pending == Some(m.id),
            }
        })
        .collect()
}

fn kind_icon(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Text => "📝",
        EntryKind::Image => "🖼️",
        EntryKind::Files => "📁",
        EntryKind::RichText => "📝",
    }
}

fn kind_sub(meta: &EntryMeta, settings: &Settings) -> String {
    if let Some(idx) = phrase_index_of(meta.id) {
        return settings
            .phrases
            .get(idx)
            .map(|p| format!("短语 · {}", p.phrase))
            .unwrap_or_else(|| "短语".to_string());
    }
    let mut parts = Vec::new();
    if meta.pinned {
        parts.push("已置顶".into());
    }
    if !meta.source_app.is_empty() {
        parts.push(meta.source_app.clone());
    }
    parts.join(" · ")
}

fn filter_label(state: &State) -> String {
    if state.phrase_only {
        return "⚡ 短语".into();
    }
    if let Some(src) = state.source_filter.as_deref() {
        return format!("来源 · {src}");
    }
    match state.filter {
        None => "全部".into(),
        Some(EntryKind::Text | EntryKind::RichText) => "📝 文本".into(),
        Some(EntryKind::Image) => "🖼️ 图片".into(),
        Some(EntryKind::Files) => "📁 文件".into(),
    }
}

fn cycle_filter(filter: Option<EntryKind>) -> Option<EntryKind> {
    match filter {
        None => Some(EntryKind::Text),
        Some(EntryKind::Text | EntryKind::RichText) => Some(EntryKind::Image),
        Some(EntryKind::Image) => Some(EntryKind::Files),
        Some(EntryKind::Files) => None,
    }
}

fn window_height(rows: usize, search: bool, max_h: f32, row_h: f32) -> f32 {
    let max_h = max_h.clamp(WIN_MIN_H, 900.0);
    let chrome = HEADER_H + FOOTER_H + if search { SEARCH_H } else { 0.0 };
    let content = (rows as f32 * row_h).min(max_h - chrome).max(0.0);
    (chrome + content).clamp(WIN_MIN_H, max_h)
}

fn row_h(s: &Settings) -> f32 {
    let n = s.preview_max_lines.clamp(1, 10);
    (ROW_H + (n as f32 - 1.0) * 16.0).min(92.0)
}

fn batch_label(state: &State) -> String {
    match state.settings.batch_mode.as_str() {
        "Fifo" => {
            if state.batch_queue.is_empty() {
                "FIFO".into()
            } else {
                format!("FIFO · {}", state.batch_queue.len())
            }
        }
        "Lifo" => {
            if state.batch_queue.is_empty() {
                "LIFO".into()
            } else {
                format!("LIFO · {}", state.batch_queue.len())
            }
        }
        _ => "普通".into(),
    }
}

fn footer_hint(state: &State) -> String {
    if !state.notice.is_empty() {
        return state.notice.clone();
    }
    if state.window_pinned {
        return "已钉住 · 粘贴不关 · 点图钉取消".into();
    }
    if state.pending_delete.is_some() {
        return "再按 Del 确认删除 · Esc 取消".into();
    }
    let m = match state.settings.panel_key.as_str() {
        "Alt" => "Alt",
        "Win" => "Win",
        "CapsLock" => "Caps",
        _ => "Ctrl",
    };
    format!("{m}+N快贴 · ↑↓选择 · ←→翻页 · Home/End · Enter粘贴")
}

// ================= FileJump Picker (M5d) =================

fn fj_show(state: &mut State, deps: &LogicDeps, found: Option<isize>, auto_jump: bool) {
    let gen = {
        let fj = &mut state.fj;
        fj.gen += 1;
        fj.visible = true;
        fj.dialog = found.unwrap_or(0);
        fj.global_mode = found.is_none();
    fj.query.clear();
    fj.items.clear();
    fj.filtered.clear();
    fj.current_folder = None;
    fj.selected = 0;
    fj.pending_auto_jump = auto_jump && !fj.global_mode;
    fj.navigating = false;
    fj.hint = "正在采集路径…".to_string();
    fj.shown_at = Some(std::time::Instant::now());
    fj.last_dlg_rect = found.and_then(crate::win_popup::dialog_rect);
    #[cfg(windows)]
    {
        fj.follow_kill = crate::filejump::follow_spawn(deps.evt_tx.clone(), fj.dialog);
    }
    fj.gen
    };
    state.fj.kind = found.and_then(|d| {
        #[cfg(windows)]
        {
            clipx_filejump::dialog::win::classify_hwnd(d).ok()
        }
        #[cfg(not(windows))]
        {
            let _ = d;
            None
        }
    });
    keyboard_hook::set_visible(true);
    crate::keyboard_hook::set_fj_picker(true);
    crate::keyboard_hook::set_fj_dialog(state.fj.dialog);
    crate::filejump::push_fj_ui(&deps.fj, &state.fj, true, state.fj.dialog);
    let req = crate::filejump::CollectReq {
        gen,
        query: String::new(),
        favorites: state.settings.filejump_favorites.clone(),
        recent: state.settings.filejump_recent.clone(),
        everything_enabled: false, // empty query: no Everything supplement yet
        dialog: state.fj.dialog,
    };
    crate::filejump::spawn_collect(deps.evt_tx.clone(), deps.gate.clone(), req);
}

fn fj_hide(state: &mut State, deps: &LogicDeps) {
    state.fj.visible = false;
    state.fj.gen += 1; // drop in-flight background results
    state.fj.navigating = false;
    state.fj.pending_auto_jump = false;
    state.fj.last_dlg_rect = None;
    #[cfg(windows)]
    {
        crate::filejump::follow_stop(state.fj.follow_kill);
        state.fj.follow_kill = 0;
    }
    if !state.visible {
        keyboard_hook::set_visible(false);
    }
    crate::keyboard_hook::set_fj_picker(false);
    crate::keyboard_hook::set_fj_dialog(0);
    crate::filejump::hide_fj_ui(&deps.fj);
}

fn fj_apply_filter_inner(fj: &mut crate::filejump::FjState) {
    fj.filtered = crate::filejump::filter_items(&fj.items, &fj.query);
    if fj.fav_only {
        fj.filtered.retain(|&i| {
            fj.items
                .get(i)
                .map(|c| c.source == clipx_filejump::CandidateSource::Favorite)
                .unwrap_or(false)
        });
    }
    if fj.selected >= fj.filtered.len() {
        fj.selected = fj.filtered.len().saturating_sub(1);
    }
}

fn fj_apply_filter(state: &mut State) {
    fj_apply_filter_inner(&mut state.fj);
}

fn fj_refilter(state: &mut State, deps: &LogicDeps, ev_requery: bool) {
    let gen = {
        let fj = &mut state.fj;
        fj.gen += 1;
        fj_apply_filter_inner(fj);
        fj.selected = 0;
        fj.first_visible = 0;
        fj.hint.clear();
        fj.gen
    };
    crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
    if ev_requery && !state.fj.query.trim().is_empty() {
        crate::filejump::spawn_ev_query(
            deps.evt_tx.clone(),
            gen,
            state.fj.query.clone(),
            state.fj.current_folder.clone(),
            state.settings.filejump_everything_search,
        );
    }
}

fn fj_activate(state: &mut State, deps: &LogicDeps) {
    let Some(&idx) = state.fj.filtered.get(state.fj.selected) else {
        return;
    };
    let Some(c) = state.fj.items.get(idx).cloned() else {
        return;
    };
    if state.fj.global_mode || state.fj.dialog == 0 {
        // No dialog: open in Explorer + learn as recent.
        #[cfg(windows)]
        clipx_filejump::inject::win::open_in_explorer(&c.path);
        let mut recent = state.settings.filejump_recent.clone();
        crate::filejump::push_recent(
            &deps.settings_path,
            &mut recent,
            &c.path,
            state.settings.filejump_recent_max,
        );
        fj_hide(state, deps);
        return;
    }
    let kind = state.fj.kind.unwrap_or(clipx_filejump::DialogKind::System);
    state.fj.navigating = true;
    state.fj.gen += 1;
    let gen = state.fj.gen;
    crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
    crate::filejump::spawn_navigate(
        deps.evt_tx.clone(),
        state.fj.dialog,
        kind,
        c.path,
        state.settings.filejump_shell_inject,
        gen,
    );
}

fn fj_toggle_favorite(state: &mut State, deps: &LogicDeps) {
    let Some(&idx) = state.fj.filtered.get(state.fj.selected) else {
        return;
    };
    let Some(path) = state.fj.items.get(idx).map(|c| c.path.clone()) else {
        return;
    };
    let mut favs = state.settings.filejump_favorites.clone();
    if favs
        .iter()
        .any(|f| f.path().to_lowercase() == path.to_lowercase())
    {
        favs.retain(|f| f.path().to_lowercase() != path.to_lowercase());
        state.fj.hint = "已从收藏移除".to_string();
    } else {
        favs.push(crate::settings::FolderFavorite::full(String::new(), path));
        state.fj.hint = "已加入收藏".to_string();
    }
    crate::filejump::persist_lists(
        &deps.settings_path,
        &favs,
        &state.settings.filejump_recent,
        state.settings.filejump_recent_max,
    );
    crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
}

fn fj_remove_selected(state: &mut State, deps: &LogicDeps) {
    use clipx_filejump::CandidateSource;
    let Some(&idx) = state.fj.filtered.get(state.fj.selected) else {
        return;
    };
    let Some(c) = state.fj.items.get(idx).cloned() else {
        return;
    };
    let removed = match c.source {
        CandidateSource::Favorite => {
            let mut favs = state.settings.filejump_favorites.clone();
            favs.retain(|f| f.path().to_lowercase() != c.path.to_lowercase());
            crate::filejump::persist_lists(
                &deps.settings_path,
                &favs,
                &state.settings.filejump_recent,
                state.settings.filejump_recent_max,
            );
            state.fj.hint = "已从收藏移除".to_string();
            true
        }
        CandidateSource::Recent => {
            let mut recent = state.settings.filejump_recent.clone();
            recent.retain(|f| f.to_lowercase() != c.path.to_lowercase());
            crate::filejump::persist_lists(
                &deps.settings_path,
                &state.settings.filejump_favorites,
                &recent,
                state.settings.filejump_recent_max,
            );
            state.fj.hint = "已从最近移除".to_string();
            true
        }
        _ => false,
    };
    if removed {
        state.fj.items.remove(idx);
        state.fj.filtered = crate::filejump::filter_items(&state.fj.items, &state.fj.query);
        state.fj.selected = state
            .fj
            .selected
            .min(state.fj.filtered.len().saturating_sub(1));
        crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
    }
}

fn fj_scroll_page(state: &mut State, deps: &LogicDeps, direction: i32) {
    let n = state.fj.filtered.len();
    if n == 0 {
        return;
    }
    let vis = n.min(8).max(1);
    let rel = state.fj.selected.saturating_sub(state.fj.first_visible);
    let max_first = n.saturating_sub(vis);
    let new_first = (state.fj.first_visible as i32 + direction * 8).clamp(0, max_first as i32) as usize;
    state.fj.first_visible = new_first;
    state.fj.selected = (new_first + rel).min(n - 1);
    crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
}

fn fj_ensure_visible(fj: &mut crate::filejump::FjState) {
    let n = fj.filtered.len();
    if n == 0 {
        fj.first_visible = 0;
        return;
    }
    let vis = n.min(8).max(1);
    if fj.selected < fj.first_visible {
        fj.first_visible = fj.selected;
    } else if fj.selected >= fj.first_visible + vis {
        fj.first_visible = fj.selected + 1 - vis;
    }
    let max_first = n.saturating_sub(vis);
    if fj.first_visible > max_first {
        fj.first_visible = max_first;
    }
}

fn fj_key(k: KeyEvt, state: &mut State, deps: &LogicDeps) {
    let n = state.fj.filtered.len();
    match k {
        KeyEvt::Esc => {
            if !state.fj.query.is_empty() {
                state.fj.query.clear();
                fj_refilter(state, deps, false);
            } else {
                fj_hide(state, deps);
            }
        }
        KeyEvt::Enter => fj_activate(state, deps),
        KeyEvt::Up => {
            if n > 0 {
                state.fj.selected = state.fj.selected.saturating_sub(1);
                fj_ensure_visible(&mut state.fj);
                crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
            }
        }
        KeyEvt::Down => {
            if n > 0 {
                state.fj.selected = (state.fj.selected + 1).min(n - 1);
                fj_ensure_visible(&mut state.fj);
                crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
            }
        }
        KeyEvt::PgUp | KeyEvt::Left => {
            if n > 0 {
                fj_scroll_page(state, deps, -1);
            }
        }
        KeyEvt::PgDn | KeyEvt::Right => {
            if n > 0 {
                fj_scroll_page(state, deps, 1);
            }
        }
        KeyEvt::QuickNum(d) => {
            if let Some(sel) = index_of_display(d, state.fj.first_visible, n) {
                state.fj.selected = sel;
                fj_activate(state, deps);
            }
        }
        KeyEvt::Home => {
            state.fj.selected = 0;
            state.fj.first_visible = 0;
            crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
        }
        KeyEvt::End => {
            if n > 0 {
                state.fj.selected = n - 1;
                fj_ensure_visible(&mut state.fj);
                crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
            }
        }
        KeyEvt::Backspace => {
            if state.fj.query.pop().is_some() {
                fj_refilter(state, deps, true);
            }
        }
        KeyEvt::Delete => fj_remove_selected(state, deps),
        KeyEvt::Menu | KeyEvt::PinToggle => fj_toggle_favorite(state, deps),
        KeyEvt::QuickTab => {
            state.fj.fav_only = !state.fj.fav_only;
            fj_refilter(state, deps, false);
        }
        KeyEvt::Char(c) => {
            if state.fj.query.chars().count() < 64 {
                state.fj.query.push(c);
                fj_refilter(state, deps, true);
            }
        }
        KeyEvt::Digit(d) => {
            if state.fj.query.chars().count() < 64 {
                state.fj.query.push((b'0' + d) as char);
                fj_refilter(state, deps, true);
            }
        }
        KeyEvt::Space => {
            if state.fj.query.chars().count() < 64 {
                state.fj.query.push(' ');
                fj_refilter(state, deps, true);
            }
        }
        _ => {}
    }
}

// ================= 设置应用（Phase A）=================

/// 保存成功后生效：写盘已在 handle_save 前完成（settings::save），
/// 此处更新真相源 + 全部副作用（热键重注册/自启/钩子开关/策略/watcher/托盘）。
fn apply_settings(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) -> bool {
    let draft = state.settings_win.draft_settings().clone();
    if crate::settings::save(&deps.settings_path, &draft).is_err() {
        state.settings_win.set_error("写设置文件失败".to_string());
        return false;
    }
    state.settings = draft;
    let s = &state.settings;
    deps.store.set_limits(clipx_store::StoreLimits {
        max_items: s.max_items,
        max_image_items: s.max_image_items,
    });
    // 热键重注册（含 Win+V 替换）。
    let _ = deps.hotkey_tx.send(crate::hotkeys_from_settings(s));
    // 开机自启（含管理员）。
    crate::autostart::set(s.run_at_startup, s.run_as_admin);
    // 钩子与策略。
    crate::keyboard_hook::set_qf_enabled(s.explorer_everything_quickfind_enabled);
    crate::keyboard_hook::set_page_hotkeys(s.page_up, s.page_down);
    crate::keyboard_hook::set_passthrough(
        s.passthrough_enabled,
        s.passthrough_mask,
        s.passthrough_keep_panel_keys,
        &s.passthrough_rules,
    );
    crate::filejump::set_follow_mouse(s.filejump_follow_mode == "Mouse");
    crate::policy::set_exclusions(&s.exclusion_apps);
    crate::policy::set_panel_key(&s.panel_key);
    crate::policy::set_ocr_enabled(s.image_ocr_enabled);
    crate::policy::apply_win_v_replace(s.replace_win_v);
    crate::keyboard_hook::set_replace_win_v(s.replace_win_v);
    clipx_monitor::set_max_image_bytes(s.max_image_bytes);
    clipx_filejump::custom::set_runtime_rules(
        clipx_filejump::custom::CustomStore::load(&custom_dialogs_path(&deps.settings_path)).rules,
    );
    crate::filejump::watch_set(
        s.filejump_enabled,
        s.filejump_auto_popup,
        s.filejump_show_delay_ms,
    );
    // 主题即时生效。
    crate::settings_win::apply_theme(&s.theme, weak);
    sync_batch_watch(state);
    refresh_tray(state, deps);
    true
}

fn notify(state: &mut State, deps: &LogicDeps, msg: impl Into<String>) {
    let msg = msg.into();
    state.notice = msg.clone();
    state.settings_win.set_notice(msg);
    if state.settings_win.open {
        crate::settings_win::push(&deps.settings_win, &state.settings_win);
    }
    refresh_tray(state, deps);
}

fn refresh_tray(state: &State, deps: &LogicDeps) {
    let Some(tray) = deps.tray.as_ref() else {
        return;
    };
    let paused = crate::policy::is_capture_paused();
    let autostart_label: slint::SharedString = if crate::autostart::is_enabled() {
        "开机自启：开".into()
    } else {
        "开机自启：关".into()
    };
    let pause_label: slint::SharedString = if paused {
        "继续采集".into()
    } else {
        "暂停采集".into()
    };
    let armed = state
        .clear_armed_at
        .map(|t| t.elapsed() < Duration::from_secs(5))
        .unwrap_or(false);
    let clear_label: slint::SharedString = if armed {
        "再点一次确认清空".into()
    } else {
        "清空历史".into()
    };
    let tip: slint::SharedString = {
        let batch = match state.settings.batch_mode.as_str() {
            "Fifo" => " · FIFO",
            "Lifo" => " · LIFO",
            _ => "",
        };
        if !state.notice.is_empty() {
            format!("clipx{batch} · {}", state.notice).into()
        } else if paused {
            format!("clipx · 已暂停采集{batch} ({})", state.settings.hotkey.display()).into()
        } else if let Some(tag) = state.settings.last_update_tag.as_ref() {
            format!("clipx{batch} · 新版本 {tag} ({})", state.settings.hotkey.display()).into()
        } else {
            format!("clipx{batch} ({})", state.settings.hotkey.display()).into()
        }
    };
    let batch_mode = state.settings.batch_mode.clone();
    let tray = tray.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(t) = tray.upgrade() {
            t.set_autostart_label(autostart_label);
            t.set_pause_label(pause_label);
            t.set_clear_label(clear_label);
            t.set_tip_text(tip);
            t.set_glyph(tray_glyph_for_mode(&batch_mode));
        }
    });
}

fn tray_glyph_for_mode(mode: &str) -> slint::Image {
    let bytes = include_bytes!("../assets/tray.png");
    let img = image::load_from_memory(bytes).unwrap_or_else(|_| image::DynamicImage::new_rgba8(16, 16));
    let mut rgba = img.to_rgba8();
    let (tr, tg, tb) = match mode {
        "Fifo" => (46u8, 204, 113),
        "Lifo" => (241, 196, 15),
        _ => (19, 148, 147),
    };
    for p in rgba.pixels_mut() {
        if p[3] > 0 {
            p[0] = tr;
            p[1] = tg;
            p[2] = tb;
        }
    }
    let (w, h) = rgba.dimensions();
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
    slint::Image::from_rgba8(buf)
}

/// 清空历史两段确认（WPF 二次确认语义；快捷短语存 JSON 不受影响）。
fn settings_clear_flow(state: &mut State, deps: &LogicDeps) {
    if !state.settings_win.clear_armed() {
        state.settings_win.arm_clear();
        crate::settings_win::push(&deps.settings_win, &state.settings_win);
        return;
    }
    let n = deps.store.clear_all();
    state.settings_win.disarm_clear();
    state.settings_win.set_error(format!("已清空 {n} 条历史（快捷短语保留）"));
    // 主列表同步刷新。
    state.thumb_cache.clear();
    state.batch_queue.clear();
    sync_batch_watch(state);
    state.query.clear();
    state.items = merge_list_items(state, deps);
    state.selected = 0;
    crate::settings_win::push(&deps.settings_win, &state.settings_win);
}

fn settings_excl_add(state: &mut State) {
    if !state.settings_win.excl_add_current() {
        state
            .settings_win
            .set_error("已在名单中或未选择进程".to_string());
    } else {
        state.settings_win.set_error(String::new());
    }
}

fn settings_excl_del(state: &mut State, i: i32) {
    state.settings_win.excl_remove(i.max(0) as usize);
}

fn settings_custom_del(state: &mut State, i: i32) {
    if !state.settings_win.custom_remove(i.max(0) as usize) {
        state.settings_win.set_error("删除失败".to_string());
    } else {
        state.settings_win.set_error(String::new());
    }
}

fn settings_custom_import(state: &mut State) {
    let msg = state.settings_win.custom_import();
    state.settings_win.set_error(msg);
}

fn settings_custom_export(state: &mut State) {
    let msg = state.settings_win.custom_export();
    state.settings_win.set_error(msg);
}

fn tray_clear_flow(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    let armed = state
        .clear_armed_at
        .map(|t| t.elapsed() < Duration::from_secs(5))
        .unwrap_or(false);
    if !armed {
        state.clear_armed_at = Some(std::time::Instant::now());
        refresh_tray(state, deps);
        return;
    }
    state.clear_armed_at = None;
    let n = deps.store.clear_all();
    state.thumb_cache.clear();
    state.batch_queue.clear();
    sync_batch_watch(state);
    state.query.clear();
    state.items = merge_list_items(state, deps);
    state.selected = 0;
    if state.visible {
        push_ui(state, weak);
    }
    refresh_tray(state, deps);
    let _ = n;
}

fn is_phrase_id(id: i64) -> bool {
    id < 0
}

fn phrase_entry_id(index: usize) -> i64 {
    -(index as i64 + 1)
}

fn phrase_index_of(id: i64) -> Option<usize> {
    if id < 0 {
        Some(((-id) as usize).saturating_sub(1))
    } else {
        None
    }
}

fn phrase_matches(qp: &QuickPaste, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    let ql = q.to_lowercase();
    if qp.phrase.to_lowercase().contains(&ql) || qp.content.to_lowercase().contains(&ql) {
        return true;
    }
    let blob = to_pinyin_blob(&format!("{}{}", qp.phrase, qp.content));
    !blob.is_empty() && blob.contains(&ql)
}

fn truncate_preview(text: &str, max_lines: i64) -> String {
    let max_lines = max_lines.clamp(1, 10) as usize;
    const MAX_CHARS: usize = 200;
    if text.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let taken: Vec<&str> = lines.iter().take(max_lines).copied().collect();
    let mut result = taken.join("\n");
    if result.chars().count() > MAX_CHARS {
        let end = result
            .char_indices()
            .nth(MAX_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(result.len());
        result.truncate(end);
        result.push('…');
    } else if lines.len() > max_lines {
        result.push_str(" …");
    }
    result
}

fn phrase_metas(settings: &Settings, query: &str) -> Vec<EntryMeta> {
    settings
        .phrases
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.content.is_empty() && phrase_matches(p, query))
        .map(|(i, p)| EntryMeta {
            id: phrase_entry_id(i),
            kind: EntryKind::Text,
            preview: truncate_preview(&p.content, settings.preview_max_lines),
            pinned: false,
            created_ms: 0,
            source_app: String::new(),
        })
        .collect()
}

fn merge_list_items(state: &State, deps: &LogicDeps) -> Vec<EntryMeta> {
    let phrases = if state.filter.is_none()
        || matches!(state.filter, Some(EntryKind::Text | EntryKind::RichText))
        || state.phrase_only
    {
        phrase_metas(&state.settings, &state.query)
    } else {
        Vec::new()
    };
    if state.phrase_only {
        return phrases;
    }
    let hist = deps.store.search_ex(
        &state.query,
        state.filter,
        state.source_filter.as_deref(),
        state.settings.deep_search,
        list_limit(&state.settings),
    );
    if state.query.trim().is_empty() {
        let mut out = hist;
        out.extend(phrases);
        out
    } else {
        let mut out = phrases;
        out.extend(hist);
        rank_results(&mut out, &state.query);
        out
    }
}

fn delete_item(state: &mut State, deps: &LogicDeps, id: i64) {
    if let Some(idx) = phrase_index_of(id) {
        if idx < state.settings.phrases.len() {
            state.settings.phrases.remove(idx);
            let _ = crate::settings::save(&deps.settings_path, &state.settings);
        }
    } else {
        deps.store.delete(id);
    }
}

fn begin_phrase_edit(state: &mut State, deps: &LogicDeps, meta: &EntryMeta) {
    if let Some(idx) = phrase_index_of(meta.id) {
        if let Some(p) = state.settings.phrases.get(idx).cloned() {
            state.phrase_edit = Some(PhraseEdit {
                content: p.content,
                buffer: p.phrase,
            });
        }
        return;
    }
    if !matches!(meta.kind, EntryKind::Text | EntryKind::RichText) {
        return;
    }
    let content = deps.store.get_text(meta.id).unwrap_or_default();
    if content.is_empty() {
        return;
    }
    state.phrase_edit = Some(PhraseEdit {
        content,
        buffer: String::new(),
    });
}

fn handle_phrase_edit_key(
    k: KeyEvt,
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    match k {
        KeyEvt::Esc => {
            state.phrase_edit = None;
            push_ui(state, weak);
        }
        KeyEvt::Enter => commit_phrase_edit(state, deps, weak),
        KeyEvt::Backspace => {
            if let Some(e) = state.phrase_edit.as_mut() {
                e.buffer.pop();
            }
            push_ui(state, weak);
        }
        KeyEvt::Char(c) => {
            if let Some(e) = state.phrase_edit.as_mut() {
                if e.buffer.chars().count() < 200 {
                    e.buffer.push(c);
                }
            }
            push_ui(state, weak);
        }
        KeyEvt::Digit(n) => {
            if let Some(e) = state.phrase_edit.as_mut() {
                if e.buffer.chars().count() < 200 {
                    e.buffer.push((b'0' + n) as char);
                }
            }
            push_ui(state, weak);
        }
        _ => {}
    }
}

fn commit_phrase_edit(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    let Some(edit) = state.phrase_edit.take() else {
        push_ui(state, weak);
        return;
    };
    let phrase = edit.buffer.trim().to_string();
    if phrase.is_empty() {
        push_ui(state, weak);
        return;
    }
    state
        .settings
        .phrases
        .retain(|p| p.content != edit.content);
    state.settings.phrases.push(QuickPaste {
        phrase,
        content: edit.content,
    });
    let _ = crate::settings::save(&deps.settings_path, &state.settings);
    refresh(state, deps, weak, false);
}

fn sel_range(state: &State) -> (usize, usize) {
    let a = state.sel_anchor.min(state.selected);
    let b = state.sel_anchor.max(state.selected);
    (a, b)
}

fn split_hit(preview: &str, query: &str) -> (String, String, String) {
    let q = query.trim();
    if q.is_empty() {
        return (String::new(), String::new(), String::new());
    }
    let pl = preview.to_lowercase();
    let ql = q.to_lowercase();
    if let Some(byte) = pl.find(&ql) {
        let end = byte + ql.len();
        if end <= preview.len() {
            return (
                preview[..byte].to_string(),
                preview[byte..end].to_string(),
                preview[end..].to_string(),
            );
        }
    }
    (String::new(), String::new(), String::new())
}

fn rank_results(items: &mut [EntryMeta], query: &str) {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return;
    }
    let now = now_ms();
    items.sort_by(|a, b| {
        rank_score(b, &q, now)
            .partial_cmp(&rank_score(a, &q, now))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn rank_score(m: &EntryMeta, q: &str, now: i64) -> f64 {
    let mut s = 0.0;
    if is_phrase_id(m.id) {
        s += 80.0;
    }
    if m.pinned {
        s += 40.0;
    }
    let p = m.preview.to_lowercase();
    if p == *q {
        s += 50.0;
    } else if p.starts_with(q) {
        s += 25.0;
    } else if p.contains(q) {
        s += 10.0;
    }
    let age_h = ((now - m.created_ms).max(0) as f64) / 3_600_000.0;
    s += (-age_h / 72.0).exp() * 8.0;
    s
}

fn paste_selection(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    with_newlines: bool,
) {
    if state.settings.batch_mode != "Off" {
        activate_item(state, deps, weak, clipboard, state.selected);
        return;
    }
    let (lo, hi) = sel_range(state);
    if lo == hi {
        activate_item(state, deps, weak, clipboard, state.selected);
        return;
    }
    let ids: Vec<(i64, EntryKind)> = state.items[lo..=hi]
        .iter()
        .map(|m| (m.id, m.kind))
        .collect();
    if !write_merged_clipboard(&ids, deps, clipboard, &state.settings, with_newlines) {
        return;
    }
    finish_paste(state, weak);
}

fn write_merged_clipboard(
    ids: &[(i64, EntryKind)],
    deps: &LogicDeps,
    clipboard: Option<&ClipboardContext>,
    settings: &Settings,
    with_newlines: bool,
) -> bool {
    let Some(ctx) = clipboard else {
        return false;
    };
    let merge_text = settings.batch_merge_text || with_newlines;
    let mut texts = Vec::new();
    let mut images = Vec::new();
    let mut files = Vec::new();
    for &(id, kind) in ids {
        if is_phrase_id(id) {
            if let Some(idx) = phrase_index_of(id) {
                if let Some(p) = settings.phrases.get(idx) {
                    texts.push(p.content.clone());
                }
            }
            continue;
        }
        match kind {
            EntryKind::Text | EntryKind::RichText => {
                if let Some(t) = deps.store.get_text(id) {
                    texts.push(t);
                }
            }
            EntryKind::Image => {
                if let Some(img) = deps.store.get_image(id) {
                    images.push(img);
                }
            }
            EntryKind::Files => {
                if let Some(p) = deps.store.get_files(id) {
                    files.extend(p);
                }
            }
        }
    }
    deps.gate.arm();
    if !files.is_empty() || !images.is_empty() {
        let mut drop_files = files;
        let dir = std::env::temp_dir().join("clipx-paste");
        if std::fs::create_dir_all(&dir).is_ok() {
            for (i, img) in images.iter().enumerate() {
                let p = dir.join(format!("clipx-img-{i}.png"));
                if std::fs::write(&p, &img.blob).is_ok() {
                    drop_files.push(p.to_string_lossy().into_owned());
                }
            }
        }
        return paste::write_files(ctx, &drop_files).is_ok();
    }
    if texts.is_empty() {
        return false;
    }
    let joined = if merge_text || with_newlines {
        texts.join("\n")
    } else {
        texts.concat()
    };
    paste::write_text(ctx, &joined).is_ok()
}

fn finish_paste(state: &mut State, weak: &slint::Weak<PopupWindow>) {
    state.last_paste_at = Some(std::time::Instant::now());
    let target = {
        #[cfg(windows)]
        {
            state.foreground_at_show
        }
        #[cfg(not(windows))]
        {
            0
        }
    };
    if !state.window_pinned {
        hide_popup(state, weak);
    }
    if state.settings.paste_simulate {
        std::thread::sleep(Duration::from_millis(80));
        #[cfg(windows)]
        win_popup::restore_foreground(target);
        std::thread::sleep(Duration::from_millis(30));
        paste::send_paste(paste::paste_mode_for_target(
            target,
            &state.settings.paste_mode,
        ));
    }
}

fn paste_ocr(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
) {
    let Some(meta) = state.items.get(idx).cloned() else {
        return;
    };
    let Some(ctx) = clipboard else {
        return;
    };
    let text = if meta.kind == EntryKind::Image {
        deps.store
            .get_ocr(meta.id)
            .and_then(|o| o.text)
            .unwrap_or_default()
    } else {
        deps.store.get_text(meta.id).unwrap_or_default()
    };
    if text.trim().is_empty() {
        return;
    }
    deps.gate.arm();
    if paste::write_text(ctx, &text).is_err() {
        return;
    }
    finish_paste(state, weak);
}

fn paste_as_file(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
    as_json: bool,
) {
    let Some(meta) = state.items.get(idx).cloned() else {
        return;
    };
    let Some(ctx) = clipboard else {
        return;
    };
    let stamp = now_ms();
    let files = if as_json {
        let payload = serde_json::json!({
            "kind": meta.kind.as_i64(),
            "preview": meta.preview,
            "source_app": meta.source_app,
            "text": deps.store.get_text(meta.id),
            "files": deps.store.get_files(meta.id),
            "ocr": deps.store.get_ocr(meta.id).and_then(|o| o.text),
        });
        let bytes = serde_json::to_vec_pretty(&payload).unwrap_or_default();
        vec![(format!("clipx-{stamp}.json"), bytes)]
    } else {
        match meta.kind {
            EntryKind::Image => {
                let Some(img) = deps.store.get_image(meta.id) else {
                    return;
                };
                vec![(format!("clipx-{stamp}.png"), img.blob)]
            }
            EntryKind::Files => {
                if let Some(paths) = deps.store.get_files(meta.id) {
                    deps.gate.arm();
                    if paste::write_files(ctx, &paths).is_ok() {
                        finish_paste(state, weak);
                    }
                }
                return;
            }
            _ => {
                let text = deps.store.get_text(meta.id).unwrap_or(meta.preview.clone());
                vec![(format!("clipx-{stamp}.txt"), text.into_bytes())]
            }
        }
    };
    deps.gate.arm();
    if paste::write_temp_files(ctx, &files).is_ok() {
        finish_paste(state, weak);
    }
}

fn save_image_to_temp(
    state: &mut State,
    deps: &LogicDeps,
    meta: &EntryMeta,
    clipboard: Option<&ClipboardContext>,
) {
    if meta.kind != EntryKind::Image {
        return;
    }
    let Some(img) = deps.store.get_image(meta.id) else {
        return;
    };
    let dir = std::env::temp_dir().join("clipx-save");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("clipx-{}.png", now_ms()));
    if std::fs::write(&path, &img.blob).is_ok() {
        if let Some(ctx) = clipboard {
            deps.gate.arm();
            let _ = paste::write_text(ctx, &path.to_string_lossy());
        }
        notify(state, deps, format!("已保存：{}", path.display()));
    }
}

fn copy_entry_path(
    state: &mut State,
    deps: &LogicDeps,
    meta: &EntryMeta,
    clipboard: Option<&ClipboardContext>,
    weak: &slint::Weak<PopupWindow>,
) {
    let Some(ctx) = clipboard else {
        return;
    };
    let text = if meta.kind == EntryKind::Files {
        deps.store
            .get_files(meta.id)
            .map(|p| p.join("\n"))
            .unwrap_or_default()
    } else if meta.kind == EntryKind::Image {
        format!("clipx-image-{}", meta.id)
    } else {
        deps.store.get_text(meta.id).unwrap_or(meta.preview.clone())
    };
    if text.is_empty() {
        return;
    }
    deps.gate.arm();
    if paste::write_text(ctx, &text).is_ok() {
        hide_popup(state, weak);
    }
}

fn begin_text_edit(
    state: &mut State,
    deps: &LogicDeps,
    meta: &EntryMeta,
    weak: &slint::Weak<PopupWindow>,
) {
    if is_phrase_id(meta.id) || !matches!(meta.kind, EntryKind::Text | EntryKind::RichText) {
        push_ui(state, weak);
        return;
    }
    let text = deps.store.get_text(meta.id).unwrap_or_default();
    state.text_edit = Some(TextEdit {
        id: meta.id,
        buffer: text,
    });
    push_ui(state, weak);
}

fn handle_text_edit_key(
    k: KeyEvt,
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    match k {
        KeyEvt::Esc => {
            state.text_edit = None;
            push_ui(state, weak);
        }
        KeyEvt::CtrlEnter | KeyEvt::Enter => {
            if matches!(k, KeyEvt::CtrlEnter) || matches!(k, KeyEvt::Enter) {
                // Ctrl+Enter 保存；单独 Enter 也允许保存（侧栏无真正多行输入控件）
            }
            if let Some(edit) = state.text_edit.take() {
                let _ = deps.store.update_text(edit.id, edit.buffer);
            }
            refresh(state, deps, weak, false);
        }
        KeyEvt::Backspace => {
            if let Some(e) = state.text_edit.as_mut() {
                e.buffer.pop();
            }
            push_ui(state, weak);
        }
        KeyEvt::Char(c) => {
            if let Some(e) = state.text_edit.as_mut() {
                e.buffer.push(c);
            }
            push_ui(state, weak);
        }
        KeyEvt::Digit(n) => {
            if let Some(e) = state.text_edit.as_mut() {
                e.buffer.push((b'0' + n) as char);
            }
            push_ui(state, weak);
        }
        KeyEvt::Space => {
            if let Some(e) = state.text_edit.as_mut() {
                e.buffer.push(' ');
            }
            push_ui(state, weak);
        }
        _ => {}
    }
}

fn batch_flush_all(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
) {
    if state.batch_queue.is_empty() {
        return;
    }
    let ids: Vec<(i64, EntryKind)> = state
        .batch_queue
        .iter()
        .filter_map(|id| {
            meta_by_id(state, deps, *id).map(|m| (m.id, m.kind))
        })
        .collect();
    if write_merged_clipboard(&ids, deps, clipboard, &state.settings, true) {
        state.batch_queue.clear();
        if state.settings.batch_auto_off_when_empty {
            state.settings.batch_mode = "Off".to_string();
            let _ = crate::settings::save(&deps.settings_path, &state.settings);
        }
        sync_batch_watch(state);
        finish_paste(state, weak);
    }
}

fn batch_enqueue_latest(
    state: &mut State,
    deps: &LogicDeps,
    clipboard: Option<&ClipboardContext>,
) {
    if state.settings.batch_mode == "Off" {
        return;
    }
    let Some(latest) = deps.store.list_recent(1).into_iter().next() else {
        return;
    };
    if state.batch_queue.contains(&latest.id) {
        return;
    }
    if state.settings.batch_mode == "Fifo" {
        state.batch_queue.push(latest.id);
    } else {
        state.batch_queue.insert(0, latest.id);
    }
    if let Some(head) = state.batch_queue.first().copied() {
        if let Some(meta) = meta_by_id(state, deps, head) {
            let _ = write_entry_clipboard(&meta, deps, clipboard, &state.settings);
        }
    }
    sync_batch_watch(state);
}

fn tray_probe_dialog(state: &mut State, deps: &LogicDeps) {
    #[cfg(windows)]
    {
        let hwnd = crate::win_popup::foreground_hwnd();
        if hwnd == 0 {
            return;
        }
        let class = clipx_filejump::dialog::win::class_of_pub(hwnd).unwrap_or_default();
        let proc = crate::policy::foreground_app();
        let title = clipx_filejump::dialog::win::title_of_pub(hwnd).unwrap_or_default();
        let path = custom_dialogs_path(&deps.settings_path);
        let mut store = clipx_filejump::custom::CustomStore::load(&path);
        store.rules.push(clipx_filejump::custom::CustomRule {
            class: class.clone(),
            process: proc.clone(),
            title_contains: String::new(),
            strategy: "alt_d".into(),
        });
        let _ = store.save(&path);
        clipx_filejump::custom::set_runtime_rules(store.rules);
        notify(
            state,
            deps,
            format!("已添加规则：{class} + {proc} 「{title}」"),
        );
    }
    #[cfg(not(windows))]
    {
        let _ = (state, deps);
    }
}

fn custom_dialogs_path(settings_path: &std::path::Path) -> std::path::PathBuf {
    settings_path
        .parent()
        .unwrap_or(settings_path)
        .join("custom_file_dialogs.json")
}

fn export_history(state: &mut State, deps: &LogicDeps) {
    let path = deps
        .settings_path
        .parent()
        .unwrap_or(&deps.settings_path)
        .join("clipx-export.json");
    match deps.store.export_json(&path) {
        Ok(n) => {
            notify(state, deps, format!("已导出 {n} 条 → {}", path.display()));
        }
        Err(e) => {
            notify(state, deps, format!("导出失败：{e}"));
        }
    }
}

fn import_history(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    let path = deps
        .settings_path
        .parent()
        .unwrap_or(&deps.settings_path)
        .join("clipx-export.json");
    match deps.store.import_json(&path) {
        Ok(st) => {
            notify(
                state,
                deps,
                format!("导入完成：新增 {}，跳过 {}", st.inserted, st.skipped_dup),
            );
            if state.visible {
                refresh(state, deps, weak, true);
            }
        }
        Err(e) => {
            notify(state, deps, format!("导入失败：{e}"));
        }
    }
}

fn check_updates_now(state: &mut State, deps: &LogicDeps) {
    crate::update_check::spawn_delayed(
        deps.evt_tx.clone(),
        state.settings.last_update_tag.clone(),
        std::time::Duration::ZERO,
    );
    notify(state, deps, "正在检查更新…");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrase_id_roundtrip() {
        assert_eq!(phrase_entry_id(0), -1);
        assert_eq!(phrase_index_of(-1), Some(0));
        assert_eq!(phrase_index_of(-3), Some(2));
        assert_eq!(phrase_index_of(12), None);
        assert!(is_phrase_id(-1));
        assert!(!is_phrase_id(1));
    }

    #[test]
    fn phrase_matches_text_and_pinyin() {
        let qp = QuickPaste {
            phrase: "邮箱".into(),
            content: "hello@example.com".into(),
        };
        assert!(phrase_matches(&qp, ""));
        assert!(phrase_matches(&qp, "hello"));
        assert!(phrase_matches(&qp, "邮箱"));
        assert!(phrase_matches(&qp, "yx"));
        assert!(!phrase_matches(&qp, "zzzz"));
    }

    #[test]
    fn truncate_preview_respects_lines() {
        let t = "one\ntwo\nthree";
        assert_eq!(truncate_preview(t, 1), "one …");
        assert_eq!(truncate_preview(t, 2), "one\ntwo …");
        assert_eq!(truncate_preview(t, 3), "one\ntwo\nthree");
    }

    #[test]
    fn split_hit_keeps_surrounding_text() {
        assert_eq!(
            split_hit("time1", "ti"),
            (String::new(), "ti".into(), "me1".into())
        );
        assert_eq!(
            split_hit("timeout 5 bash", "ti"),
            (String::new(), "ti".into(), "meout 5 bash".into())
        );
        assert_eq!(
            split_hit("notice title", "TI"),
            ("no".into(), "ti".into(), "ce title".into())
        );
        assert_eq!(
            split_hit("你好世界", "ti"),
            (String::new(), String::new(), String::new())
        );
        assert_eq!(
            split_hit("hello", ""),
            (String::new(), String::new(), String::new())
        );
    }

    #[test]
    fn visible_index_follows_first_row() {
        assert_eq!(visible_index_label(0, 0), "1");
        assert_eq!(visible_index_label(8, 0), "9");
        assert_eq!(visible_index_label(9, 0), "");
        assert_eq!(visible_index_label(8, 8), "1");
        assert_eq!(visible_index_label(16, 8), "9");
        assert_eq!(visible_index_label(17, 8), "");
        assert_eq!(index_of_display(1, 8, 20), Some(8));
        assert_eq!(index_of_display(9, 8, 20), Some(16));
        assert_eq!(index_of_display(1, 0, 0), None);
    }

    #[test]
    fn hotkey_matches_exact() {
        let hk = crate::settings::Hotkey::new(crate::settings::MOD_CONTROL, 0xBD);
        assert!(hk.matches(crate::settings::MOD_CONTROL, 0xBD));
        assert!(!hk.matches(crate::settings::MOD_CONTROL | crate::settings::MOD_SHIFT, 0xBD));
        assert!(!hk.matches(crate::settings::MOD_CONTROL, 0xBB));
    }
}
 