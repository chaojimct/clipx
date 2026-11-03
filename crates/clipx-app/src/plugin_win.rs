//! 统一配置窗口（原插件管理窗口；设置组并入后改名，见 section.rs）。
//!
//! - 全即时语义：任何改动点下即落盘（旧设置页草稿+保存已废，删窗时清）。
//! - 模型照抄 `settings_win`（按需重建实例 + 事件循环线程强引用 + 无状态渲染），
//!   但**无草稿**：每次 push 按 live 设置重算快照。
//! - 事件：导航选中/下载/链接/配置行全是现成 AppEvt（插件开关已并入各组，
//!   专用 PluginToggle 通道已删，见 git 历史）。

use std::cell::RefCell;
use std::sync::mpsc;

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::logic::AppEvt;
use crate::plugin::PackInfo;
use crate::section::NavGroup;

/// 管理窗口快照（无状态，每次 push 重算）。
/// 选中序号不进快照：存在逻辑层 State（`plugin_selected`），随快照一起下发。
/// 导航只有一种行（`sections` 按左栏顺序排好；分组只是展示切分，序号扁平）。
pub fn nav_count(snap: &PluginSnapshot) -> usize {
    snap.sections.len()
}

/// 选中钳制（行数变化/旧值越界时兜底 0；空表也返回 0）。
pub fn clamp_selected(snap: &PluginSnapshot, selected: usize) -> usize {
    let n = nav_count(snap);
    if n == 0 || selected >= n {
        0
    } else {
        selected
    }
}

pub struct PackView {
    pub name: String,
    pub desc: String,
    pub state: String,
    pub has_btn: bool,
    pub btn_label: String,
    pub btn_is_url: bool,
    pub btn_target: String,
    pub note: String,
}

/// 名下拓展包回查（`PackInfo.id` 精确匹配；认领了不存在的键返回 None）。
pub fn pack_view(packs: &[PackInfo], key: &str) -> Option<PackView> {    let pk = packs.iter().find(|p| p.id == key)?;
    let (is_url, target) = match pk.btn_cmd {
        Some(c) if c.starts_with("url:") => (true, c[4..].to_string()),
        Some(c) if c.starts_with("pack:") => (false, c[5..].to_string()),
        _ => (false, String::new()),
    };
    Some(PackView {
        name: pk.name.to_string(),
        desc: pk.desc.to_string(),
        state: pk.state.clone(),
        has_btn: pk.btn_label.is_some(),
        btn_label: pk.btn_label.unwrap_or("").to_string(),
        btn_is_url: is_url,
        btn_target: target,
        note: pk.config_note.to_string(),
    })
}

pub struct PluginSnapshot {
    pub packs: Vec<PackInfo>,
    /// 全部导航项（左栏顺序：设置组 + 插件行 + 其他；序号即下标，分组只是展示切分）。
    /// 无状态：每次 push 按 live 设置重算。
    pub sections: Vec<crate::section::SectionInfo>,
    /// 录制槽（overlay 显隐；后端复用 WinState，见 section.rs）。
    pub recording: i32,
    /// 短语编辑器视图（live 短语 + 插件侧独立下标；旧窗双轨）。
    pub phrase: PhraseView,
}

/// 短语编辑器一帧（纯展示；写走 Phrase* 事件，live 直写）。
pub struct PhraseView {
    pub rows: Vec<String>,
    pub index: usize,
    pub trig: String,
    pub body: String,
}

/// 打开（销毁重建，躲软件渲染器 ReusedBuffer 的 hide→show 残缺坑——与设置窗同因）。
/// 实例 weak 经 `AppEvt::PluginWindowReady` 回传（与 SettingsWindowReady 同模式）。
pub fn open(
    snap: PluginSnapshot,
    selected: usize,
    vault_pw: String,
    evt_tx: mpsc::Sender<AppEvt>,
) {
    let _ = slint::invoke_from_event_loop(move || {
        CURRENT.with(|c| *c.borrow_mut() = None);
        close_current();
        let Ok(ui) = crate::PluginWindow::new() else {
            return;
        };
        bind(&ui, evt_tx.clone());
        apply(&ui, snap, selected, &vault_pw);
        crate::settings_win::paint_theme_handle(ui.global::<crate::Theme>());
        // 原生 X 走同一条关闭路径（清密码 + 销毁实例）：默认行为只隐藏窗口，
        // CURRENT 残留旧实例 + 密码不清，必须拦截（右上「关闭」按钮已删，此处是唯一出口）。
        {
            let tx = evt_tx.clone();
            ui.window().on_close_requested(move || {
                let _ = tx.send(AppEvt::PluginClose);
                slint::CloseRequestResponse::KeepWindowShown
            });
        }
        let size = slint::LogicalSize::new(760.0, 620.0);
        ui.window().set_size(slint::WindowSize::Logical(size));
        let _ = ui.window().show();
        #[cfg(windows)]
        {
            if let Some(hwnd) = crate::win_popup::find_window_by_title("clipx 设置") {
                crate::win_popup::activate_window(hwnd);
            }
        }
        let _ = evt_tx.send(AppEvt::PluginWindowReady(crate::logic::PluginWinReady(
            ui.as_weak(),
        )));
        CURRENT.with(|c| *c.borrow_mut() = Some(ui));
    });
}

