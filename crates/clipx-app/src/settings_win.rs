//! 设置窗口（对齐 WPF `SettingsWindow`，暂存模式 + 保存校验 + 取消回滚）。
//!
//! - 所有控件只改 `draft`；保存时解析数字 → `validate()` → 错误展示并拒存
//! - 主题循环即时预览，取消时回滚（WPF 语义）
//! - 自定义规则即时写盘（WPF：与点保存无关）；清空历史两段确认
//! - 热键录制经 `keyboard_hook::RECORDING` + `RecordVk` 事件组修饰键

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;

use slint::ComponentHandle;
use crate::logic::AppEvt;
use crate::settings::{Hotkey, PassthroughRule, Settings, MOD_ALT, MOD_CONTROL, MOD_SHIFT};

/// 录制槽位：0 呼出 / 1 批量 / 2 跳转 / 3 上翻 / 4 下翻 / 100 新穿透规则。
pub const SLOT_MAIN: i32 = 0;
pub const SLOT_BATCH: i32 = 1;
pub const SLOT_FJ: i32 = 2;
pub const SLOT_PGUP: i32 = 3;
pub const SLOT_PGDN: i32 = 4;
pub const SLOT_RULE: i32 = 100;

#[derive(Default)]
pub struct WinState {
    pub open: bool,
    draft: Settings,
    nums: HashMap<String, String>,
    recording: i32,
    /// Slint 录制期的修饰按住快照（裸修饰自反文本 \x10..\x12 去歧义用）。
    rec_held: u32,
    clear_armed: bool,
    error: String,
    proc_index: usize,
    proc_list: Vec<String>,
    proc_loading: bool,
    custom_rules: Vec<clipx_filejump::custom::CustomRule>,
    custom_path: PathBuf,
    import_path: String,
    phrase_index: usize,
}

impl WinState {
    pub fn set_notice(&mut self, msg: impl Into<String>) {
        self.error = msg.into();
    }
}

fn num_fields(s: &Settings) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("max_items".into(), s.max_items.to_string());
    m.insert("max_images".into(), s.max_image_items.to_string());
    m.insert("preview".into(), s.preview_max_lines.to_string());
    m.insert("width".into(), trim_num(s.popup_width));
    m.insert("maxheight".into(), trim_num(s.popup_max_height));
    m.insert("pageitems".into(), s.popup_page_items.to_string());
    m.insert("delay".into(), s.filejump_show_delay_ms.to_string());
    m.insert("recentmax".into(), s.filejump_recent_max.to_string());
    m.insert("addmin".into(), s.filejump_auto_add_min.to_string());
    m.insert("imgbytes".into(), s.max_image_bytes.to_string());
    m.insert(
        "qfmax".into(),
        s.explorer_everything_quickfind_max_results.to_string(),
    );
    m
}

fn trim_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn theme_label(t: &str) -> &str {
    match t {
        "Dark" => "暗色",
        "Light" => "亮色",
        _ => "跟随系统",
    }
}
fn pos_label(t: &str) -> &str {
    if t == "Mouse" {
        "鼠标处"
    } else {
        "光标处"
    }
}
fn hide_label(b: bool) -> &'static str {
    if b {
        "任意点击隐藏"
    } else {
        "仅切换应用隐藏"
    }
}
fn paste_label(t: &str) -> &str {
    if t == "ShiftInsert" {
        "Shift+Insert"
    } else {
        "Ctrl+V"
    }
}
fn follow_label(t: &str) -> &str {
    if t == "Mouse" {
        "跟随鼠标"
    } else {
        "跟随对话框"
    }
}
fn qfopen_label(t: &str) -> &str {
    if t == "DirectOpen" {
        "直接打开"
    } else {
        "从资源管理器打开"
    }
}

fn cycle(s: &str, opts: &[&str]) -> String {
    let i = opts.iter().position(|o| *o == s).unwrap_or(0);
    opts[(i + 1) % opts.len()].to_string()
}

pub fn open(
    st: &mut WinState,
    settings: &Settings,
    settings_path: &PathBuf,
    evt_tx: mpsc::Sender<AppEvt>,
) {
    st.open = true;
    st.draft = settings.clone();
    st.nums = num_fields(settings);
    st.recording = -1;
    st.rec_held = 0;
    st.clear_armed = false;
    st.error.clear();
    st.proc_index = 0;
    st.proc_list.clear();
    st.proc_loading = false;
    let dir = settings_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    st.custom_path = dir.join("custom_file_dialogs.json");
    st.import_path = st.custom_path.to_string_lossy().to_string();
    st.custom_rules = clipx_filejump::custom::CustomStore::load(&st.custom_path).rules;
    let snap = snapshot_of(st);
    let theme = st.draft.theme.clone();
    let _ = slint::invoke_from_event_loop(move || {
        // 每次打开都销毁重建实例：Slint 软件渲染器是 ReusedBuffer（只重绘变化区域），
        // 常驻窗 hide→show 后缓冲内容已丢，页面残缺/空白（切页才能恢复）。
        // 新实例等价首次显示，无此问题。强引用只放事件循环线程（CURRENT），
        // drop 也必须发生在该线程。
        CURRENT.with(|c| *c.borrow_mut() = None);
        close_current();
        let Ok(ui) = crate::SettingsWindow::new() else {
            return;
        };
        bind(&ui, evt_tx.clone());
        apply(&ui, snap);
        paint_theme(ui.global::<crate::Theme>(), &palette(&theme));
        ui.set_page(0);
        crate::win_popup::center_on_cursor_monitor(&ui.window(), 640.0, 720.0);
        let size = slint::LogicalSize::new(640.0, 720.0);
        ui.window().set_size(slint::WindowSize::Logical(size));
        let _ = ui.window().show();
        #[cfg(windows)]
        {
            // 视觉置顶已由 Slint 的 always-on-top 保证（settings.slint）；
            // 这里再用枚举拿 HWND 尽力抢一次焦点，让标题栏呈现激活色。
            if let Some(hwnd) = crate::win_popup::find_window_by_title("clipx 设置") {
                crate::win_popup::activate_window(hwnd);
            }
        }
        let _ = evt_tx.send(AppEvt::SettingsWindowReady(crate::logic::SettingsWinReady(ui.as_weak())));
        CURRENT.with(|c| *c.borrow_mut() = Some(ui));
    });
}

