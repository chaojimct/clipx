//! 诊断：对指定 pid 远程加载导航 DLL，验证 32/64 位地址换算。
//! 用法：cargo run --release -p clipx-filejump --example inject_probe -- <pid>
fn main() {
    let pid: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if pid == 0 {
        eprintln!("用法: inject_probe <pid>");
        std::process::exit(2);
    }
    match clipx_filejump::inject::win::remote_load_dll_probe(pid) {
        Ok((is64, code)) => {
            println!(
                "pid={pid} is64={is64} exit_code={code:#X} -> {}",
                if code != 0 { "LoadLibrary OK" } else { "FAILED" }
            );
        }
        Err(e) => {
            println!("pid={pid} error: {e:?}");
            std::process::exit(1);
        }
    }
}
