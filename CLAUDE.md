# CLAUDE.md

本文件为 AI 辅助开发（Claude Code / TRAE 等）在本仓库工作时提供规约。

## 项目状态

**v0.10.4 已定版（2026-09-20）：** Windows 全功能日用，对齐并超越 WPF 1.9.8，并叠加图上 OCR 选词、OCR 精度拓展包（feature `ocr-rapid`）、`clipx-doc` 文档型文件预览、应用内自动更新；本轮补弹窗质感（自绘滚动条 / 内高光 / 过渡动画 / 暗色 emoji）、检索体验（拼音命中高亮、空格分词交集、不完全拼音）与键盘翻译修正（`ToUnicodeEx` 取代手写布局表）。Everything 在 M4 完成、FileJump 在 M5 完成，本机可关 WPF `ClipboardX-filejump.exe`。下一迭代：**M6 macOS**（真机验证待办见 ROADMAP「遗留手动验证登记」）。

clipx：跨平台（Windows/macOS/Linux）剪贴板管理器，Rust + Slint。前身为 Windows 单平台的 WPF ClipboardX（路径 ../clipboard），其交互行为是本项目的规格书。

FileJump 与 Everything 已单进程吸收（M4–M5 + 对齐 WPF）；Windows 上可关 WPF `ClipboardX-filejump.exe`。见 PRD §7 / ARCHITECTURE ADR-008 / ROADMAP。

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
- 可选 OCR 拓展包（默认关，ort 静态链接约 +25MB）：`cargo run -p clipx-app --features ocr-rapid`
- 测试：`cargo test --workspace`；core 层改动必须有对应单测。活体测试（依赖本机 Everything/FindX）已 `#[ignore]`，单独跑：`cargo test -p clipx-everything -- --ignored`

## 代码约定

- workspace 成员放在 crates/ 下；动手前先确认功能属于哪个 crate，跨层改动先更新 ARCHITECTURE.md 再写码
- 交互行为与 WPF 版冲突时以 WPF 版为准（有意偏离需登记 PRD §8）
- 数据库结构变更必须走 PRAGMA user_version 迁移，禁止改表不升版本
- 新依赖需能在 ARCHITECTURE.md 的 ADR 中找到对应决策或理由

## UI 开发陷阱（血泪，务必先读）

本项目的渲染器是 **Slint 软件渲染器**（`slint` features: `renderer-software`），有两条会静默失效的坑：

1. **8 位十六进制是 `#RRGGBBAA`，不是 WPF/CSS 的 `#AARRGGBB`。**
   依据：`i-slint-common-1.17.1/color_parsing.rs:34`（`8 => (R,G,B,A)`）。
   按 WPF 习惯写 `#55000000` 会被读成 R=0x55,G=0,B=0,**A=0** —— 即完全透明。
   本项目曾因此让 16 处颜色全部失效（阴影、模态遮罩、预览选区高亮、加载蒙层）。
   从 WPF xaml 抄颜色时，**必须把 alpha 挪到最后两位**。Rust 侧请用
   `slint::Color::from_argb_u8(a,r,g,b)`，它是显式参数、不受此坑影响。

2. **`drop-shadow-blur / -offset-y / -color` 在本渲染器下不渲染。**
   依据：`i-slint-renderer-software-1.17.1/lib.rs:3088` 的 `draw_box_shadow()` 函数体
   只有一行 `// TODO`。想要投影只能用同心圆角矩形累加，见
   `ui/widgets.slint` 的 `UiCardShadow`（三处浮窗共用）。

3. **彩色 emoji 不支持；且 emoji 的 `Text` 必须显式写 `color`，否则暗色主题下隐形。**
   `i-slint-core` 无任何 COLR/CBDT/彩色字形处理，`📋📝🖼️📁` 只会渲染出 Segoe UI Symbol
   的灰阶字形。WPF 侧这些 emoji（`PopupWindow.xaml` 的 `📋`/`📌`/`⚙`/`TypeIcon`）**大都不设
   `Foreground`**，靠 Segoe UI Emoji 的彩色字形出图 —— 照抄到 Slint 就会踩坑：
   不设 `color` 的 `Text` 取默认**黑**，浅色主题下还"像个深色图标"能蒙混过关，
   **暗色主题下就是 #1E1E1E 底上的 #020202，对比度约 1.1:1，整块消失**
   （实测：修复前图标区 21.6% 像素为 `#020202`）。
   本项目已修的点：`popup.slint` 表头 `📋`(primary-text) / 行类型图标(secondary-text) /
   空状态 `🔍`·`📭`，`widgets.slint` 的 `📌`·`⚙`(secondary-text，与 WPF 的
   `Foreground=SecondaryText` 同语义)。**新增任何 emoji `Text` 都必须带 `color`。**
   自查命令：`python .workbuddy/tmp/scan_nocolor.py`（扫全部未设 color 的 `Text` 块）。
   要真彩色需自行栅格化成图片资源。