// 设置窗口强引用只存放在事件循环线程（Slint 组件的创建与 drop 都须在该线程）。
thread_local! {
    static CURRENT: RefCell<Option<crate::SettingsWindow>> = const { RefCell::new(None) };
}

/// UI 回调 → 逻辑线程事件（窗口每次重建都要重新绑定）。
fn bind(ui: &crate::SettingsWindow, tx: mpsc::Sender<AppEvt>) {
    ui.on_setting_bool({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::SettingBool(v.into())); } });
    ui.on_setting_cycle({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::SettingCycle(v.into())); } });
    ui.on_setting_record({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::SettingRecord(v)); } });
    ui.on_setting_key({ let tx = tx.clone(); move |t, m, r| {
        let _ = tx.send(AppEvt::SettingKey(t.to_string(), m as u32, r != 0));
    } });
    ui.on_setting_key_rel({ let tx = tx.clone(); move |t| { let _ = tx.send(AppEvt::SettingKeyRel(t.to_string())); } });
    ui.on_setting_text({ let tx = tx.clone(); move |a, b| { let _ = tx.send(AppEvt::SettingText(a.into(), b.into())); } });
    ui.on_setting_save({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::SettingSave); } });
    ui.on_setting_cancel({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::SettingCancel); } });
    ui.on_setting_clear({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::SettingClear); } });
    ui.on_excl_add({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::ExclAdd); } });
    ui.on_proc_selected({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::SettingText("proc".into(), v.into())); } });
    ui.on_excl_del({ let tx = tx.clone(); move |i| { let _ = tx.send(AppEvt::ExclDel(i)); } });
    ui.on_rule_del({ let tx = tx.clone(); move |i| { let _ = tx.send(AppEvt::RuleDel(i)); } });
    ui.on_custom_del({ let tx = tx.clone(); move |i| { let _ = tx.send(AppEvt::CustomDel(i)); } });
    ui.on_custom_import({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::CustomImport); } });
    ui.on_custom_export({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::CustomExport); } });
    ui.on_phrase_sel({ let tx = tx.clone(); move |i| { let _ = tx.send(AppEvt::SettingText("psel".into(), i.to_string())); } });
    ui.on_page_changed(move |p| { let _ = tx.send(AppEvt::SettingPage(p)); });
}

pub fn hide(_weak: &slint::Weak<crate::SettingsWindow>) {
    crate::keyboard_hook::set_recording(-1);
    let _ = slint::invoke_from_event_loop(move || {
        close_current();
    });
}

/// 关闭并销毁当前设置窗口实例。必须先显式 `window().hide()`：
/// Slint/winit 后端会保活组件，单纯 drop ComponentHandle 不关闭屏幕窗口，
/// 留下的"幽灵窗口"回调仍活但逻辑态已关，表现为所有按钮无效。
fn close_current() {
    CURRENT.with(|c| {
        if let Some(ui) = c.borrow_mut().take() {
            let _ = ui.window().hide();
            // ui 在此 drop，下一次 open 重建即等价首次显示
        }
    });
}

/// UI 快照（全 owned，进 invoke 闭包，无借用逃逸）。
struct Snapshot {
    nums: HashMap<String, String>,
    rules: Vec<String>,
    excl: Vec<String>,
    customs: Vec<String>,
    procs: Vec<String>,
    proc_index: i32,
    mask: u32,
    opacity: f64,
    error: String,
    clear_armed: bool,
    clear_label: String,
    import_path: String,
    custom_path: String,
    recording: i32,
    rec_main: String,
    rec_batch: String,
    rec_fj: String,
    rec_pgup: String,
    rec_pgdn: String,
    theme: String,
    pos: String,
    hide_outside: bool,
    touch_top: bool,
    paste_mode: String,
    panel_key: String,
    follow_mode: String,
    qfopen: String,
    auto_popup: bool,
    bools: Vec<bool>,
    phrases: Vec<String>,
    phrase_index: i32,
    phrase_trigger: String,
    phrase_body: String,
}

pub fn push(weak: &slint::Weak<crate::SettingsWindow>, st: &WinState) {
    let snap = snapshot_of(st);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        apply(&ui, snap);
    });
}

