//! Bitwarden CLI 自动下载（`bw` 内建化，抄 OCR 拓展包模式）。
//!
//! - 为清安装 friction：`Data/bin/` 缺 `bw` 时，插件管理页一键下载官方包。
//! - 版本 pin 死在 [`BW_VERSION`]，SHA256 pin 死在资产表（值取自 GitHub
//!   Release API 的 asset digest，本机实测下载校验一致才入库，见模块底注释）。
//! - 只用 OSS 构建（`bw-oss-*`，全开源；`device-approval` 等用不到的非 OSS
//!   命令本就不需要）。
//! - 下载手法与更新通道一致（Windows `curl.exe`，v0.10.17 起的标准栈），
//!   zip 解包只取 `bw(.exe)`（包内唯一文件，2026.10.1 实测），SHA256 校验，
//!   失败清理半截文件；版本标记文件 `bw.version` 记版本+哈希。
//! - 进程内同时只跑一个 ensure；结果经 `AppEvt::BwReady`（成功）或
//!   `AppEvt::PackNotice`（失败）回投，不蹭更新通道。
//! - SHA256 helper 与 `ocr_pack` 各存一份（见该模块注释：12 行稳定代码，
//!   ocr_pack 模块 feature 门控，跨模块复用反而引入门控纠缠）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use crate::logic::AppEvt;

/// pin 的 CLI 版本（升级 = 改这里 + 换三平台 SHA，见下表）。
pub const BW_VERSION: &str = "2026.10.1";

pub struct Asset {
    os: &'static str,
    arch: &'static str,
    file: &'static str,
    sha256: &'static str,
}

/// pin 清单（2026-11-02 取自 GitHub Release API asset digest）。
/// 非 pin 版本一律不碰：新版出→人工核验→改表→发版。
const ASSETS: &[Asset] = &[
    Asset {
        os: "windows",
        arch: "x86_64",
        file: "bw-oss-windows-2026.10.1.zip",
        sha256: "8ec2a4235c6c8db93cf42b444161882842e28f14dd6116369dea61efde8a418f",
    },
    Asset {
        os: "macos",
        arch: "aarch64",
        file: "bw-oss-macos-arm64-2026.10.1.zip",
        sha256: "8b64bc7887f03c8d3ee10be2d720f3904ab632553434a47703f428d8ece9c9f2",
    },
    Asset {
        os: "macos",
        arch: "x86_64",
        file: "bw-oss-macos-2026.10.1.zip",
        sha256: "faa5c6ec794bb2b3c969e2f08cba04a0f0ff8f1b2c10c5cf7b88911eaa0c1f4b",
    },
    Asset {
        os: "linux",
        arch: "x86_64",
        file: "bw-oss-linux-2026.10.1.zip",
        sha256: "53384064abe7555039281bc2e6fe51846368ba60db9b7ba89de183409d7f6738",
    },
    Asset {
        os: "linux",
        arch: "aarch64",
        file: "bw-oss-linux-arm64-2026.10.1.zip",
        sha256: "9413e412ba2f8d3b179c54314c3d2d51c0b2ec25ed17b96a98b7c2d404b6d2a7",
    },
];

fn asset_for_this_platform() -> Option<&'static Asset> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    ASSETS.iter().find(|a| a.os == os && a.arch == arch)
}

pub fn download_url(a: &Asset) -> String {
    format!(
        "https://github.com/bitwarden/clients/releases/download/cli-v{}/{}",
        BW_VERSION, a.file
    )
}

/// 内建目录（settings.json 同级 `bin`，随 Data 走便携/安装双模式）。
pub fn bin_dir(settings_path: &Path) -> PathBuf {
    settings_path
        .parent()
        .unwrap_or(settings_path)
        .join("bin")
}

/// 内建可执行文件路径。
pub fn exe_path(settings_path: &Path) -> PathBuf {
    bin_dir(settings_path).join(exe_name())
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "bw.exe"
    } else {
        "bw"
    }
}

fn marker_path(settings_path: &Path) -> PathBuf {
    bin_dir(settings_path).join("bw.version")
}

/// 内建可用：标记版本+哈希与 pin 一致，且 exe 存在可执行。
pub fn is_ready(settings_path: &Path) -> bool {
    let marker = std::fs::read_to_string(marker_path(settings_path)).unwrap_or_default();
    let mut lines = marker.lines();
    let ok = lines.next() == Some(BW_VERSION)
        && lines.next() == Some(current_sha().unwrap_or(""));
    ok && exe_path(settings_path).is_file()
}

/// 当前平台 pin 的 SHA（`pub(crate)` 是给 logic 测试的 seam：
/// 测 `resolve_vault_exe` 需要写出合法标记，没有它写不对）。
pub(crate) fn current_sha() -> Option<&'static str> {
    asset_for_this_platform().map(|a| a.sha256)
}

