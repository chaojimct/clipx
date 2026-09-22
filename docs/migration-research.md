# 老版（WPF ClipboardX）用户丝滑迁移研究

> 2026-09-22 · 小满 · 结论基于本机源码（`../clipboard`）、`gh api` 实测与 clipx 现网代码
> 对象：老版 ClipboardX（WPF，`chaojimct/clipboardx`，最新 v1.9.9）→ clipx（`chaojimct/clipx`，v0.10.8）

## 1. 结论速览

- **「老代码直接更新版本过来」技术上可行**：老版的自动更新通道可以承载一个「迁移版」包，用户点更新、重启后就落到我们的迁移向导上，零学习成本。
- 但有**硬约束**：包名必须匹配老更新器的资产规则（含两个运行形态变体），包内根目录 exe 名必须与原 exe 同名，否则老更新器会拒绝安装。
- **纯 clipx 侧不够**：clipx 无法通知老用户「有个新东西」，所以至少要在老仓库发一次版本。
- **推荐组合**：老仓库发「迁移版 launcher」（路径 A）＋ clipx 侧三处加固（路径 C）。低成本备选是路径 B（老版只加提醒）。

## 2. 现状盘点

### 2.1 老版 WPF（v1.9.9，2026-09-02）

| 项 | 事实 | 出处 |
|---|---|---|
| 更新源 | `api.github.com/repos/chaojimct/clipboardx/releases/latest` | `Services/AppInfo.cs` `GitHubUrl` |
| 触发 | 启动 45s 后静默检查（`CheckUpdatesOnStartup`）→ 托盘气球 12s「发现新版本」；托盘右键「检查更新…」手动 | `App.xaml.cs` 180-232 |
| 去重 | `LastStartupUpdateNotifiedTag` 记录已提示 tag | 同上 |
| 资产命名 | `ClipboardX-<v>-win-x64-{self-contained,no-runtime}.zip`；另有 `ClipboardX-clipboard-*` / `ClipboardX-filejump-*` flavor | `gh api` v1.9.9 assets |
| 选包规则 | 按**当前运行 exe 名前缀**匹配 + 运行形态（CoreCLR 来自 dotnet shared → 优先 no-runtime） | `GitHubUpdateService.PickZipAsset/PreferNoRuntimeZip` |
| 安装动作 | 写 ps1 → 等主进程退出（≤15s 后强杀）→ **按目录递归覆盖 Copy-Item** → `Start-Process <installDir>\<当前 exe 名>` → 清理临时目录 | `LaunchDeferredReplaceAndRestart` |
| 安装形态 | per-user `%LocalAppData%\Programs\ClipboardX`；**数据根 = `%LocalAppData%\ClipboardX`**（`clipboard_history.db` + `settings.json`）；便携形态数据在 exe 同级 `Data\` | `Install/PerUserInstall.cs`、`Services/AppPaths.cs` |
| 自启 | `HKCU\...\Run` 值名 `ClipboardX`（legacy `ClipboardManager`）；**管理员模式改用计划任务 `ClipboardX_AutoStart`** | `Services/StartupRegistration.cs` |
| 单实例互斥体 | `ClipboardX_F7A2E9B0`（clipx 可用来检测「老版在跑」） | `AppPaths.MutexName` |
| 卸载 | `ClipboardX.exe --uninstall`；向导询问是否删数据，**选「是」会递归删 `%LocalAppData%\ClipboardX`（历史库所在）** | `PerUserInstall.cs` 121/146/155 |
| 默认热键 | Ctrl+`（FileJump Ctrl+G、批量切换 Alt+/） | `Models/AppSettings.cs` 21-26 |

### 2.2 clipx（v0.10.8）

**已有**：
- 首启自动导入 WPF 历史（`wpf_import.rs`）：候选 `%LocalAppData%\ClipboardX\clipboard_history.db` → 同目录 `Data\` → 便携/开发布局；`.wpf-import.json` 标记幂等；分批（200 行/24MB）防峰值；**带走 ocr_text**；容量跟随（MaxItems/MaxImageItems 只抬不降）；完成/失败都发 Toast；手动入口 `clipx --import-wpf <db>`。
- 安装器（Inno，`scripts/clipx.iss`）：独立 AppId、装 `%LocalAppData%\clipx`、Run 值名 `clipx`、`PrivilegesRequired=lowest`（无 UAC）、自身升级走同 AppId。

**缺**：
- 老版安装/运行/自启项检测（源码搜不到任何相关判定）。
- 一键停用/卸载老版。
- 设置迁移：老版 83 个设置项里，只跟随 `max_items`/`max_image_items`；热键、主题、排除、OCR 语言、批量模式等全部回落 clipx 默认值（`settings.rs::migrate_legacy` 处理的是 clipx 自己旧 JSON，不是 WPF 文件）。
- 用户可见的迁移指引（README/PRD 只有一句话带过）。

## 3. 四个真问题（为什么「直接装 clipx」不丝滑）

1. **用户不知道要装**：老版更新通道查到 releases/latest = v1.9.9 = 自己，只会说「已是最新」。没有触达手段。
2. **双份运行**：装完 clipx 老版仍自启 → 两个剪贴板监听、两个托盘、两份入库；热键撞（用户若把 clipx 热键改成 Ctrl+` 会 RegisterHotKey 失败）；老版继续写老库，而 clipx **只导入一次** → 数据分叉。
3. **设置不跟**：用户要重设热键/主题/容量等。
4. **卸载顺序陷阱**：老版卸载向导「是否删除配置与历史」选「是」会删掉历史库。必须**先让 clipx 导入完，再卸载老版且选「否」**。