fn snapshot_of(st: &WinState) -> Snapshot {
    let d = &st.draft;
    let snap = Snapshot {
        nums: st.nums.clone(),
        rules: d.passthrough_rules.iter().map(|r| r.display()).collect(),
        excl: d.exclusion_apps.clone(),
        customs: st.custom_rules.iter().map(|r| r.summary()).collect(),
        procs: st.proc_list.clone(),
        proc_index: st.proc_index as i32,
        mask: d.passthrough_mask,
        opacity: d.popup_opacity.clamp(0.4, 1.0),
        error: st.error.clone(),
        clear_armed: st.clear_armed,
        clear_label: if st.clear_armed {
            "再次点击确认清空".to_string()
        } else {
            "清空所有历史记录".to_string()
        },
        import_path: st.import_path.clone(),
        custom_path: st.custom_path.to_string_lossy().to_string(),
        recording: st.recording,
        rec_main: d.hotkey.display(),
        rec_batch: d.batch_hotkey.display(),
        rec_fj: d.filejump_hotkey.display(),
        rec_pgup: d.page_up.display(),
        rec_pgdn: d.page_down.display(),
        theme: d.theme.clone(),
        pos: d.popup_position.clone(),
        hide_outside: d.hide_on_click_outside,
        touch_top: d.paste_touch_top,
        paste_mode: d.paste_mode.clone(),
        panel_key: d.panel_key.clone(),
        follow_mode: d.filejump_follow_mode.clone(),
        qfopen: d.explorer_quickfind_open_mode.clone(),
        auto_popup: d.filejump_auto_popup,
        bools: vec![
            d.run_at_startup,
            d.run_as_admin,
            d.check_updates,
            d.replace_win_v,
            d.batch_merge_text,
            d.batch_auto_off_when_empty,
            d.paste_double_click,
            d.clear_history_on_exit,
            d.image_ocr_enabled,
            d.paste_simulate,
            d.filejump_enabled,
            d.deep_search,
            d.filejump_auto_jump,
            d.filejump_auto_sync,
            d.filejump_shell_inject,
            d.filejump_everything_search,
            d.explorer_everything_quickfind_enabled,
            d.passthrough_enabled,
            d.passthrough_keep_panel_keys,
        ],
        phrases: d
            .phrases
            .iter()
            .map(|p| {
                if p.phrase.is_empty() {
                    format!("(空) → {}", truncate_p(&p.content))
                } else {
                    format!("{} → {}", p.phrase, truncate_p(&p.content))
                }
            })
            .collect(),
        phrase_index: st.phrase_index.min(d.phrases.len().saturating_sub(1)) as i32,
        phrase_trigger: d
            .phrases
            .get(st.phrase_index)
            .map(|p| p.phrase.clone())
            .unwrap_or_default(),
        phrase_body: d
            .phrases
            .get(st.phrase_index)
            .map(|p| p.content.clone())
            .unwrap_or_default(),
    };
    snap
}

/// Snapshot 应用到 UI 实例（open 重建与后续 patch 共用）。
fn apply(ui: &crate::SettingsWindow, snap: Snapshot) {
    use slint::{ModelRc, SharedString, VecModel};
    // 主题只在打开/循环时上色；每次交互再 paint 会卡软件渲染。
    let g = |k: &str| -> SharedString {
            snap.nums.get(k).cloned().unwrap_or_default().into()
        };
        ui.set_num_max_items(g("max_items"));
        ui.set_num_max_images(g("max_images"));
        ui.set_rec_main(snap.rec_main.into());
        ui.set_rec_batch(snap.rec_batch.into());
        ui.set_theme_label(theme_label(&snap.theme).into());
        ui.set_pos_label(pos_label(&snap.pos).into());
        ui.set_hide_label(hide_label(snap.hide_outside).into());
        ui.set_opacity_val(snap.opacity as f32);
        ui.set_opacity_label(format!("{}%", (snap.opacity * 100.0).round() as i64).into());
        ui.set_num_preview_lines(g("preview"));
        ui.set_paste_label(paste_label(&snap.paste_mode).into());
        ui.set_panel_label(snap.panel_key.into());
        ui.set_rec_pgup(snap.rec_pgup.into());
        ui.set_rec_pgdn(snap.rec_pgdn.into());
        ui.set_num_width(g("width"));
        ui.set_num_maxheight(g("maxheight"));
        ui.set_num_pageitems(g("pageitems"));
        let b = &snap.bools;
        ui.set_opt_startup(b[0]);
        ui.set_opt_admin(b[1]);
        ui.set_opt_updates(b[2]);
        ui.set_opt_winv(b[3]);
        ui.set_opt_merge(b[4]);
        ui.set_opt_autooff(b[5]);
        ui.set_opt_dblclick(b[6]);
        ui.set_opt_touch(snap.touch_top);
        ui.set_opt_clearexit(b[7]);
        ui.set_opt_ocr(b[8]);
        ui.set_opt_simulate(b[9]);
        ui.set_opt_fj(b[10]);
        ui.set_opt_deep(b[11]);
        ui.set_clear_armed(snap.clear_armed);
        ui.set_clear_label(snap.clear_label.into());
        ui.set_rec_fj(snap.rec_fj.into());
        ui.set_num_delay(g("delay"));
        ui.set_opt_autopopup(snap.auto_popup);
        ui.set_opt_autojump(b[12]);
        ui.set_opt_autosync(b[13]);
        ui.set_follow_visible(!snap.auto_popup);
        ui.set_follow_label(follow_label(&snap.follow_mode).into());
        ui.set_opt_inject(b[14]);
        ui.set_opt_evsearch(b[15]);
        ui.set_num_recentmax(g("recentmax"));
        ui.set_num_addmin(g("addmin"));
        ui.set_num_imgbytes(g("imgbytes"));
        ui.set_opt_qf(b[16]);
        ui.set_qf_open_label(qfopen_label(&snap.qfopen).into());
        ui.set_num_qfmax(g("qfmax"));
        ui.set_opt_pt(b[17]);
        ui.set_mask_caps(snap.mask & crate::settings::MOD_CAPS != 0);
        ui.set_mask_shift(snap.mask & crate::settings::MOD_SHIFT != 0);
        ui.set_mask_ctrl(snap.mask & MOD_CONTROL != 0);
        ui.set_mask_alt(snap.mask & MOD_ALT != 0);
        ui.set_mask_win(snap.mask & crate::settings::MOD_WIN != 0);
        ui.set_opt_keepkeys(b[18]);
        ui.set_phrase_rows(ModelRc::new(VecModel::from(
            snap.phrases.into_iter().map(SharedString::from).collect::<Vec<_>>(),
        )));
        ui.set_phrase_index(snap.phrase_index);
        ui.set_phrase_trigger(snap.phrase_trigger.into());
        ui.set_phrase_body(snap.phrase_body.into());
        ui.set_rule_rows(ModelRc::new(VecModel::from(
            snap.rules.into_iter().map(SharedString::from).collect::<Vec<_>>(),
        )));
        ui.set_rec_rule("录制组合键…".into());
        ui.set_excl_rows(ModelRc::new(VecModel::from(
            snap.excl.into_iter().map(SharedString::from).collect::<Vec<_>>(),
        )));
        ui.set_proc_list(ModelRc::new(VecModel::from(
            snap.procs.into_iter().map(SharedString::from).collect::<Vec<_>>(),
        )));
        ui.set_proc_index(snap.proc_index);
        ui.set_custom_rows(ModelRc::new(VecModel::from(
            snap.customs.into_iter().map(SharedString::from).collect::<Vec<_>>(),
        )));
        ui.set_custom_path(snap.custom_path.into());
        ui.set_custom_import_path(snap.import_path.into());
        ui.set_error_text(snap.error.into());
        ui.set_recording(snap.recording);
}

