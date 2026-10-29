//! Bitwarden / Vaultwarden 只读填充（M2）。
//!
//! - 只调外部 `bw` CLI（`bw login --apikey` 一次性登录后，`unlock → sync →
//!   list/get`），不自建 vault、不存主密码、不碰加解密。
//! - 会话密钥只放内存（`zeroize::Zeroizing`），经 `BW_SESSION` **环境变量**
//!   传给子进程，永不进 argv（进程列表可见）、永不落盘、不进日志。
//! - 主密码只经 `--passwordenv` 传给单次 `unlock` 子进程（同样只在子进程环境），
//!   不在父进程环境留存。
//! - `bw` 不存在 / 未登录 / 已锁定全部降级为提示行，不阻断主流程。
//! - 秘密字段（password/totp）只以 `Zeroizing<String>` 短暂持有，用完即清；
//!   `Debug` 实现全部脱敏；错误里不带秘密内容。
//!
//! 解锁 UI（主密码输入框）是明确的下一步；当前解锁靠终端
//! `bw unlock --raw` 后把会话 key 放入进程环境 `BW_SESSION` 启动 clipx
//! （见 [`Vault::open_env`]），或调用 [`Vault::unlock`]（内存会话，进程退出即失）。

pub mod client;
pub mod model;

pub use client::{Vault, VaultError, VaultState};
pub use model::VaultItem;
