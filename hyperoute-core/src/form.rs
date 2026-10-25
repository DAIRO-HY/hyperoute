//! 表单取值：标量 / `Option` / `Vec` 参数和表单结构体参数都从这里取值。
//!
//! - 默认（`ParamBinding::Separate`）：[`FormData`] 只包含查询串，同名 key 重复出现时 `Vec` 参数取全部值。
//! - 合并模式（`ParamBinding::Merged`）：和 Go 的 `request.Form` 一致，把查询串和表单请求体合并：
//!   urlencoded 请求体的值在前、查询串在后；multipart 是查询串在前、文本字段在后。
//!   另外兼容 `"[a,b,c]"` 写法：某个 key 只有一个值且首尾是 `[` `]` 时，去掉首尾后按逗号拆开，
//!   每项里的 `_2c` 还原成 `,`；标量参数取拆开后的第一项。
//!
//! 宽松模式（`ParamFallback::Default` / `#[PARAM_LENIENT]`）下，参数缺失或转换失败时得到
//! `Default::default()`（`Option` 得到 `None`），和 Go 版忽略转换错误的行为一致。

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{self, Display};
use std::str::FromStr;

use hyper::body::Bytes;
use serde::de::{self, DeserializeOwned, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};

use crate::extract::{parse_param, unsupported_content_type};
use crate::multipart::{read_body_limited, read_multipart_list};
use crate::param::{ParamError, ParamErrorKind, ParamLocation};
use crate::{MultipartField, Request};

#[derive(Debug, Clone)]
struct FormPair {
    key: String,
    value: String,
    location: ParamLocation,
}

/// 按出现顺序保存的 `key -> value` 列表，同名 key 可以出现多次。
#[derive(Debug, Clone, Default)]
pub struct FormData {
    pairs: Vec<FormPair>,
    merged: bool,
}

impl FormData {
    /// 只包含查询串（默认模式）。
    pub fn from_query(query: Option<&str>) -> Self {
        let mut form = Self::default();
        form.push_urlencoded(query.unwrap_or("").as_bytes(), ParamLocation::Query);
        form
    }

    /// 合并模式下只取查询串（例如 WebSocket 路由、带 `BodyStream` 参数的路由，请求体不读取）。
    pub fn merged_query(query: Option<&str>) -> Self {
        let mut form = Self::from_query(query);
        form.merged = true;
        form
    }

    /// 是否是合并模式（决定是否兼容 `"[a,b,c]"` 写法，以及缺参数时报错的位置）。
    pub fn is_merged(&self) -> bool {
        self.merged
    }

    /// 追加一个值，排在已有的值后面。
    pub fn push(&mut self, key: impl Into<String>, value: impl Into<String>, location: ParamLocation) {
        self.pairs.push(FormPair { key: key.into(), value: value.into(), location });
    }

    fn push_urlencoded(&mut self, bytes: &[u8], location: ParamLocation) {
        for (key, value) in form_urlencoded::parse(bytes) {
            self.push(key, value, location);
        }
    }

    /// 某个 key 的第一个原始值。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs.iter().find(|p| p.key == key).map(|p| p.value.as_str())
    }

    /// 某个 key 的全部原始值，按出现顺序。
    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.pairs.iter().filter(move |p| p.key == key).map(|p| p.value.as_str())
    }

    fn contains(&self, key: &str) -> bool {
        self.pairs.iter().any(|p| p.key == key)
    }

    /// 按候选名依次查找（前面的优先），返回匹配到的名字和展开后的值。
    fn lookup<'k>(&self, keys: &[&'k str]) -> Option<(&'k str, Vec<Item<'_>>)> {
        keys.iter().find_map(|key| {
            let pairs: Vec<&FormPair> = self.pairs.iter().filter(|p| p.key == *key).collect();
            (!pairs.is_empty()).then(|| (*key, expand(pairs, self.merged)))
        })
    }

    fn missing_location(&self) -> ParamLocation {
        if self.merged { ParamLocation::Form } else { ParamLocation::Query }
    }

    fn missing(&self, display: &str, expected: Option<&'static str>, expected_type: &'static str) -> ParamError {
        ParamError::missing(self.missing_location(), display).with_expected(expected, expected_type)
    }
}

