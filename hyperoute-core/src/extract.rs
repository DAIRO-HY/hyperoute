//! 给 hyperoute-codegen 生成的路由代码调用的参数提取函数。
//! 每个函数失败时都返回带中性描述的 [`ParamError`]，而不是 panic。

use std::any::Any;
use std::collections::HashMap;
use std::fmt::Display;
use std::num::{IntErrorKind, ParseIntError};
use std::str::FromStr;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_path_to_error::{Path, Segment};

use crate::MultipartField;
use crate::filter::Query;
use crate::param::{ParamError, ParamErrorKind, ParamLocation};

/// 把一个字符串参数（路径段 / 查询值 / multipart 文本字段）转换成目标类型。
/// 整数溢出单独归为 [`ParamErrorKind::OutOfRange`]，其它失败归为 [`ParamErrorKind::Invalid`]。
pub fn parse_param<T>(
    location: ParamLocation,
    name: &str,
    raw: &str,
    expected: Option<&'static str>,
    expected_type: &'static str,
) -> Result<T, ParamError>
where
    T: FromStr,
    T::Err: Display + 'static,
{
    raw.parse::<T>().map_err(|e| {
        let out_of_range = (&e as &dyn Any)
            .downcast_ref::<ParseIntError>()
            .is_some_and(|ie| matches!(ie.kind(), IntErrorKind::PosOverflow | IntErrorKind::NegOverflow));
        let kind = if out_of_range {
            ParamErrorKind::OutOfRange
        } else {
            ParamErrorKind::Invalid
        };
        ParamError::new(location, Some(name.to_string()), kind)
            .with_expected(expected, expected_type)
            .with_raw_value(raw)
            .with_reason(e)
    })
}

/// 解析 URL 查询串为 `参数名 -> 值`；格式不合法时当作没有查询参数。
pub fn query_map(query: Option<&str>) -> HashMap<String, String> {
    query
        .map(|q| serde_urlencoded::from_str(q).unwrap_or_default())
        .unwrap_or_default()
}

pub fn query_param<T>(
    query: &HashMap<String, String>,
    name: &str,
    expected: Option<&'static str>,
    expected_type: &'static str,
) -> Result<T, ParamError>
where
    T: FromStr,
    T::Err: Display + 'static,
{
    query_param_any(query, &[name], name, expected, expected_type)
}

/// 按候选名依次查找查询参数（例如 `["page_size", "pageSize"]`，前面的优先）。
/// 候选名由 hyperoute-codegen 在编译期按 `ParamNaming` 算好；`display` 是缺失时错误信息里的名字，
/// 值不合法时错误信息用实际匹配到的那个名字。
pub fn query_param_any<T>(
    query: &HashMap<String, String>,
    keys: &[&str],
    display: &str,
    expected: Option<&'static str>,
    expected_type: &'static str,
) -> Result<T, ParamError>
where
    T: FromStr,
    T::Err: Display + 'static,
{
    let Some((key, raw)) = keys.iter().find_map(|key| query.get(*key).map(|raw| (*key, raw))) else {
        return Err(ParamError::missing(ParamLocation::Query, display).with_expected(expected, expected_type));
    };
    parse_param(ParamLocation::Query, key, raw, expected, expected_type)
}

/// `&str` 类型的查询参数：不需要转换，只检查是否存在。
pub fn query_param_str<'a>(query: &'a HashMap<String, String>, name: &str) -> Result<&'a str, ParamError> {
    query_param_str_any(query, &[name], name)
}

/// `&str` 版本的 [`query_param_any`]。
pub fn query_param_str_any<'a>(
    query: &'a HashMap<String, String>,
    keys: &[&str],
    display: &str,
) -> Result<&'a str, ParamError> {
    keys.iter()
        .find_map(|key| query.get(*key))
        .map(String::as_str)
        .ok_or_else(|| ParamError::missing(ParamLocation::Query, display).with_expected(Some("string"), "&str"))
}

/// 请求体的 content-type 这个路由不支持。
pub fn unsupported_content_type(content_type: &str, expected_type: &'static str) -> ParamError {
    ParamError::new(ParamLocation::Body, None, ParamErrorKind::UnsupportedContentType)
        .with_expected(None, expected_type)
        .with_raw_value(content_type)
}