4. **`clip: true` 的圆角会被忽略 —— 贴卡片外缘的不透明子元素会把卡片圆角「填平」。**
   依据：`i-slint-renderer-software-1.17.1/lib.rs` 的 `combine_clip()`，radius 形参写作
   `_radius`，函数体只有 `// TODO: handle radius and border`，clip 实际只做矩形交集。
   另外 `draw_rectangle()`（纯填充）构造 `DrawRectangleArgs` 时 **radius 恒为 0**，
   只有 `draw_border_rectangle()` 才带四角半径。编译器按「属性最近声明点」选原生类
   （`passes/resolve_native_classes.rs`，层次为
   `Empty → Rectangle → BasicBorderRectangle(声明 border-radius) → BorderRectangle(声明四角)`），
   所以**写了 `border-radius` 的元素圆角本身是有效的**。
   真正的坑在别处：卡片内层那个 `Rectangle { border-radius: 12px; clip: true }` 的圆角被忽略后，
   任何**贴卡片外缘的不透明子元素**（表头 / 底栏 / 预览面板 / 空态铺满层）都会以**矩形**
   铺到卡片角上，把圆角盖成直角 —— 现象是「卡片看着是方的，但阴影是圆的」。
   WPF 三窗口的对照写法（PopupWindow / FileDialogJumpPicker / ExplorerQuickFind 完全一致）：
   `MainBorder` 用 `CornerRadius=12`，**表头 `<Grid>` 不设 Background**，
   底栏用 `CornerRadius="0,0,12,12"`。对应到本项目：
   - 表头 / 空态铺满层 → `background: transparent`，让 `card-bg` 透出来；
   - 底栏 / 预览面板 → 用 `border-bottom-*-radius` / `border-*-right-radius` 自带对应角的圆角
     （只写分角属性即可选到 `BorderRectangle`，**不需要** `border-width`）。

   诊断手法：`python .workbuddy/tmp/corner4.py <png> <scale>` 打印四角 alpha 矩阵，
   角像素的 **RGB 直接指出「谁」填的角**：`#1E1E1E`=表头、`#252526`=底栏、
   `a<255`=卡片自身（说明圆角正常）。

5. **渐变填充与圆角不可共存 —— 写渐变等于放弃圆角。**
   依据：`process_rectangle_impl()`（lib.rs:1578 起）命中 `Brush::LinearGradient` 时直接
   `process_linear_gradient(act_rect, gr)`，**整条分支不读 radius**，产出直角矩形。
   所以「卡片底色做顶部微亮的玻璃渐变」这种常见做法在本渲染器下会把 12px 圆角重新填平
   （与上一条 `combine_clip` 属同一类静默失效）。要层次感只能用**纯色**：
   本项目用 `Theme.card-sheen` + `UiCardSheen` 在卡片上沿压 1px 高光，
   横坐标从圆角弧之后起算（`inset + card-radius`）以彻底躲开圆弧。

6. **组件根元素不能引用 `parent`。**
   `export component X inherits Rectangle { x: parent.width - 12px; }` 会直接在 Slint 编译期
   报 `Cannot access id 'parent'`（组件定义时父级未知）。几何必须写在**内层**元素上，
   内层的 `parent` 才是调用方容器 —— 见 `UiCardSheen`（根 100%×100%，几何在内层矩形）。

## 输入与检索陷阱（血泪，务必先读）

7. **VK→字符不要手写布局表，交给 `ToUnicodeEx`。**
   钩子侧原先是手写的 US 布局表，数字行上档写成 `"!@#$%^&*()"[(vk - 0x30)]`——
   **索引整体右移一位**，Shift+1 出 `@`、Shift+2 出 `#`，每键都拿到右邻键的字符；
   非 US 布局更是整片错乱。WPF 参考实现（`LowLevelKeyboardText.VkToChar`）用的是
   `ToUnicodeEx(vk, scanCode, keyState, …, GetKeyboardLayout(0))`，移植时**要移植机制，
   不要自己造表**。要点：Shift 的 `0x10`/`0xA0` 都要置位；CapsLock 用
   `GetAsyncKeyState(0x14)&1`（钩子线程里 `GetKeyState` 只反映本线程队列的陈旧状态）；
   返回负数是死键，**必须再调一次冲掉**，否则下一个键会被粘上变音符。
   改完务必真机按键验证：Shift+数字行、`-`/`=`/`[`/`]`/`;`/`'`/`,`/`.`/`/` 全扫一遍。

