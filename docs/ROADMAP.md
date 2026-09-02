# clipx 里程碑路线图

> 状态：v1.0 · 2026-09-02 · 当前阶段：规划完成，M0 未启动

## 开发纪律

1. **单仓库推进**：所有工作在 clipboardx-rs 内完成，不再另起炉灶（历史教训：三次尝试三次重开，无一善终）。
2. **行为对齐 WPF 版**：交互行为以 WPF ClipboardX 为规格书，有意偏离需登记 PRD §8。
3. **里程碑必须可日用**：每个 M 结束时产品达到"愿意天天用"的状态，不达标不进下一个 M。
4. **core/UI 分离**：UI 框架相关代码不得进入 core/store/monitor/ocr（这是 ADR-001 中 egui 备案可切换的前提）。

## M0 骨架验证（预计 1-2 周）

目标：证明技术路线成立，跑通最小闭环。

范围：

- 初始化 workspace：删除空占位 build.rs，根 Cargo.toml 改写为 workspace，五个 crate 建立骨架并锁定依赖版本
- Windows 事件监听 → channel → core 去重 → SQLite 入库（含 FTS5 建表）
- Slint 最小弹窗：热键呼出、列表显示、Esc 关闭
- 完成 S1 / S2 / S3 三个 spike（见 ARCHITECTURE §8）

验收标准：

- 复制文本 1s 内出现在列表；**重启应用数据还在**（两个 tauri 原型从未跨过的门槛）
- 热键弹窗不抢原应用焦点（S1 通过）
- 2000 条文本滚动 ≥55fps，常驻内存 ≤30MB
- 连续复制 100 次无漏采、无重复风暴（S3 通过）

不在范围：搜索、图片、粘贴回写、托盘、设置界面。

## M1 Windows 日用（预计 2-3 周）

目标：替代手动 Ctrl+V 日常使用。

范围：键盘导航、拼音与文本搜索、粘贴回写 + ClipboardGate、系统托盘、设置持久化、单实例、失焦关闭。

日用形态：clipx（剪贴板）+ WPF ClipboardX-filejump.exe 双进程并行——FileJumpOnly flavor 同时包含 FileJump 与 Everything 快速查找（同属 CLIPX_FILEJUMP 门控，已从源码核实），过渡期功能零缺失。

验收：开发者本人连续一周将其作为主力剪贴板工具，期间发现的阻塞问题归零。

## M2 图片与 OCR（预计 2 周）

范围：图片采集、缩略图、原图懒加载、uniOCR 异步队列、OCR 文本入 FTS、S4 spike。

验收：

- 截图复制后缩略图即时可见
- OCR 完成后图中文字可搜
- 连续采集 100 张图片后，常驻内存回落至 ≤35MB
- 4K 大图预览不卡顿

## M3 打磨与迁移（预计 2 周）

范围：文件列表与富文本格式、收藏、右键菜单、多屏定位、开机自启、WPF 数据迁移命令、万条压测。

验收：

- WPF 版全部历史迁移无丢失（含图片与 ocr_text）
- 万条历史下搜索 <100ms、滚动流畅
- 常驻内存稳定在 10-30MB 区间
- 核心路径体验对齐 WPF 版

排位检查点（确认型，非重开排序）：Windows 高级功能的排位已在规划期（2026-09-02）依据三项输入预先确定——FileJump 与 Everything 均为日均 10+ 次的高频使用、Mac 非日常开机（macOS 验证周期天然偏长）、跨平台 1.0 无发布硬节点——结论为 M4/M5（Everything、FileJump）先于 macOS 执行。本检查点只做三件事：复核 M0-M3 双进程共存的摩擦记录；从 Data/ 下 ShellNavigate 与 explorer_quickfind 两个日志提取实际使用分布，据此排定 M5 各文件管理器采集器的移植优先级；确认无新的相反证据。若共存摩擦在 M1/M2 期间提前激化，M4（Everything）依赖少、周期短，可提前插入。

## M4 Everything 集成（预计 1 周）

Windows 高级功能阶段一（PRD §7）。目标：Explorer 内快速查找回归，WPF 侧该功能下线。