fn flip(b: &mut bool) {
    *b = !*b;
}

/// 开关类（pending）。
pub fn handle_bool(st: &mut WinState, name: &str) {
    let d = &mut st.draft;
    match name {
        "dblclick" => flip(&mut d.paste_double_click),
        "touch" => flip(&mut d.paste_touch_top),
        "startup" => flip(&mut d.run_at_startup),
        "admin" => flip(&mut d.run_as_admin),
        "updates" => flip(&mut d.check_updates),
        "winv" => flip(&mut d.replace_win_v),
        "merge" => flip(&mut d.batch_merge_text),
        "autooff" => flip(&mut d.batch_auto_off_when_empty),
        "clearexit" => flip(&mut d.clear_history_on_exit),
        "ocr" => flip(&mut d.image_ocr_enabled),
        "simulate" => flip(&mut d.paste_simulate),
        "fj" => flip(&mut d.filejump_enabled),
        "deep" => flip(&mut d.deep_search),
        "autopopup" => flip(&mut d.filejump_auto_popup),
        "autojump" => flip(&mut d.filejump_auto_jump),
        "autosync" => flip(&mut d.filejump_auto_sync),
        "inject" => flip(&mut d.filejump_shell_inject),
        "evsearch" => flip(&mut d.filejump_everything_search),
        "qf" => flip(&mut d.explorer_everything_quickfind_enabled),
        "pt" => flip(&mut d.passthrough_enabled),
        "keepkeys" => flip(&mut d.passthrough_keep_panel_keys),
        "mask-caps" => flip_mask(&mut d.passthrough_mask, crate::settings::MOD_CAPS),
        "mask-shift" => flip_mask(&mut d.passthrough_mask, crate::settings::MOD_SHIFT),
        "mask-ctrl" => flip_mask(&mut d.passthrough_mask, MOD_CONTROL),
        "mask-alt" => flip_mask(&mut d.passthrough_mask, MOD_ALT),
        "mask-win" => flip_mask(&mut d.passthrough_mask, crate::settings::MOD_WIN),
        _ => {}
    }
    st.clear_armed = false;
}

fn flip_mask(m: &mut u32, bit: u32) {
    *m ^= bit;
}

// ================= 供逻辑层调用的草稿访问器 =================

impl WinState {
    pub fn draft_settings(&self) -> &Settings {
        &self.draft
    }

    pub fn draft_passthrough_len(&self) -> usize {
        self.draft.passthrough_rules.len()
    }

    pub fn draft_rule_remove(&mut self, idx: usize) {
        if idx < self.draft.passthrough_rules.len() {
            self.draft.passthrough_rules.remove(idx);
        }
    }

