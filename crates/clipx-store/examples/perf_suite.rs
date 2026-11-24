//! clipx 性能基线：`cargo run --release -p clipx-store --example perf_suite`
//!
//! 对应 `docs/listary-reference.md` §5.6。输出一份可直接贴进
//! `docs/perf-baseline.md` 的实测表，覆盖：
//!   1. 启动：`Store::open`（reopen，OS-cached）+ 库文件大小
//!   2. 搜索耗时 vs 历史条数：空查/原文子串/中文子串/拼音/空格 AND/深搜(FTS)/行内，
//!      每档 N 轮取中位数（走真实 channel 调度链路，limit=200 对齐弹窗页大小）
//!   3. 弹窗逐键：query 逐字加长，对内存语料跑 `text_matches_query` +
//!      `pinyin_hit_spans`（即弹窗每键过滤+高亮的纯逻辑热路径，不含渲染）
//!   4. 常驻内存 RSS（Windows 读 WorkingSet，其他平台读 /proc）
//!   5. 粘贴端到端：只 MANUAL（真剪贴板不可在 harness 里碰；见 ROADMAP 遗留 #21）
//!
//! 环境变量：`SCALES="1000,5000,20000"`，`ROUNDS=9`（findx 对等九轮中位数）。
//! release 下跑才有意义；debug 构建会打印警告。

use std::time::Instant;

use anyhow::Result;
use clipx_core::pinyin::{pinyin_hit_spans, text_matches_query};
use clipx_core::EntryMeta;
use clipx_store::{Store, StoreLimits};

/// 当前进程常驻内存（MB）。
fn rss_mb() -> f64 {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_non_paged_pool_usage: usize,
            quota_non_paged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
        }
        extern "system" {
            fn GetCurrentProcess() -> isize;
            fn K32GetProcessMemoryInfo(
                process: isize,
                counters: *mut ProcessMemoryCounters,
                cb: u32,
            ) -> i32;
        }
        unsafe {
            let mut pmc: ProcessMemoryCounters = std::mem::zeroed();
            pmc.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) != 0 {
                return pmc.working_set_size as f64 / (1024.0 * 1024.0);
            }
        }
        0.0
    }
    #[cfg(not(windows))]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    if let Some(kb) = rest.split_whitespace().next() {
                        return kb.parse::<f64>().unwrap_or(0.0) / 1024.0;
                    }
                }
            }
        }
        0.0
    }
}

/// 跑 N 轮，返回 (中位数ms, P99ms, 最大ms, 末轮命中数)。
fn bench<F: FnMut() -> usize>(rounds: usize, mut f: F) -> (f64, f64, f64, usize) {
    let mut times = Vec::with_capacity(rounds);
    let mut hits = 0;
    for _ in 0..rounds {
        let t = Instant::now();
        hits = f();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = times[times.len() / 2];
    let p99 = times[(times.len() * 99 / 100).min(times.len() - 1)];
    let max = times[times.len() - 1];
    (med, p99, max, hits)
}

/// 逐键模拟语料：中英混合 + 路径 + URL + 代码，对齐真实剪贴板内容分布。
fn keystroke_corpus() -> Vec<&'static str> {
    vec![
        "青松翠柏长青",
        "萍姐的会议纪要",
        "ai-edu-dataset 下载地址",
        "C:\\Users\\mact\\Documents\\季度报告.docx",
        "https://github.com/chaojimct/clipx",
        "The quick brown fox jumps over the lazy dog",
        "SELECT * FROM entries WHERE kind = 0",
        "Fix: 非 Windows 目标编译错误",
        "便携 ZIP 解压即用",
        "任务栏一闪根治",
        "TOTP 顺位键入验证码框",
        "Win+V 同款连打轮选",
        "README.md 待更新",
        "Cargo.lock 同步到 0.10.26",
        "今晚吃什么：黄焖鸡米饭",
        "echo hello world",
        "fn main() { println!(\"hi\"); }",
        "13800138000",
        "clipx-20261124-120000 落到 TEMP",
        "会议纪要 2026-11-24",
        "周报：检索提速 47ms→3ms",
        "密码库 M0–M3 验收通过",
        "ocr_text 回填完成",
        "payload_search 窄表同步",
        "Alt+回车永远全填",
        "无前缀融合置顶两条",
        "Chromium 直填假阴性修复",
        "填充下拉窗贴输入框下沿",
        "焦点感知填充认出账号框",
        "第 3 次联调：拼音多音字",
    ]
}

