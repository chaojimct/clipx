# CLAUDE.md

本文件为 AI 辅助开发（Claude Code / TRAE 等）在本仓库工作时提供规约。

## 项目状态

**v0.10.13 已定版（2026-10-23）：** 落点跟随修正 —— TSF 信标提到 30s 光标缓存之前（治「只有第一下对，后面位置都不变」：缓存 TTL 内把窗口钉在老位置），退化矩形（宿主此刻无 TSF 文档，如焦点在会话列表）不再计入信标黑名单、冷却 60s→6s、阈值 2→3（治「偶尔首次位置不对」：非输入区呼出两次就把整进程拉黑一分钟，期间点回输入框只吃 gui 假点），首次装钩超时 400→600ms；新增 `--caret-probe` 分支诊断。v0.10.12 已定版（2026-10-23）：秒搜 —— Everything 链路重构（release 下同步回执被自己的消息泵饿死致 100% 超时；改 120ms 发送预算 + 主动泵消息 + 原子填充标志 + 递增 token，热路径 12–33ms），FileJump 改常驻 worker 覆盖式投递（去掉每键 150ms 白等），引擎可选（Everything 默认 / FindX）+ 都没装时内置递归兜底；新增 **x86 光标信标**，32 位宿主（企业微信 / WPS / 腾讯会议）也能像素级定位，跨位数 `LoadLibraryW` 地址换算修好上传框跳转。v0.10.11 TSF 光标信标（DLL 注入路线乙，Electron 精确跟随，输入法零干扰）。v0.10.10 修弹窗「所有按键监听失效」（Alt armed 跨会话残留，详见「输入与检索陷阱」#15）。

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
- **测试代码要跨平台**（CI 是 Windows/macOS/Linux 三平台矩阵，本地只在 Windows 上跑）：路径一律用
  `Path::new("a").join("b")` 拼，**不要写 `r"C:\app\clipx"` 这类字面量** —— Unix 上反斜杠不是分隔符，
  `parent()` 会返回空串，断言在 mac/Linux 假失败（v0.10.5 正是这么挂的，Windows job 全绿所以本地看不见）。
  同理别假设 `\n` 行尾、别硬编码盘符；涉及平台差异的断言用 `#[cfg(windows)]` 圈起来。
- **本机 `cargo check` 全绿 ≠ CI 绿**：本机只有 Windows target，而 **clipx-app 被排除在 CI 的
  cross-check 之外**（`fontconfig-sys` 的 build script 在 Windows 上过不去），所以 clipx-app 的
  非 Windows 编译错误**只有 CI 的 test job 能发现** —— 2026-09-22 起连红三轮才被注意到
  （`legacy_wpf::log()` 这个跨平台 `pub fn` 无条件调用 Windows 专有的 `write_data_log` → `E0425`）。
  规矩：**调用 `win_popup` 等 Windows 专有函数时，二选一** —— 调用点加 `#[cfg(windows)]`，
  或在该函数旁边补一个同签名的 `#[cfg(not(windows))]` 空实现（`win_popup.rs` 里
  `append_debug_log` / `resolve_popup_anchor` / `fg_debug` / `write_data_log` 都是这个写法）。
  推 main 后**必须**看 ubuntu/macos 的 test job 结论，不能只看 Windows。
- **面向用户的后台/托盘动作必须落到可见反馈**，一律经 `logic::notify()` / `notify_error()`
  （→ 右下角提示条 `ui/toast.slint`），**禁止裸 `return` 静默收场**。反面教材：托盘
  「文件夹跳转」在功能未启用时直接 return、「检查更新」无新版时静默返回、「关于」只改托盘
  tooltip —— 用户不开设置窗、不把鼠标悬到托盘图标上就一个字都看不到，结论必然是「菜单坏了」。
  长任务（下载 / 安装）用带进度的形态：`AppEvt::UpdateProgress { text, progress, done }` ——
  **`done` 必须显式区分终态**（成功收尾 / 失败 / 交用户手动完成）。少了它，终态文案也会顶着
  「更新中…」的禁用按钮，用户不知道还能不能操作（v0.10.8 的「点了没反应」就是这么来的）。
