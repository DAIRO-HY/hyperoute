//! 参数解析失败时的错误模型。
//!
//! hyperoute-codegen 生成的路由代码在路径/查询串/请求体解析失败时不再 panic，
//! 而是构造一个 [`ParamError`]，交给开发者用 `#[PARAM_ERROR]` 标注的函数（没有则用
//! [`DefaultParamErrorResponse`]）转换成响应。
//!
//! 默认文案（[`ParamError`] 的 `Display`）刻意只用中性词汇：不出现 Rust 类型名、
//! 标准库 / serde 的错误原文、内部结构体名，避免客户端据此推断服务端技术栈。
//! 这些原始信息仍保留在 `expected_type` / `reason` 字段里，供日志或自定义响应使用。

use std::fmt;

use hyper::StatusCode;

use crate::IntoErrorResponse;

/// 默认文案里回显原始值时最多保留的字符数，超出部分用 `...` 代替。
pub const RAW_VALUE_DISPLAY_LIMIT: usize = 64;

/// 出错的参数来自请求的哪个部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamLocation {
    /// 路径段，例如 `/user/{id}` 里的 `id`
    Path,
    /// URL 查询串 `?name=value`
    Query,
    /// 请求体（JSON / urlencoded）整体或其中某个字段
    Body,
    /// `multipart/form-data` 里的某个字段
    MultipartField,
    /// 请求头，例如 WebSocket 握手需要的 `upgrade` / `sec-websocket-key`
    Header,
    /// 合并表单（`ParamBinding::Merged`）：查询串和表单请求体合并后取值，参数没传时报这个位置
    Form,
}

impl ParamLocation {
    pub fn as_str(&self) -> &'static str {
        match self {
            ParamLocation::Path => "path",
            ParamLocation::Query => "query",
            ParamLocation::Body => "body",
            ParamLocation::MultipartField => "multipart",
            ParamLocation::Header => "header",
            ParamLocation::Form => "form",
        }
    }
}

impl fmt::Display for ParamLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamErrorKind {
    /// 参数 / 字段没传
    Missing,
    /// 传了，但转换失败
    Invalid,
    /// 传了，是数字，但超出目标类型的取值范围
    OutOfRange,
    /// 请求的 content-type 这个路由不支持
    UnsupportedContentType,
    /// 请求体本身读取失败或格式不合法（例如 JSON 语法错误、multipart boundary 非法）
    MalformedBody,
    /// 请求体超过 `HYPEROUTE_MAX_BODY_SIZE`，或 multipart 文本字段超过内存阈值：
    /// 生成代码直接返回 413，不经过 `#[PARAM_ERROR]`
    PayloadTooLarge,
    /// 服务端处理请求体时出错（例如上传临时文件写入失败），不是客户端的问题：
    /// 生成代码记日志并返回 500，不经过 `#[PARAM_ERROR]`
    Internal,
}

impl ParamErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ParamErrorKind::Missing => "missing",
            ParamErrorKind::Invalid => "invalid",
            ParamErrorKind::OutOfRange => "out_of_range",
            ParamErrorKind::UnsupportedContentType => "unsupported_content_type",
            ParamErrorKind::MalformedBody => "malformed_body",
            ParamErrorKind::PayloadTooLarge => "payload_too_large",
            ParamErrorKind::Internal => "internal",
        }
    }
}

impl fmt::Display for ParamErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个参数解析错误。
#[derive(Debug, Clone)]
pub struct ParamError {
    /// 出错的路由，例如 `"GET /path_test/{id}"`，由生成代码填写
    pub route: &'static str,
    pub location: ParamLocation,
    /// 客户端可见的参数名 / 字段路径（例如 `"id"`、`"items[2].age"`）；
    /// 错误针对整个请求体时为 `None`
    pub param: Option<String>,
    pub kind: ParamErrorKind,
    /// 中性的期望类型描述（`"integer"`、`"number"`、`"boolean"` 等），无法中性描述时为 `None`
    pub expected: Option<&'static str>,
    /// 客户端传来的原始值（完整，不截断）；请求体整体、文件字段、非 UTF-8 的值不记录
    pub raw_value: Option<String>,
    /// 中性的补充说明，会出现在默认文案里，例如 `"invalid JSON at line 1, column 9"`
    pub detail: Option<String>,
    /// 签名里的 Rust 类型，例如 `"i64"`；只供日志使用，默认文案不会输出
    pub expected_type: &'static str,
    /// 底层错误原文（标准库 / serde 的报错）；只供日志使用，默认文案不会输出
    pub reason: Option<String>,
}

