# clipx

跨平台剪贴板管理器：Rust + Slint 原生渲染，常驻托盘，热键呼出不抢焦点弹窗，SQLite 持久化，拼音搜索，内建 OCR（Windows Media OCR / macOS Vision / Linux Tesseract）。

目标：Windows / macOS / Linux 全平台，常驻内存 10-30MB。

| 文档 | 内容 |
|---|---|
| [docs/PRD.md](docs/PRD.md) | 产品需求、功能清单（P0/P1/P2）、验收指标 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 架构、选型决策（ADR）、数据流、数据库设计、平台矩阵 |
| [docs/ROADMAP.md](docs/ROADMAP.md) | 里程碑 M0-M7、验收标准、风险跟踪 |
| [CLAUDE.md](CLAUDE.md) | AI 辅助开发规约与硬约束 |

状态：**v0.10.9**（2026-09-24）。Windows 全功能日用，对齐并超越 WPF 1.9.8；另含首次启动自动导入老版 WPF 历史（含已做过的 OCR）、弹窗质感（自绘滚动条 / 内高光 / 过渡动画）、检索体验（拼音命中高亮、空格分词交集、不完全拼音可高亮）、图上 OCR 选词、可选 OCR 精度拓展包（`--features ocr-rapid`）、文档型文件预览（docx/pptx/xlsx/pdf/文本）、应用内自动更新（带下载进度条与右下角提示条反馈；设置含「关于」页，托盘菜单精简为高频 6 项）。修复快速查找 / 文件跳转浮层呼出瞬间表头与底栏整块不渲染的问题（Slint 软件渲染器增量重绘所致）；更新中心重做——关于页集中展示检查/下载状态与进度，两个更新开关（启动检查 / 自动安装）并入，发现新版本时才出现「下载并安装」。Windows 上可关 `ClipboardX-filejump.exe`，剪贴板 + FileJump + Explorer 打字查找单进程，内存目标 10-30MB。安装包脚本 `scripts/clipx.iss`。v0.10.9 修**批量粘贴在 Cursor 等 Electron 应用里「时好时坏」**（剪贴板写入改为单次 OpenClipboard 内原子写 + 真实等待重试；旧路径是 clear/set 两次独立打开、且重试只 `Sleep(0)` 等于没重试；批量推进改「按下武装、松开消费」，修饰键只信自记账位——旧实现 Ctrl 比 V 先松开会丢一次推进；`cursor`/`code` 收回终端判定，不再把编辑器/对话输入框的 Ctrl+V 换成 Shift+Insert）与**检索卡顿**（拼音列从含图片 BLOB 的 payloads 抽到独立窄表，单次检索 47ms → 3~5ms；升级不再重建 FTS）。

待办：macOS 真机验证（M6a）与几项交互手动点验，清单见 [docs/ROADMAP.md](docs/ROADMAP.md) 的「遗留手动验证登记」。

前身：Windows 版 ClipboardX（WPF，../clipboard），其交互行为是本项目的规格书。老版历史**首次启动自动导入**（自动发现 WPF 数据目录，随后写标记不再重复；手动重跑 `clipx --import-wpf <db>`），容量设置一并跟随。

## 从老版 WPF ClipboardX 迁移

老版历史在 clipx 首启时**自动导入**，无需手工操作。导入之后还有一件事要做：**让老版退休**——它若仍在运行或开机自启，会出现两套剪贴板监听并行、热键相撞、历史双写的分叉。

clipx 会探测老版状态（安装目录 / 数据根 / 是否在跑 / 是否仍自启），并在「设置 → 关于」给出「老版 ClipboardX 迁移收尾」卡片：

1. 点**「停用老版开机自启」**——只删自启项（HKCU Run 值 + 登录计划任务），**不动老版的程序与历史数据**。
   - 老版以**管理员模式**启动时，其自启任务为最高权限注册，普通权限删不掉。此时提示会告知「需管理员权限」并保留按钮，**以管理员身份重新运行 clipx 再点一次**即可（或直接运行老版卸载程序）。
2. 确认 clipx 里能查到老数据后，运行**老版自己的卸载程序**。
   - ⚠️ 卸载向导会问「是否同时删除配置与历史记录」，**必须选「否」**——选「是」会递归删除 `%LocalAppData%\ClipboardX`（历史库就在这里）。

老版路径参考（Windows）：程序 `%LocalAppData%\Programs\ClipboardX`，数据 `%LocalAppData%\ClipboardX`。迁移路径与老版更新通道的可行性研究见 [docs/migration-research.md](docs/migration-research.md)。