static ENSURING: AtomicBool = AtomicBool::new(false);

/// 后台确保内建 `bw` 齐备：已齐直接报就绪；缺则下载校验解包。
pub fn ensure_async(settings_path: PathBuf, tx: mpsc::Sender<AppEvt>) {
    if ENSURING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("clipx-bw-pack".into())
        .spawn(move || {
            let r = ensure_blocking(&settings_path);
            match r {
                Ok(exe) => {
                    let _ = tx.send(AppEvt::BwReady(exe.to_string_lossy().into_owned()));
                }
                Err((msg, warn)) => {
                    let _ = tx.send(AppEvt::PackNotice { text: msg, warn });
                }
            }
            ENSURING.store(false, Ordering::SeqCst);
        })
        .ok();
}

/// Ok(exe 路径) / Err((提示, 是否危险色))。
fn ensure_blocking(settings_path: &Path) -> Result<PathBuf, (String, bool)> {
    let Some(asset) = asset_for_this_platform() else {
        return Err((
            format!(
                "bw 自动下载不支持当前平台（{}-{}），请官网手动安装",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            true,
        ));
    };
    if is_ready(settings_path) {
        return Ok(exe_path(settings_path));
    }
    let dir = bin_dir(settings_path);
    if std::fs::create_dir_all(&dir).is_err() {
        return Err(("bw 下载：目录创建失败".to_string(), true));
    }
    let zip_path = dir.join(&asset.file);
    if !file_ok(&zip_path, asset.sha256) {
        if !download_file(&download_url(asset), &zip_path) || !file_ok(&zip_path, asset.sha256) {
            let _ = std::fs::remove_file(&zip_path);
            return Err((
                "bw 下载失败或校验不通过（网络/代理问题可重试），已清理半截文件".to_string(),
                true,
            ));
        }
    }
    let exe = exe_path(settings_path);
    extract_bw_exe(&zip_path, &exe).map_err(|e| (format!("bw 解包失败：{e}"), true))?;
    let _ = std::fs::remove_file(&zip_path);
    if std::fs::write(marker_path(settings_path), format!("{}\n{}\n", BW_VERSION, asset.sha256))
        .is_err()
    {
        return Err(("bw 标记写入失败".to_string(), true));
    }
    Ok(exe)
}

/// zip 内找 `bw(.exe)` 解出（包内唯一文件，2026.10.1 实测；多文件则取首个命中）。
fn extract_bw_exe(zip_path: &Path, dest: &Path) -> anyhow::Result<()> {
    let f = std::fs::File::open(zip_path)?;
    let mut zip = zip::ZipArchive::new(f)?;
    let want = exe_name();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        let base = name.rsplit(['/', '\\']).next().unwrap_or(&name);
        if base.eq_ignore_ascii_case(want) {
            let mut out = std::fs::File::create(dest)?;
            std::io::copy(&mut entry, &mut out)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755));
            }
            return Ok(());
        }
    }
    Err(anyhow::anyhow!("包内未找到 {want}"))
}

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

fn file_sha256(path: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "(Get-FileHash -Algorithm SHA256 -Path '{}').Hash",
                    path.display()
                ),
            ])
            .creation_flags(0x0800_0000)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
    #[cfg(not(windows))]
    {
        let out = std::process::Command::new("sha256sum").arg(path).output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .map(|s| s.to_string())
    }
}