fn main() -> Result<()> {
    let profile = if cfg!(debug_assertions) {
        "debug（数字仅供参考，基线请用 --release）"
    } else {
        "release"
    };
    let rounds: usize = std::env::var("ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9);
    let scales: Vec<usize> = std::env::var("SCALES")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|p| p.trim().parse().ok())
                .collect()
        })
        .filter(|v: &Vec<usize>| !v.is_empty())
        .unwrap_or_else(|| vec![1000, 5000, 20000]);

    println!("======== clipx 性能基线 ========");
    println!("profile: {profile} · rounds: {rounds}（中位数） · scales: {scales:?}");

    let rss_start = rss_mb();
    println!("启动前进程 RSS: {rss_start:.1} MB");
    println!();

    for n in scales {
        let dir = tempfile::TempDir::new()?;
        let db = dir.path().join("perf.db");
        let limits = StoreLimits {
            max_items: 1_000_000,
            max_image_items: 150,
        };

        // ── 启动：open + seed + reopen ──
        let t0 = Instant::now();
        let store = Store::open(&db, limits)?;
        let open_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t0 = Instant::now();
        let seeded = store.seed(n)?;
        let seed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        drop(store);

        let t0 = Instant::now();
        let store = Store::open(&db, limits)?;
        let reopen_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let file_bytes = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
        let rss_after_open = rss_mb();

        println!("── 库规模 n={n}（seed {seeded} 条）──");
        println!("Store::open（空库） : {open_ms:>8.1} ms");
        println!("seed {n} 条          : {seed_ms:>8.1} ms");
        println!("Store::open（reopen）: {reopen_ms:>8.1} ms");
        println!("库文件               : {file_bytes:>8} B ({:.1} KB)", file_bytes as f64 / 1024.0);
        println!("reopen 后 RSS        : {rss_after_open:.1} MB");
        println!();
        println!("  {:<22} {:>10} {:>10} {:>10} {:>8}", "查询", "中位数", "P99", "最大", "命中");
        // 查询矩阵：(label, query, deep)
        let cases: &[(&str, &str, bool)] = &[
            ("空查（最近列表）", "", false),
            ("原文: Seed（全命中）", "Seed", false),
            ("原文: fox（全命中）", "fox", false),
            ("中文: 剪贴板（全命中）", "剪贴板", false),
            ("拼音: jian（全命中）", "jian", false),
            ("空格AND: ai edu（零命中）", "ai edu", false),
            ("零命中: zhuang", "zhuang", false),
            ("深搜FTS: lazy", "lazy", true),
        ];
        for (label, q, deep) in cases {
            let (med, p99, max, hits) = bench(rounds, || {
                store.search_ex(q, None, None, *deep, 200).len()
            });
            println!("  {label:<22} {med:>8.1}ms {p99:>8.1}ms {max:>8.1}ms {hits:>8}");
        }
        let (med, p99, max, hits) = bench(rounds, || store.search_inline("Seed", 8).len());
        println!("  {:<22} {med:>8.1}ms {p99:>8.1}ms {max:>8.1}ms {hits:>8}", "行内: Seed");
        // 会话增量：逐键扩展，旧路（每键全查）vs 新路（首查+内存过滤）。
        // 等价性断言：内存过滤行数必须等于全查行数（复用判定正确性的实证）。
        // 两条路径：命中路 "jian"（索引早停本来就快）与 miss 路 "zhuangx"
        //（全表扫，会话的主战场；首键扫全表，后续键在空集上 O(1)）。
        for typing in ["jian", "zhuangx"] {
            let chars: Vec<char> = typing.chars().collect();
            let mut old_total = 0.0;
            let mut new_total = 0.0;
            let mut prev: Vec<EntryMeta> = Vec::new();
            for i in 1..=chars.len() {
                let prefix: String = chars[..i].iter().collect();
                let t = Instant::now();
                let full = store.search_ex(&prefix, None, None, false, 200);
                old_total += t.elapsed().as_secs_f64() * 1000.0;
                let t = Instant::now();
                let sess: Vec<EntryMeta> = if i == 1 {
                    store.search_ex(&prefix, None, None, false, 200)
                } else {
                    prev.iter()
                        .filter(|m| text_matches_query(&m.preview, &prefix))
                        .cloned()
                        .collect()
                };
                new_total += t.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(
                    sess.len(),
                    full.len(),
                    "会话过滤必须与全查同行数（prefix={prefix})"
                );
                prev = full;
            }
            let nkeys = chars.len();
            println!(
                "  {:<22} {old_total:>8.1}ms {:>10} {:>10} {:>8}",
                format!("逐键旧路'{typing}'({nkeys}键合计)"),
                "",
                "",
                ""
            );
            println!(
                "  {:<22} {new_total:>8.1}ms {:>10} {:>10} {:>8}",
                format!("逐键新路'{typing}'({nkeys}键合计)"),
                "",
                "",
                ""
            );
        }
        println!();
    }

    // ── 弹窗逐键：query 逐字加长 × 全语料判定+高亮 ──
    let corpus = keystroke_corpus();
    println!("── 弹窗逐键（{} 条语料 × 判定+高亮）──", corpus.len());
    println!("  {:<16} {:>10} {:>10} {:>10}", "已输入", "中位数", "P99", "最大");
    for query in ["qingsong", "ai edu"] {
        let mut per_key: Vec<f64> = Vec::new();
        // 按 Unicode 标量逐字加长，空格同样走真实 Space 分流语义。
        let chars: Vec<char> = query.chars().collect();
        for i in 1..=chars.len() {
            let prefix: String = chars[..i].iter().collect();
            let mut times = Vec::with_capacity(rounds);
            for _ in 0..rounds {
                let t = Instant::now();
                for text in &corpus {
                    if text_matches_query(text, &prefix) {
                        let _ = pinyin_hit_spans(text, &prefix);
                    }
                }
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            per_key.push(times[times.len() / 2]);
            if i == chars.len() {
                let p99 = times[(times.len() * 99 / 100).min(times.len() - 1)];
                let max = times[times.len() - 1];
                println!(
                    "  {:<16} {:>8.3}ms {:>8.3}ms {:>8.3}ms",
                    format!("'{query}' 末键"),
                    times[times.len() / 2],
                    p99,
                    max
                );
            }
        }
        let mut sorted = per_key.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = sorted[sorted.len() / 2];
        let p99 = sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)];
        let max = sorted[sorted.len() - 1];
        println!("  '{query}' 逐键({} 键): P50 {p50:.3}ms · P99 {p99:.3}ms · 最大 {max:.3}ms", chars.len());
    }
    println!();

    // ── 粘贴端到端：MANUAL ──
    println!("── 粘贴端到端 ──");
    println!("  MANUAL：真剪贴板不可在 harness 里触碰；按 ROADMAP 遗留 #21 真机走。");

    let rss_final = rss_mb();
    println!();
    println!("── 汇总 ──");
    println!("结束时进程 RSS: {rss_final:.1} MB（启动前 {rss_start:.1} MB）");
    println!("================================");
    Ok(())
}
