//! 文件响应：[`Stream::file`] 按请求头处理 Range（断点下载、视频拖动）、If-Range 和条件请求（304）。

use std::io::{self, SeekFrom};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use hyper::header::{self, HeaderMap};
use hyper::StatusCode;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::Stream;

/// Range 头的处理结果。
#[derive(Debug, PartialEq, Eq)]
enum RangeDecision {
    /// 没有 Range 头、写法不认识（例如多段）、或者 If-Range 不匹配：返回整个文件
    Full,
    /// 闭区间 `[start, end]`
    Partial(u64, u64),
    /// 范围不合法：416
    Unsatisfiable,
}

impl Stream {
    /// 返回一个文件，按请求头处理：
    /// - `Range: bytes=a-b` / `a-` / `-n`（单段）→ 206 + `Content-Range`；范围不合法 → 416；
    ///   多段或写法不认识时忽略 Range，返回整个文件（RFC 9110 允许）；
    /// - `If-Range` 和当前 ETag / Last-Modified 不一致时忽略 Range；
    /// - `If-None-Match` / `If-Modified-Since` 命中 → 304；
    /// - 总是带 `Accept-Ranges: bytes`、`ETag`（按文件大小和修改时间）、`Last-Modified`、`Content-Length`。
    ///
    /// content-type 按扩展名猜，可以用 [`Stream::content_type`] 覆盖；下载文件名用 [`Stream::attachment`]。
    /// 文件不存在、不是普通文件、打不开时返回 `io::Error`，由 handler 决定返回 404 还是别的。
    pub async fn file(path: impl AsRef<Path>, headers: &HeaderMap) -> io::Result<Stream> {
        let path = path.as_ref();
        let mut file = tokio::fs::File::open(path).await?;
        let meta = file.metadata().await?;
        if !meta.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "not a regular file"));
        }
        let size = meta.len();
        let modified = meta.modified().ok();
        let etag = file_etag(size, modified);

        let mut base: Vec<(&'static str, String)> = vec![
            ("content-type", crate::content_type(&path.to_string_lossy()).to_string()),
            ("accept-ranges", "bytes".to_string()),
            ("etag", etag.clone()),
        ];
        if let Some(modified) = modified {
            base.push(("last-modified", httpdate::fmt_http_date(modified)));
        }

        if not_modified(headers, &etag, modified) {
            return Ok(empty(StatusCode::NOT_MODIFIED, base));
        }

        match range_decision(headers, size, &etag, modified) {
            RangeDecision::Full => {
                base.push(("content-length", size.to_string()));
                Ok(with_headers(Stream::new(ReaderStream::new(file)), base))
            }
            RangeDecision::Partial(start, end) => {
                let len = end - start + 1;
                file.seek(SeekFrom::Start(start)).await?;
                base.push(("content-range", format!("bytes {start}-{end}/{size}")));
                base.push(("content-length", len.to_string()));
                let stream = Stream::new(ReaderStream::new(file.take(len))).status(StatusCode::PARTIAL_CONTENT);
                Ok(with_headers(stream, base))
            }
            RangeDecision::Unsatisfiable => {
                base.retain(|(name, _)| *name != "content-type");
                base.push(("content-range", format!("bytes */{size}")));
                base.push(("content-length", "0".to_string()));
                Ok(empty(StatusCode::RANGE_NOT_SATISFIABLE, base))
            }
        }
    }

    /// 设置 content-type（替换已有的）。
    pub fn content_type(self, content_type: impl Into<String>) -> Self {
        self.set_header("content-type", content_type)
    }

    /// 作为附件下载：`Content-Disposition: attachment`，文件名按 RFC 5987 编码（支持中文）。
    pub fn attachment(self, filename: &str) -> Self {
        self.set_header("content-disposition", content_disposition("attachment", filename))
    }

    /// 浏览器内直接打开（图片、视频、PDF），同时带上文件名：`Content-Disposition: inline`。
    pub fn inline(self, filename: &str) -> Self {
        self.set_header("content-disposition", content_disposition("inline", filename))
    }

    /// 设置一个 header，替换同名的已有值（[`Stream::header`] 是追加）。
    pub fn set_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.headers.push((name, value.into()));
        self
    }
}