/// 关闭（叉号语义：只隐藏实例，无草稿可丢）。
pub fn hide() {
    let _ = slint::invoke_from_event_loop(|| {
        close_current();
    });
}

/// 关闭并销毁当前插件窗口实例（与设置窗同因：只 hide 不 drop 会留幽灵窗口）。
fn close_current() {
    CURRENT.with(|c| {
        if let Some(ui) = c.borrow_mut().take() {
            let _ = ui.window().hide();
        }
    });
}

/// UI 自检用：把当前插件窗口渲染成 PNG（`CURRENT` 只在事件循环线程，
/// 故本函数也必须在事件循环线程调用）。返回 false = 当前没有打开的窗口。
pub fn snapshot_current_to(path: &str) -> bool {
    CURRENT.with(|c| {
        let borrow = c.borrow();
        let Some(ui) = borrow.as_ref() else {
            return false;
        };
        crate::save_snapshot(&ui.window(), path);
        true
    })
}

// 设置窗口强引用只存放在事件循环线程（Slint 组件的创建与 drop 都须在该线程）。
thread_local! {
    static CURRENT: RefCell<Option<crate::PluginWindow>> = const { RefCell::new(None) };
}

/// UI 回调 → 逻辑线程事件（窗口每次重建都要重新绑定）。
/// 导航/下载/链接/配置行全是现成 AppEvt；SettingBool 走草稿（旧窗用，新窗不用）。
fn bind(ui: &crate::PluginWindow, tx: mpsc::Sender<AppEvt>) {
    ui.on_plugin_select({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PluginSelect(v)); } });
    ui.on_pack_download({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PackDownload(v.into())); } });
    ui.on_open_url({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::OpenUrl(v.into())); } });
    ui.on_plugin_close({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::PluginClose); } });
    ui.on_plugin_vault_pw({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PluginVaultPw(v.into())); } });
    ui.on_plugin_vault_unlock({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::VaultUnlock); } });
    ui.on_plugin_vault_lock({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::VaultLock); } });
    ui.on_cfg_bool({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::CfgBool(v.into())); } });
    ui.on_cfg_text({ let tx = tx.clone(); move |a, b| { let _ = tx.send(AppEvt::CfgText(a.into(), b.into())); } });
    ui.on_cfg_choice({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::CfgChoice(v.into())); } });
    ui.on_cfg_action({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::CfgAction(v.into())); } });
    ui.on_cfg_record({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::CfgRecord(v)); } });
    ui.on_cfg_slider({ let tx = tx.clone(); move |a, b| { let _ = tx.send(AppEvt::CfgSlider(a.into(), b)); } });
    ui.on_plugin_key({ let tx = tx.clone(); move |t, m, r| { let _ = tx.send(AppEvt::PluginKey(t.to_string(), m as u32, r != 0)); } });
    ui.on_plugin_key_rel({ let tx = tx.clone(); move |t| { let _ = tx.send(AppEvt::PluginKeyRel(t.into())); } });
    ui.on_phrase_sel({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PhraseSel(v)); } });
    ui.on_phrase_add({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::PhraseAdd); } });
    ui.on_phrase_del({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::PhraseDel); } });
    ui.on_phrase_trig({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PhraseTrig(v.into())); } });
    ui.on_phrase_body({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PhraseBody(v.into())); } });
}