/// 一个展开后的值：`"[a,b]"` 拆开后的每一项都记着原始值来自哪里（错误信息里用）。
#[derive(Debug, Clone)]
struct Item<'a> {
    text: Cow<'a, str>,
    location: ParamLocation,
}

/// 合并模式下兼容 `"[a,b,c]"`：只有一个值且首尾是 `[` `]` 时拆开（和 Go 的 getStringArray 一致，
/// 所以 `"[]"` 得到一个空字符串）。
fn expand(pairs: Vec<&FormPair>, brackets: bool) -> Vec<Item<'_>> {
    if brackets && pairs.len() == 1 {
        let pair = pairs[0];
        if let Some(inner) = pair.value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            return inner
                .split(',')
                .map(|part| Item { text: Cow::Owned(part.replace("_2c", ",")), location: pair.location })
                .collect();
        }
    }
    pairs.into_iter().map(|p| Item { text: Cow::Borrowed(p.value.as_str()), location: p.location }).collect()
}

/// 标量参数：取第一个值并转换。
pub fn form_value<T>(
    form: &FormData,
    keys: &[&str],
    display: &str,
    expected: Option<&'static str>,
    expected_type: &'static str,
) -> Result<T, ParamError>
where
    T: FromStr,
    T::Err: Display + 'static,
{
    match form_opt(form, keys, expected, expected_type)? {
        Some(value) => Ok(value),
        None => Err(form.missing(display, expected, expected_type)),
    }
}

/// `Option<标量>` 参数：没传是 `None`，传了但转换失败仍然报错（宽松模式下由 [`lenient_opt`] 转成 `None`）。
pub fn form_opt<T>(
    form: &FormData,
    keys: &[&str],
    expected: Option<&'static str>,
    expected_type: &'static str,
) -> Result<Option<T>, ParamError>
where
    T: FromStr,
    T::Err: Display + 'static,
{
    let Some((key, items)) = form.lookup(keys) else {
        return Ok(None);
    };
    let item = &items[0];
    parse_param(item.location, key, &item.text, expected, expected_type).map(Some)
}

/// `&str` 参数（默认模式）：直接借用第一个原始值，不做转换。
pub fn form_str<'a>(form: &'a FormData, keys: &[&str], display: &str) -> Result<&'a str, ParamError> {
    form_opt_str(form, keys)?.ok_or_else(|| form.missing(display, Some("string"), "&str"))
}

/// `Option<&str>` 参数（默认模式）。
pub fn form_opt_str<'a>(form: &'a FormData, keys: &[&str]) -> Result<Option<&'a str>, ParamError> {
    Ok(keys.iter().find_map(|key| form.get(key)))
}

/// `Vec<标量>` 参数：取全部值；没传是空 `Vec`。
pub fn form_vec<T>(
    form: &FormData,
    keys: &[&str],
    expected: Option<&'static str>,
    expected_type: &'static str,
) -> Result<Vec<T>, ParamError>
where
    T: FromStr,
    T::Err: Display + 'static,
{
    let Some((key, items)) = form.lookup(keys) else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| parse_param(item.location, key, &item.text, expected, expected_type))
        .collect()
}

/// 宽松模式的 `Vec<标量>`：转换失败的项得到 `Default::default()`（和 Go 的 getIntArray 一致）。
pub fn form_vec_lenient<T>(
    route: &'static str,
    form: &FormData,
    keys: &[&str],
    expected: Option<&'static str>,
    expected_type: &'static str,
) -> Vec<T>
where
    T: FromStr + Default,
    T::Err: Display + 'static,
{
    let Some((key, items)) = form.lookup(keys) else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| lenient(route, parse_param(item.location, key, &item.text, expected, expected_type)))
        .collect()
}

/// 宽松模式：参数缺失或转换失败时得到 `Default::default()`。
pub fn lenient<T: Default>(route: &'static str, result: Result<T, ParamError>) -> T {
    result.unwrap_or_else(|error| {
        note_fallback(route, &error);
        T::default()
    })
}

/// 宽松模式的 `Option` 参数：转换失败时得到 `None`。
pub fn lenient_opt<T>(route: &'static str, result: Result<Option<T>, ParamError>) -> Option<T> {
    result.unwrap_or_else(|error| {
        note_fallback(route, &error);
        None
    })
}

