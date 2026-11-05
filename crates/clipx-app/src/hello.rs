//! Windows Hello 快速解锁（记住设备）。
//!
//! 为何存在：`bw` CLI 完全不懂生物识别（无 Hello 参数、无对应服务端点），
//! 指纹/面容/PIN 只能由本应用自己接——官方客户端也是这个路数：
//! 主密码只在首次登录时用一次，之后把**会话 key**（注意：不是主密码，
//! 主密码永不落盘）存下来，后续解锁只验证 Hello 就放行。
//!
//! 安全边界（丑话说前面）：
//! - 存的是 BW_SESSION（服务端会话，会过期/可撤销），丢了顶多重新输密码，
//!    éché 不会危及主密码；主密码从不进这个文件。
 //! - at-rest 保护是 DPAPI 用户域（CryptProtectData，不加 LOCAL_MACHINE）：
//!   同一 Windows 用户下的任何进程理论上都能解——和 Chrome 密码一个档次。
//!   Hello 在这里是**交互式同意门**（人不在/没点确认就解不开），
//!   不是硬件绑定加密。想要 TPM 绑定得到 Passport/NGC 那套，另议。
//! - 换号/登出/关开关即删文件；会话失效用一次就自动忘掉。
//!
//! 文件位置：`Data/vault_hello.bin`（DPAPI blob，外人打不开，内容是 JSON）。
//! 跨平台：非 Windows 全是 stub（Unavailable），逻辑层照常编译。

use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const HELLO_FILE: &str = "vault_hello.bin";
/// DPAPI 附加熵（防其它软件拿固定输入撞：熵公开在二进制里，只防顺手不防逆向）。
const ENTROPY: &[u8] = b"clipx-vault-hello-v1";

/// Hello 可用性（进程内缓存：系统能力不会中途变，查一次）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelloAvail {
    Ready,
    Unavailable,
}

fn hello_file(data_dir: &Path) -> PathBuf {
    data_dir.join(HELLO_FILE)
}

/// 存一条会话（解锁/Login 成功后，后台线程里调）。
pub fn store_session(data_dir: &Path, session: &str) -> Result<(), String> {
    let body = serde_json::json!({ "v": 1u32, "session": session }).to_string();
    let blob = imp::protect(body.as_bytes())?;
    if let Some(parent) = hello_file(data_dir).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败：{e}"))?;
    }
    // 先写临时再改名：崩溃不留半截文件。
    let tmp = hello_file(data_dir).with_extension("tmp");
    std::fs::write(&tmp, &blob).map_err(|e| format!("写记住设备文件失败：{e}"))?;
    std::fs::rename(&tmp, hello_file(data_dir)).map_err(|e| format!("落盘失败：{e}"))?;
    Ok(())
}

/// 读一条会话（None = 没记住/坏了/删了；坏文件顺手清掉）。
pub fn load_session(data_dir: &Path) -> Option<Zeroizing<String>> {
    let raw = std::fs::read(hello_file(data_dir)).ok()?;
    let plain = imp::unprotect(&raw).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&plain).ok()?;
    if v.get("v")?.as_u64()? != 1 {
        return None;
    }
    let s = v.get("session")?.as_str().filter(|s| !s.is_empty())?;
    Some(Zeroizing::new(s.to_string()))
}

/// 删记住的会话（关开关/登出时调；文件不在也算成功）。
pub fn clear_session(data_dir: &Path) {
    let _ = std::fs::remove_file(hello_file(data_dir));
}

/// 是否有记住的会话（弹窗入口行显隐用；只读文件不碰系统 API）。
pub fn has_stored_session(data_dir: &Path) -> bool {
    load_session(data_dir).is_some()
}