/// 下载手法与更新通道一致（Windows `curl.exe`，v0.10.17 起的标准栈）。
fn download_file(url: &str, dest: &Path) -> bool {
    let _ = std::fs::remove_file(dest);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("curl.exe")
            .args(["-L", "--retry", "2", "-o"])
            .arg(dest)
            .arg(url)
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
    use std::io::Write as _;

    #[test]
    fn manifest_urls_and_shas_wellformed() {
        // URL 形状 + pin 版本一致 + SHA256 长度 + 平台去重（发版改表时先看这里）。
        for a in ASSETS {
            let url = download_url(a);
            assert!(
                url.starts_with("https://github.com/bitwarden/clients/releases/download/cli-v"),
                "{url}"
            );
            assert!(url.contains(BW_VERSION), "{url}");
            assert!(url.ends_with(&format!("-{}.zip", BW_VERSION)), "{url}");
            assert_eq!(a.sha256.len(), 64, "SHA256 长度");
            assert!(a.sha256.chars().all(|c| c.is_ascii_hexdigit()));
        }
        let mut keys = std::collections::HashSet::new();
        for a in ASSETS {
            assert!(keys.insert((a.os, a.arch)), "平台重复：{}-{}", a.os, a.arch);
        }
        // 本机平台必须在表里（否则 ensure 直接报不支持）。
        assert!(
            asset_for_this_platform().is_some(),
            "当前平台 {}-{} 不在 pin 表里",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    }

    #[test]
    fn file_ok_rejects_missing_empty_and_wrong_sha() {
        let dir = std::env::temp_dir().join(format!("clipx-bw-ok-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // 不存在 → false。
        assert!(!file_ok(&dir.join("nope"), ""));
        // 空文件 → false（不分 sha 是否为空：半截文件永不算数）。
        let empty = dir.join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert!(!file_ok(&empty, ""));
        // 有内容但 sha 对不上 → false（篡改/下错包挡在这里）。
        let f = dir.join("f");
        std::fs::write(&f, b"hello").unwrap();
        assert!(!file_ok(&f, &"0".repeat(64)));
        // sha 为空 = 不校验（只判非空，兼容以后可能加的免校验资产）。
        assert!(file_ok(&f, ""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_ok_accepts_matching_sha() {
        // file_sha256 自举：真算一次再比（实现与断言用同一条路，防手写 sha 写错）。
        let dir = std::env::temp_dir().join(format!("clipx-bw-sha-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("f");
        std::fs::write(&f, b"bw-sha-probe").unwrap();
        let sha = file_sha256(&f).expect("本机算不出 SHA256，ensure 也跑不动");
        assert_eq!(sha.len(), 64);
        assert!(file_ok(&f, &sha));
        assert!(file_ok(&f, &sha.to_uppercase()), "大小写不敏感");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_missing_bw_exe_errors() {
        // 包里没有 bw(.exe) → 明确报错（不是静默装个空壳）。
        let dir = std::env::temp_dir().join(format!("clipx-bw-noexe-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zp = dir.join("t.zip");
        {
            let f = std::fs::File::create(&zp).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts: zip::write::SimpleFileOptions =
                zip::write::SimpleFileOptions::default();
            w.start_file("README.txt", opts).unwrap();
            w.write_all(b"noise").unwrap();
            w.finish().unwrap();
        }
        let err = extract_bw_exe(&zp, &dir.join(exe_name())).unwrap_err();
        assert!(err.to_string().contains(exe_name()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_wrong_version_or_sha_is_not_ready() {
        // 版本/哈希任一对不上 → 未就绪（旧版残留不会被当成新版用）。
        let dir = std::env::temp_dir().join(format!("clipx-bw-stale-{}", std::process::id()));
        let settings = dir.join("settings.json");
        std::fs::create_dir_all(bin_dir(&settings)).unwrap();
        std::fs::write(exe_path(&settings), b"fake").unwrap();
        std::fs::write(marker_path(&settings), "0.0.0\ndeadbeef\n").unwrap();
        assert!(!is_ready(&settings));
        std::fs::write(
            marker_path(&settings),
            format!("{}\n{}\n", BW_VERSION, "0".repeat(64)),
        )
        .unwrap();
        assert!(!is_ready(&settings));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_roundtrip_and_readiness() {
        let dir = std::env::temp_dir().join(format!("clipx-bw-test-{}", std::process::id()));
        let settings = dir.join("settings.json");
        assert!(!is_ready(&settings));
        std::fs::create_dir_all(bin_dir(&settings)).unwrap();
        std::fs::write(exe_path(&settings), b"fake").unwrap();
        // 有 exe 无标记 → 未就绪。
        assert!(!is_ready(&settings));
        std::fs::write(
            marker_path(&settings),
            format!("{}\n{}\n", BW_VERSION, current_sha().unwrap_or("")),
        )
        .unwrap();
        assert!(is_ready(&settings));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_picks_bw_exe() {
        // 内存造包：bw.exe + 干扰文件，解包只取 bw.exe。
        let dir = std::env::temp_dir().join(format!("clipx-bw-zip-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zp = dir.join("t.zip");
        {
            let f = std::fs::File::create(&zp).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts: zip::write::SimpleFileOptions =
                zip::write::SimpleFileOptions::default();
            w.start_file("README.txt", opts).unwrap();
            w.write_all(b"noise").unwrap();
            w.start_file(exe_name(), opts).unwrap();
            w.write_all(b"fake-bw").unwrap();
            w.finish().unwrap();
        }
        let dest = dir.join(exe_name());
        extract_bw_exe(&zp, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"fake-bw");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 活体：真下载 + 校验 + 解包 + `bw.exe --version`（~40MB，仅手动）。
    /// 跑法：cargo test -p clipx-app bw_pack::tests::live_download -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_download_verify_extract_version() {
        let dir = std::env::temp_dir().join(format!("clipx-bw-live-{}", std::process::id()));
        let settings = dir.join("settings.json");
        let exe = ensure_blocking(&settings).expect("下载链路失败");
        assert!(is_ready(&settings));
        let out = std::process::Command::new(&exe)
            .arg("--version")
            .output()
            .expect("bw.exe 不可执行");
        assert!(out.status.success());
        println!("bw version: {}", String::from_utf8_lossy(&out.stdout).trim());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
