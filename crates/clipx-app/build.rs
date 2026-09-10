use std::path::PathBuf;
use std::process::Command;

fn main() {
    // 单一编译入口：popup.slint 再导出 quickfind.slint 的窗口。
    // slint_build::compile 每次调用都会覆盖 SLINT_INCLUDE_GENERATED，
    // 多次编译只有最后一个文件会被 include_modules! 引入。
    slint_build::compile("ui/popup.slint").expect("failed to compile slint ui");
    stage_shell_navigate_dlls();
    embed_app_icon();
}

/// exe / 任务栏 / 资源管理器图标：沿用 WPF 版 assets/clipboard.ico。
/// 用 Windows SDK 的 rc.exe 编译 app.rc，产物交给 MSVC 链接器（link 直接吃 .res）。
fn embed_app_icon() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let rc_src = manifest.join("app.rc");
    println!("cargo:rerun-if-changed={}", rc_src.display());
    println!("cargo:rerun-if-changed={}", manifest.join("assets/clipboard.ico").display());

    let Some(rc) = find_rc_exe() else {
        println!("cargo:warning=未找到 rc.exe，跳过图标嵌入（不影响运行）");
        return;
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let res = out.join("clipx_icon.res");
    let ok = Command::new(&rc)
        .args(["/nologo", "/fo"])
        .arg(&res)
        .arg(&rc_src)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok && res.is_file() {
        println!("cargo:rustc-link-arg={}", res.display());
    } else {
        println!("cargo:warning=rc.exe 编译 app.rc 失败，跳过图标嵌入");
    }
}

/// Windows SDK 10 的 x64 rc.exe（取版本号最高的 SDK）。
fn find_rc_exe() -> Option<PathBuf> {
    let root = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut best: Option<(String, PathBuf)> = None;
    for ver in std::fs::read_dir(&root).ok()?.flatten() {
        let p = ver.path();
        if !p.is_dir() {
            continue;
        }
        let cand = p.join("x64").join("rc.exe");
        if !cand.is_file() {
            continue;
        }
        let name = ver.file_name().to_string_lossy().to_string();
        if best.as_ref().is_none_or(|(n, _)| name > *n) {
            best = Some((name, cand));
        }
    }
    best.map(|(_, p)| p)
}

/// FileJump 原生跳转依赖与 exe 同目录的 ShellNavigate DLL。
/// debug 构建以前从不拷贝，注入失败后只能 Alt+D 地址栏。
fn stage_shell_navigate_dlls() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let Some(dest) = out.ancestors().nth(3) else {
        return;
    };
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let native = manifest
        .join("../../../clipboard/native/ShellNavigate/bin");
    let copies = [
        (
            native.join("x64/Release/ClipboardXShellNavigate.dll"),
            dest.join("ClipboardXShellNavigate.dll"),
        ),
        (
            native.join("Win32/Release/ClipboardXShellNavigate32.dll"),
            dest.join("ClipboardXShellNavigate32.dll"),
        ),
    ];
    for (src, dst) in copies {
        println!("cargo:rerun-if-changed={}", src.display());
        if src.is_file() {
            let _ = std::fs::copy(&src, &dst);
        }
    }
}
