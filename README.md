# clipx

跨平台剪贴板管理器：Rust + Slint 原生渲染，常驻托盘，热键呼出不抢焦点弹窗，SQLite 持久化，拼音搜索，内建 OCR（Windows Media OCR / macOS Vision / Linux Tesseract）。

目标：Windows / macOS / Linux 全平台，常驻内存 10-30MB。

| 文档 | 内容 |
|---|---|
| [docs/PRD.md](docs/PRD.md) | 产品需求、功能清单（P0/P1/P2）、验收指标 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 架构、选型决策（ADR）、数据流、数据库设计、平台矩阵 |
| [docs/ROADMAP.md](docs/ROADMAP.md) | 里程碑 M0-M7、验收标准、风险跟踪 |
| [CLAUDE.md](CLAUDE.md) | AI 辅助开发规约与硬约束 |

状态：**v0.10.7**（2026-09-21）。Windows 全功能日用，对齐并超越 WPF 1.9.8；另含首次启动自动导入老版 WPF 历史（含已做过的 OCR）、弹窗质感（自绘滚动条 / 内高光 / 过渡动画）、检索体验（拼音命中高亮、空格分词交集、不完全拼音可高亮）、图上 OCR 选词、可选 OCR 精度拓展包（`--features ocr-rapid`）、文档型文件预览（docx/pptx/xlsx/pdf/文本）、应用内自动更新（带下载进度条与右下角提示条反馈；设置含「关于」页，托盘菜单精简为高频 6 项）。修复快速查找 / 文件跳转浮层呼出瞬间表头与底栏整块不渲染的问题（Slint 软件渲染器增量重绘所致）。Windows 上可关 `ClipboardX-filejump.exe`，剪贴板 + FileJump + Explorer 打字查找单进程，内存目标 10-30MB。安装包脚本 `scripts/clipx.iss`。

待办：macOS 真机验证（M6a）与几项交互手动点验，清单见 [docs/ROADMAP.md](docs/ROADMAP.md) 的「遗留手动验证登记」。

前身：Windows 版 ClipboardX（WPF，../clipboard），其交互行为是本项目的规格书。老版历史**首次启动自动导入**（自动发现 WPF 数据目录，随后写标记不再重复；手动重跑 `clipx --import-wpf <db>`），容量设置一并跟随。