/// 注册行 → Slint 行（kind 展平成字符串，UI 侧按 kind 分支渲染）。
/// action 的目标（URL/数据目录）后端按 key 自取（deps/常量），行上不带路径字符串。
fn map_cfg_row(r: &crate::section::CfgRow) -> crate::CfgRow {
    use crate::section::CfgKind as K;
    let (kind, key, on, text, width, choice, action, danger, enabled, recording, slot, num) =
        match &r.kind {
            K::Toggle { key, on } => ("toggle", *key, *on, "", 0, "", "", false, true, false, -1, 0.0),
            K::Text { key, value, width } => ("text", *key, false, value.as_str(), *width, "", "", false, true, false, -1, 0.0),
            K::Choice { key, label } => ("choice", *key, false, "", 0, label.as_str(), "", false, true, false, -1, 0.0),
            K::Action { key, label, danger, enabled } => ("action", *key, false, "", 0, "", label.as_str(), *danger, *enabled, false, -1, 0.0),
            K::Info { text, warn } => ("info", "", false, text.as_str(), 0, "", "", *warn, true, false, -1, 0.0),
            K::Record { slot, label, recording } => ("record", "", false, "", 0, label.as_str(), "", false, true, *recording, *slot, 0.0),
            K::Slider { key, value } => ("slider", *key, false, "", 0, "", "", false, true, false, -1, *value),
            K::Progress { value } => ("progress", "", false, "", 0, "", "", false, true, false, -1, *value),
        };
    crate::CfgRow {
        label: r.label.into(),
        help: r.help.into(),
        kind: kind.into(),
        key: key.into(),
        on,
        text: text.into(),
        width,
        choice: choice.into(),
        action: action.into(),
        danger,
        enabled,
        recording,
        slot,
        num,
    }
}

/// 快照应用到 UI 实例（open 重建与后续 patch 共用；selected 为扁平序号）。
/// `vault_pw` 只做密码框回显（平时空；unlock/lock 后空即清框）——绝不进快照 struct。
fn apply(ui: &crate::PluginWindow, snap: PluginSnapshot, selected: usize, vault_pw: &str) {
    let sel = clamp_selected(&snap, selected);
    // 导航三组：同一 vec 按 group 切分，flat 即 vec 下标（单源，换序不散）。
    let mut nav_settings = Vec::new();
    let mut nav_plugins = Vec::new();
    let mut nav_other = Vec::new();
    for (i, s) in snap.sections.iter().enumerate() {
        let row = crate::NavRow {
            title: s.name.into(),
            dot: s.dot.into(),
            flat: i as i32,
        };
        match s.group {
            NavGroup::Settings => nav_settings.push(row),
            NavGroup::Plugins => nav_plugins.push(row),
            NavGroup::Other => nav_other.push(row),
        }
    }
    ui.set_nav_settings(ModelRc::new(VecModel::from(nav_settings)));
    ui.set_nav_plugins(ModelRc::new(VecModel::from(nav_plugins)));
    ui.set_nav_other(ModelRc::new(VecModel::from(nav_other)));
    ui.set_selected(sel as i32);
    ui.set_detail_recording(snap.recording);
    ui.set_detail_phrase_rows(ModelRc::new(VecModel::from(
        snap.phrase
            .rows
            .iter()
            .map(|r| slint::SharedString::from(r.as_str()))
            .collect::<Vec<slint::SharedString>>(),
    )));
    ui.set_detail_phrase_index(snap.phrase.index as i32);
    ui.set_detail_phrase_trig(snap.phrase.trig.as_str().into());
    ui.set_detail_phrase_body(snap.phrase.body.as_str().into());
    // 详情单路：标题/行永远下发（切行不残留，见组渲染注释）。
    if let Some(sec) = snap.sections.get(sel) {
        ui.set_detail_title(sec.name.into());
        ui.set_detail_desc(sec.note.into());
        ui.set_detail_show_phrases(sec.phrases);
        let rows: Vec<crate::CfgRow> = sec.rows.iter().map(map_cfg_row).collect();
        ui.set_detail_rows(ModelRc::new(VecModel::from(rows)));
        // 名下拓展包（目前：OCR 行认领 ocr-models，密码库行认领 bw）。
        let pv = sec.pack.and_then(|key| pack_view(&snap.packs, key));
        ui.set_detail_has_pack(pv.is_some());
        if let Some(v) = pv {
            ui.set_detail_pack_name(v.name.into());
            ui.set_detail_pack_desc(v.desc.into());
            ui.set_detail_pack_state(v.state.into());
            ui.set_detail_pack_has_btn(v.has_btn);
            ui.set_detail_pack_btn_label(v.btn_label.into());
            ui.set_detail_pack_btn_is_url(v.btn_is_url);
            ui.set_detail_pack_btn_target(v.btn_target.into());
            ui.set_detail_pack_note(v.note.into());
        }
        ui.set_detail_has_vault(sec.vault);
    }
    ui.set_detail_vault_pw(vault_pw.into());
}