    /// 排除添加：取当前 ComboBox 选中进程名（去重，大小写不敏感）。
    /// 返回 true=加入成功。
    pub fn excl_add_current(&mut self) -> bool {
        let Some(name) = self.proc_list.get(self.proc_index).cloned() else {
            return false;
        };
        if self
            .draft
            .exclusion_apps
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&name))
        {
            return false;
        }
        self.draft.exclusion_apps.push(name);
        self.draft.exclusion_apps.sort();
        true
    }

    pub fn excl_remove(&mut self, idx: usize) {
        if idx < self.draft.exclusion_apps.len() {
            self.draft.exclusion_apps.remove(idx);
        }
    }

    pub fn set_proc_index(&mut self, _idx: usize) {
        // ComboBox 当前项由 Rust push 下发；选中态 Slint 侧维护，添加时读取。
    }

    /// 自定义规则删除（即时写盘，WPF 语义）。返回是否成功。
    pub fn custom_remove(&mut self, idx: usize) -> bool {
        if idx >= self.custom_rules.len() {
            return false;
        }
        self.custom_rules.remove(idx);
        self.persist_custom().is_ok()
    }

    /// 自定义规则导入合并（即时写盘）。返回展示文案。
    pub fn custom_import(&mut self) -> String {
        let path = PathBuf::from(self.import_path.trim());
        let incoming = clipx_filejump::custom::CustomStore::load(&path);
        if incoming.rules.is_empty() {
            return format!("导入文件无规则：{}", path.to_string_lossy());
        }
        let mut store = clipx_filejump::custom::CustomStore { rules: self.custom_rules.clone() };
        let (added, replaced) = store.import_merge(incoming);
        self.custom_rules = store.rules;
        match self.persist_custom() {
            Ok(_) => format!("导入完成：新增 {added}，覆盖 {replaced}"),
            Err(e) => format!("写盘失败：{e}"),
        }
    }

    /// 自定义规则导出全部。返回展示文案。
    pub fn custom_export(&mut self) -> String {
        let path = PathBuf::from(self.import_path.trim());
        let store = clipx_filejump::custom::CustomStore { rules: self.custom_rules.clone() };
        match store.save(&path) {
            Ok(_) => format!("已导出 {} 条到 {}", store.rules.len(), path.to_string_lossy()),
            Err(e) => format!("导出失败：{e}"),
        }
    }

    fn persist_custom(&self) -> anyhow::Result<()> {
        clipx_filejump::custom::CustomStore { rules: self.custom_rules.clone() }
            .save(&self.custom_path)
    }

    pub fn set_error(&mut self, s: String) {
        self.error = s;
    }

    pub fn clear_armed(&self) -> bool {
        self.clear_armed
    }

    pub fn arm_clear(&mut self) {
        self.clear_armed = true;
    }

    pub fn disarm_clear(&mut self) {
        self.clear_armed = false;
    }

    pub fn set_proc_index_value(&mut self, idx: usize) {
        self.proc_index = idx.min(self.proc_list.len().saturating_sub(1));
    }
}

/// 循环类（pending；主题即时预览）。
pub fn handle_cycle(
    st: &mut WinState,
    name: &str,
    main_weak: &slint::Weak<crate::PopupWindow>,
) {
    let d = &mut st.draft;
    match name {
        "theme" => {
            d.theme = cycle(&d.theme, &["System", "Dark", "Light"]);
            apply_theme(&d.theme, main_weak);
        }
        "pos" => d.popup_position = cycle(&d.popup_position, &["Caret", "Mouse"]),
        "hide" => flip(&mut d.hide_on_click_outside),
        "paste" => d.paste_mode = cycle(&d.paste_mode, &["CtrlV", "ShiftInsert"]),
        "panel" => d.panel_key = cycle(&d.panel_key, &["Ctrl", "Alt", "Win", "CapsLock"]),
        "follow" => d.filejump_follow_mode = cycle(&d.filejump_follow_mode, &["Dialog", "Mouse"]),
        "qfopen" => {
            d.explorer_quickfind_open_mode =
                cycle(&d.explorer_quickfind_open_mode, &["Explorer", "DirectOpen"])
        }
        _ => {}
    }
    st.clear_armed = false;
}

/// 切到「实验性」或打开设置时后台枚举进程，不挡 UI。
pub fn request_procs_if_needed(
    st: &mut WinState,
    page: i32,
    tx: &std::sync::mpsc::Sender<crate::logic::AppEvt>,
) {
    if page != 2 || !st.proc_list.is_empty() || st.proc_loading {
        return;
    }
    st.proc_loading = true;
    let tx = tx.clone();
    let _ = std::thread::Builder::new()
        .name("clipx-procs".into())
        .spawn(move || {
            let names = crate::policy::top_process_names();
            let _ = tx.send(crate::logic::AppEvt::SettingProcs(names));
        });
}

pub fn apply_procs(st: &mut WinState, names: Vec<String>) {
    st.proc_list = names;
    st.proc_loading = false;
}

pub fn push_proc_list(weak: &slint::Weak<crate::SettingsWindow>, st: &WinState) {
    use slint::{ModelRc, SharedString, VecModel};
    let procs: Vec<SharedString> = st.proc_list.iter().cloned().map(SharedString::from).collect();
    let idx = st.proc_index as i32;
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_proc_list(ModelRc::new(VecModel::from(procs)));
        ui.set_proc_index(idx);
    });
}

pub fn patch_autopopup(weak: &slint::Weak<crate::SettingsWindow>, st: &WinState) {
    let vis = !st.draft.filejump_auto_popup;
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_follow_visible(vis);
        }
    });
}

pub fn patch_cycle(weak: &slint::Weak<crate::SettingsWindow>, st: &WinState, name: &str) {
    let theme = st.draft.theme.clone();
    let pos = pos_label(&st.draft.popup_position).to_string();
    let hide = hide_label(st.draft.hide_on_click_outside).to_string();
    let paste = paste_label(&st.draft.paste_mode).to_string();
    let panel = st.draft.panel_key.clone();
    let follow = follow_label(&st.draft.filejump_follow_mode).to_string();
    let qfopen = qfopen_label(&st.draft.explorer_quickfind_open_mode).to_string();
    let name = name.to_string();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        match name.as_str() {
            "theme" => {
                paint_theme(ui.global::<crate::Theme>(), &palette(&theme));
                ui.set_theme_label(theme_label(&theme).into());
            }
            "pos" => ui.set_pos_label(pos.into()),
            "hide" => ui.set_hide_label(hide.into()),
            "paste" => ui.set_paste_label(paste.into()),
            "panel" => ui.set_panel_label(panel.into()),
            "follow" => ui.set_follow_label(follow.into()),
            "qfopen" => ui.set_qf_open_label(qfopen.into()),
            _ => {}
        }
    });
}