范围：

- Everything 窗口消息 IPC（WM_COPYDATA）直连，替代 Everything64.dll SDK：查询协议封装，安装包不再分发原生 DLL
- ExplorerQuickFind 体验移植：活动资源管理器检测、快速查找弹窗（Slint）、结果导航与选中
- 移植基线（WPF EverythingIpc 源码注记）：搜索串用 parent: / path: 限定；勿依赖 SetMatchPath；「盘符:\ 关键词」形式在 IPC 实测恒 0 条

验收：Explorer 内热键呼出、输入即搜、回车跳转并选中，行为与性能对齐 WPF 版；Everything 未运行时的降级提示一致。

## M5 FileJump 移植（预计 2-3 周）

Windows 高级功能阶段二，clipx 在 Windows 上补完最后一块。

范围：

- 宿主侧移植三块：#32770 对话框检测（标题/子控件特征 + WPS 纯消息框误判排除）、多管理器路径采集（Explorer COM / Total Commander / XYplorer / Directory Opus，优先级按 M3 检查点的日志使用分布排定）、注入调度与渐进退避重试（0/120/300/600/1000/1500ms）
- 注入 DLL 复用 native/ShellNavigate 现有产物，不重写
- 新 crate clipx-filejump：仅 Windows，cargo feature 门控（ADR-008）

验收：

- 对 WPF v1.9.8 行为回归：系统对话框原生跳转、浏览器/微信保存对话框多次切换稳定、WPS 场景无误触、常用管理器路径采集正确
- Windows 单进程运行，整机常驻内存回到 10-30MB 目标区间
- WPF 版退役（仅保留历史数据迁移入口）

## M6 macOS（预计 2-3 周）

范围：monitor 的 macOS 实现（changeCount 轮询）、Vision OCR、NSPanel 风格弹窗、LoginItems 自启、Cmd+V 模拟、打包（Developer ID 签名与 notarization 视开发者账号情况，无账号则先出未签名包）。

验收：Mac 上完成与 M1 等价的日用验收。

## M7 Linux 与发布（预计 3 周）

范围：X11 全功能、Wayland（wl-clipboard-rs + 热键方案评估）、GTK 托盘适配、Tesseract OCR、三平台 CI 矩阵与安装包（Windows Inno Setup / macOS dmg / Linux deb + AppImage，流程沿用 findx 的 CI 模式）、首个全平台 release。

验收：三平台 CI 绿灯；每平台过冒烟清单；GitHub Release 发布成功——首发即含完整 Windows 能力（剪贴板 + FileJump + Everything）。

## 风险跟踪

| 风险 | 影响 | 缓解 | 状态 |
|---|---|---|---|
| Slint 弹窗或列表不达标（S1/S2） | 路线返工 | 前置到 M0 首日验证；egui 备案条件已写死 | 待验证 |
| Wayland 监听与热键 | M7 范围 | 业界普遍难点（CopyQ 在 Sway 亦有 issue）；最坏情况 Wayland 仅支持复制监听 | 接受 |
| uniOCR 中文质量（S4） | OCR 体验 | 平台原生引擎直调替换路径已定 | 待验证 |
| tray-icon 在 Linux 需 GTK loop | Linux 内存 | M7 实测；超标则直连 StatusNotifierItem | 待验证 |
| 单人开发节奏 | 周期 | 里程碑粒度小、每段可日用，随时可停在可用状态 | 纪律约束 |
| FileJump 移植的 Win32 脆弱面（注入/COM/各家管理器私有协议） | M5 | 复用已验证的原生 DLL 与 v1.9.7/1.9.8 行为基线；feature 门控隔离在 clipx-filejump；回退方案为长期双进程共存 | 排位已定（先于 macOS），待启动 |

## 里程碑之外的持续事项

- 每完成一个 M：更新 CHANGELOG、打 tag、发版（沿用 findx 的 CI 模式）
- 文档随代码同步：PRD / ARCHITECTURE / ROADMAP 的变更与代码同一提交，避免文档腐化
- 内存与性能实测数字随每个 M 更新到 PRD §5