/// 宽松模式用了默认值：只在 debug 构建里记一条日志，方便排查是哪个客户端漏传了参数。
fn note_fallback(route: &str, error: &ParamError) {
    if cfg!(debug_assertions) {
        eprintln!("[hyperoute] lenient fallback to default on {route}: {error}");
    }
}

/// 表单结构体的解析选项，由生成代码按 Scanner 开关和 handler 上的属性填写。
#[derive(Debug, Clone, Copy, Default)]
pub struct FormOptions {
    /// 缺字段 / 转换失败时得到默认值（不会覆盖 serde 的 `default`）
    pub lenient: bool,
    /// `ParamNaming::CamelCase`：字段 `page_size` 也接受 `pageSize`（原名优先，前导下划线保留）
    pub camel: bool,
}

/// 表单结构体参数：按字段名从 [`FormData`] 取值，反序列化到 `T`（需要 `Deserialize`）。
/// 支持标量、`Option`、`Vec`（同名 key 重复 / 合并模式下的 `"[a,b]"`）、枚举（单元变体）和 serde 的改名属性。
pub fn form_struct<T: DeserializeOwned>(
    route: &'static str,
    form: &FormData,
    expected_type: &'static str,
    options: FormOptions,
) -> Result<T, ParamError> {
    // 宽松模式下缺字段：把它补成“空值”（得到默认值）再试一次。先让 serde 自己处理缺失字段，
    // 这样字段上的 `#[serde(default = ...)]` 仍然生效，只有 serde 报缺失的字段才补。
    let mut synthesized: Vec<&'static str> = Vec::new();
    loop {
        let de = StructDeserializer { route, form, options, synthesized: &synthesized };
        match T::deserialize(de) {
            Ok(value) => return Ok(value),
            Err(FormError::Missing(field)) if options.lenient && !synthesized.contains(&field) => {
                note_fallback(route, &form.missing(field, None, expected_type));
                synthesized.push(field);
            }
            Err(FormError::Missing(field)) => return Err(form.missing(field, None, expected_type)),
            Err(FormError::Param(error)) => return Err(error.with_expected_type(expected_type)),
            Err(FormError::Custom(message)) => {
                return Err(ParamError::new(form.missing_location(), None, ParamErrorKind::Invalid)
                    .with_expected(None, expected_type)
                    .with_reason(message));
            }
        }
    }
}

impl ParamError {
    /// 只在还没有记录 Rust 类型时补上（字段级错误已经有字段类型了）。
    fn with_expected_type(mut self, expected_type: &'static str) -> Self {
        if self.expected_type.is_empty() {
            self.expected_type = expected_type;
        }
        self
    }
}

/// 驼峰名：`page_size` → `pageSize`，前导下划线保留（`_client_flag` → `_clientFlag`）。
/// 和 hyperoute-codegen 里 `ParamNaming::CamelCase` 的规则一致。
pub fn param_camel_case(name: &str) -> String {
    let rest = name.trim_start_matches('_');
    let mut out = String::with_capacity(name.len());
    out.push_str(&name[..name.len() - rest.len()]);
    let mut capitalize = false;
    for ch in rest.chars() {
        if ch == '_' {
            capitalize = true;
        } else if capitalize {
            out.extend(ch.to_uppercase());
            capitalize = false;
        } else {
            out.push(ch);
        }
    }
    out
}

/// 读取请求，按合并模式组装 [`FormData`]；请求体只读一次，multipart 的全部字段（含文件）放在 `files` 里。
pub struct FormRequest {
    pub form: FormData,
    pub content_type: String,
    /// 原始请求体（JSON 结构体参数用）；multipart 和其它不需要读取的 content-type 为空
    pub body: Bytes,
    /// multipart 字段（文件和文本都在），同名字段只保留最后一个
    pub files: HashMap<String, MultipartField>,
}