pub fn patch_recording(weak: &slint::Weak<crate::SettingsWindow>, st: &WinState) {
    let recording = st.recording;
    let rec_main = st.draft.hotkey.display();
    let rec_batch = st.draft.batch_hotkey.display();
    let rec_fj = st.draft.filejump_hotkey.display();
    let rec_pgup = st.draft.page_up.display();
    let rec_pgdn = st.draft.page_down.display();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_recording(recording);
        ui.set_rec_main(rec_main.into());
        ui.set_rec_batch(rec_batch.into());
        ui.set_rec_fj(rec_fj.into());
        ui.set_rec_pgup(rec_pgup.into());
        ui.set_rec_pgdn(rec_pgdn.into());
    });
}

/// 录制入口（-1 关闭）。
pub fn handle_record(st: &mut WinState, slot: i32) {
    if st.recording == slot {
        st.recording = -1;
    } else {
        st.recording = slot;
    }
    st.rec_held = 0;
    crate::win_popup::append_debug_log(
        "hotkey_debug.log",
        &format!(
            "record state slot={} (target={slot}) popup_visible={}",
            st.recording,
            crate::keyboard_hook::is_visible(),
        ),
    );
    crate::keyboard_hook::set_recording(st.recording);
}

/// Slint 侧录制按键（覆盖层 FocusScope 直收，不依赖低级钩子）。
/// `text` 为组合字符（Ctrl 组合是 \x01..\x1a 控制字符），`mods` 已是 MOD_* 位。
/// 返回 true=已消耗；F 键等无文本键返回 false，留给钩子兜底。
pub fn handle_slint_key(st: &mut WinState, text: &str, mods: u32, repeat: bool) {
    if st.recording < 0 {
        return;
    }
    if repeat {
        return; // 按住修饰的自动重复不是新键（否则按住 Ctrl 即误采 Ctrl+Q）
    }
    crate::win_popup::append_debug_log(
        "hotkey_debug.log",
        &format!(
            "slint-key slot={} text={:?} mods=0x{mods:02X} held=0x{:02X}",
            st.recording, text, st.rec_held,
        ),
    );
    if text == "\u{1b}" {
        // Esc=取消（与钩子路径一致）。
        handle_record_vk(st, 0x1B, mods);
        return;
    }
    let Some(vk) = slint_text_to_vk(st, text) else {
        return;
    };
    handle_record_vk(st, vk, mods);
}

/// Slint 侧松键：只维护裸修饰按住快照（捕获/取消后会话已结束，无影响）。
pub fn handle_slint_rel(st: &mut WinState, text: &str) {
    if st.recording < 0 {
        return;
    }
    let mut ch = text.chars();
    let (Some(c), None) = (ch.next(), ch.next()) else {
        return;
    };
    let bit = match c {
        '\x10' => MOD_SHIFT,
        '\x11' => MOD_CONTROL,
        '\x12' => MOD_ALT,
        _ => return,
    };
    st.rec_held &= !bit;
}

/// Slint KeyEvent.text → 虚键码（US 布局假设，与钩子侧 char_from_vk 一致）。
/// 裸修饰自反文本（\x10=Shift \x11=Ctrl \x12=Alt）：对应位按下前已按住才是
/// 字母键（Ctrl+Q 的 Q），否则是裸修饰 —— 标记按住并返回 None（WPF：纯修饰忽略）。
fn slint_text_to_vk(st: &mut WinState, text: &str) -> Option<u32> {
    let mut ch = text.chars();
    let c = ch.next()?;
    if ch.next().is_some() {
        return None;
    }
    if let Some(bit) = match c {
        '\x10' => Some(MOD_SHIFT),
        '\x11' => Some(MOD_CONTROL),
        '\x12' => Some(MOD_ALT),
        _ => None,
    } {
        if st.rec_held & bit != 0 {
            // 已按住 → 这是字母键（Ctrl+P/Q，Alt 罕见同理）。
            return Some(match c {
                '\x10' => 0x50,
                '\x11' => 0x51,
                _ => 0x52,
            });
        }
        st.rec_held |= bit;
        return None;
    }
    Some(match c {
        'a'..='z' => 0x41 + (c as u32 - 'a' as u32),
        'A'..='Z' => 0x41 + (c as u32 - 'A' as u32),
        '0'..='9' => 0x30 + (c as u32 - '0' as u32),
        ' ' | '\x00' => 0x20, // 后者=Ctrl+Space
        '\t' => 0x09,
        '\n' => 0x0D,
        '\u{8}' => 0x08,
        ';' | ':' => 0xBA,
        '=' | '+' => 0xBB,
        ',' | '<' => 0xBC,
        '-' | '_' => 0xBD,
        '.' | '>' => 0xBE,
        '/' | '?' | '\x1f' => 0xBF, // \x1f=Ctrl+/
        '`' | '~' => 0xC0,
        '[' | '{' => 0xDB,
        '\\' | '|' | '\x1c' => 0xDC, // \x1c=Ctrl+\
        ']' | '}' | '\x1d' => 0xDD, // \x1d=Ctrl+]
        '\'' | '"' => 0xDE,
        '^' | '\x1e' => 0x36, // \x1e=Ctrl+^（物理键 6）
        '\x01'..='\x1a' => 0x41 + (c as u32 - 1), // Ctrl+字母
        _ => return None,
    })
}

