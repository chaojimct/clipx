# CLAUDE.md

本文件为 AI 辅助开发（Claude Code / TRAE 等）在本仓库工作时提供规约。

## 项目状态

**规划阶段（2026-09-02）：仓库尚无代码，仅文档。M0 未启动。** 根目录的空 Cargo.toml / build.rs / .gitignore 是占位文件；M0 将把根 Cargo.toml 改写为 workspace 定义并删除 build.rs。

clipx：跨平台（Windows/macOS/Linux）剪贴板管理器，Rust + Slint。前身为 Windows 单平台的 WPF ClipboardX（路径 ../clipboard），其交互行为是本项目的规格书。

FileJump 与 Everything 集成不进 v1（Windows-only 能力）：过渡期与 WPF FileJumpOnly flavor（ClipboardX-filejump.exe，含 FileJump 与 Everything）双进程共存，M4-M5 移植吸收（先于 macOS），见 PRD §7 / ARCHITECTURE ADR-008 / ROADMAP M4-M5。

## 必读文档（按此顺序）

1. docs/PRD.md — 做什么、不做什么、验收数字
2. docs/ARCHITECTURE.md — crate 结构、选型决策（ADR）、数据流、schema、硬约束
3. docs/ROADMAP.md — 当前里程碑、范围与验收、开发纪律

## 硬约束（违反即返工）

- 常驻内存 ≤30MB；图片 blob 懒加载，取后立即释放，禁止长生命周期缓存
- OCR 必须保留；OCR 后立即释放图片 bytes；OCR 队列有界
- 去重哈希用 blake3，禁止弱哈希（如标准库 DefaultHasher）
- core / store / monitor / ocr 四个 crate 不得依赖 UI 框架；平台代码放在各自平台 feature 后面
- 线程通信只用 channel（worker 持 Sender，消费侧持 Receiver），禁止跨线程共享 Mutex 状态机
- 边界代码禁止裸 unwrap 与静默 .ok()：剪贴板、OCR、IO 失败必须降级为日志 + 功能关闭

## 构建与运行（M0 建立后生效）

- 开发：`cargo run -p clipx-app`
- 发布：`cargo build -p clipx-app --release`；打包流程参考 ../findx 的 CI（Windows Inno Setup、macOS dmg、Linux deb + AppImage）
- 测试：`cargo test --workspace`；core 层改动必须有对应单测

## 代码约定

- workspace 成员放在 crates/ 下；动手前先确认功能属于哪个 crate，跨层改动先更新 ARCHITECTURE.md 再写码
- 交互行为与 WPF 版冲突时以 WPF 版为准（有意偏离需登记 PRD §8）
- 数据库结构变更必须走 PRAGMA user_version 迁移，禁止改表不升版本
- 新依赖需能在 ARCHITECTURE.md 的 ADR 中找到对应决策或理由

## 数据位置约定

- 便携模式（默认）：exe 同级 `Data/`（clipx.db + settings.json）
- 安装模式：%LocalAppData%\clipx（Win）/ ~/Library/Application Support/clipx（mac）/ ~/.local/share/clipx（Linux）
- WPF 版历史迁移：读取 ../clipboard/Data/clipboard_history.db，字段映射见 ARCHITECTURE §4
