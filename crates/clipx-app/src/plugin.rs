//! 内部插件注册表（Step 2b/3）：事件收敛 + trait 落地。
//!
//! - 事件包络：`AppEvt::Qf*` / `AppEvt::Fj*` 收成一个 `AppEvt::PluginEvt`
//!  （plugin id + [`PluginPayload`]）。inline 后续照此加分支，不再加顶层变体。
//! - trait：[`AppPlugin`]（`id()`）是注册表的锚点；`QfPlugin` 是第一个实现。
//!   注册表结构体（`Vec<Box<dyn AppPlugin>>` + 统一 `dispatch`）等 FjState
//!   所有权随 Step 4 的 State 改工一起迁移时再立——状态机还在 State 手里，
//!   硬立注册表就是伪通用（见 runtime-design §4）。
//! - keyboard_hook 的 `KeyEvt::*` 生产侧不动：钩子继续发键事件，
//!   `main.rs` 转发器/热键表包成包络，逻辑层按 id 路由。

/// 插件事件载荷。QF/FJ 事件原样搬运（字段不动），只换包络。
#[derive(Debug, Clone)]
pub enum PluginPayload {
    QfKey(crate::keyboard_hook::KeyEvt),
    QfRowActivated(i32),
    QfFolderResolved {
        gen: u64,
        folder: Option<String>,
    },
    QfResults {
        gen: u64,
        view: crate::explorer_quickfind::QfView,
    },
    // ===== 文件夹跳转（Step 3a：worker 回投收敛；函数体暂留 logic.rs 同名 helper，
    // 3c 随 fj_show/fj_activate 一起搬进 FjPlugin）=====
    /// 后台采集完成（gen 过期则丢弃）。
    FjCollected {
        gen: u64,
        items: Vec<clipx_filejump::Candidate>,
    },
    /// 打字查询补充分阶段回投（gen 过期则丢弃）。
    FjEvResults {
        gen: u64,
        items: Vec<clipx_filejump::Candidate>,
    },
    /// 后台回读对话框当前目录（poll tick，不进按键路径）。
    FjFolder {
        dialog: isize,
        folder: Option<String>,
    },
    /// 后台导航完成（无条件复位 navigating，gen 过期也复位）。
    FjNavigated {
        gen: u64,
        path: String,
        ok: bool,
        is_file: bool,
    },
    // ===== 文件夹跳转 watcher/follow（Step 3b）=====
    /// 对话框前台自动弹出（watcher 线程，400ms 轮询 + ShowDelay 二次确认）。
    FjAuto(isize),
    /// WinEvent 实时跟随：对话框移动（跟随线程）。
    FjDialogMoved,
    /// WinEvent：对话框销毁（跟随线程）。
    FjDialogGone,
    // ===== 文件夹跳转触发与行动作（Step 3c）=====
    /// 全局 Ctrl+G / 托盘 / 热键表：切换 Picker（ FjToggle 经包络，见分发）。
    FjToggle,
    /// Picker 列表点击（行回调经包络）。
    FjRowActivated(i32),
    /// Picker header 手动拖拽（逻辑像素位移，经包络）。
    FjDragged(f32, f32),
    // ===== 行内补全（Step 1：键事件 + 行动，经包络；状态暂留主 State，
    // InlinePlugin 壳等交互冻结解除后立）=====
    /// 行内补全键（hook 观察到的键入/导航/确认/关闭，经 Key arm 转包络）。
    InlineKey(crate::keyboard_hook::KeyEvt),
    /// 行内小窗行点击补全。
    InlineActivate(i32),
    /// 行内小窗关闭。
    InlineDismissed,
}

/// 载荷类型名（诊断日志用，只记名不记内容——`InlineChar` 装着击键字符，
/// `QfResults` 装着路径，永远不许整包 `Debug` 落盘）。
/// 新增变体必须同步加分支（exhaustive match，编译器强制）。
pub fn payload_name(p: &PluginPayload) -> &'static str {
    match p {
        PluginPayload::QfKey(_) => "QfKey",
        PluginPayload::QfRowActivated(_) => "QfRowActivated",
        PluginPayload::QfFolderResolved { .. } => "QfFolderResolved",
        PluginPayload::QfResults { .. } => "QfResults",
        PluginPayload::FjCollected { .. } => "FjCollected",
        PluginPayload::FjEvResults { .. } => "FjEvResults",
        PluginPayload::FjFolder { .. } => "FjFolder",
        PluginPayload::FjNavigated { .. } => "FjNavigated",
        PluginPayload::FjAuto(_) => "FjAuto",
        PluginPayload::FjDialogMoved => "FjDialogMoved",
        PluginPayload::FjDialogGone => "FjDialogGone",
        PluginPayload::FjToggle => "FjToggle",
        PluginPayload::FjRowActivated(_) => "FjRowActivated",
        PluginPayload::FjDragged(..) => "FjDragged",
        PluginPayload::InlineKey(_) => "InlineKey",
        PluginPayload::InlineActivate(_) => "InlineActivate",
        PluginPayload::InlineDismissed => "InlineDismissed",
    }
}