/// 存量推送（开关拨动/下载状态变化后刷新内容，不重建实例）。
pub fn push(
    weak: &slint::Weak<crate::PluginWindow>,
    snap: PluginSnapshot,
    selected: usize,
    vault_pw: String,
) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        apply(&ui, snap, selected, &vault_pw);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PackInfo;
    use crate::section::{CfgKind, CfgRow, NavGroup, SectionInfo};

    fn section(
        id: &'static str,
        name: &'static str,
        group: NavGroup,
        dot: &'static str,
        pack: Option<&'static str>,
        vault: bool,
        rows: Vec<CfgRow>,
    ) -> SectionInfo {
        SectionInfo {
            id,
            name,
            note: "n",
            group,
            dot,
            pack,
            vault,
            rows,
            phrases: false,
        }
    }

    fn row_toggle(key: &'static str) -> CfgRow {
        CfgRow {
            label: "L",
            help: "H",
            kind: CfgKind::Toggle { key, on: true },
        }
    }

    fn snap() -> PluginSnapshot {
        PluginSnapshot {
            packs: vec![PackInfo {
                id: "bw",
                name: "包",
                desc: "d",
                state: "未安装".to_string(),
                btn_label: Some("下载"),
                btn_cmd: Some("pack:bw"),
                config_note: "n3",
            }],
            sections: vec![
                section("a", "甲", NavGroup::Settings, "", None, false, vec![]),
                section(
                    "b",
                    "乙",
                    NavGroup::Plugins,
                    "warn",
                    Some("bw"),
                    true,
                    vec![row_toggle("dblclick")],
                ),
            ],
            recording: -1,
            phrase: PhraseView {
                rows: vec![],
                index: 0,
                trig: String::new(),
                body: String::new(),
            },
        }
    }

    #[test]
    fn nav_count_is_flat_sections_len() {
        // 导航只有一种行；序号即 sections 下标，分组只是展示切分。
        assert_eq!(nav_count(&snap()), 2);
        let empty = PluginSnapshot {
            packs: vec![],
            sections: vec![],
            recording: -1,
            phrase: PhraseView {
                rows: vec![],
                index: 0,
                trig: String::new(),
                body: String::new(),
            },
        };
        assert_eq!(nav_count(&empty), 0);
        assert_eq!(clamp_selected(&empty, 5), 0);
    }

    #[test]
    fn pack_view_resolves_and_parses_actions() {
        let s = snap();
        // bw 归属：状态 + 下载动作（键即 PackDownload id）。
        let v = pack_view(&s.packs, "bw").expect("bw 应在");
        assert_eq!(v.state, "未安装");
        assert!(v.has_btn && !v.btn_is_url);
        assert_eq!(v.btn_target, "bw");
        assert_eq!(v.note, "n3");
        // 认领了不存在的键 → None（不显示坏按钮）。
        assert!(pack_view(&s.packs, "ghost").is_none());
        // url 动作解析。
        let mut s2 = snap();
        s2.packs[0].btn_cmd = Some("url:https://example.com");
        s2.packs[0].btn_label = Some("看看");
        let v = pack_view(&s2.packs, "bw").expect("bw 应在");
        assert!(v.btn_is_url);
        assert_eq!(v.btn_target, "https://example.com");
    }

    #[test]
    fn map_cfg_row_flattens_kinds() {
        let r = CfgRow {
            label: "L",
            help: "H",
            kind: CfgKind::Slider { key: "opacity", value: 0.8 },
        };
        let m = map_cfg_row(&r);
        assert_eq!(m.kind.as_str(), "slider");
        assert_eq!(m.key.as_str(), "opacity");
        assert!((m.num - 0.8).abs() < 1e-6);
        let r = CfgRow {
            label: "L",
            help: "",
            kind: CfgKind::Record { slot: 0, label: "Ctrl+`".to_string(), recording: true },
        };
        let m = map_cfg_row(&r);
        assert_eq!(m.kind.as_str(), "record");
        assert_eq!(m.slot, 0);
        assert!(m.recording);
        assert_eq!(m.choice.as_str(), "Ctrl+`");
    }
}
