# Changelog

本项目遵循里程碑发版（见 docs/ROADMAP.md），tag `v*` 触发 CI。

## v0.4.0-m3 — 打磨与迁移（2026-09-03）

M3 完成：格式补全 + 收藏 + 菜单 + 自启 + WPF 历史迁移。

- 格式补全：文件列表（CF_HDROP）与富文本（CF_UNICODETEXT + HTML Format，kind=2/3）；富文本粘贴回写投影 + HTML 双格式，html 缺失退化为纯文本
- 剪贴板原子快照（ARCHITECTURE §7.7）：类型判定与数据读取同一次 OpenClipboard 周期，open 争抢/中途被打断整体重试，杜绝富文本误判降级
- 收藏/置顶：Ctrl+P 或右键菜单切换，置顶浮动顶部（ORDER BY pinned DESC）且不受容量裁剪影响；选中跟随原条目
- 右键上下文菜单：复制到剪贴板（不模拟粘贴）/ 置顶 / 删除；Menu 键打开，Esc 只关菜单不关弹窗
- 多屏定位：MonitorFromPoint 光标所在显示器工作区夹紧
- 开机自启：schtasks XML（对齐 WPF v1.9.8：无执行时限、电池可用、仅当前用户登录触发），托盘菜单可切换
- WPF 迁移命令 `--import-wpf`：真实库 6693 条幂等迁移（content_hash 去重、保留时间戳与 ocr_text），容量设置自动跟随
- 万条压测 `--bench`：seed 10000 条 162ms，最慢查询路径 7.09ms（验收线 100ms）
- `--uitest` 参数：程序化显示弹窗（UI 截图验收用，绕过热键依赖）
- 验收：scripts/m3_acceptance.ps1 剪贴板段全部通过；UI 段（F/G/H/J）待用户交互终端复跑（工具宿主环境窗口站权限受限，ARCHITECTURE §9）；单测 44/44 绿

## v0.3.0-m2 — 图片与 OCR（2026-09-03）

M2 完成：图片全链路 + OCR 可搜。

- 图片采集：位图 → PNG 入库（15MB 上限），宽 64px 等比缩略图即时生成
- OCR：Windows Media OCR 直调（windows crate WinRT，ADR-004 修订），有界队列异步执行、即用即释；CJK 后处理移植 WPF 版 OcrTextPostProcessor
- OCR 文本可搜：搜图中文字命中后 Enter 原样回贴图片（DIB 回写）
- Space 预览：原图懒加载（解码限长边 1600）+ OCR 文本展示，4K 图不崩不卡
- schema v4：payloads 增 ocr_text 列；总条数与图片条数双轨容量裁剪（max_items / max_image_items）
- 内存：启动 23.1MB；100 图连发峰值 82.8MB，空闲 trim 后回落 0.2MB
- 验收：scripts/m2_acceptance.ps1 全部通过；单测 37/37 绿

## v0.2.0-m1 — Windows 日用（2026-09-02）

M1 完成：可作为主力剪贴板工具日用。

- 搜索：FTS5 英文/数字词匹配 + LIKE 子串（preview / full_text）+ 拼音 blob（全拼连写 + 首字母连写，`nihao`/`nh` 均可命中）
- 键盘导航：↑↓ 移动、Enter 粘贴、Esc 关闭、可打印字符直接进搜索、空搜索下数字 1-9 快贴
- 粘贴回写：ClipboardGate 防自采 + Ctrl+V 模拟（不抢前台焦点）
- 无焦点输入：WH_KEYBOARD_LL 钩子接收弹窗按键（含 Shift 符号、小键盘数字），WH_MOUSE_LL 点击外部关闭
- UI 复刻 WPF 版：#1E1E1E 暗色主题、accent #139493、圆角行选中态、置顶 📌 徽标、空状态提示
- 系统托盘（显示/退出）、设置持久化（settings.json）、单实例互斥、失焦关闭
- store schema v3：payloads 表 + pinyin_blob 列，v1/v2 自动迁移
- 验收：scripts/m1_acceptance.ps1 全部通过；单测 8/8 绿

## v0.1.0-m0 — 骨架验证（2026-09-02）

M0 完成：技术路线跑通最小闭环。

- workspace 五 crate 骨架（core / store / monitor / ocr / app），依赖版本锁定
- Windows 剪贴板事件监听 → channel → blake3 去重 → SQLite（WAL + FTS5）入库
- Slint 最小弹窗：Ctrl+Alt+V 呼出/隐藏、列表显示、Esc 关闭（WH_KEYBOARD_LL 钩子，不抢前台焦点）
- 软件渲染器（ADR-009）：常驻 19.7MB / 私有 4.2MB，达标 10-30MB 目标
- 验收：入库延迟 63ms；100 连发 100/100 零漏采零重复；杀进程 WAL 零丢失；重启数据完整
- 验收脚本：scripts/m0_acceptance.ps1、scripts/s1_focus.ps1