impl FormRequest {
    /// 合并模式：urlencoded（或没有 content-type）时请求体在前、查询串在后；
    /// multipart 时查询串在前、文本字段在后（边收边解析，大文件转存到临时文件）；
    /// JSON 只读取请求体、不参与合并；其它 content-type 不读取请求体，只取查询串。
    pub async fn read(req: Request) -> Result<Self, ParamError> {
        let query = req.uri().query().map(str::to_string);
        let content_type = content_type_of(&req);
        let urlencoded = content_type.is_empty() || content_type.starts_with("application/x-www-form-urlencoded");

        let mut form = FormData { pairs: Vec::new(), merged: true };
        let mut files = HashMap::new();
        let mut body = Bytes::new();
        if content_type.starts_with("multipart/form-data") {
            form.push_urlencoded(query.as_deref().unwrap_or("").as_bytes(), ParamLocation::Query);
            for (name, field) in read_multipart_list(req.into_body(), &content_type).await? {
                if field.file_name.is_none() {
                    if let Ok(text) = std::str::from_utf8(&field.data) {
                        form.push(name.clone(), text, ParamLocation::MultipartField);
                    }
                }
                files.insert(name, field);
            }
        } else {
            if urlencoded || content_type.starts_with("application/json") {
                body = read_body_limited(req.into_body()).await?;
            }
            if urlencoded {
                form.push_urlencoded(&body, ParamLocation::Body);
            }
            form.push_urlencoded(query.as_deref().unwrap_or("").as_bytes(), ParamLocation::Query);
        }
        Ok(Self { form, content_type, body, files })
    }

    /// 默认模式下 handler 有 multipart 文件参数：请求体必须是 multipart，`form` 只包含查询串。
    pub async fn read_multipart(req: Request, expected_type: &'static str) -> Result<Self, ParamError> {
        let form = FormData::from_query(req.uri().query());
        let content_type = content_type_of(&req);
        if !content_type.starts_with("multipart/form-data") {
            return Err(unsupported_content_type(&content_type, expected_type));
        }
        let files = read_multipart_list(req.into_body(), &content_type).await?.into_iter().collect();
        Ok(Self { form, content_type, body: Bytes::new(), files })
    }
}