- **一条事件通道只讲一件事**。OCR 拓展包下载原先蹭 `UpdateProgress` 发，结果被显示成
  「正在更新」并顺带禁用了关于页的更新按钮 —— 两件无关的事共用通道必然串扰。改为
  独立 `AppEvt::PackNotice`。判断标准：**这两个状态会不会同时存在 / 会不会互相干扰 UI**。
- **「只在启动时读一次」的开关，改了必须补跑一次**。`check_updates` 这种在 `main.rs` 启动时
  读的设置，用户在设置窗里打开它却要等下次开机才生效 —— 表现就是「开关是开的但没动静」。
  保存时记住改动前的值（`prev_*`），只在「关→开」时补触发一次运行时副作用。
- **托盘菜单只放运行期高频入口**（显隐 / 暂停 / 清空 / 设置 / 关于 / 退出六个）。配置类开关（自启、
  自动更新）和低频数据/诊断动作（导出导入历史、探测对话框、检查更新）一律进设置窗口 ——
  平铺 14 项的菜单会把「暂停采集」这种真正要手快的东西淹掉。加菜单项前先问一句：这个动作
  用户一天会点几次？
- **设置页顺序与页码是三处联动的**，改一处必须同步另两处，否则自检拍到别的页、进程列表拉不到：
  1. `ui/settings.slint` 的 tab 列表 + `if root.page == N` 的页面块
  2. `logic.rs` 的 `PAGE_CLIPBOARD` / `PAGE_ABOUT` / `PAGE_EXPERIMENTAL` 常量（`open_settings_at`
     用到「实验性」页时顺带拉进程列表）
  3. `settings_win.rs::snapshot_of` 的 `BoolOpts` —— 开关快照是**具名字段结构体**
     （2026-10-28 前是按下标取用的 `bools` 向量，中间插入会错位；现已消除该坑，
     新开关直接加具名字段，构造与 `apply` 两处同名setter配对即可）。

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

7. **`Window` 不能重声明内置属性名（`title` 等）。**
   `export component ToastWindow inherits Window { in-out property <string> title: "clipx"; }`
   会在 Slint 编译期直接报 `Cannot override property 'title'` ——
   `title / width / height / background / no-frame / always-on-top / icon` 都是
   `WindowItem` 的成员，只能**赋值**、不能重新声明。提示条因此叫 `heading`
   （见 `ui/toast.slint`）。给新窗口起属性名前先扫一眼 `builtins.slint` 的 `WindowItem`。

8. **给窗口 `SetWindowPos` 时不要钉物理尺寸 —— 尺寸永远留给 Slint/winit。**
   Slint 的 `width: 400px` 是**逻辑**尺寸，winit 会按 scale 换算成物理
   （200% 缩放下即 800x216）。若定位代码又拿「逻辑尺寸 × scale」当物理尺寸钉一遍，
   在 200% 缩放屏上等于把逻辑尺寸砍半：400x108 逻辑的窗口变成 200x54 逻辑，
   内容只剩左上四分之一（自检实测 `take_snapshot` 从 400x108 掉到 200x54 才暴露）。
   定位只做两件事：`window.set_position(Physical(..))` + `SetWindowPos(SWP_NOSIZE)`。
   正确写法见 `win_popup::show_toast` 与 `commit_hwnd_pos_only`。