8. **检索判定与高亮判定必须同构 —— 否则「搜到了却一个字都不亮」。**
   DB 侧是 `pinyin_blob LIKE '%q%'`，而 blob 是**全拼连写 + 首字母连写**的串，
   所以任意子串都算命中：`pin` / `pingj` / `ingj` 都命中「萍姐」。
   高亮侧若用「逐字消费拼音」（只认完整音节或音节前缀，且碰到「，」这类无拼音字符就断），
   这些查询就会**检索命中、区间为空**（实测 `pin`/`ingj`/`jiew` 旧实现全返回 `None`）。
   正确做法：按 blob 建「blob 字节区间 → 源字符下标」映射，直接在 **blob 上找子串**再映射回
   字符区间（`clipx_core::pinyin::indexed_blob` + `map_byte_range`）。命中不足一字的部分音节
   （`pin` 落在「萍」的 `ping` 里）→ **整字高亮**；跨过中间的非汉字（`jiew` = 姐+我）
   → 区间连标点一起包进来（UI 只有一段连续高亮，包络是唯一可行表达）。
   **不变量测试**：`indexed_blob(t).0 == to_pinyin_blob(t)` 逐字节相等 +
   `hit_span_matches_blob_semantics`（blob 命中 ⇒ 必有区间）。改这两处任一必跑。

9. **store 侧空格分词 = 交集，别再拼整串 LIKE。**
   原先把整个 query（含空格）塞进一个 `LIKE '%a b%'`，带空格的查询**必然 0 结果**。
   现在按空白分词，每个 token 生成一个 `(preview LIKE ?n OR pinyin_blob LIKE ?n [OR full_text/OCR])`
   再用 `AND` 连起来，FTS 分支保持整串 AND 前缀语义作为并集召回通道。
   实现注意：占位符编号要随 token 数动态递增，参数用 `Vec<rusqlite::types::Value>` +
   `params_from_iter`（旧的 4 分支 `match (fts, src)` 结构撑不住可变参数）。
   单元测试见 `search_spaces_are_and_tokens` / `search_keeps_deep_and_source_filters`。

10. **Space 是双义的：空查询=切换预览，检索中=分词符。**
   WPF 版 Space 只会切预览、不能搜空格；本项目有意超越（登记于此）。
   逻辑在 `logic.rs` 的 `KeyEvt::Space` 分支，底栏 `footer_hint` 会跟着状态改写文案。
   钩子侧**不需要**改：`translate` 照旧发 `KeyEvt::Space`，由逻辑层按 query 是否为空分流。
   `--query` 自检遇到空格也发 `KeyEvt::Space`（而非 `Char(' ')`），保证自检覆盖这条分支。

11. **全局热键线程不能阻塞在 `GetMessageW` —— 否则「改完快捷键不生效，必须重启」。**
   热键线程只注册了 global-hotkey 的隐藏窗口，**没有热键按下时一个消息都不来**。
   消息泵若写成 `while GetMessageW(..) { …; update_rx.try_recv() }`，`update_rx` 就只在
   按下热键那一刻被读一次：设置里保存后不重注册。默认 `` Ctrl+` `` 被别的程序占用时最明显——
   改成任何键都不响应，只能重启程序（v0.10.5 修的正是这个）。
   正确写法：`MsgWaitForMultipleObjects(None, false, 50, QS_ALLINPUT)` 等「有消息或 50ms 超时」，
   再 `PeekMessageW(.., PM_REMOVE)` 逐条派发；有消息立即醒，无消息最多等 50ms（肉眼不可感）。
   配套：注册失败**必须回投 UI**（`AppEvt::HotkeyReport` → 设置窗口红字/托盘提示）。
   失败原来只写 `eprintln` + 日志，用户看不到，只会以为程序坏了。

12. **WPF 数据要「装了就能看到」，不能只留一条命令行。**
   首启自动导入在 `wpf_import.rs`：`candidates()` 按优先级找源库
   （`%LocalAppData%\ClipboardX\clipboard_history.db` 安装模式 → 同级 `Data/` → `../clipboard/Data/`），
   跑完写标记 `Data/.wpf-import.json`，此后不再自动跑；手动重跑仍是 `--import-wpf <db>`。
   两个硬要求：**分批读**（`wpf::BatchReader`，行数 + blob 字节双限，源库实测 197MB/7002 条，
   整读会顶穿 30MB 内存线）与**批间让位**（`import_batch` 走 store 单线程 channel，
   连批占满会让 UI 查询排队卡顿）。`ocr_text` 必须在迁移时带走（WPF 已做过 OCR，
   丢了等于让用户重做一遍且结果不一定复现）。

### 主题自查（改完配色必须两套主题都过一遍）

`Data/settings.json` 的 `"theme"` 取 `Light`/`Dark`/`System`，改它即可切换。
**只验浅色等于没验** —— 上面的 emoji 坑正是只在暗色下暴露。每次动配色/图标至少跑两次
`--snapshot`，并在快照里检查：图标区无近黑像素（`#020202±3` 应≈0）、各有色元素
（选中 `#185656` 暗 / `#97CBCD` 浅、accent `#139493`、文字各级灰）都在。