/// 读取请求体失败（连接中断等）。
pub fn body_read_error(reason: impl ToString) -> ParamError {
    ParamError::new(ParamLocation::Body, None, ParamErrorKind::MalformedBody).with_reason(reason.to_string())
}

/// 按 JSON 反序列化请求体；出错时借助 serde_path_to_error 定位到具体字段。
pub fn json_body<'de, T: Deserialize<'de>>(bytes: &'de [u8], expected_type: &'static str) -> Result<T, ParamError> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let value = serde_path_to_error::deserialize(&mut de)
        .map_err(|e| json_error(Some(e.path()), e.inner(), expected_type))?;
    // 跟 serde_json::from_slice 一样，拒绝合法 JSON 后面还跟着多余内容
    de.end().map_err(|e| json_error(None, &e, expected_type))?;
    Ok(value)
}

fn json_error(path: Option<&Path>, err: &serde_json::Error, expected_type: &'static str) -> ParamError {
    use serde_json::error::Category;

    let position = format!("line {}, column {}", err.line(), err.column());
    let error = match err.classify() {
        Category::Data => {
            let message = err.to_string();
            data_error(ParamLocation::Body, path, &message)
                .with_detail(position)
        }
        Category::Syntax => ParamError::new(ParamLocation::Body, None, ParamErrorKind::MalformedBody)
            .with_detail(format!("invalid JSON at {position}")),
        Category::Eof => ParamError::new(ParamLocation::Body, None, ParamErrorKind::MalformedBody)
            .with_detail(format!("unexpected end of JSON at {position}")),
        Category::Io => ParamError::new(ParamLocation::Body, None, ParamErrorKind::MalformedBody),
    };
    error.with_expected(None, expected_type).with_reason(err)
}

/// 按 `application/x-www-form-urlencoded` 反序列化请求体。
pub fn urlencoded_body<'de, T: Deserialize<'de>>(bytes: &'de [u8], expected_type: &'static str) -> Result<T, ParamError> {
    urlencoded_struct(bytes, ParamLocation::Body, expected_type)
}

/// [`Query<T>`] 参数：把整个查询串（不论请求方法）反序列化到结构体 `T`。
/// 没有查询串时按空串处理（全是 `Option` / `#[serde(default)]` 字段的结构体可以成功）。
/// `T` 必须是 `DeserializeOwned`：查询串属于请求，而请求体参数会在之后消费掉整个请求。
pub fn query_struct<T: DeserializeOwned>(query: Option<&str>, expected_type: &'static str) -> Result<Query<T>, ParamError> {
    urlencoded_struct(query.unwrap_or("").as_bytes(), ParamLocation::Query, expected_type).map(Query)
}

/// urlencoded 格式（表单请求体 / 查询串共用）反序列化，失败时定位到具体字段并回显原始值。
fn urlencoded_struct<'de, T: Deserialize<'de>>(
    bytes: &'de [u8],
    location: ParamLocation,
    expected_type: &'static str,
) -> Result<T, ParamError> {
    let de = serde_urlencoded::Deserializer::new(form_urlencoded::parse(bytes));
    serde_path_to_error::deserialize(de).map_err(|e| {
        let message = e.inner().to_string();
        let mut error = data_error(location, Some(e.path()), &message);
        // urlencoded 的值都是字符串，可以按字段名把客户端传的原始值找回来用于回显
        if error.kind != ParamErrorKind::Missing {
            if let Some(key) = single_map_key(e.path()) {
                if let Some((_, raw)) = form_urlencoded::parse(bytes).filter(|(k, _)| k == key).last() {
                    error = error.with_raw_value(raw.into_owned());
                }
            }
        }
        error.with_expected(None, expected_type).with_reason(message)
    })
}