/// 插件路由键。QF 用 `QfPlugin::ID`；FJ 壳（`FjPlugin`）/行内壳（`InlinePlugin`）
/// 落地前暂用常量，改名只改这一处。
pub const FILEJUMP_ID: &str = "filejump";
pub const INLINE_ID: &str = "inline";

/// 内部插件：`id()` 是路由键（分发按它匹配），也是审计日志的插件名；
/// `on_plugin_event()` 是包络分发唯一入口。`PluginCtx` 一次给齐四个分支
/// 所需（设置透传/回投通道/自家窗口弱引用/主窗可见性），插件不再直连 State。
pub trait AppPlugin {
    fn id(&self) -> &'static str;
    fn on_plugin_event(&mut self, payload: PluginPayload, ctx: &PluginCtx);
}

// ================= 拓展包（统一窗口「关联拓展包」区的数据底座）=====================
//
// 设计：pack 是下载项（Bitwarden CLI / OCR 精度引擎），认领键挂在导航项上
// （`SectionInfo.pack`），详情「关联拓展包」区渲染。插件/设置二元对立已取消，
// 导航只有一种行（见 section.rs）。
//
// 原 PluginInfo/builtin_plugins（插件行注册表）已随三组重构删除（见 git 历史）：
// qf/fj/inline/ocr 开关并入各插件行，采集并入剪贴板组，密码库行并入插件组。

/// 可下载拓展包一行：状态 + 动作（下载 / 看安装说明 / 无）。
/// `id` 即 PackDownload id（认领键，两边改名必须同步）。
pub struct PackInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub desc: &'static str,
    pub state: String,
    pub btn_label: Option<&'static str>,
    pub btn_cmd: Option<&'static str>,
    /// 右侧配置区指引（下载状态机说明，同样可挂未来配置行）。
    pub config_note: &'static str,
}

/// 拓展包清单（pack-state 查询层；展示收进导航项「关联拓展包」区）。
pub fn packs(settings_path: &std::path::Path) -> Vec<PackInfo> {
    vec![ocr_pack_info(settings_path), bw_pack_info(settings_path)]
}

fn ocr_pack_info(settings_path: &std::path::Path) -> PackInfo {
    #[cfg(feature = "ocr-rapid")]
    {
        let dir = crate::ocr_pack::models_dir(settings_path);
        // 就绪短路：齐了就别重算 32MB 的 hash（快照每 push 都调，更新下载 tick 更密）。
        let state = if clipx_ocr::rapid::models_ready(&dir) {
            "已下载".to_string()
        } else {
            let assets = clipx_ocr::rapid::required_assets();
            let total = assets.len();
            let done = assets
                .iter()
                .filter(|(name, _, sha)| {
                    dir.join(name).metadata().map(|m| m.len()).unwrap_or(0) > 0
                        && (sha.is_empty()
                            || crate::ocr_pack::file_sha256(&dir.join(name))
                                .is_some_and(|h| h.eq_ignore_ascii_case(sha)))
                })
                .count();
            format!("未下载（{done}/{total}）")
        };
        PackInfo {
            id: "ocr-models",
            name: "OCR 精度引擎",
            desc: "RapidOCR 模型（约 32MB），中文识别精度路径",
            state,
            btn_label: Some("下载 / 检查更新"),
            btn_cmd: Some("pack:ocr-models"),
            config_note: "下载完成后重启生效；进度走右下角提示条。",
        }
    }
    #[cfg(not(feature = "ocr-rapid"))]
    {
        let _ = settings_path;
        PackInfo {
            id: "ocr-models",
            name: "OCR 精度引擎",
            desc: "当前构建未含精度引擎（--no-default-features 应急瘦身构建）",
            state: "不可用".to_string(),
            btn_label: None,
            btn_cmd: None,
            config_note: "换标准构建即有；进度走右下角提示条。",
        }
    }
}

