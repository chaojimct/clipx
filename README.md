# clipx

跨平台剪贴板管理器：Rust + Slint 原生渲染，常驻托盘，热键呼出不抢焦点弹窗，SQLite 持久化，拼音搜索，内建 OCR（Windows Media OCR / macOS Vision / Linux Tesseract）。

目标：Windows / macOS / Linux 全平台，常驻内存 10-30MB。

| 文档 | 内容 |
|---|---|
| [docs/PRD.md](docs/PRD.md) | 产品需求、功能清单（P0/P1/P2）、验收指标 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 架构、选型决策（ADR）、数据流、数据库设计、平台矩阵 |
| [docs/ROADMAP.md](docs/ROADMAP.md) | 里程碑 M0-M7、验收标准、风险跟踪 |
| [CLAUDE.md](CLAUDE.md) | AI 辅助开发规约与硬约束 |

状态：对齐并超越 WPF 1.9.8（2026-09-03）。Windows 上可关 `ClipboardX-filejump.exe`，剪贴板 + FileJump + Explorer 打字查找单进程。安装包脚本 `scripts/clipx.iss`。

前身：Windows 版 ClipboardX（WPF，../clipboard），其交互行为是本项目的规格书，历史数据支持一键迁移。