9. **常驻浮窗 `hide()`→`show()` 后会出现「首帧残缺」（表头/底栏整块留白），必须抖一次尺寸。**
   现象（v0.10.6 用户报告）：快速查找浮层呼出瞬间**只有输入框**，表头（`everything` / `N 项`）
   与底栏（快捷键提示）整块不见；等搜索结果回来把内容撑大后又「自己好了」。
   根因是 Slint 软件渲染器的**增量重绘策略**，与 UI 代码无关：
   - `i-slint-backend-winit/renderer/sw.rs:111` 按 softbuffer 的 `buffer.age()` 选重绘策略：
     age==1 → `RepaintBufferType::ReusedBuffer`，此时**只重绘脏区**；
   - 常驻窗 `hide()`→`show()` 复用同一个 surface，而 softbuffer 的 Win32 后端
     （`softbuffer-0.4.8/src/backends/win32.rs`）既不知道窗口被隐藏过
     （`age()` 只看 `buffer.presented`），也只在 `resize()` **真正换尺寸**时才重建缓冲
     （同尺寸直接 `return Ok(())`）—— 于是 `age()` 恒为 1，脏区外的元素永远不重绘；
   - 「无属性变化」的静态元素（表头标题、分隔线、底栏）正好一个都不在脏区里，整块留白。

   修法：**显示前把高度抖小 1px、显示后下一帧再恢复**，借两次尺寸变化强制 softbuffer
   重建缓冲（`age()`→0→`NewBuffer`，脏区=整窗）。顺序是关键，且**必须跨帧**：
   1) `force_full_repaint_before_show(win, w, h)`（隐藏态调用，`show()` **之前**）；
   2) `win.show()`；
   3) `restore_size_after_show(&weak, w, h)`（用 `Timer::single_shot(0)` 延到下一帧）。
   若在同一帧里抖动又改回，首帧看到的仍是原尺寸、缓冲不重建，**等于没做**。
   别试图用官方入口：`force_screen_refresh()` / `Renderer::mark_dirty_region()` 都够不到 ——
   `Window` 无 renderer 访问器，`WindowAdapter::renderer()` 返回封印 trait 无法向下转型，
   `i-slint-core` 又是 `slint` 的私有依赖。当前实现见 `win_popup.rs` 两个同名前缀函数。
   自检：`clipx.exe --no-instance-lock --qf-demo <关键词> --snapshot out.png`，
   会跑**两轮**同一布局的会话（第二轮即复现路径），产出 `out.png` 与 `out-r2.png`
   （各带一张 `-late.png`，等 Everything 结果回来后窗口长高那一帧）；全部必须能看到
   表头与底栏。

   ⚠️ **`--snapshot` 证明不了本 bug 已修**：它走 `Window::take_snapshot()`，是 Slint
   内部直调 renderer 渲染，**绕过 softbuffer 的 present 路径**；而本 bug 恰恰发生在
   present 层（脏区裁剪）。要真验证，必须抓**真实 HWND 像素**：
   `PrintWindow(hwnd, memdc, PW_RENDERFULLCONTENT)` —— 分层透明窗口用屏幕 BitBlt
   抓不到内容，但 PW_RENDERFULLCONTENT 可以。脚本 `.workbuddy/tmp/pshot.py` 与
   `grab_qf.py`（后者在 demo 运行时轮询 `title='clipx-quickfind'` 的窗口并抓首帧）
   可直接用。判据：表头 `everything` 与底栏快捷键必须在**首帧**（此时还显示
   「正在定位当前文件夹...」、结果尚未到达）就已完整。

## 输入与检索陷阱（血泪，务必先读）

9. **VK→字符不要手写布局表，交给 `ToUnicodeEx`。**
   钩子侧原先是手写的 US 布局表，数字行上档写成 `"!@#$%^&*()"[(vk - 0x30)]`——
   **索引整体右移一位**，Shift+1 出 `@`、Shift+2 出 `#`，每键都拿到右邻键的字符；
   非 US 布局更是整片错乱。WPF 参考实现（`LowLevelKeyboardText.VkToChar`）用的是
   `ToUnicodeEx(vk, scanCode, keyState, …, GetKeyboardLayout(0))`，移植时**要移植机制，
   不要自己造表**。要点：Shift 的 `0x10`/`0xA0` 都要置位；CapsLock 用
   `GetAsyncKeyState(0x14)&1`（钩子线程里 `GetKeyState` 只反映本线程队列的陈旧状态）；
   返回负数是死键，**必须再调一次冲掉**，否则下一个键会被粘上变音符。
   改完务必真机按键验证：Shift+数字行、`-`/`=`/`[`/`]`/`;`/`'`/`,`/`.`/`/` 全扫一遍。

