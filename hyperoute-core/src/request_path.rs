//! 请求路径规范化。
//!
//! 生成的路由入口对每个请求只规范化一次，结果放进请求 extensions；过滤器匹配、动态路由匹配、
//! 静态资源查找三处都用这同一个结果。否则会出现“过滤器看到的是编码前的路径、静态资源解码后
//! 却命中了受保护文件”的绕过问题（例如 `/assets/%70rivate/x` 绕过 `/assets/private/**` 的鉴权）。

use http_body_util::{BodyExt, Full};
use hyper::StatusCode;
use hyper::body::Bytes;

use crate::{Request, Response};

/// 规范化后的请求路径，由生成代码在入口处放进请求 extensions。
#[derive(Debug, Clone)]
pub struct RequestPath(String);

impl RequestPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 规范化路径：
/// - 按 `/` 分段后逐段 percent 解码（所以 `%2F` 不会变成新的分隔符）；
/// - 去掉空段（合并 `//`）和 `.` 段；
/// - 拒绝 `..` 段、解码后含 `/`、`\` 或 NUL 的段、非法的 percent 编码、非 UTF-8 内容；
/// - 保留末尾的 `/`（静态资源据此区分“目录”和“文件”）。
///
/// 不合法时返回 None，入口直接响应 400。
pub fn normalize_path(raw: &str) -> Option<String> {
    let mut segments = Vec::new();
    for segment in raw.split('/') {
        if segment.is_empty() {
            continue;
        }
        let decoded = percent_decode(segment)?;
        match decoded.as_str() {
            "." => continue,
            ".." => return None,
            _ => {}
        }
        if decoded.contains(['/', '\\', '\0']) {
            return None;
        }
        segments.push(decoded);
    }

    let mut normalized = String::with_capacity(raw.len());
    normalized.push('/');
    normalized.push_str(&segments.join("/"));
    if !segments.is_empty() && raw.ends_with('/') {
        normalized.push('/');
    }
    Some(normalized)
}

fn percent_decode(segment: &str) -> Option<String> {
    if !segment.contains('%') {
        return Some(segment.to_string());
    }
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 生成代码在入口调用：规范化路径并放进 extensions；路径不合法时返回 400 响应。
pub fn normalize_request_path(req: &mut Request) -> Result<(), Response> {
    match normalize_path(req.uri().path()) {
        Some(path) => {
            req.extensions_mut().insert(RequestPath(path));
            Ok(())
        }
        None => Err(hyper::Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Full::new(Bytes::from_static(b"Bad Request")).map_err(|never| match never {}).boxed())
            .unwrap()),
    }
}

/// 规范化后的请求路径；没经过入口规范化（例如自己写的测试）时退回原始路径。
pub fn request_path<B>(req: &hyper::Request<B>) -> &str {
    req.extensions()
        .get::<RequestPath>()
        .map(RequestPath::as_str)
        .unwrap_or_else(|| req.uri().path())
}

#[cfg(test)]
mod tests {
    use super::normalize_path;

    #[test]
    fn decodes_and_normalizes() {
        assert_eq!(normalize_path("/").as_deref(), Some("/"));
        assert_eq!(normalize_path("//a///b").as_deref(), Some("/a/b"));
        assert_eq!(normalize_path("/a/./b/").as_deref(), Some("/a/b/"));
        assert_eq!(normalize_path("/assets/%70rivate/x").as_deref(), Some("/assets/private/x"));
        assert_eq!(normalize_path("/%E4%B8%AD%E6%96%87/a%20b.png").as_deref(), Some("/中文/a b.png"));
        assert_eq!(normalize_path("/a+b").as_deref(), Some("/a+b"));
    }

    #[test]
    fn rejects_dangerous_paths() {
        assert_eq!(normalize_path("/a/../b"), None);
        assert_eq!(normalize_path("/a/%2e%2e/b"), None);
        assert_eq!(normalize_path("/a/%2E%2E"), None);
        assert_eq!(normalize_path("/a%2Fb"), None);
        assert_eq!(normalize_path("/a%5Cb"), None);
        assert_eq!(normalize_path("/a%00"), None);
        assert_eq!(normalize_path("/a%zz"), None);
        assert_eq!(normalize_path("/a%2"), None);
        assert_eq!(normalize_path("/%ff%fe"), None);
    }
}
