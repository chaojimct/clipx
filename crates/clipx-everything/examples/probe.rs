//! 手工验证全盘检索链路：`cargo run -p clipx-everything --example probe -- skills`
//!
//! 打印每次查询耗时 / 命中数 / 当前选中的 IPC 布局，用来区分「FindX 掉线」
//! 「Everything IPC 超时」和「表达式写错」三种失败。
//!
//! 默认走 `Engine::Everything`（与设置默认值一致），加 `--findx` 切到 FindX，
//! 加 `--folder` 用 `folder:"q"`（选文件夹意图的表达式）。

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let engine = if args.iter().any(|a| a == "--findx") {
        args.retain(|a| a != "--findx");
        clipx_everything::Engine::FindX
    } else {
        clipx_everything::Engine::Everything
    };
    let folder_only = if args.iter().any(|a| a == "--folder") {
        args.retain(|a| a != "--folder");
        true
    } else {
        false
    };
    let queries: Vec<String> = if args.is_empty() {
        vec!["skills".to_string(), "declar".to_string()]
    } else {
        args
    };
    for q in queries {
        let expr = if folder_only && engine == clipx_everything::Engine::Everything {
            clipx_everything::search::build_folder_search(&q)
        } else {
            q.clone()
        };
        let t0 = std::time::Instant::now();
        match clipx_everything::query_engine(
            engine,
            &expr,
            20,
            std::time::Duration::from_millis(1500),
        ) {
            Ok(r) => {
                println!(
                    "{q:?} (expr={expr:?}): {:?} items={} total={} layout={}",
                    t0.elapsed(),
                    r.items.len(),
                    r.total_items,
                    clipx_everything::debug_layout()
                );
                for it in r.items.iter().take(3) {
                    println!("    {}{}", if it.is_folder { "[D] " } else { "[F] " }, it.full_path);
                }
            }
            Err(e) => println!(
                "{q:?}: ERR {e:?} ({e}) layout={} elapsed={:?}",
                clipx_everything::debug_layout(),
                t0.elapsed()
            ),
        }
    }
}
