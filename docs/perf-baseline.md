# clipx 性能基线（v0.10.26，2026-11-24）

来源：`cargo run --release -p clipx-store --example perf_suite`
（`ROUNDS=9` 九轮中位数；`SCALES=1000,5000,20000`；本机 Windows 沙箱）。
入口脚本：`scripts/perf.ps1`。对应 `docs/listary-reference.md` §5.6。

## 搜索耗时 vs 历史条数（limit=200，真实 channel 链路）

| 查询 | 1k 中位数/P99 | 5k 中位数/P99 | 20k 中位数/P99 | 命中 |
|---|---|---|---|---|
| 空查（最近列表） | 0.3 / 1.5ms | 0.3 / 0.7ms | 0.3 / 0.6ms | 200 |
| 原文 Seed（全命中） | 0.5 / 0.7ms | 0.5 / 0.8ms | 0.5 / 1.0ms | 200 |
| 原文 fox（全命中） | 0.5 / 0.5ms | 0.5 / 2.8ms | 0.6 / 1.2ms | 200 |
| 中文 剪贴板（全命中） | 0.3 / 0.4ms | 0.5 / 0.7ms | 0.5 / 1.2ms | 200 |
| 拼音 jian（全命中） | 0.4 / 0.8ms | 0.5 / 0.8ms | 0.4 / 0.5ms | 200 |
| 空格AND ai edu（零命中，全表扫） | 1.1 / 1.8ms | 5.6 / 9.4ms | 20.9 / 28.1ms | 0 |
| 零命中 zhuang（全表扫） | 1.1 / 1.4ms | 4.8 / 5.8ms | 20.5 / 29.1ms | 0 |
| 深搜FTS lazy | 0.5 / 0.7ms | 0.4 / 1.1ms | 0.6 / 0.8ms | 200 |
| 行内 Seed | 0.2 / 0.6ms | 0.2 / 0.2ms | 0.3 / 0.4ms | 8 |

结论：

- 命中类查询被 `LIMIT 200` 提前截断，1k→20k 几乎零增长（窄表 `payload_search` + 预排索引的功劳）。
- 零命中查询走全表扫，随规模线性涨，20k 下 ~20–30ms，仍远低于 PRD 万条 <100ms 线。
- 融合排序的 SQL 预排（`pinned/use/created`）靠 `idx_entries_fusion` 复合索引走
  索引顺序扫描 + LIMIT 早停。教训：`ORDER BY` 里包 `COALESCE()` 会让规划器认不出
  索引列——首次上线时全命中查询 20k 下 0.3ms→43ms，换裸列 + 复合索引后回到 0.5ms。
  列是 `NOT NULL` 的（schema + 迁移保证），裸列安全。
  顺带修了行内查询的同款 filesort（20k 下 16.7ms→0.3ms）。
- 行内 `search_inline` 同理走索引，20k 下 0.3ms——第 5 步（增量/取消/虚拟化）的
  压力点只剩零命中全扫。

## 启动 / 体积 / 内存

| 规模 | open（空库） | seed 耗时 | reopen | 库文件 | RSS |
|---|---|---|---|---|---|
| 1k | 9.5ms | 96.5ms | 16.2ms | 704KB | 10.9MB |
| 5k | 8.2ms | 1132ms | 31.8ms | 3.3MB | 11.6MB |
| 20k | 8.2ms | 17774ms | 15.3ms | 13.4MB | 11.7MB |

结论：open/reopen 与规模无关（8–30ms）；常驻 RSS 只涨到 11.7MB（测试进程含 harness 自身）。

⚠️ 待查：`seed`（测试专用 upsert 路径）耗时超线性（1k→0.1s，5k→1.1s，20k→17.8s），
疑似逐条去重 `SELECT ... WHERE full_text = ?` 无索引导致 O(n²)。
M3 曾报"seed 10000 条 162ms"，与本次矛盾——是回归还是口径不同（生产 `ImportBatch`
批量路径 vs 测试 `Seed` 路径），留待 §5.7 第 5 步用本 harness 对照。
生产迁移走 `ImportBatch`，不在此结论射程内。

## 弹窗逐键（30 条中英混合语料 × 判定+高亮，纯逻辑）

| 输入 | 逐键 P50 | 逐键 P99 | 逐键最大 |
|---|---|---|---|
| `qingsong`（8 键） | 0.024ms | 0.029ms | 0.029ms |
| `ai edu`（6 键） | 0.027ms | 0.049ms | 0.049ms |

结论：逐键纯逻辑成本可忽略（<0.05ms），弹窗卡顿的锅不在匹配算法，在 DB 查询 + 渲染。
`text_matches_query` 的逐次 `to_lowercase` + 重算 blob（listary #6）确实存在，
但 30 条语料下连 0.1ms 都不到——优化它之前先 profile，大概率不值得。

## 粘贴端到端

MANUAL：真剪贴板不可在 harness 里触碰；按 ROADMAP 遗留 #21 真机走。
