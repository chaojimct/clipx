//! 插件管理独立窗口（设置页之外的单独窗口）。
//!
//! - 为什么独立：设置页是草稿+保存语义，插件开关要即时生效语义，混一起打架。
//! - 模型照抄 `settings_win`（按需重建实例 + 事件循环线程强引用 + 无状态渲染），
//!   但**无草稿**：每次 push 按 live 设置重算快照（`plugin::builtin_plugins`）。
//! - 开关走 `AppEvt::PluginToggle`（即时落盘），下载走 `AppEvt::PackDownload`，
//!   链接走 `AppEvt::OpenUrl`——全是现成事件，无新逻辑路径。

use std::cell::RefCell;
use std::sync::mpsc;

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::logic::AppEvt;
use crate::plugin::{PackInfo, PluginInfo};

/// 管理窗口快照（无状态，每次 push 重算）。
/// 选中序号不进快照：存在逻辑层 State（`plugin_selected`），随快照一起下发。
/// 右侧详情/配置区的一帧（纯数据，Slint 只读；`select_detail` 纯函数可单测）。
/// `pack_*` 是该插件名下的关联拓展包（`PluginInfo.pack` 认领，`snap.packs`
/// 按 `pack:<id>` 回查）——下载按钮住在各自插件详情里，不再单列。
pub struct PluginDetail {
    pub title: String,
    pub desc: String,
    pub state: String,
    pub is_plugin: bool,
    pub has_toggle: bool,
    pub toggle_on: bool,
    pub toggle_target: String,
    pub has_btn: bool,
    pub btn_label: String,
    pub btn_is_url: bool,
    pub btn_target: String,
    pub note: String,
    /// 密码库行专属解锁区（`pack == "bw"` 即是；迁移领航：设置页解锁区的另一半入口）。
    pub has_vault: bool,
    pub has_pack: bool,
    pub pack_name: String,
    pub pack_desc: String,
    pub pack_state: String,
    pub pack_has_btn: bool,
    pub pack_btn_label: String,
    pub pack_btn_is_url: bool,
    pub pack_btn_target: String,
    pub pack_note: String,
}

/// 导航总行数（插件 + 设置组；选中序号横跨两者）。
pub fn nav_count(snap: &PluginSnapshot) -> usize {
    snap.plugins.len() + snap.sections.len()
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

/// 统一序号 → 右侧详情（序号即插件下标；名下拓展包按 `pack:<id>` 回查）。
/// 插件侧永无直属按钮（`has_btn` 恒 false，字段留给 Slint 结构稳定）；动作全在
/// 开关与名下拓展包两处——后续真配置行（如下拉/数值）同样挂详情，不加新通道。
pub fn select_detail(snap: &PluginSnapshot, selected: usize) -> PluginDetail {
    let sel = clamp_selected(snap, selected);
    let Some(p) = snap.plugins.get(sel) else {
        // 空表兜底（注册表恒非空，防未来重构悬空）。
        return PluginDetail {
            title: String::new(),
            desc: String::new(),
            state: String::new(),
            is_plugin: true,
            has_toggle: false,
            toggle_on: false,
            toggle_target: String::new(),
            has_btn: false,
            btn_label: String::new(),
            btn_is_url: false,
            btn_target: String::new(),
            note: String::new(),
            has_vault: false,
            has_pack: false,
            pack_name: String::new(),
            pack_desc: String::new(),
            pack_state: String::new(),
            pack_has_btn: false,
            pack_btn_label: String::new(),
            pack_btn_is_url: false,
            pack_btn_target: String::new(),
            pack_note: String::new(),
        };
    };
    let (has_pack, pack_name, pack_desc, pack_state, pack_has_btn, pack_btn_label, pack_btn_is_url, pack_btn_target, pack_note) =
        match p.pack.and_then(|key| find_pack(&snap.packs, key)) {
            Some(pk) => {
                let (is_url, target) = match pk.btn_cmd {
                    Some(c) if c.starts_with("url:") => (true, c[4..].to_string()),
                    Some(c) if c.starts_with("pack:") => (false, c[5..].to_string()),
                    _ => (false, String::new()),
                };
                (
                    true,
                    pk.name.to_string(),
                    pk.desc.to_string(),
                    pk.state.clone(),
                    pk.btn_label.is_some(),
                    pk.btn_label.unwrap_or("").to_string(),
                    is_url,
                    target,
                    pk.config_note.to_string(),
                )
            }
            None => (
                false,
                String::new(),
                String::new(),
                String::new(),
                false,
                String::new(),
                false,
                String::new(),
                String::new(),
            ),
        };
    PluginDetail {
        title: p.name.to_string(),
        desc: p.desc.to_string(),
        state: p.status.clone(),
        is_plugin: true,
        has_toggle: p.toggle.is_some(),
        toggle_on: p.enabled,
        toggle_target: p.toggle.unwrap_or("").to_string(),
        has_btn: false,
        btn_label: String::new(),
        btn_is_url: false,
        btn_target: String::new(),
        note: p.config_note.to_string(),
        has_vault: p.pack == Some("bw"),
        has_pack,
        pack_name,
        pack_desc,
        pack_state,
        pack_has_btn,
        pack_btn_label,
        pack_btn_is_url,
        pack_btn_target,
        pack_note,
    }
}

/// 名下拓展包回查（`PackInfo.id` 精确匹配；认领了不存在的键就当没认领）。
fn find_pack<'a>(packs: &'a [PackInfo], key: &str) -> Option<&'a PackInfo> {
    packs.iter().find(|p| p.id == key)
}

