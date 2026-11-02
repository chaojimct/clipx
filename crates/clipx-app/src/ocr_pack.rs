//! OCR 拓展包管理（cargo feature `ocr-rapid` 门控，默认不编译）：
//!
//! - 模型目录：`Data/ocr-models/`（随 Data 走便携/安装双模式）。
//! - 下载清单取 rapidocr-core 注册表（PPOCRV6_SMALL，文件名/直链/SHA256），不自建第二份。
//! - 下载手法复用更新通道（Windows powershell / 其他 curl），SHA256 校验，失败清理半截文件。
//! - 进程内同时只跑一个 ensure；结果经 `AppEvt::PackNotice` 通知栏提示（**不**蹭更新通道）。
//! - 无 feature 构建时本模块不存在，调用方全部 cfg 门控。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use crate::logic::AppEvt;

/// 模型目录（settings.json 同级 `ocr-models`）。
pub fn models_dir(settings_path: &Path) -> PathBuf {
    settings_path
        .parent()
        .unwrap_or(settings_path)
        .join("ocr-models")
}

static ENSURING: AtomicBool = AtomicBool::new(false);

/// 后台确保模型齐备：已齐直接报就绪；缺则逐个下载校验。
/// 切换引擎需重启（引擎在队列线程持有），完成提示含重启指引。
pub fn ensure_async(dir: PathBuf, tx: mpsc::Sender<AppEvt>) {
    if ENSURING.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("clipx-ocr-pack".into())
        .spawn(move || {
            // 就绪无事不打扰；下载完成/失败才提示。
            if let Some((msg, warn)) = ensure_blocking(&dir) {
                let _ = tx.send(AppEvt::PackNotice { text: msg, warn });
            }
            ENSURING.store(false, Ordering::SeqCst);
        });
}

/// 返回 Some((msg, warn)) 表示有事需提示；None = 早就绪、无事发生。
/// `warn = true` 走危险色（失败），否则中性播报。
fn ensure_blocking(dir: &Path) -> Option<(String, bool)> {
    if clipx_ocr::rapid::models_ready(dir) {
        return None;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return Some(("OCR拓展包：模型目录创建失败".to_string(), true));
    }
    let total = clipx_ocr::rapid::required_assets().len();
    let mut done = 0;
    for (name, url, sha) in clipx_ocr::rapid::required_assets() {
        let dest = dir.join(&name);
        if file_ok(&dest, &sha) {
            done += 1;
            continue;
        }
        if !download_file(&url, &dest) || !file_ok(&dest, &sha) {
            let _ = std::fs::remove_file(&dest);
            return Some((
                format!(
                    "OCR拓展包下载失败（{done}/{total}）：{}，稍后重试或检查网络",
                    name.display()
                ),
                true,
            ));
        }
        done += 1;
    }
    Some((
        "OCR拓展包下载完成（约32MB），重启 clipx 生效".to_string(),
        false,
    ))
}

/// 文件存在、非空、SHA256 相符（无期望哈希时只判存在非空）。
fn file_ok(path: &Path, sha: &str) -> bool {
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) == 0 {
        return false;
    }
    if sha.is_empty() {
        return true;
    }
    match file_sha256(path) {
        Some(got) => got.eq_ignore_ascii_case(sha),
        None => false,
    }
}

/// SHA256 走 `bw_pack` 的纯 Rust 实现（曾各存一份子进程版，windows-latest
/// CI 上挂过——子进程在受限环境不可靠；`bw_pack` 常编，跨模块复用无门控纠缠）。
pub(crate) use crate::bw_pack::file_sha256;

fn download_file(url: &str, dest: &Path) -> bool {
    let _ = std::fs::remove_file(dest);
    // 手法与更新通道一致（update.rs），不新增 HTTP 依赖。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Invoke-WebRequest -Uri '{url}' -OutFile '{}' -UseBasicParsing",
                    dest.display()
                ),
            ])
            .creation_flags(0x0800_0000)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && dest.is_file()
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("curl")
            .args(["-sL", "-o"])
            .arg(dest)
            .arg(url)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && dest.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_dir_follows_settings_dir() {
        // 平台无关写法（曾写死 `C:\x\...`，Linux/mac 上 parent 直接没了）。
        let base = std::env::temp_dir().join("clipx-ocr-pack-dir");
        let p = base.join("settings.json");
        assert_eq!(models_dir(&p), base.join("ocr-models"));
    }

    /// 活体：真下载 4 模型（约 32MB）+ `models_ready`（仅手动，对标 bw 活体）。
    /// 跑法：cargo test -p clipx-app ocr_pack::tests::live_download_models -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_download_models() {
        let dir = std::env::temp_dir().join(format!("clipx-ocr-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let msg = ensure_blocking(&dir);
        assert!(
            clipx_ocr::rapid::models_ready(&dir),
            "下载后应就绪：{msg:?}"
        );
        for f in clipx_ocr::rapid::PACK_FILES {
            let len = std::fs::metadata(dir.join(f)).map(|m| m.len()).unwrap_or(0);
            println!("{f}: {len} B");
            assert!(len > 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_ok_rejects_missing_and_empty() {
        let dir = std::env::temp_dir().join("clipx-ocr-pack-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.bin");
        assert!(!file_ok(&f, ""));
        std::fs::write(&f, b"").unwrap();
        assert!(!file_ok(&f, ""));
        std::fs::write(&f, b"abc").unwrap();
        assert!(file_ok(&f, ""));
        // "abc" 的 SHA256（git 空树 well-known 向量之一，非空可验）
        assert!(file_ok(
            &f,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ));
        assert!(!file_ok(&f, "00".repeat(32).as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
