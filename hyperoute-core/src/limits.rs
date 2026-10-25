//! 请求体相关的限制，由框架自己读取配置：
//!
//! | 配置 | 环境变量 | 默认值 |
//! |---|---|---|
//! | multipart 内存阈值：单个文件超过它就转存到临时文件；单个文本字段超过它返回 413 | `HYPEROUTE_MULTIPART_MEMORY_LIMIT` | 1MB |
//! | 请求体大小上限：超过返回 413（只限制框架替 handler 读取的请求体，`BodyStream` 不受限制） | `HYPEROUTE_MAX_BODY_SIZE` | 不限制 |
//! | 上传临时文件的目录（不存在时自动创建） | `HYPEROUTE_UPLOAD_TEMP_DIR` | 系统临时目录 |
//!
//! 大小可以写成 `65536`、`512KB`、`64MB`、`10GB`（按 1024 换算）。环境变量在第一次用到时读取并缓存，
//! 改了要重启进程。iOS 这类没法设置环境变量的环境可以用 [`set_multipart_memory_limit`] /
//! [`set_max_body_size`] / [`set_upload_temp_dir`] 在代码里设置，优先级高于环境变量。

use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

pub const DEFAULT_MULTIPART_MEMORY_LIMIT: u64 = 1024 * 1024;

const MEMORY_ENV: &str = "HYPEROUTE_MULTIPART_MEMORY_LIMIT";
const MAX_BODY_ENV: &str = "HYPEROUTE_MAX_BODY_SIZE";
const TEMP_DIR_ENV: &str = "HYPEROUTE_UPLOAD_TEMP_DIR";

static CODE_MEMORY_LIMIT: RwLock<Option<u64>> = RwLock::new(None);
static CODE_MAX_BODY_SIZE: RwLock<Option<Option<u64>>> = RwLock::new(None);
static CODE_TEMP_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

/// 代码设置 multipart 内存阈值（字节），优先级高于环境变量 `HYPEROUTE_MULTIPART_MEMORY_LIMIT`。
/// 一般部署用环境变量即可，这个函数给没法设置环境变量的环境（例如 iOS）用。之后进来的请求按新值处理。
pub fn set_multipart_memory_limit(bytes: u64) {
    *CODE_MEMORY_LIMIT.write().unwrap() = Some(bytes);
}

/// 代码设置请求体大小上限（字节，`None` 表示不限制），优先级高于环境变量 `HYPEROUTE_MAX_BODY_SIZE`。
/// 一般部署用环境变量即可，这个函数给没法设置环境变量的环境（例如 iOS）用。之后进来的请求按新值处理。
pub fn set_max_body_size(limit: Option<u64>) {
    *CODE_MAX_BODY_SIZE.write().unwrap() = Some(limit);
}

/// 代码设置上传临时文件的目录，优先级高于环境变量 `HYPEROUTE_UPLOAD_TEMP_DIR`。
/// 建议和最终存储目录放在同一个文件系统上，handler 里就能直接 `rename`，不用复制大文件。
/// 目录不存在时在第一次写临时文件时自动创建。
pub fn set_upload_temp_dir(dir: impl Into<PathBuf>) {
    *CODE_TEMP_DIR.write().unwrap() = Some(dir.into());
}

/// 当前的上传临时目录：代码设置 > 环境变量 > 系统临时目录。
pub fn upload_temp_dir() -> PathBuf {
    if let Some(dir) = CODE_TEMP_DIR.read().unwrap().clone() {
        return dir;
    }
    static ENV: OnceLock<Option<PathBuf>> = OnceLock::new();
    ENV.get_or_init(|| std::env::var_os(TEMP_DIR_ENV).filter(|v| !v.is_empty()).map(PathBuf::from))
        .clone()
        .unwrap_or_else(std::env::temp_dir)
}

/// 当前的 multipart 内存阈值：代码设置 > 环境变量 > 1MB。
pub fn multipart_memory_limit() -> u64 {
    if let Some(bytes) = *CODE_MEMORY_LIMIT.read().unwrap() {
        return bytes;
    }
    static ENV: OnceLock<Option<u64>> = OnceLock::new();
    ENV.get_or_init(|| env_size(MEMORY_ENV)).unwrap_or(DEFAULT_MULTIPART_MEMORY_LIMIT)
}

/// 当前的请求体大小上限：代码设置 > 环境变量 > 不限制。
pub fn max_body_size() -> Option<u64> {
    if let Some(limit) = *CODE_MAX_BODY_SIZE.read().unwrap() {
        return limit;
    }
    static ENV: OnceLock<Option<u64>> = OnceLock::new();
    *ENV.get_or_init(|| env_size(MAX_BODY_ENV))
}

fn env_size(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    let parsed = parse_size(&value);
    if parsed.is_none() {
        eprintln!("[hyperoute] ignoring {name}={value:?}: expected a size such as 65536, 512KB, 64MB or 10GB");
    }
    parsed
}

/// `65536` / `512KB` / `64MB` / `10GB`（大小写不限，`B` 可省略，按 1024 换算）。
pub(crate) fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim().to_ascii_uppercase();
    let value = value.strip_suffix('B').unwrap_or(&value);
    let (number, unit) = match value.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((index, _)) => value.split_at(index),
        None => (value, ""),
    };
    let multiplier: u64 = match unit.trim() {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => return None,
    };
    if number.is_empty() {
        return None;
    }
    number.parse::<u64>().ok()?.checked_mul(multiplier)
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("65536"), Some(65536));
        assert_eq!(parse_size("512KB"), Some(512 * 1024));
        assert_eq!(parse_size("64mb"), Some(64 << 20));
        assert_eq!(parse_size(" 10G "), Some(10 << 30));
        assert_eq!(parse_size("1 MB"), Some(1 << 20));
        assert_eq!(parse_size("MB"), None);
        assert_eq!(parse_size("1.5MB"), None);
        assert_eq!(parse_size("10XB"), None);
        assert_eq!(parse_size(""), None);
    }
}