/// serde 的“数据错误”（类型不匹配、缺字段等）转换成 ParamError。
///
/// serde 的错误只有文本，没有结构化的错误种类；这里只识别两种格式稳定的文本：
/// - serde 默认的 `missing field `xxx``：缺字段时 path 指向父级，字段名只在文本里
/// - 标准库整数解析的溢出文案
/// 其它一律归为 Invalid，具体原因只进 `reason`，不进默认文案。
fn data_error(location: ParamLocation, path: Option<&Path>, message: &str) -> ParamError {
    let path = path.filter(|p| p.iter().next().is_some()).map(|p| p.to_string());

    if let Some(field) = missing_field_name(message) {
        let param = match path {
            Some(parent) => format!("{parent}.{field}"),
            None => field.to_string(),
        };
        return ParamError::missing(location, param);
    }

    let kind = if message.contains("number too large to fit in target type")
        || message.contains("number too small to fit in target type")
    {
        ParamErrorKind::OutOfRange
    } else {
        ParamErrorKind::Invalid
    };
    ParamError::new(location, path, kind)
}

fn missing_field_name(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("missing field `")?;
    rest.split_once('`').map(|(field, _)| field)
}

fn single_map_key(path: &Path) -> Option<&str> {
    let mut segments = path.iter();
    match (segments.next(), segments.next()) {
        (Some(Segment::Map { key }), None) => Some(key),
        _ => None,
    }
}

/// 取出一个 multipart 字段（文件或文本），没传则报 Missing。
pub fn multipart_take(
    fields: &mut HashMap<String, MultipartField>,
    name: &str,
    expected_type: &'static str,
) -> Result<MultipartField, ParamError> {
    multipart_take_any(fields, &[name], name, expected_type)
}

/// 按候选名依次取出 multipart 字段（前面的优先），用法同 [`query_param_any`]。
pub fn multipart_take_any(
    fields: &mut HashMap<String, MultipartField>,
    keys: &[&str],
    display: &str,
    expected_type: &'static str,
) -> Result<MultipartField, ParamError> {
    take_any(fields, keys).map(|(_, field)| field).ok_or_else(|| {
        ParamError::missing(ParamLocation::MultipartField, display).with_expected(Some("file"), expected_type)
    })
}

fn take_any<'k>(fields: &mut HashMap<String, MultipartField>, keys: &[&'k str]) -> Option<(&'k str, MultipartField)> {
    keys.iter().find_map(|key| fields.remove(*key).map(|field| (*key, field)))
}

/// 取出一个 multipart 文本字段并转换成目标类型。
pub fn multipart_text<T>(
    fields: &mut HashMap<String, MultipartField>,
    name: &str,
    expected: Option<&'static str>,
    expected_type: &'static str,
) -> Result<T, ParamError>
where
    T: FromStr,
    T::Err: Display + 'static,
{
    multipart_text_any(fields, &[name], name, expected, expected_type)
}

