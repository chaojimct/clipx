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

// ================= 插件行（统一窗口左侧导航）=====================
//
// 设计：只剩设置组里装不下的独立能力行（目前仅密码库：vault 解锁不属于
// 任何设置组，单列一行）。qf/fj/inline/ocr 四开关已并入各组，行已删除
// （删行史见 git）；采集行因信息稀疏并入剪贴板组（capture 开关）。
// 新增插件只加行，不改分发。

/// 管理页一行插件：开关（有）/状态（有）/详情（去设置页）。
/// 键即 `toggle` 名（`SettingBool` 名）——注册表是现有开关的视图，不另立 id。
/// `config_note` 是右侧「配置」区的指引行：开关拨动之外的东西放这儿写，
/// 后续真配置行（如下拉/数值）也挂同一区，数据走同 struct 加字段。
pub struct PluginInfo {
    pub name: &'static str,
    pub desc: &'static str,
    pub enabled: bool,
    pub status: String,
    /// 对应 `SettingBool` 名（`handle_bool`）；None = 无开关（纯状态行）。
    pub toggle: Option<&'static str>,
    /// 右侧配置区指引（必须指到真实存在的设置位置，页名与 settings.slint 的 UiTab 一致）。
    pub config_note: &'static str,
    /// 名下关联拓展包（PackDownload id，如 "ocr-models"/"bw"；None = 无）。
    /// 下载按钮收进该插件右侧详情，不再单列——拓展包就该住在各自插件设置里。
    pub pack: Option<&'static str>,
    /// 左侧导航圆点语义（"ok" 正常 / "off" 已关停 / "warn" 待处理），注册侧定，
    /// 展示侧只映射颜色——圆点必须反映状态，不能只看开关。
    pub dot: &'static str,
}

/// 可下载拓展包一行：状态 + 动作（下载 / 看安装说明 / 无）。
/// `id` 即 PackDownload id（`PluginInfo.pack` 认领键，两边改名必须同步）。
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

/// 密码库圆点：缺 bw/未知 → 待处理；已锁定 → 中性闲置；已解锁 → 正常。
/// 按展示文案匹配（`vault_status_text` 的输出，空即未知）。
fn vault_dot(status: &str) -> &'static str {
    if status.is_empty() || status.starts_with("未检测") {
        "warn"
    } else if status == "已解锁" {
        "ok"
    } else if status == "已锁定" {
        "off"
    } else {
        "warn"
    }
}

/// 内置插件清单（顺序即管理页展示顺序）。
pub fn builtin_plugins(
    _settings: &crate::settings::Settings,
    vault_status: &str,
) -> Vec<PluginInfo> {
    vec![
        PluginInfo {
            name: "密码库",
            desc: "Bitwarden / Vaultwarden 只读填充；解锁在下方「解锁」区",
            enabled: true,
            status: if vault_status.is_empty() {
                "未知".to_string()
            } else {
                vault_status.to_string()
            },
            toggle: None,
            config_note: "解锁在本页「解锁」区（或设置「高级」页「密码库」区）；主密码只驻内存，永不落盘。",
            pack: Some("bw"),
            dot: vault_dot(vault_status),
        },
    ]
}

/// 拓展包清单（pack-state 查询层；展示收进各插件右侧详情，不再单列）。
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

    /// 注册表形状测试：目前只剩密码库 1 行（qf/fj/inline/ocr 开关已并入各组，
    /// 行已删除；采集行因信息稀疏并入剪贴板组）。行数变化同步改 SECTION_OFFSET。
    #[test]
    fn registry_shape() {
        let settings = crate::settings::Settings::default();
        let plugins = builtin_plugins(&settings, "已锁定");
        assert_eq!(plugins.len(), 1, "插件行只剩密码库");
        let p = &plugins[0];
        assert_eq!(p.name, "密码库");
        assert!(!p.desc.is_empty());
        // 右侧配置区指引不能为空。
        assert!(!p.config_note.is_empty(), "{} 缺 config_note", p.name);
        // 圆点语义收敛三值（展示侧只认这三个，多了就画不出来）。
        assert!(
            matches!(p.dot, "ok" | "off" | "warn"),
            "{} dot 非法：{}",
            p.name,
            p.dot
        );
        // 密码库行无开关（解锁走解锁区），认领 bw 包。
        assert!(p.toggle.is_none());
        assert_eq!(p.pack, Some("bw"));
        assert!(!p.status.is_empty());
    }

    #[test]
    fn vault_dot_maps_display_states() {
        assert_eq!(vault_dot(""), "warn");
        assert_eq!(vault_dot("未检测到 bw（请先安装 Bitwarden CLI）"), "warn");
        assert_eq!(vault_dot("已锁定"), "off");
        assert_eq!(vault_dot("已解锁"), "ok");
        assert_eq!(vault_dot("未来新状态"), "warn");
    }

    #[test]
    fn packs_are_owned() {
        // 拓展包住在行名下：bw 归密码库行，ocr-models 归记录与检索组；
        // 认领键打错/漏认领即挂（展示侧按 id 回查，见 pack_view）。
        let settings = crate::settings::Settings::default();
        let plugins = builtin_plugins(&settings, "已锁定");
        let dir = std::env::temp_dir();
        let packs = packs(&dir);
        assert_eq!(packs.len(), 2);
        let owners_bw: Vec<_> = plugins.iter().filter(|p| p.pack == Some("bw")).collect();
        assert_eq!(owners_bw.len(), 1, "bw 认领者数量");
        let secs = crate::section::sections(
            &settings,
            &crate::settings_win::WinState::default(),
            &dir.join("settings.json"),
            false,
        );
        let owners_ocr: Vec<_> = secs.iter().filter(|s| s.pack == Some("ocr-models")).collect();
        assert_eq!(owners_ocr.len(), 1, "ocr-models 认领者数量");
        for id in ["ocr-models", "bw"] {
            // id 显式对应（不抠 btn_cmd 字符串；无按钮的已安装态也对得上）。
            assert!(
                packs.iter().any(|p| p.id == id),
                "packs() 缺 id={id}"
            );
        }
        // pack id 即 PackDownload id（两边改名必须同步，否则按钮点了没反应）。
        for p in &plugins {
            if let Some(id) = p.pack {
                assert!(matches!(id, "ocr-models" | "bw"), "未知 pack 键：{id}");
            }
        }
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