impl ParamError {
    pub fn new(location: ParamLocation, param: Option<String>, kind: ParamErrorKind) -> Self {
        Self {
            route: "",
            location,
            param,
            kind,
            expected: None,
            raw_value: None,
            detail: None,
            expected_type: "",
            reason: None,
        }
    }

    pub fn missing(location: ParamLocation, param: impl Into<String>) -> Self {
        Self::new(location, Some(param.into()), ParamErrorKind::Missing)
    }

    pub fn with_expected(mut self, expected: Option<&'static str>, expected_type: &'static str) -> Self {
        self.expected = expected;
        self.expected_type = expected_type;
        self
    }

    pub fn with_raw_value(mut self, raw_value: impl Into<String>) -> Self {
        self.raw_value = Some(raw_value.into());
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_reason(mut self, reason: impl ToString) -> Self {
        self.reason = Some(reason.to_string());
        self
    }

    /// 按字符（不是字节，避免切在多字节字符中间）截断原始值，超出 `max_chars` 时末尾加 `...`。
    pub fn raw_value_truncated(&self, max_chars: usize) -> Option<String> {
        self.raw_value.as_deref().map(|raw| truncate_chars(raw, max_chars))
    }

    /// 默认文案里这个参数的称呼，例如 `Query parameter "id"`。
    fn subject(&self) -> String {
        match (&self.param, self.location) {
            (None, ParamLocation::Query) => "Query string".to_string(),
            (None, ParamLocation::Form) => "Request parameters".to_string(),
            (None, _) => "Request body".to_string(),
            (Some(p), ParamLocation::Path) => format!("Path parameter \"{}\"", escape(p)),
            (Some(p), ParamLocation::Query) => format!("Query parameter \"{}\"", escape(p)),
            (Some(p), ParamLocation::Body) => format!("Field \"{}\" in request body", escape(p)),
            (Some(p), ParamLocation::MultipartField) => format!("Form field \"{}\"", escape(p)),
            (Some(p), ParamLocation::Header) => format!("Header \"{}\"", escape(p)),
            (Some(p), ParamLocation::Form) => format!("Parameter \"{}\"", escape(p)),
        }
    }

    /// `, got "abc"`；超长时截断并附上总长度：`, got "abc..." (1024 chars)`。
    fn got_clause(&self) -> String {
        let Some(raw) = &self.raw_value else {
            return String::new();
        };
        let total = raw.chars().count();
        if total > RAW_VALUE_DISPLAY_LIMIT {
            format!(", got \"{}\" ({total} chars)", escape(&truncate_chars(raw, RAW_VALUE_DISPLAY_LIMIT)))
        } else {
            format!(", got \"{}\"", escape(raw))
        }
    }

    fn detail_clause(&self) -> String {
        match &self.detail {
            Some(detail) => format!(" ({detail})"),
            None => String::new(),
        }
    }
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let subject = self.subject();
        let got = self.got_clause();
        let detail = self.detail_clause();
        match self.kind {
            ParamErrorKind::Missing => write!(f, "{subject} is required."),
            ParamErrorKind::Invalid => match self.expected {
                Some(expected) => {
                    write!(f, "{subject} must be {} {expected}{got}{detail}.", article(expected))
                }
                None => write!(f, "{subject} is invalid{got}{detail}."),
            },
            ParamErrorKind::OutOfRange => write!(f, "{subject} is out of range{got}{detail}."),
            ParamErrorKind::UnsupportedContentType => match self.raw_value.as_deref() {
                None | Some("") => write!(f, "Content-Type header is missing or not supported for this endpoint."),
                Some(_) => write!(f, "Content-Type{} is not supported for this endpoint.", got_value(&got)),
            },
            ParamErrorKind::MalformedBody => write!(f, "Request body is malformed{detail}."),
            ParamErrorKind::PayloadTooLarge => write!(f, "Request body is too large."),
            ParamErrorKind::Internal => write!(f, "Internal Server Error"),
        }
    }
}

impl std::error::Error for ParamError {}