10. **检索判定与高亮判定必须同构 —— 否则「搜到了却一个字都不亮」。**
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

11. **store 侧空格分词 = 交集，别再拼整串 LIKE。**
   原先把整个 query（含空格）塞进一个 `LIKE '%a b%'`，带空格的查询**必然 0 结果**。
   现在按空白分词，每个 token 生成一个 `(preview LIKE ?n OR pinyin_blob LIKE ?n [OR full_text/OCR])`
   再用 `AND` 连起来，FTS 分支保持整串 AND 前缀语义作为并集召回通道。
   实现注意：占位符编号要随 token 数动态递增，参数用 `Vec<rusqlite::types::Value>` +
   `params_from_iter`（旧的 4 分支 `match (fts, src)` 结构撑不住可变参数）。
   单元测试见 `search_spaces_are_and_tokens` / `search_keeps_deep_and_source_filters`。

12. **Space 是双义的：空查询=切换预览，检索中=分词符。**
   WPF 版 Space 只会切预览、不能搜空格；本项目有意超越（登记于此）。
   逻辑在 `logic.rs` 的 `KeyEvt::Space` 分支，底栏 `footer_hint` 会跟着状态改写文案。
   钩子侧**不需要**改：`translate` 照旧发 `KeyEvt::Space`，由逻辑层按 query 是否为空分流。
   `--query` 自检遇到空格也发 `KeyEvt::Space`（而非 `Char(' ')`），保证自检覆盖这条分支。