/// RecordVk 事件（Esc=取消；纯修饰/无修饰键忽略，停留录制态）。
/// `mods` 为钩子侧按下瞬间的快照（快按快松时逻辑层现读会读到空）。
pub fn handle_record_vk(st: &mut WinState, vk: u32, mods: u32) {
    if st.recording < 0 {
        return;
    }
    if vk == 0x1B {
        crate::win_popup::append_debug_log(
            "hotkey_debug.log",
            &format!("record slot={} vk=Esc cancelled", st.recording),
        );
        st.recording = -1;
        st.rec_held = 0;
        crate::keyboard_hook::set_recording(-1);
        return;
    }
    // 纯修饰键忽略（WPF）。
    if matches!(vk, 0x10 | 0x11 | 0x12 | 0x14 | 0x5B | 0x5C | 0xA0..=0xA5) {
        return;
    }
    if mods == 0 {
        crate::win_popup::append_debug_log(
            "hotkey_debug.log",
            &format!("record slot={} vk=0x{vk:02X} ignored(no-mod)", st.recording),
        );
        return; // 须含修饰键（WPF）
    }
    let hk = Hotkey::new(mods, vk);
    match st.recording {
        SLOT_MAIN => st.draft.hotkey = hk,
        SLOT_BATCH => st.draft.batch_hotkey = hk,
        SLOT_FJ => st.draft.filejump_hotkey = hk,
        SLOT_PGUP => st.draft.page_up = hk,
        SLOT_PGDN => st.draft.page_down = hk,
        SLOT_RULE => {
            if !st.draft.passthrough_rules.iter().any(|r| {
                r.modifiers == hk.modifiers && r.key == hk.key
            }) {
                st.draft.passthrough_rules.push(PassthroughRule {
                    modifiers: hk.modifiers,
                    key: hk.key,
                });
            }
        }
        _ => {}
    }
    crate::win_popup::append_debug_log(
        "hotkey_debug.log",
        &format!("record slot={} captured {}", st.recording, hk.display()),
    );
    st.recording = -1;
    st.rec_held = 0;
    crate::keyboard_hook::set_recording(-1);
}

/// 文本类（pending；opacity 即时透过 slider 值进 draft）。
pub fn handle_text(st: &mut WinState, field: &str, text: String) {
    match field {
        "opacity" => {
            if let Ok(v) = text.parse::<f64>() {
                st.draft.popup_opacity = v.clamp(0.4, 1.0);
            }
        }
        "custom-path" => st.import_path = text,
        "phrase-add" => {
            st.draft.phrases.push(crate::settings::QuickPaste::default());
            st.phrase_index = st.draft.phrases.len().saturating_sub(1);
        }
        "phrase-del" => {
            let i = text
                .parse::<usize>()
                .unwrap_or(st.phrase_index);
            if i < st.draft.phrases.len() {
                st.draft.phrases.remove(i);
                st.phrase_index = st.phrase_index.min(st.draft.phrases.len().saturating_sub(1));
            }
        }
        "psel" => {
            if let Ok(i) = text.parse::<usize>() {
                st.phrase_index = i.min(st.draft.phrases.len().saturating_sub(1));
            }
        }
        "ptrig" => {
            if let Some(p) = st.draft.phrases.get_mut(st.phrase_index) {
                p.phrase = text;
            }
        }
        "pbody" => {
            if let Some(p) = st.draft.phrases.get_mut(st.phrase_index) {
                p.content = text;
            }
        }
        "proc" => {
            if let Some(i) = st.proc_list.iter().position(|p| p == &text) {
                st.proc_index = i;
            }
        }
        _ => {
            st.nums.insert(field.to_string(), text);
        }
    }
    st.clear_armed = false;
}

/// 保存：解析数字 → validate → 写盘。返回 true=成功关闭。
pub fn handle_save(st: &mut WinState) -> bool {
    let mut errs: Vec<String> = Vec::new();
    let get_i = |m: &HashMap<String, String>, k: &str| -> Option<i64> {
        m.get(k)?.trim().parse::<i64>().ok()
    };
    let get_f = |m: &HashMap<String, String>, k: &str| -> Option<f64> {
        m.get(k)?.trim().parse::<f64>().ok()
    };
    let d = &mut st.draft;
    macro_rules! num_i {
        ($key:expr, $label:expr, $slot:expr) => {
            match get_i(&st.nums, $key) {
                Some(v) => $slot = v,
                None => errs.push(format!("{}：请输入有效整数", $label)),
            }
        };
    }
    num_i!("max_items", "最大记录数", d.max_items);
    num_i!("max_images", "最大图片条数", d.max_image_items);
    num_i!("preview", "预览行数", d.preview_max_lines);
    num_i!("pageitems", "每次翻页条数", d.popup_page_items);
    match get_f(&st.nums, "width") {
        Some(v) => d.popup_width = v,
        None => errs.push("面板宽度：请输入有效数字".to_string()),
    }
    match get_f(&st.nums, "maxheight") {
        Some(v) => d.popup_max_height = v,
        None => errs.push("面板最大高度：请输入有效数字".to_string()),
    }
    match get_i(&st.nums, "delay") {
        Some(v) if v >= 0 => d.filejump_show_delay_ms = v as u64,
        _ => errs.push("跳转列表延时：请输入 0~10000 的整数".to_string()),
    }
    match get_i(&st.nums, "recentmax") {
        Some(v) => d.filejump_recent_max = v.max(0) as usize,
        None => errs.push("常用路径最大数量：请输入有效整数".to_string()),
    }
    match get_i(&st.nums, "addmin") {
        Some(v) => d.filejump_auto_add_min = v,
        None => errs.push("自动加入阈值：请输入有效整数".to_string()),
    }
    match get_i(&st.nums, "imgbytes") {
        Some(v) if v > 0 => d.max_image_bytes = v as u64,
        _ => errs.push("单图体积上限：请输入正整数（字节）".to_string()),
    }
    match get_i(&st.nums, "qfmax") {
        Some(v) => d.explorer_everything_quickfind_max_results = v.max(0) as u32,
        None => errs.push("筛选最大条数：请输入有效整数".to_string()),
    }
    if !errs.is_empty() {
        st.error = errs.join("\n");
        return false;
    }
    d.normalize();
    errs.extend(d.validate());
    if !errs.is_empty() {
        st.error = errs.join("\n");
        return false;
    }
    st.error.clear();
    st.clear_armed = false;
    true
}