/// 参数错误集合，交给 `#[PARAM_ERROR]` 函数。
///
/// 目前遇到第一个解析错误就返回，所以里面恰好只有一个错误；
/// 用集合类型是为了以后（例如增加校验层、一次返回多条）不必改自定义函数的签名。
/// 保证非空。
#[derive(Debug, Clone)]
pub struct ParamErrors {
    errors: Vec<ParamError>,
}

impl ParamErrors {
    pub fn single(error: ParamError) -> Self {
        Self { errors: vec![error] }
    }

    pub fn first(&self) -> &ParamError {
        &self.errors[0]
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ParamError> {
        self.errors.iter()
    }

    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// 永远是 false（集合保证非空），提供它只是为了配合 `len()`。
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn into_vec(self) -> Vec<ParamError> {
        self.errors
    }
}

impl<'a> IntoIterator for &'a ParamErrors {
    type Item = &'a ParamError;
    type IntoIter = std::slice::Iter<'a, ParamError>;

    fn into_iter(self) -> Self::IntoIter {
        self.errors.iter()
    }
}

impl IntoIterator for ParamErrors {
    type Item = ParamError;
    type IntoIter = std::vec::IntoIter<ParamError>;

    fn into_iter(self) -> Self::IntoIter {
        self.errors.into_iter()
    }
}

/// 多个错误时每条一行。
impl fmt::Display for ParamErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, error) in self.errors.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

/// 没有 `#[PARAM_ERROR]` 函数时使用的默认响应：400 + 纯文本默认文案。
pub struct DefaultParamErrorResponse(pub ParamErrors);

impl IntoErrorResponse for DefaultParamErrorResponse {
    fn status(&self) -> StatusCode {
        match self.0.first().kind {
            ParamErrorKind::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ParamErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }

    fn message(&self) -> String {
        self.0.to_string()
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => format!("{}...", &s[..byte_idx]),
        None => s.to_string(),
    }
}

/// JSON 风格转义（不用 `{:?}`，那是 Rust 特有的 `\u{1b}` 风格）。
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn article(noun: &str) -> &'static str {
    match noun.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    }
}

/// 把 `, got "x"` 变成 ` "x"`，用于 Content-Type 文案。
fn got_value(got: &str) -> String {
    got.strip_prefix(", got").unwrap_or(got).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_neutral() {
        let e = ParamError::new(ParamLocation::Query, Some("id".into()), ParamErrorKind::Invalid)
            .with_expected(Some("integer"), "i64")
            .with_raw_value("abc")
            .with_reason("invalid digit found in string");
        assert_eq!(e.to_string(), r#"Query parameter "id" must be an integer, got "abc"."#);

        let e = ParamError::missing(ParamLocation::Query, "name");
        assert_eq!(e.to_string(), r#"Query parameter "name" is required."#);

        let e = ParamError::new(ParamLocation::Body, None, ParamErrorKind::MalformedBody)
            .with_detail("invalid JSON at line 1, column 9");
        assert_eq!(e.to_string(), "Request body is malformed (invalid JSON at line 1, column 9).");

        let e = ParamError::new(ParamLocation::Body, None, ParamErrorKind::UnsupportedContentType)
            .with_raw_value("application/xml");
        assert_eq!(e.to_string(), r#"Content-Type "application/xml" is not supported for this endpoint."#);
    }

    #[test]
    fn long_raw_value_is_truncated_by_chars() {
        let raw = "周".repeat(100);
        let e = ParamError::new(ParamLocation::Path, Some("id".into()), ParamErrorKind::Invalid)
            .with_expected(Some("integer"), "i64")
            .with_raw_value(raw.clone());
        let expected = format!(
            r#"Path parameter "id" must be an integer, got "{}..." (100 chars)."#,
            "周".repeat(RAW_VALUE_DISPLAY_LIMIT)
        );
        assert_eq!(e.to_string(), expected);
        // 结构体里保留完整原始值
        assert_eq!(e.raw_value.as_deref(), Some(raw.as_str()));
    }

    #[test]
    fn raw_value_is_escaped() {
        let e = ParamError::new(ParamLocation::Query, Some("q".into()), ParamErrorKind::Invalid)
            .with_raw_value("a\"b\n\u{1b}");
        assert_eq!(e.to_string(), r#"Query parameter "q" is invalid, got "a\"b\n\u001b"."#);
    }
}
