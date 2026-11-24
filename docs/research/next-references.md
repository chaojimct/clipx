# 待深挖的参考（2026-11-24 随记）

Listary 逆向（T120–T122 + 本仓 `listary-reference.md`）覆盖的是搜索/索引/文件跳转。
弹窗定位是自研（`tsf-caret-research.md` + v0.10.11/12/13/21），Listary 那边没有。

##  landscape（按贴合度排序）

- 剪贴板同类：Ditto（开源 C++，分组/统计/网络同步；缺密码管理器门）、
  CopyQ（开源 Qt，脚本/标签页；Wayland 硬骨头对照 M7 定降级）、
  PowerToys Advanced Paste（微软官方粘贴 UX，热键冲突处理）。
- 启动器与排序：Flow Launcher / Wox（插件模型 + Everything 插件）、
  PowerToys Run（查询管线 + 防卡顿）、fzf / nucleo（Rust，模糊打分标杆；
  `rank_score` 细化时对常数）。
- 行内补全：Espanso（开源 Rust，触发判定/跨平台注入/按应用配置——同类问题同语言，优先深挖）。
- 插件生态：Raycast API（扩展 API 金标准，runtime 立项前通读）、Quicker / uTools（动作生态，远期）。
- 平台底层：FlaUI（UIA 条件构造 + 元素缓存）、enigo（Rust 跨平台模拟输入，M6/M7 候选）、
  WinSparkle（Windows 静默更新，对照 curl+Inno 链）。
- 拼音：pinyin-pro / pypinyin（词语级多音消歧；当前全收录 naive 版的升级路径）。

## 优先级

1. Espanso（行内补全下一阶段直接相关）。
2. nucleo（排序细化时）。
3. Raycast API（runtime 立项前）。