13. **全局热键线程不能阻塞在 `GetMessageW` —— 否则「改完快捷键不生效，必须重启」。**
   热键线程只注册了 global-hotkey 的隐藏窗口，**没有热键按下时一个消息都不来**。
   消息泵若写成 `while GetMessageW(..) { …; update_rx.try_recv() }`，`update_rx` 就只在
   按下热键那一刻被读一次：设置里保存后不重注册。默认 `` Ctrl+` `` 被别的程序占用时最明显——
   改成任何键都不响应，只能重启程序（v0.10.5 修的正是这个）。
   正确写法：`MsgWaitForMultipleObjects(None, false, 50, QS_ALLINPUT)` 等「有消息或 50ms 超时」，
   再 `PeekMessageW(.., PM_REMOVE)` 逐条派发；有消息立即醒，无消息最多等 50ms（肉眼不可感）。
   配套：注册失败**必须回投 UI**（`AppEvt::HotkeyReport` → 设置窗口红字/托盘提示）。
   失败原来只写 `eprintln` + 日志，用户看不到，只会以为程序坏了。

14. **WPF 数据要「装了就能看到」，不能只留一条命令行。**
   首启自动导入在 `wpf_import.rs`：`candidates()` 按优先级找源库
   （`%LocalAppData%\ClipboardX\clipboard_history.db` 安装模式 → 同级 `Data/` → `../clipboard/Data/`），
   跑完写标记 `Data/.wpf-import.json`，此后不再自动跑；手动重跑仍是 `--import-wpf <db>`。
   两个硬要求：**分批读**（`wpf::BatchReader`，行数 + blob 字节双限，源库实测 197MB/7002 条，
   整读会顶穿 30MB 内存线）与**批间让位**（`import_batch` 走 store 单线程 channel，
   连批占满会让 UI 查询排队卡顿）。`ocr_text` 必须在迁移时带走（WPF 已做过 OCR，
    丢了等于让用户重做一遍且结果不一定复现）。

15. **吞键的不变量：吞了 keydown 就必须吞对应的 keyup；armed/combo 类会话态必须随显隐切换清零。**
   用户报告（2026-10-09）：「弹窗 Shift+方向键多选后，回车/其他按键全部失效，没法粘贴检索」。
   根因与热键是 **Alt+\`** 直接相关：隐藏时弹窗仍可见，Alt↓ 先到 → `handle_alt_down` 吞掉并置
   `ALT_ARMED`；Alt↑ 到达时弹窗已隐藏 → `handle_alt_up` 有可见性门不消费 → **armed 跨会话残留**。
   下次 Alt+\` 呼出：Alt↓ 正常放行（**系统记下了 Alt 按下**）、Alt↑ 却被残留 armed 吞掉 →
   **系统永远收不到 Alt 的 keyup，`GetAsyncKeyState(VK_MENU)` 卡在按下** → `translate` 里
   `ctrl_or_alt_or_win_down()` 恒真 → 所有普通键返回 None 放行给前台应用（打字不检索、回车不粘贴、
   Esc 关不掉）；唯独精确热键组合（Alt+\` 等）在放行判定**之前**匹配，一切照旧 —— 所以用户能反复
   Alt+\` 呼出/隐藏自救（`pos_debug.log` 留下一串 2~10s 间隔的 show 打转记录），且每次隐藏又武装
   一次、下次呼出再次卡死，**自循环**。修复：`set_visible` 显隐切换时 `clear_alt_tap_state()`。
   教训：**「单击修饰键」类 armed/combo 状态是会话态，必须随可见性切换清零**，否则下一次会话会拿
   旧状态吞一个「系统见过 keydown」的 keyup，打破「吞 down 必吞 up」的不变量。
   配套保险网：`heal_stuck_modifiers()`（`show_popup` + 30s 看门狗调用）双向核对「系统物理态 vs
   钩子记账位」——不一致就注入 keyup / 清记账位。`reinstall()` 的毫秒级无钩间隙同样会丢修饰键
   keyup（记账位或系统态单边卡住），症状同上，靠这个自愈兜底。**绝不在钩子回调里跑**（SendInput
   会顶 300ms 硬超时）。

16. **跨位数没有捷径：x64 进程给 32 位宿主装不了钩子，注入器取 `LoadLibraryW`
    地址也必须换 32 位 kernel32 的基址。**
    企业微信（`WXWork.exe`，32 位）、WPS、腾讯会议（Roombox / promecefpluginhost）、
    WeMail 这些宿主占了国产桌面软件的大半：x64 的 clipx 直接调
    `SetWindowsHookEx(WH_GETMESSAGE, …)` 装它们身上**必然失败**（位数必须一致）。
    同一批坑还有远程调用：`GetProcAddress(GetModuleHandleW("kernel32.dll"), "LoadLibraryW")`
    拿到的是**本进程 64 位** kernel32 的地址，投进 32 位进程会跳到无效地址、
    远线程静默退出 0 —— `browse_object_inject: LoadLibraryW 远端失败 ret=0` 拖了很久
    就是这个（不是 DLL 缺依赖）。
    正确做法两条，缺一不可：
    - **注入**：`%SystemRoot%\SysWOW64\kernel32.dll` 的 `LoadLibraryW` 导出 RVA
      + 目标进程里 32 位 kernel32 基址（`EnumProcessModulesEx(..., LIST_MODULES_32BIT)`）。
    - **装钩子**：把 **x86 版信标 DLL** 注入宿主，由导出 `ClipxBeaconStart` 在宿主
      进程内给所有线程装 `WH_GETMESSAGE` 钩子。
      **真正的坑在「谁去装」而不是 `hMod`**：`CreateRemoteThread` 创建出来的线程
      没有窗口、没有 desktop 关联（`tagTHREADINFO` 里没有 `pDeskInfo`），在它上面
      调 `SetWindowsHookEx` 会**返回成功句柄、但目标线程一条回调都收不到**
      （实测 `installed=54` 而零 `hook entered`）。同一个函数改从宿主主线程调用
      立刻正常 —— 所以 DLL 里必须**另起一条钩子线程**：先
      `CreateWindowExW(HWND_MESSAGE)` 建 message-only 窗口拿到 desktop 关联，
      再装钩子，最后跑消息循环保持存活。`hMod` 传本 DLL 的 `HINSTANCE` 即可
      （传 NULL 反而不触发，与部分文档描述不符，以实测为准）。
    产物：`clipx_caret_beacon.dll`（x64）+ `clipx_caret_beacon32.dll`（x86），
    构建 `scripts\build_beacon.ps1`，注册（路线甲）两份分别落
    `HKLM\SOFTWARE\Classes\CLSID\{…}\InProcServer32` 与
    `HKLM\SOFTWARE\WOW6432Node\Classes\CLSID\{…}\InProcServer32`。

17. **「候选点排序」和「失败记账」是两件事：真值永远先于缓存，退化不等于失败。**
    两条都是 v0.10.12 上线后踩出来的，症状都像「定位不准」，根因完全不同：
    - **缓存抢跑**：30s 复用插入点的快速路径排在信标之前 —— 好处是绕开慢链，
      代价是 TTL 内光标一移动，窗口就钉在老位置（用户原话「只有第一下是对的，
      后面位置都不会变了」）。信标是像素级真值、warm 路径只要一次请求/应答
      （几毫秒），**任何时候都该排在启发式与缓存之前**；缓存只服务「信标拿不到
      的宿主」。
    - **退化误判**：`GetTextExt` 返回高度 0 的矩形（焦点在会话列表 / 图片预览时
      必然如此）说明「宿主此刻没有 TSF 文档」，不是「信标坏了」。这段路径若也
      记失败，连续两次（原阈值 2）就把整个 pid 拉黑 60s，期间点回输入框直接
      `skip (blacklisted)`、只吃 gui 假点 —— 而 Qt 自绘宿主（企业微信）的
      `rcCaret` 恒在输入框左侧、不随光标移动，症状就是「偶尔首次位置不对，
      而且是连续一阵子全错」。**记账按「有没有应答」分**：应答了 = 链路通，
      退化只是环境问题，记 `beacon_note_ok`；只有连应答都没有的真超时才计数，
      冷却也别开太长（6s 级，别 60s）。
    - 排查工具：`clipx --caret-probe` 并列打印各分支原始候选点与 `usable`，
      配合 `Data/pos_debug.log` 的 `branch=` 字段，能一眼看出是哪条链短路。

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

**自检一律加 `--no-instance-lock`**，不必再折腾运行中的实例（以前得先杀掉桌面上那个
日用实例，提权实例还杀不动）。自检只渲染窗口、不改业务数据：

```bash
# 右下角提示条：正常态（带进度条）/ 失败态
clipx.exe --no-instance-lock --toast-demo --snapshot C:/tmp/toast.png
clipx.exe --no-instance-lock --toast-demo-error --snapshot C:/tmp/toast_err.png
# 设置窗口指定页（顺序：剪贴板 0 · 记录与检索 1 · 常规 2 · 文件夹跳转 3 · 高级 4 · 关于 5）
clipx.exe --no-instance-lock --settings-page 2 --snapshot C:/tmp/general.png
clipx.exe --no-instance-lock --settings-page 5 --snapshot C:/tmp/about.png
```

仍要注意：`Data/settings.json` 的 `run_as_admin` 若为 `true`，**提权重启会丢掉命令行参数**，
进程静默转成后台实例、快照不执行；自检前临时置 `false` 并记得还原。
确实需要杀掉残留实例时（新代码要跑常驻流程、单实例锁必须清干净）：若实例是提权启动的，
`taskkill /F` 会"拒绝访问"，而 PowerShell 的 `Invoke-CimMethod ... Terminate`（WMI）
可能被沙箱安全策略拦；可退回 Python + ctypes：
`k.OpenProcess(1, False, PID)` 拿句柄 → `k.TerminateProcess(h, 1)` 返回 1 即成功。

改完自检要还原 `settings.json`（`cp settings.json.uibak settings.json`）。

## 剪贴板写入与检索陷阱（血泪，务必先读）

### 写剪贴板：必须「单周期原子写 + 真实等待重试」

唯一正确的文本写入路径是 `clipx-app/src/paste.rs::write_clipboard_atomic`。

- **不要用 clipboard-rs 的 `clear()` + `set_text()`**：那是两次独立 `OpenClipboard` 周期。
  clear 成功而 set 失败时，剪贴板会被留成**空的** —— 用户按 Ctrl+V 粘出空内容。
- **它的重试是假的**：clipboard-win 的 `new_attempts(10)` 每次失败只 `Sleep(0)`
  （让出时间片、**不等待**），争抢下 10 次重试在微秒内跑完。WPF 老版用的 WinForms
  `Clipboard.SetText` 内部是 **10 次 × 100ms 真实等待** —— 「同一个目标应用，老版贴得上、
  clipx 时好时坏」的差异就在这里。**看到 `new_attempts` 别当成有重试。**
- 正确做法：一次 `OpenClipboard` 内 `EmptyClipboard` + 写完所有格式，失败 `sleep(15ms)`
  重试 20 次（≈300ms 上限）。图片路径（`write_image_native`）本来就是这个写法，文本路径当初漏了。
- **Electron 目标（Cursor / VS Code / WorkBuddy）格外容易撞上**：它们的粘贴走异步 IPC，
  读剪贴板的时刻会落在我们「松开粘贴键即写回」之后，两个进程的 OpenClipboard 重叠概率远高于
  原生应用（原生应用在按键同步阶段就读完了）。
- **失败必须留痕**：写剪贴板失败曾经是静默 `return`（批量推进则静默回滚队列），用户只能凭体感
  描述「时好时坏」。现在单条失败给可见提示、批量推进失败写 `Data/batch_paste.log`。
- ⚠️ **工具宿主的沙箱访问不了剪贴板**：沙箱内进程 `OpenClipboard` 直接返回
  `ERROR_ACCESS_DENIED(5)`，连 PowerShell 的 `Get-Clipboard` 都报「所请求的剪贴板操作失败」，
  且 `GetOpenClipboardWindow()` 为空 —— 很容易误判成「有进程泄漏了剪贴板」。
  `write_text_survives_contention` 这类测例必须在**能访问剪贴板的宿主机**上跑。

### 批量推进的触发：必须「按下武装、松开消费」，修饰键只信自记账位

- 批量队列靠监听目标应用里的 Ctrl+V / Shift+Insert **松键**来推进。**不要在松键那一刻现读物理
  键态判修饰键**：用户把 Ctrl 比 V 先松开（连着快按时很常见，先后由硬件顺序决定）就丢掉一次推进
  —— 队列不动、剪贴板还是上一条，用户看到的就是「批量粘贴时好时坏」。
  正确姿势：`keyboard_hook.rs::paste_advance_arms` 在 **KEYDOWN** 判定并置 `PASTE_ARMED`，
  KEYUP 只看武装位（且**无论本次是否推进都要 `swap(false)` 清掉**，免留到下一次无关松键）。
- **修饰键一律取自记账位**（`CTRL_HELD` / `ALT_HELD` / `SHIFT_HELD`），别读 `GetAsyncKeyState` ——
  被钩子吞掉的键不进系统输入队列，物理键态会停在过期值（文件头那三个静态量的注释记着同源的实测
  bug）。这里尤其**不能**写成「物理态 OR 自记账」：物理态一旦卡在 down，用户在输入框里打一个 `v`
  就会被当成 Ctrl+V 推进队列。
- **按下时不要额外要求面板已隐藏**（KEYUP 那一侧仍然要求）：旧实现只在 KEYUP 判可见性，
  等于「按下时面板还在、松开前已隐藏」也算数 —— 放宽到同等宽松，**漏一次推进**（用户报的正是
  「不生效」）比多一次推进严重得多。范围只到 `V` / `Insert`（`is_paste_key`）；
  `Ctrl+Shift+Insert` 留给系统。

### 终端判定：别按进程名把 Electron 编辑器算进去

- `is_terminal_process_name` 里**不要**放 `cursor` / `code`：Electron 编辑器的集成终端画在**主窗口**
  里（没有独立 HWND），按进程名判等于把「编辑器 / 对话输入框」一起判成终端 —— 用户配置的 Ctrl+V
  被换成 Shift+Insert、文本还被去 CR。WPF 老版的 `PasteTargetHeuristics` 也没这两个
  （`9ae9c09` 顺手加的，v0.10.9 收回）。
- 依据：VS Code 官方文档 —— **Windows 下集成终端的复制粘贴就是 Ctrl+C / Ctrl+V**（只有 Linux 是
  Ctrl+Shift+V），加它既没必要也有害。真实终端（类名 `ConsoleWindowClass` / `CASCADIA_*`，
  进程 `cmd` / `pwsh` / `conhost` / `mintty` / `wezterm-gui` 等）照旧保留。
- 代价：Cursor 内嵌 WSL/Linux PTY 里贴多行不再自动去 CR（可能显示 `^M`）。要从 HWND 区分「编辑器」
  与「集成终端」本来就不可能；真需要就另加按应用（或按焦点元素）的强制终端规则。
- 动这张表**必须**同步 `terminal_class_and_process` 测例，否则下一个改动者不知道哪些是有意为之。

### 检索：拼音列不能和图片 BLOB 同表

- `payloads` 与 `image_blob` 同居（本机 63MB / 7228 条）。`pinyin_blob LIKE '%词%'` 是前导通配
  全表扫，每行都要跨溢出页取记录 → **实测 73~95ms**，且随图片条目增多线性劣化。
- 把同一份串放进只含两列的窄表 `payload_search` 后，**同样扫描 2~3ms**（同机同数据实测）。
- **写入方不需要改**：`payloads` 上挂三个触发器（INSERT / `UPDATE OF pinyin_blob` / DELETE）自动同步。
  迁移回填走 `INSERT ... SELECT` 写窄表自身，不会反过来触发 payloads 的触发器，所以不会写两遍。
- **深搜**（`full_text` / `ocr_text`）仍要回 `payloads`，所以只有 `deep = true` 时才 JOIN 它。
- 改检索后**必须**用同一组关键词比对优化前后的命中行数（本机实测 46/12/200/192/200 逐条一致）。
- **迁移不要无脑重建 FTS**：FTS 重建 + 全量重算拼音只在 v1/v2 → v3（加 `pinyin_blob` 列）那次需要。
  之后的版本跳过 —— 否则每次升级都在 `Store::open` 的**同步路径**上白付秒级代价（111MB 库），
  用户看到的是「启动卡住」。
- `payload_search` 的行数应与 `entries` 一致（触发器漏挂/漏回填会静默少行 → 检索莫名丢结果，
  但**预览/高亮仍正常**，因为那条路走内存侧 `text_matches_query`）。

## 数据位置约定

- 便携模式（默认）：exe 同级 `Data/`（clipx.db + settings.json）
- 安装模式：%LocalAppData%\clipx（Win）/ ~/Library/Application Support/clipx（mac）/ ~/.local/share/clipx（Linux）
- WPF 版历史迁移：**首次启动自动导入**（装配好就能看到老数据）——候选源库见
  `wpf_import::candidates`：`%LocalAppData%\ClipboardX\clipboard_history.db`（WPF 安装模式，本机实测位置）
  → 同级 `Data/clipboard_history.db`（便携对便携）→ `../clipboard/Data/clipboard_history.db`（开发机）。
  成功后写标记 `Data/.wpf-import.json`，此后不再自动跑；手动重跑 `clipx --import-wpf <clipboard_history.db>`。
  字段映射见 ARCHITECTURE §4。