fn content_type_of(req: &Request) -> String {
    req.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------------------------
// serde 反序列化器：把 FormData 当成一个 map，每个字段的值按目标类型转换。

#[derive(Debug)]
enum FormError {
    /// 已经定位到字段的错误
    Param(ParamError),
    /// serde 报缺字段
    Missing(&'static str),
    /// serde 的其它报错（未知字段、类型不匹配等），还不知道是哪个字段
    Custom(String),
}

impl Display for FormError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormError::Param(error) => error.fmt(f),
            FormError::Missing(field) => write!(f, "missing field `{field}`"),
            FormError::Custom(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for FormError {}

impl de::Error for FormError {
    fn custom<M: Display>(message: M) -> Self {
        FormError::Custom(message.to_string())
    }

    fn missing_field(field: &'static str) -> Self {
        FormError::Missing(field)
    }
}

struct StructDeserializer<'a> {
    route: &'static str,
    form: &'a FormData,
    options: FormOptions,
    synthesized: &'a [&'static str],
}

impl<'de, 'a> de::Deserializer<'de> for StructDeserializer<'a> {
    type Error = FormError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        self.deserialize_struct("", &[], visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, FormError> {
        let form = self.form;
        let mut keys: Vec<&str> = Vec::new();
        for pair in &form.pairs {
            if !keys.contains(&pair.key.as_str()) {
                keys.push(&pair.key);
            }
        }

        let mut entries: Vec<(Cow<'a, str>, Option<&'a str>)> = Vec::new();
        for key in keys {
            let mut name = Cow::Borrowed(key);
            if self.options.camel && !fields.contains(&key) {
                // `pageSize` 对应字段 `page_size`；原名也传了时原名优先，这个驼峰 key 忽略
                if let Some(field) = fields.iter().find(|f| **f != key && param_camel_case(f) == key) {
                    if form.contains(field) {
                        continue;
                    }
                    name = Cow::Borrowed(*field);
                }
            }
            entries.push((name, Some(key)));
        }
        for field in self.synthesized {
            if !entries.iter().any(|(name, _)| name == field) {
                entries.push((Cow::Borrowed(*field), None));
            }
        }

        visitor.visit_map(FormMap {
            route: self.route,
            form,
            lenient: self.options.lenient,
            entries: entries.into_iter(),
            current: None,
        })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option unit
        unit_struct newtype_struct seq tuple tuple_struct map enum identifier ignored_any
    }
}

struct FormMap<'a> {
    route: &'static str,
    form: &'a FormData,
    lenient: bool,
    /// (交给 serde 的字段名, 请求里实际的 key；None 表示宽松模式补上的缺失字段)
    entries: std::vec::IntoIter<(Cow<'a, str>, Option<&'a str>)>,
    current: Option<Option<&'a str>>,
}

impl<'de, 'a> MapAccess<'de> for FormMap<'a> {
    type Error = FormError;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, FormError> {
        let Some((name, key)) = self.entries.next() else {
            return Ok(None);
        };
        self.current = Some(key);
        seed.deserialize(de::value::StrDeserializer::<FormError>::new(&name)).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, FormError> {
        let key = self.current.take().expect("next_value_seed 之前必须先调用 next_key_seed");
        let value = match key {
            Some(key) => {
                let pairs: Vec<&FormPair> = self.form.pairs.iter().filter(|p| p.key == key).collect();
                FormValue { route: self.route, key, items: expand(pairs, self.form.merged), lenient: self.lenient }
            }
            None => FormValue { route: self.route, key: "", items: Vec::new(), lenient: true },
        };
        let (key, first) = (value.key, value.items.first().cloned());
        seed.deserialize(value).map_err(|error| match error {
            // 枚举取值不对等 serde 自己报的错：补上字段名和原始值
            FormError::Custom(message) if !key.is_empty() => {
                let location = first.as_ref().map_or(self.form.missing_location(), |i| i.location);
                let mut error = ParamError::new(location, Some(key.to_string()), ParamErrorKind::Invalid).with_reason(message);
                if let Some(item) = first {
                    error = error.with_raw_value(item.text.into_owned());
                }
                FormError::Param(error)
            }
            other => other,
        })
    }
}

/// 一个字段的值（可能有多个，`Vec` 字段用）；`items` 为空表示宽松模式下补上的缺失字段。
struct FormValue<'a> {
    route: &'static str,
    key: &'a str,
    items: Vec<Item<'a>>,
    lenient: bool,
}

impl FormValue<'_> {
    fn parse<T>(&self, expected: &'static str, expected_type: &'static str) -> Result<T, FormError>
    where
        T: FromStr + Default,
        T::Err: Display + 'static,
    {
        let Some(item) = self.items.first() else {
            return Ok(T::default());
        };
        match parse_param(item.location, self.key, &item.text, Some(expected), expected_type) {
            Ok(value) => Ok(value),
            Err(error) if self.lenient => {
                note_fallback(self.route, &error);
                Ok(T::default())
            }
            Err(error) => Err(FormError::Param(error)),
        }
    }

    fn text(&self) -> String {
        self.items.first().map(|i| i.text.to_string()).unwrap_or_default()
    }
}