pub struct PluginSnapshot {
    pub plugins: Vec<PluginInfo>,
    pub packs: Vec<PackInfo>,
    /// 统一配置组（设置页迁移过来的行），导航跟在插件之后。
    /// 无状态：每次 push 按 live 设置重算（与 plugins 同 discipline）。
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
/// 开关/下载/链接全复用现成 AppEvt：SettingBool 走草稿（此处不用）、
/// PluginToggle 即时生效、PackDownload 下载、OpenUrl 浏览器。
fn bind(ui: &crate::PluginWindow, tx: mpsc::Sender<AppEvt>) {
    ui.on_plugin_select({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PluginSelect(v)); } });
    ui.on_plugin_toggle({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PluginToggle(v.into())); } });
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

/// 快照应用到 UI 实例（open 重建与后续 patch 共用；selected 为统一序号）。
/// `vault_pw` 只做密码框回显（平时空；unlock/lock 后空即清框）——绝不进快照 struct。
fn apply(ui: &crate::PluginWindow, snap: PluginSnapshot, selected: usize, vault_pw: &str) {
    let sel = clamp_selected(&snap, selected);
    let plugins: Vec<crate::PluginCard> = snap
        .plugins
        .iter()
        .map(|p| crate::PluginCard {
            name: p.name.into(),
            desc: p.desc.into(),
            status: p.status.clone().into(),
            enabled: p.enabled,
            has_toggle: p.toggle.is_some(),
            toggle: p.toggle.unwrap_or("").into(),
            dot: p.dot.into(),
        })
        .collect();
    ui.set_plugin_rows(ModelRc::new(VecModel::from(plugins)));
    // 插件恒 6 行（SECTION_OFFSET 锁死；变了就是注册表事故，单测也会挂）。
    debug_assert_eq!(snap.plugins.len(), crate::section::SECTION_OFFSET);
    let sections: Vec<crate::SectionNavRow> = snap
        .sections
        .iter()
        .map(|s| crate::SectionNavRow {
            title: s.name.into(),
            sub: s.sub.into(),
        })
        .collect();
    ui.set_section_rows(ModelRc::new(VecModel::from(sections)));
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
    // 组详情 vs 插件详情：序号落在组里就走组渲染（标题/行），插件块全灭。
    // 行永远下发（插件选中时为空，for 无行可画；不能只在组分支设，切换回来会残留旧行）。
    {
        let rows: Vec<crate::CfgRow> = if sel >= snap.plugins.len() {
            snap.sections
                .get(sel - snap.plugins.len())
                .map(|sec| sec.rows.iter().map(map_cfg_row).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        ui.set_detail_rows(ModelRc::new(VecModel::from(rows)));
    }
    if sel >= snap.plugins.len() {
        if let Some(sec) = snap.sections.get(sel - snap.plugins.len()) {
            ui.set_detail_is_section(true);
            ui.set_detail_title(sec.name.into());
            ui.set_detail_desc(sec.note.into());
            ui.set_detail_show_phrases(sec.phrases);
        }
        return;
    }
    ui.set_detail_is_section(false);
    let d = select_detail(&snap, sel);
    ui.set_detail_title(d.title.into());
    ui.set_detail_desc(d.desc.into());
    ui.set_detail_state(d.state.into());
    ui.set_detail_is_plugin(d.is_plugin);
    ui.set_detail_has_toggle(d.has_toggle);
    ui.set_detail_toggle_on(d.toggle_on);
    ui.set_detail_toggle_target(d.toggle_target.into());
    ui.set_detail_has_btn(d.has_btn);
    ui.set_detail_btn_label(d.btn_label.into());
    ui.set_detail_btn_is_url(d.btn_is_url);
    ui.set_detail_btn_target(d.btn_target.into());
    ui.set_detail_note(d.note.into());
    ui.set_detail_has_pack(d.has_pack);
    ui.set_detail_pack_name(d.pack_name.into());
    ui.set_detail_pack_desc(d.pack_desc.into());
    ui.set_detail_pack_state(d.pack_state.into());
    ui.set_detail_pack_has_btn(d.pack_has_btn);
    ui.set_detail_pack_btn_label(d.pack_btn_label.into());
    ui.set_detail_pack_btn_is_url(d.pack_btn_is_url);
    ui.set_detail_pack_btn_target(d.pack_btn_target.into());
    ui.set_detail_pack_note(d.pack_note.into());
    ui.set_detail_has_vault(d.has_vault);
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
    use crate::plugin::{PackInfo, PluginInfo};

    fn snap() -> PluginSnapshot {
        PluginSnapshot {
            plugins: vec![
                PluginInfo {
                    name: "甲",
                    desc: "d",
                    enabled: true,
                    status: "开".to_string(),
                    toggle: Some("qf"),
                    config_note: "n1",
                    pack: None,
                    dot: "ok",
                },
                PluginInfo {
                    name: "乙",
                    desc: "d",
                    enabled: false,
                    status: "纯状态".to_string(),
                    toggle: None,
                    config_note: "n2",
                    pack: Some("bw"),
                    dot: "warn",
                },
            ],
            packs: vec![PackInfo {
                id: "bw",
                name: "包",
                desc: "d",
                state: "未安装".to_string(),
                btn_label: Some("下载"),
                btn_cmd: Some("pack:bw"),
                config_note: "n3",
            }],
            sections: vec![],
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
    fn select_detail_attaches_owned_pack() {
        let s = snap();
        assert_eq!(nav_count(&s), 2);
        // 无归属：pack 区整体隐藏，vault 区也隐藏。
        let d0 = select_detail(&s, 0);
        assert!(d0.is_plugin && d0.has_toggle && d0.toggle_on);
        assert_eq!(d0.toggle_target, "qf");
        assert_eq!(d0.note, "n1");
        assert!(!d0.has_pack);
        assert!(!d0.has_vault);
        // 有归属：pack 区带状态 + 下载动作（键即 PackDownload id）；
        // bw 归属即密码库行，解锁区同显。
        let d1 = select_detail(&s, 1);
        assert!(!d1.has_toggle);
        assert!(d1.has_pack);
        assert!(d1.has_vault);
        assert_eq!(d1.pack_state, "未安装");
        assert!(d1.pack_has_btn && !d1.pack_btn_is_url);
        assert_eq!(d1.pack_btn_target, "bw");
        assert_eq!(d1.pack_note, "n3");
    }

    #[test]
    fn select_detail_clamps_out_of_range() {
        let s = snap();
        // 越界兜底 0（行数变化时旧选中不 panic、不悬空）。
        let d = select_detail(&s, 99);
        assert_eq!(d.title, "甲");
        let empty = PluginSnapshot {
            plugins: vec![],
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
        assert_eq!(clamp_selected(&empty, 5), 0);
        assert_eq!(nav_count(&empty), 0);
        // 空表不 panic（注册表恒非空，此分支只防未来重构悬空）。
        let d = select_detail(&empty, 0);
        assert!(!d.has_pack && !d.has_toggle && !d.has_vault);
    }

    #[test]
    fn select_detail_parses_url_action() {
        let mut s = snap();
        s.packs[0].btn_cmd = Some("url:https://example.com");
        s.packs[0].btn_label = Some("看看");
        let d = select_detail(&s, 1);
        assert!(d.pack_btn_is_url);
        assert_eq!(d.pack_btn_target, "https://example.com");
    }

    #[test]
    fn orphan_pack_key_hides_section() {
        // 认领了不存在的键 → 当没认领（不断言崩溃、不显示坏按钮）。
        let mut s = snap();
        s.plugins[1].pack = Some("ghost");
        let d = select_detail(&s, 1);
        assert!(!d.has_pack);
    }

    #[test]
    fn nav_covers_plugins_and_sections() {
        use crate::section::{CfgKind, CfgRow, SectionInfo};
        let mut s = snap();
        s.sections = vec![SectionInfo {
            id: "x",
            name: "X",
            note: "n",
            sub: "s",
            rows: vec![CfgRow {
                label: "L",
                help: "H",
                kind: CfgKind::Toggle { key: "dblclick", on: true },
            }],
            phrases: false,
        }];
        // 统一序号横跨插件 + 组。
        assert_eq!(nav_count(&s), 3);
        // 组序号进 select_detail 走空兜底（组渲染走另一条路，不复用插件详情）。
        let d = select_detail(&s, 2);
        assert!(d.title.is_empty() && !d.has_pack);
    }

    #[test]
    fn map_cfg_row_flattens_kinds() {
        use crate::section::{CfgKind, CfgRow};
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
