# Changelog

本项目遵循里程碑发版（见 docs/ROADMAP.md），tag `v*` 触发 CI。

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