macro_rules! deserialize_parsed {
    ($($method:ident => $ty:ty, $visit:ident, $expected:literal;)*) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
                visitor.$visit(self.parse::<$ty>($expected, stringify!($ty))?)
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for FormValue<'_> {
    type Error = FormError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        match self.items.len() {
            0 => visitor.visit_unit(),
            1 => visitor.visit_string(self.text()),
            _ => self.deserialize_seq(visitor),
        }
    }

    deserialize_parsed! {
        deserialize_bool => bool, visit_bool, "boolean";
        deserialize_i8 => i8, visit_i8, "integer";
        deserialize_i16 => i16, visit_i16, "integer";
        deserialize_i32 => i32, visit_i32, "integer";
        deserialize_i64 => i64, visit_i64, "integer";
        deserialize_i128 => i128, visit_i128, "integer";
        deserialize_u8 => u8, visit_u8, "integer";
        deserialize_u16 => u16, visit_u16, "integer";
        deserialize_u32 => u32, visit_u32, "integer";
        deserialize_u64 => u64, visit_u64, "integer";
        deserialize_u128 => u128, visit_u128, "integer";
        deserialize_f32 => f32, visit_f32, "number";
        deserialize_f64 => f64, visit_f64, "number";
        deserialize_char => char, visit_char, "single character";
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        visitor.visit_string(self.text())
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        visitor.visit_string(self.text())
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        visitor.visit_byte_buf(self.text().into_bytes())
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        visitor.visit_byte_buf(self.text().into_bytes())
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        if self.items.is_empty() { visitor.visit_none() } else { visitor.visit_some(self) }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, FormError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, FormError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        let FormValue { route, key, items, lenient } = self;
        visitor.visit_seq(FormSeq { route, key, items: items.into_iter(), lenient })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, FormError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, FormError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, FormError> {
        Err(de::Error::custom("nested structures are not supported in form data"))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, FormError> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, FormError> {
        let text: de::value::StringDeserializer<FormError> = self.text().into_deserializer();
        de::Deserializer::deserialize_enum(text, name, variants, visitor)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormError> {
        visitor.visit_unit()
    }
}

struct FormSeq<'a> {
    route: &'static str,
    key: &'a str,
    items: std::vec::IntoIter<Item<'a>>,
    lenient: bool,
}

impl<'de, 'a> SeqAccess<'de> for FormSeq<'a> {
    type Error = FormError;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, FormError> {
        let Some(item) = self.items.next() else {
            return Ok(None);
        };
        let value = FormValue { route: self.route, key: self.key, items: vec![item], lenient: self.lenient };
        seed.deserialize(value).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn form(pairs: &[(&str, &str, ParamLocation)], merged: bool) -> FormData {
        let mut form = FormData { pairs: Vec::new(), merged };
        for (k, v, l) in pairs {
            form.push(*k, *v, *l);
        }
        form
    }

    const Q: ParamLocation = ParamLocation::Query;
    const B: ParamLocation = ParamLocation::Body;

    #[test]
    fn scalar_vec_and_brackets() {
        let f = form(&[("id", "1", B), ("id", "2", Q), ("tags", "[a,b_2cc]", Q), ("one", "[7]", Q)], true);
        assert_eq!(form_value::<i64>(&f, &["id"], "id", None, "i64").unwrap(), 1);
        assert_eq!(form_vec::<i64>(&f, &["id"], None, "i64").unwrap(), [1, 2]);
        assert_eq!(form_vec::<String>(&f, &["tags"], None, "String").unwrap(), ["a", "b,c"]);
        // 和 Go 一致：标量也按 "[...]" 拆开取第一项
        assert_eq!(form_value::<i64>(&f, &["one"], "one", None, "i64").unwrap(), 7);
        assert_eq!(form_value::<String>(&f, &["tags"], "tags", None, "String").unwrap(), "a");
        assert_eq!(form_vec::<i64>(&f, &["none"], None, "i64").unwrap(), Vec::<i64>::new());
        assert_eq!(form_opt::<i64>(&f, &["none"], None, "i64").unwrap(), None);

        // 默认模式不拆 "[...]"
        let f = form(&[("tags", "[a,b]", Q)], false);
        assert_eq!(form_vec::<String>(&f, &["tags"], None, "String").unwrap(), ["[a,b]"]);
        assert_eq!(form_str(&f, &["tags"], "tags").unwrap(), "[a,b]");

        // "[]" 和 Go 一样得到一个空字符串
        let f = form(&[("ids", "[]", Q)], true);
        assert_eq!(form_vec::<String>(&f, &["ids"], None, "String").unwrap(), [""]);
    }

    #[test]
    fn errors_and_lenient() {
        let f = form(&[("age", "x", B)], true);
        let e = form_value::<u8>(&f, &["age"], "age", Some("integer"), "u8").unwrap_err();
        assert_eq!(e.to_string(), r#"Field "age" in request body must be an integer, got "x"."#);
        let e = form_value::<u8>(&f, &["page_size", "pageSize"], "pageSize", Some("integer"), "u8").unwrap_err();
        assert_eq!(e.to_string(), r#"Parameter "pageSize" is required."#);
        assert_eq!(lenient::<u8>("r", form_value(&f, &["age"], "age", None, "u8")), 0);
        assert_eq!(lenient_opt::<u8>("r", form_opt(&f, &["age"], None, "u8")), None);
        let f = form(&[("ids", "[1,x,3]", Q)], true);
        assert_eq!(form_vec_lenient::<i64>("r", &f, &["ids"], None, "i64"), [1, 0, 3]);
        assert!(form_vec::<i64>(&f, &["ids"], None, "i64").is_err());

        let f = FormData::from_query(Some("a=1"));
        let e = form_value::<u8>(&f, &["b"], "b", None, "u8").unwrap_err();
        assert_eq!(e.to_string(), r#"Query parameter "b" is required."#);
    }

    #[derive(Debug, Deserialize, PartialEq, Default)]
    struct Upload {
        folder: String,
        page_size: u32,
        ids: Vec<i64>,
        tag: Option<String>,
        _client_flag: i32,
        #[serde(default = "seven")]
        retry: u8,
        kind: Kind,
    }

    #[derive(Debug, Deserialize, PartialEq, Default)]
    #[serde(rename_all = "lowercase")]
    enum Kind {
        #[default]
        File,
        Folder,
    }

    fn seven() -> u8 {
        7
    }

    const STRICT: FormOptions = FormOptions { lenient: false, camel: true };
    const LENIENT: FormOptions = FormOptions { lenient: true, camel: true };

    #[test]
    fn struct_binding() {
        let f = form(
            &[
                ("folder", "/a", B),
                ("pageSize", "20", Q),
                ("ids", "[1,2]", Q),
                ("_clientFlag", "3", Q),
                ("kind", "folder", Q),
                ("other", "x", Q),
            ],
            true,
        );
        let u: Upload = form_struct("r", &f, "Upload", STRICT).unwrap();
        assert_eq!(
            u,
            Upload {
                folder: "/a".into(),
                page_size: 20,
                ids: vec![1, 2],
                tag: None,
                _client_flag: 3,
                retry: 7,
                kind: Kind::Folder
            }
        );

        // 原名和驼峰名都传了：原名优先
        let f = form(&[("folder", "", Q), ("pageSize", "1", Q), ("page_size", "2", Q), ("ids", "5", Q), ("ids", "6", Q), ("_client_flag", "0", Q), ("kind", "file", Q)], false);
        let u: Upload = form_struct("r", &f, "Upload", STRICT).unwrap();
        assert_eq!((u.page_size, u.ids), (2, vec![5, 6]));
    }

    #[test]
    fn struct_errors_and_lenient() {
        let f = form(&[("folder", "/a", Q), ("pageSize", "x", B), ("kind", "zip", Q)], true);
        let e = form_struct::<Upload>("r", &f, "Upload", STRICT).unwrap_err();
        assert_eq!(e.to_string(), r#"Field "pageSize" in request body must be an integer, got "x"."#);
        assert_eq!(e.expected_type, "u32");

        let f = form(&[("folder", "/a", Q)], true);
        let e = form_struct::<Upload>("r", &f, "Upload", STRICT).unwrap_err();
        assert_eq!(e.to_string(), r#"Parameter "page_size" is required."#);

        let f = form(&[("folder", "/a", Q), ("page_size", "1", Q), ("ids", "[]", Q), ("_client_flag", "1", Q), ("kind", "zip", Q)], true);
        let e = form_struct::<Upload>("r", &f, "Upload", STRICT).unwrap_err();
        assert_eq!(e.param.as_deref(), Some("ids"));

        // 宽松：缺字段 / 转换失败都是默认值，serde 的 default 仍然生效；枚举没有通用的默认值，取值不对仍然报错
        let f = form(&[("pageSize", "x", B), ("ids", "[1,x]", Q), ("kind", "file", Q)], true);
        let u: Upload = form_struct("r", &f, "Upload", LENIENT).unwrap();
        assert_eq!(u, Upload { ids: vec![1, 0], retry: 7, ..Default::default() });
        let f = form(&[("kind", "zip", Q)], true);
        let e = form_struct::<Upload>("r", &f, "Upload", LENIENT).unwrap_err();
        assert_eq!(e.to_string(), r#"Query parameter "kind" is invalid, got "zip"."#);
    }

    #[test]
    fn camel_case_keeps_leading_underscore() {
        assert_eq!(param_camel_case("_client_flag"), "_clientFlag");
        assert_eq!(param_camel_case("page_size"), "pageSize");
        assert_eq!(param_camel_case("__a_b"), "__aB");
        assert_eq!(param_camel_case("id"), "id");
    }
}