fn bw_pack_info(settings_path: &std::path::Path) -> PackInfo {
    if crate::bw_pack::is_ready(settings_path) {
        return PackInfo {
            id: "bw",
            name: "Bitwarden CLI",
            desc: "密码库填充的前置依赖（官方 OSS 构建，随 Data 存放）",
            state: format!("已安装（内建 {}）", crate::bw_pack::BW_VERSION),
            btn_label: None,
            btn_cmd: None,
            config_note: "内建包随 Data 走，搬家/重装自动跟随；换外部 bw 可手改 settings.json 的 vault_exe。",
        };
    }
    match clipx_vault::Vault::detect() {
        Some(p) => PackInfo {
            id: "bw",
            name: "Bitwarden CLI",
            desc: "密码库填充的前置依赖（官方维护）",
            state: format!("已安装（外部）：{}", p.display()),
            btn_label: None,
            btn_cmd: None,
            config_note: "用的是 PATH 里的外部 bw；改回 \"bw\" 即跟随内建包/PATH，固定路径可手改 settings.json 的 vault_exe。",
        },
        None => PackInfo {
            id: "bw",
            name: "Bitwarden CLI",
            desc: "密码库填充的前置依赖（官方 OSS 构建，约 40MB，随 Data 存放）",
            state: "未安装".to_string(),
            btn_label: Some("下载"),
            btn_cmd: Some("pack:bw"),
            config_note: "下载官方 OSS 包并 SHA256 校验；完成后自动启用，进度走右下角提示条。",
        },
    }
}

/// 分发上下文（`logic.rs` 的 `PluginEvt` arm 构造，字段借用可与插件可变借用并存）。
pub struct PluginCtx<'a> {
    pub settings: &'a crate::settings::Settings,
    pub evt_tx: &'a std::sync::mpsc::Sender<crate::logic::AppEvt>,
    pub qf: &'a slint::Weak<crate::QuickFindWindow>,
    pub main_visible: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_are_owned() {
        // 拓展包住在行名下：bw 归密码库行，ocr-models 归 OCR 行；
        // 认领键打错/漏认领即挂（展示侧按 id 回查，见 pack_view）。
        let settings = crate::settings::Settings::default();
        let dir = std::env::temp_dir();
        let packs = packs(&dir);
        assert_eq!(packs.len(), 2);
        let secs = crate::section::sections(
            &settings,
            &crate::settings_win::WinState::default(),
            &dir.join("settings.json"),
            false,
            "已锁定",
        );
        let owners_bw: Vec<_> = secs.iter().filter(|s| s.pack == Some("bw")).collect();
        assert_eq!(owners_bw.len(), 1, "bw 认领者数量");
        assert_eq!(owners_bw[0].id, "vault");
        let owners_ocr: Vec<_> = secs.iter().filter(|s| s.pack == Some("ocr-models")).collect();
        assert_eq!(owners_ocr.len(), 1, "ocr-models 认领者数量");
        assert_eq!(owners_ocr[0].id, "ocr");
        for id in ["ocr-models", "bw"] {
            // id 显式对应（不抠 btn_cmd 字符串；无按钮的已安装态也对得上）。
            assert!(
                packs.iter().any(|p| p.id == id),
                "packs() 缺 id={id}"
            );
        }
        // pack id 即 PackDownload id（两边改名必须同步，否则按钮点了没反应）。
        for s in &secs {
            if let Some(id) = s.pack {
                assert!(matches!(id, "ocr-models" | "bw"), "未知 pack 键：{id}");
            }
        }
    }

    #[test]
    fn packs_have_states_and_wellformed_actions() {
        let dir = std::env::temp_dir();
        let packs = packs(&dir);
        assert!(packs.len() >= 2);
        for p in &packs {
            assert!(!p.id.is_empty() && !p.state.is_empty());
            assert!(!p.config_note.is_empty(), "{} 缺 config_note", p.name);
            match p.btn_cmd {
                None => assert!(p.btn_label.is_none(), "无动作就不能有按钮文案"),
                Some(c) => {
                    assert!(
                        c.starts_with("pack:") || c.starts_with("url:"),
                        "动作格式非法：{c}"
                    );
                    assert!(
                        p.btn_label.is_some_and(|s| !s.is_empty()),
                        "有动作必须有按钮文案"
                    );
                }
            }
        }
    }
    #[test]
    fn payload_names_are_stable_for_log_parsers() {
        // 日志只认这些名字；改名同步改排障文档。
        assert_eq!(
            payload_name(&PluginPayload::QfKey(crate::keyboard_hook::KeyEvt::Esc)),
            "QfKey"
        );
        assert_eq!(payload_name(&PluginPayload::FjToggle), "FjToggle");
        assert_eq!(
            payload_name(&PluginPayload::FjDialogMoved),
            "FjDialogMoved"
        );
        assert_eq!(
            payload_name(&PluginPayload::InlineDismissed),
            "InlineDismissed"
        );
        assert_eq!(
            payload_name(&PluginPayload::FjFolder {
                dialog: 0,
                folder: None
            }),
            "FjFolder"
        );
    }
}