fn truncate_p(s: &str) -> String {
    let t: String = s.chars().take(24).collect();
    if s.chars().count() > 24 {
        format!("{t}…")
    } else {
        t
    }
}

// ================= 主题 =================

/// 系统浅色判定（注册表 AppsUseLightTheme；取不到按深色）。禁止 spawn `reg.exe`。
#[cfg(windows)]
pub fn system_uses_light() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CACHED: AtomicU8 = AtomicU8::new(2);
    match CACHED.load(Ordering::Relaxed) {
        0 => return false,
        1 => return true,
        _ => {}
    }
    let light = read_apps_use_light();
    CACHED.store(if light { 1 } else { 0 }, Ordering::Relaxed);
    light
}

#[cfg(windows)]
fn read_apps_use_light() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
    };
    let mut dword: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut dword as *mut u32).cast()),
            Some(&mut size),
        )
    };
    err == ERROR_SUCCESS && dword == 1
}

#[cfg(not(windows))]
pub fn system_uses_light() -> bool {
    false
}

fn hex_color(s: &str) -> slint::Color {
    let h = s.trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0xFF1E1E1E);
    let (a, r, g, b) = if h.len() > 6 {
        (
            (v >> 24) as u8,
            (v >> 16) as u8,
            (v >> 8) as u8,
            v as u8,
        )
    } else {
        (0xFF, (v >> 16) as u8, (v >> 8) as u8, v as u8)
    };
    slint::Color::from_argb_u8(a, r, g, b)
}

static LAST_THEME: Mutex<String> = Mutex::new(String::new());

fn last_theme_name() -> String {
    LAST_THEME
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| "System".into())
}

/// 给独立 Window 的 Theme 副本上色（设置 / FileJump / QuickFind 各一份）。
pub fn paint_theme_handle(t: crate::Theme) {
    paint_theme(t, &palette(&last_theme_name()));
}

fn palette(name: &str) -> [&'static str; 19] {
    let light = match name {
        "Light" => true,
        "Dark" => false,
        _ => system_uses_light(),
    };
    if light {
        [
            "#EFF1F5", "#E6E9EF", "#E0F0EE", "#CEE8E6", "#4C4F69", "#8C8FA1", "#9CA0B0",
            "#139493", "#BCC0CC", "#EFF1F5", "#E6E9EF", "#E0F0EE", "#FFFFFF", "#FFFFFF",
            "#D20F39", "#E0E3EB", "#EFF1F5", "#CCD0DA", "#C5C9D4",
        ]
    } else {
        [
            "#1E1E1E", "#252526", "#1B3F3F", "#245756", "#CCCCCC", "#9D9D9D", "#858585",
            "#139493", "#3E3E42", "#1E1E1E", "#252526", "#1B3F3F", "#252526", "#FFFFFF",
            "#F48771", "#1E1E1E", "#1E1E1E", "#252526", "#0C0C0C",
        ]
    }
}

fn paint_theme(t: crate::Theme, p: &[&str; 19]) {
    t.set_window_bg(hex_color(p[0]));
    t.set_surface(hex_color(p[1]));
    t.set_hover(hex_color(p[2]));
    t.set_selected(hex_color(p[3]));
    t.set_primary_text(hex_color(p[4]));
    t.set_secondary_text(hex_color(p[5]));
    t.set_muted_text(hex_color(p[6]));
    t.set_accent(hex_color(p[7]));
    t.set_border(hex_color(p[8]));
    t.set_popup_bg(hex_color(p[9]));
    t.set_btn_bg(hex_color(p[10]));
    t.set_btn_hover(hex_color(p[11]));
    t.set_input_bg(hex_color(p[12]));
    t.set_on_accent(hex_color(p[13]));
    t.set_danger(hex_color(p[14]));
    t.set_track(hex_color(p[15]));
    t.set_header_bg(hex_color(p[16]));
    t.set_footer_bg(hex_color(p[17]));
    t.set_frame_bg(hex_color(p[18]));
}

/// 即时应用主题（设置窗口循环/保存/取消回滚共用）。
pub fn apply_theme(name: &str, main_weak: &slint::Weak<crate::PopupWindow>) {
    if let Ok(mut g) = LAST_THEME.lock() {
        *g = name.to_string();
    }
    let p = palette(name);
    let weak = main_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        paint_theme(ui.global::<crate::Theme>(), &p);
    });
}