/// 按候选名依次取出 multipart 文本字段并转换（前面的优先），用法同 [`query_param_any`]。
pub fn multipart_text_any<T>(
    fields: &mut HashMap<String, MultipartField>,
    keys: &[&str],
    display: &str,
    expected: Option<&'static str>,
    expected_type: &'static str,
) -> Result<T, ParamError>
where
    T: FromStr,
    T::Err: Display + 'static,
{
    let Some((name, field)) = take_any(fields, keys) else {
        return Err(ParamError::missing(ParamLocation::MultipartField, display).with_expected(expected, expected_type));
    };
    let text = String::from_utf8(field.data).map_err(|e| {
        // 非 UTF-8 的内容不回显
        ParamError::new(ParamLocation::MultipartField, Some(name.to_string()), ParamErrorKind::Invalid)
            .with_expected(expected, expected_type)
            .with_detail("not valid UTF-8 text")
            .with_reason(e)
    })?;
    parse_param(ParamLocation::MultipartField, name, &text, expected, expected_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct Form {
        id: i64,
        age: u8,
    }

    #[test]
    fn parse_param_classifies_overflow() {
        let e = parse_param::<u8>(ParamLocation::Path, "age", "300", Some("integer"), "u8").unwrap_err();
        assert_eq!(e.kind, ParamErrorKind::OutOfRange);
        let e = parse_param::<u8>(ParamLocation::Path, "age", "x", Some("integer"), "u8").unwrap_err();
        assert_eq!(e.kind, ParamErrorKind::Invalid);
        assert_eq!(e.reason.as_deref(), Some("invalid digit found in string"));
    }

    #[test]
    fn json_missing_and_invalid_fields() {
        let e = json_body::<Form>(br#"{"id": 1}"#, "Form").unwrap_err();
        assert_eq!(e.kind, ParamErrorKind::Missing);
        assert_eq!(e.to_string(), r#"Field "age" in request body is required."#);

        let e = json_body::<Form>(br#"{"id": "x", "age": 1}"#, "Form").unwrap_err();
        assert_eq!(e.kind, ParamErrorKind::Invalid);
        assert_eq!(e.param.as_deref(), Some("id"));
        assert!(!e.to_string().contains("i64"), "{e}");

        let e = json_body::<Form>(br#"{"id": 1,"#, "Form").unwrap_err();
        assert_eq!(e.kind, ParamErrorKind::MalformedBody);
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct Search {
        name: String,
        age: u8,
        tag: Option<String>,
    }

    #[test]
    fn lookup_by_candidate_names() {
        let query: HashMap<String, String> =
            [("pageSize", "20"), ("page_size", "10"), ("sortBy", "x")].map(|(k, v)| (k.to_string(), v.to_string())).into();
        // 原名优先
        assert_eq!(query_param_any::<u32>(&query, &["page_size", "pageSize"], "pageSize", None, "u32").unwrap(), 10);
        assert_eq!(query_param_str_any(&query, &["sort_by", "sortBy"], "sortBy").unwrap(), "x");
        // 缺失时用 display 名；值不合法时用实际匹配到的名字
        let e = query_param_any::<u32>(&query, &["limit_num", "limitNum"], "limitNum", None, "u32").unwrap_err();
        assert_eq!(e.param.as_deref(), Some("limitNum"));
        let e = query_param_any::<u32>(&query, &["sort_by", "sortBy"], "sortBy", Some("integer"), "u32").unwrap_err();
        assert_eq!(e.to_string(), r#"Query parameter "sortBy" must be an integer, got "x"."#);

        let field = |v: &str| MultipartField { file_name: None, content_type: None, data: v.as_bytes().to_vec() };
        let mut fields: HashMap<String, MultipartField> = [("userId".to_string(), field("7"))].into();
        assert_eq!(multipart_text_any::<i64>(&mut fields, &["user_id", "userId"], "userId", None, "i64").unwrap(), 7);
        let e = multipart_take_any(&mut fields, &["avatar_file", "avatarFile"], "avatarFile", "Vec<u8>").err().unwrap();
        assert_eq!(e.to_string(), r#"Form field "avatarFile" is required."#);
    }

    #[test]
    fn query_struct_binding() {
        // 其它查询参数（例如同时被标量参数使用的 id）不影响结构体
        let q = query_struct::<Search>(Some("id=10&name=A%20A&age=10"), "Search").unwrap();
        assert_eq!((q.name.as_str(), q.age, q.tag.as_deref()), ("A A", 10, None));

        let e = query_struct::<Search>(Some("name=AA&age=x"), "Search").unwrap_err();
        assert_eq!(e.location, ParamLocation::Query);
        assert_eq!(e.to_string(), r#"Query parameter "age" is invalid, got "x"."#);

        let e = query_struct::<Search>(Some("name=AA&age=300"), "Search").unwrap_err();
        assert_eq!(e.kind, ParamErrorKind::OutOfRange);

        let e = query_struct::<Search>(None, "Search").unwrap_err();
        assert_eq!(e.to_string(), r#"Query parameter "name" is required."#);
    }

    #[test]
    fn urlencoded_echoes_raw_value() {
        let e = urlencoded_body::<Form>(b"id=1&age=300", "Form").unwrap_err();
        assert_eq!(e.kind, ParamErrorKind::OutOfRange);
        assert_eq!(e.to_string(), r#"Field "age" in request body is out of range, got "300"."#);

        let e = urlencoded_body::<Form>(b"age=3", "Form").unwrap_err();
        assert_eq!(e.to_string(), r#"Field "id" in request body is required."#);
    }
}