/// Hello 可用性 + 有记住的会话（弹窗入口行显示条件）。
pub fn hello_usable(data_dir: &Path) -> bool {
    availability() == HelloAvail::Ready && has_stored_session(data_dir)
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::OnceLock;
    use windows::core::HSTRING;
    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier,
    };

    static AVAIL: OnceLock<HelloAvail> = OnceLock::new();

    fn ensure_apartment() -> Result<(), String> {
        // MTA：后台线程调 WinRT 不抢 UI 线程；已初始化返回 S_FALSE 也算成功。
        unsafe {
            windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            )
            .map_err(|e| format!("WinRT 初始化失败：{e}"))?;
        }
        Ok(())
    }

    /// 系统有没有 Hello（指纹/面容/PIN 任一可用即算；结果进程内缓存）。
    pub fn availability() -> HelloAvail {
        *AVAIL.get_or_init(|| {
            if ensure_apartment().is_err() {
                return HelloAvail::Unavailable;
            }
            let r = UserConsentVerifier::CheckAvailabilityAsync()
                .and_then(|op| op.get())
                .map(|v| v == UserConsentVerifierAvailability::Available);
            match r {
                Ok(true) => HelloAvail::Ready,
                _ => HelloAvail::Unavailable,
            }
        })
    }

    /// 弹系统确认框（指纹/面容/PIN 按系统 enrol 走）；用户取消/失败直接 Err。
    /// 必须在后台线程调（阻塞等用户，逻辑线程禁入）。
    pub fn verify(reason: &str) -> Result<(), String> {
        ensure_apartment()?;
        let op = UserConsentVerifier::RequestVerificationAsync(&HSTRING::from(reason))
            .map_err(|e| format!("Hello 调不起来：{e}"))?;
        let r = op.get().map_err(|e| format!("Hello 等待失败：{e}"))?;
        match r {
            UserConsentVerificationResult::Verified => Ok(()),
            UserConsentVerificationResult::NotConfiguredForUser => {
                Err("这台设备没录 Hello（指纹/面容/PIN 任一）".into())
            }
            UserConsentVerificationResult::DisabledByPolicy => {
                Err("Hello 被组策略/管理员禁用".into())
            }
            UserConsentVerificationResult::DeviceBusy => Err("Hello 设备忙，稍后再试".into()),
            UserConsentVerificationResult::RetriesExhausted => {
                Err("Hello 重试次数用完被锁，先去系统里解锁".into())
            }
            _ => Err("Hello 验证没通过（取消/超时）".into()),
        }
    }

    use windows::Security::Credentials::UI::UserConsentVerifierAvailability;

    #[repr(C)]
    struct Blob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            p_data_in: *const Blob,
            sz_data_descr: *const u16,
            p_optional_entropy: *const Blob,
            pv_reserved: *mut std::ffi::c_void,
            p_prompt_struct: *mut std::ffi::c_void,
            dw_flags: u32,
            p_data_out: *mut Blob,
        ) -> i32;
        fn CryptUnprotectData(
            p_data_in: *const Blob,
            ppsz_data_descr: *mut *mut u16,
            p_optional_entropy: *const Blob,
            pv_reserved: *mut std::ffi::c_void,
            p_prompt_struct: *mut std::ffi::c_void,
            dw_flags: u32,
            p_data_out: *mut Blob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }

    const UI_FORBIDDEN: u32 = 0x1;

    fn entropy_blob() -> (Vec<u8>, Blob) {
        let mut v = ENTROPY.to_vec();
        let b = Blob {
            cb_data: v.len() as u32,
            pb_data: v.as_mut_ptr(),
        };
        // v 的所有权随元组走调用方持有到调用结束指针才有效。
        (v, unsafe { std::ptr::read(&b as *const Blob) })
    }

    /// DPAPI 用户域加密（UI_FORBIDDEN：绝不弹窗；静默失败直接 Err）。
    pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
        let _ent;
        let entropy = if ENTROPY.is_empty() {
            None
        } else {
            let (v, b) = entropy_blob();
            _ent = v;
            Some(b)
        };
        let input = Blob {
            cb_data: data.len() as u32,
            pb_data: data.as_ptr() as *mut u8,
        };
        let mut out = Blob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        // SAFETY：指针均指向调用栈/调用方存活内存；out 由系统分配后拷走即 free。
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                entropy.as_ref().map(|b| b as *const Blob).unwrap_or(std::ptr::null()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 || out.pb_data.is_null() || out.cb_data == 0 {
            return Err("DPAPI 加密失败".into());
        }
        let buf = unsafe { std::slice::from_raw_parts(out.pb_data, out.cb_data as usize).to_vec() };
        unsafe {
            LocalFree(out.pb_data as *mut std::ffi::c_void);
        }
        Ok(buf)
    }

    /// DPAPI 解密（换用户/换机就解不开→ Err调用方当无记住处理）。
    pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
        let _ent;
        let entropy = if ENTROPY.is_empty() {
            None
        } else {
            let (v, b) = entropy_blob();
            _ent = v;
            Some(b)
        };
        let input = Blob {
            cb_data: blob.len() as u32,
            pb_data: blob.as_ptr() as *mut u8,
        };
        let mut out = Blob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                entropy.as_ref().map(|b| b as *const Blob).unwrap_or(std::ptr::null()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 || out.pb_data.is_null() {
            return Err("DPAPI 解密失败（可能换了用户/机器）".into());
        }
        // 空明文也算成功（上层按 JSON 校验拦）。
        let buf = if out.cb_data == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(out.pb_data, out.cb_data as usize).to_vec() }
        };
        unsafe {
            LocalFree(out.pb_data as *mut std::ffi::c_void);
        }
        Ok(buf)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn availability() -> HelloAvail {
        HelloAvail::Unavailable
    }

    pub fn verify(_reason: &str) -> Result<(), String> {
        Err("当前平台不支持 Windows Hello".into())
    }

    pub fn protect(_data: &[u8]) -> Result<Vec<u8>, String> {
        Err("当前平台不支持 DPAPI".into())
    }

    pub fn unprotect(_blob: &[u8]) -> Result<Vec<u8>, String> {
        Err("当前平台不支持 DPAPI".into())
    }
}

pub use imp::{availability, verify};


#[cfg(test)]
mod tests {
    use super::*;

    /// DPAPI 回环：存→读→删（Windows 真跑加解密；其它平台走 stub 不炸）。
    #[test]
    fn hello_store_roundtrip_or_unsupported() {
        let dir = std::env::temp_dir().join(format!(
            "clipx-hello-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        match store_session(&dir, "sess-key-123") {
            Ok(()) => {
                let back = load_session(&dir).expect("刚存的必须读回来");
                assert_eq!(&*back, "sess-key-123");
                drop(back);
                clear_session(&dir);
                assert!(load_session(&dir).is_none(), "删完读不到");
                let _ = std::fs::remove_dir_all(&dir);
            }
            Err(e) => {
                // 非 Windows stub：存失败则读也必须没有。
                assert!(load_session(&dir).is_none(), "{e}");
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}