fn with_headers(mut stream: Stream, headers: Vec<(&'static str, String)>) -> Stream {
    stream.headers = headers;
    stream
}

fn empty(status: StatusCode, headers: Vec<(&'static str, String)>) -> Stream {
    let stream = Stream::new(futures_util::stream::empty()).status(status);
    with_headers(stream, headers)
}

/// `Content-Disposition` 的值：`attachment; filename="a.txt"`；文件名含非 ASCII 字符或 `"` `\` 时，
/// `filename` 里换成 `_` 作为兼容写法，另外附上 `filename*=UTF-8''<百分号编码>`。
pub fn content_disposition(kind: &str, filename: &str) -> String {
    let fallback: String = filename
        .chars()
        .map(|c| if c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\' { c } else { '_' })
        .collect();
    if fallback == filename {
        return format!("{kind}; filename=\"{filename}\"");
    }
    let mut encoded = String::new();
    for byte in filename.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("{kind}; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

fn file_etag(size: u64, modified: Option<SystemTime>) -> String {
    let nanos = modified
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("\"{size:x}-{nanos:x}\"")
}

fn secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// If-None-Match 优先；没有它时才看 If-Modified-Since（精确到秒）。
fn not_modified(headers: &HeaderMap, etag: &str, modified: Option<SystemTime>) -> bool {
    if let Some(value) = header_str(headers, header::IF_NONE_MATCH) {
        return value
            .split(',')
            .map(str::trim)
            .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag);
    }
    match (modified, header_str(headers, header::IF_MODIFIED_SINCE).and_then(|v| httpdate::parse_http_date(v).ok())) {
        (Some(modified), Some(since)) => secs(modified) <= secs(since),
        _ => false,
    }
}

fn range_decision(headers: &HeaderMap, size: u64, etag: &str, modified: Option<SystemTime>) -> RangeDecision {
    let Some(range) = header_str(headers, header::RANGE) else {
        return RangeDecision::Full;
    };
    if let Some(if_range) = header_str(headers, header::IF_RANGE) {
        let if_range = if_range.trim();
        let matches = if if_range.starts_with('"') {
            // 强比较：弱 ETag（W/"..."）不匹配
            if_range == etag
        } else if if_range.starts_with("W/") {
            false
        } else {
            match (modified, httpdate::parse_http_date(if_range).ok()) {
                (Some(modified), Some(date)) => secs(modified) == secs(date),
                _ => false,
            }
        };
        if !matches {
            return RangeDecision::Full;
        }
    }
    parse_range(range, size)
}

/// 单段 `bytes=a-b` / `a-` / `-n`；写法不认识或多段时返回 Full（忽略 Range 头）。
fn parse_range(value: &str, size: u64) -> RangeDecision {
    let value = value.trim();
    let Some(spec) = value.get(..6).filter(|p| p.eq_ignore_ascii_case("bytes=")).map(|_| &value[6..]) else {
        return RangeDecision::Full;
    };
    if spec.contains(',') {
        return RangeDecision::Full;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return RangeDecision::Full;
    };
    let (first, last) = (first.trim(), last.trim());
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());

    if first.is_empty() {
        // 最后 n 个字节
        if !digits(last) {
            return RangeDecision::Full;
        }
        let n: u64 = last.parse().unwrap_or(u64::MAX);
        if n == 0 || size == 0 {
            return RangeDecision::Unsatisfiable;
        }
        return RangeDecision::Partial(size.saturating_sub(n), size - 1);
    }

    if !digits(first) || !(last.is_empty() || digits(last)) {
        return RangeDecision::Full;
    }
    let Ok(start) = first.parse::<u64>() else {
        return RangeDecision::Unsatisfiable;
    };
    let end = if last.is_empty() { u64::MAX } else { last.parse::<u64>().unwrap_or(u64::MAX) };
    if end < start {
        return RangeDecision::Full;
    }
    if start >= size {
        return RangeDecision::Unsatisfiable;
    }
    RangeDecision::Partial(start, end.min(size - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        use RangeDecision::*;
        assert_eq!(parse_range("bytes=0-99", 1000), Partial(0, 99));
        assert_eq!(parse_range("bytes=900-", 1000), Partial(900, 999));
        assert_eq!(parse_range("bytes=-100", 1000), Partial(900, 999));
        assert_eq!(parse_range("bytes=-5000", 1000), Partial(0, 999));
        assert_eq!(parse_range("bytes=500-5000", 1000), Partial(500, 999));
        assert_eq!(parse_range("BYTES=1-1", 10), Partial(1, 1));
        assert_eq!(parse_range("bytes=1000-", 1000), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 1000), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=5-1", 1000), Full);
        assert_eq!(parse_range("bytes=0-1,5-9", 1000), Full);
        assert_eq!(parse_range("items=0-1", 1000), Full);
        assert_eq!(parse_range("bytes=x-1", 1000), Full);
        assert_eq!(parse_range("bytes=99999999999999999999999-", 1000), Unsatisfiable);
    }

    #[test]
    fn disposition_encoding() {
        assert_eq!(content_disposition("attachment", "a b.txt"), r#"attachment; filename="a b.txt""#);
        assert_eq!(
            content_disposition("attachment", "报告 1.pdf"),
            r#"attachment; filename="__ 1.pdf"; filename*=UTF-8''%E6%8A%A5%E5%91%8A%201.pdf"#
        );
        assert_eq!(
            content_disposition("inline", "a\"b.png"),
            r#"inline; filename="a_b.png"; filename*=UTF-8''a%22b.png"#
        );
    }
}