### UI 视觉自检（分层透明窗口抓不到，必须走这条）

clipx 是 `AllowsTransparency` 式分层窗口，屏幕 BitBlt / `mss` / `PrintWindow` 都抓不到
它的窗口内容（抓到的边距会是黑或桌面）。唯一可靠手段是 Slint 自带的窗口快照：

```bash
# --uitest 让弹窗自显（绕过全局热键）；--snapshot 渲染稳定后写带 alpha 的 PNG 再退出
clipx.exe --uitest --snapshot C:/tmp/snap.png
# --query <text>：显示后逐字走真实按键通道输入，拍到「搜索态」
#   （命中高亮/结果计数/空态/深层命中标记）。缺了它只能拍空搜索框。
#   空格发的是 KeyEvt::Space，覆盖「检索中空格=分词符」那条分支。
clipx.exe --uitest --query "ping ju" --snapshot C:/tmp/snap_query.png
```

验证命中高亮是否真的画出来，用 `.workbuddy/tmp/highlight_check.py <png> <Dark|Light>`：
抗锯齿会把字缘混向底色，但字身必有一批**精确等于**下发色值的像素，
扫 `d==0` 的近邻即可判定，比人眼在截图里找色块可靠。

得到的是**预乘 alpha** 的 RGBA PNG，用 `Image.alpha_composite` 合成到浅底上即可
量测阴影/圆角/半透明。注意快照分辨率随缩放因子可能为 1x 或 2x。

跑快照前必须先清掉正在运行的实例，否则会被单实例锁挡住（日志只有
`clipx 已在运行，退出本实例`，不出图）：

1. `Data/settings.json` 的 `run_as_admin` 临时置 `false` —— 否则**提权重启会丢掉命令行参数**，
   进程静默转成后台实例，快照不执行。
2. 杀掉残留实例。若该实例是提权启动的，`taskkill /F` 会"拒绝访问"，
   而 PowerShell 的 `Invoke-CimMethod ... Terminate`（WMI）可能被沙箱安全策略拦；
   可退回 Python + ctypes：
   ```python
   k = ctypes.windll.kernel32
   h = k.OpenProcess(1, False, PID)   # PROCESS_TERMINATE
   k.TerminateProcess(h, 1)           # 返回 1 即成功
   ```
3. 截完后**记得还原** `settings.json`（`cp settings.json.uibak settings.json`）。

## 数据位置约定

- 便携模式（默认）：exe 同级 `Data/`（clipx.db + settings.json）
- 安装模式：%LocalAppData%\clipx（Win）/ ~/Library/Application Support/clipx（mac）/ ~/.local/share/clipx（Linux）
- WPF 版历史迁移：**首次启动自动导入**（装配好就能看到老数据）——候选源库见
  `wpf_import::candidates`：`%LocalAppData%\ClipboardX\clipboard_history.db`（WPF 安装模式，本机实测位置）
  → 同级 `Data/clipboard_history.db`（便携对便携）→ `../clipboard/Data/clipboard_history.db`（开发机）。
  成功后写标记 `Data/.wpf-import.json`，此后不再自动跑；手动重跑 `clipx --import-wpf <clipboard_history.db>`。
  字段映射见 ARCHITECTURE §4。