## 4. 三条路径

### 路径 A：老仓库发「迁移版 launcher」（推荐主力）

在老仓库发 v1.9.10，资产是同名 zip（内容换成 launcher）。launcher 流程：

1. 检测老版运行（互斥体 `ClipboardX_F7A2E9B0` / 进程）→ 请求关闭；
2. 弹迁移说明（品牌切换 + 一键）；
3. 下载（或内置）`clipx-<v>-setup.exe` → 静默安装 `/SILENT /SUPPRESSMSGBOXES`（clipx 安装器 `PrivilegesRequired=lowest` → 无 UAC）；
4. clipx 首启自动导入历史（现有能力）+ 选「保留」语义；
5. 清理老版自启：删 Run 值 `ClipboardX`/`ClipboardManager`、删计划任务 `ClipboardX_AutoStart`、删卸载注册表项 `HKCU\...\Uninstall\ClipboardX`；
6. 退出后自删安装目录（ps1 延迟删除，复用老版同款手法）。

**硬约束（不满足会被老更新器拒绝）**：
- zip 名必须是 `ClipboardX-<v>-win-x64-self-contained.zip` **和** `...-no-runtime.zip` 两个变体（老用户两种运行形态都有）；filejump/clipboard 精简 flavor 用户还需对应前缀包；
- zip 内根目录必须有 `ClipboardX.exe`（或对应该 flavor 的 exe 名），否则 `Start-Process` 找不到；
- 版本 tag 必须**大于** v1.9.9（老更新器 `IsRemoteNewerThanCurrent` 比较）。

**风险**：覆盖式替换不删旧文件（launcher 需自清理）；老版可能仍被杀软/用户干扰；launcher 自身要极小（否则下载慢）。

### 路径 B：老版发「只提醒」小版本（低成本备选）

v1.9.10 只加：启动气球「clipx 已发布，点此迁移」+ 托盘菜单项打开迁移说明页。用户手动下载 clipx setup。改动小、风险低，但体验半自动（用户要自己完成导入/卸载顺序）。

### 路径 C：clipx 侧加固（A/B 都必须做）

1. **首启检测与提示**：检测三处信号（`%LocalAppData%\Programs\ClipboardX` 存在 / 互斥体在跑 / Run 值存在）→ 托盘提示「检测到 WPF 版仍在运行，迁移完成后建议停用」。
2. **一键停用老版**：按钮清 Run 值 + 计划任务（不做卸载，避免误删数据；卸载引导用户走老卸载器并明确选「否」）。
3. **导入完成后的引导**：Toast 追加「老版可卸载（卸载时选『否』保留数据，历史已导入）」；可选「安全卸载」按钮（先备份 db 再调 `ClipboardX.exe --uninstall`）。
4. **设置迁移增强（可选）**：至少映射热键、主题、容量；老版 settings.json 是 PascalCase，需显式映射表。
5. **文档**：README 加「从 WPF 版迁移」小节（步骤 + 卸载选「否」的红字警告）。

## 5. 建议落地顺序

1. 路径 C 的 1/2/3（无外部依赖，半天到一天，立刻降低「双份运行」事故率）；
2. 路径 A 的 launcher（独立小项目，产出单 exe；可复用 clipx 的下载/解压逻辑）；
3. 老仓库发 v1.9.10（**先自机演练**：v1.9.9 安装态 + 便携态各一遍）；
4. README/迁移说明 + 群里通告。

## 6. 验收清单

- [ ] v1.9.9 用户收到更新气球 → 更新 → 重启后是迁移向导（不是老界面）
- [ ] 一键迁移后：历史条目数/图片/OCR 与老库一致；容量跟随；`.wpf-import.json` 生成
- [ ] 老版自启项（Run / 计划任务）消失，clipx 自启生效，热键可用
- [ ] 重复执行迁移不重复导入（幂等）
- [ ] 网络失败可重试；用户取消时**老版数据与安装完整无损**
- [ ] 便携态老用户（手动覆盖 zip）路径同样可用

## 7. 事实 / 推断分界

- **已核实**：老版更新链全流程与选包规则、资产命名、安装/数据/自启/互斥体/卸载删数据、默认热键；clipx 导入现状与安装器参数；两仓库 release 现状（v1.9.9 / v0.10.8）。
- **强推断（未实测）**：迁移包能被老更新器完整接受；launcher 静默安装 clipx 无 UAC（依据 iss `PrivilegesRequired=lowest`）；老版 `--uninstall` 在 launcher 场景不必调用。
- **需一次真机演练**才能转「已核实」：建议先在自己机器的一份 v1.9.9 便携副本上跑迁移版。
