//! 给 `build.rs` 用的路由扫描 + 代码生成逻辑。
//! 不是 proc-macro（那部分在 hyperoute-macro 里），是普通 lib，
//! 由使用方的 build.rs 作为 build-dependency 调用。

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use syn::{FieldsNamed, FnArg, Item, ItemFn, Pat, ReturnType};

mod html_include;
mod static_dir;

pub use static_dir::{Embed, StaticDir};

/// 所有识别的属性名；REQUEST 表示匹配任意 HTTP 方法。
const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "CONNECT", "TRACE", "REQUEST",
];

/// 返回值被判定为“纯文本”的类型（原样当 body），其余类型一律按 JSON 序列化，
/// 除非类型名是 `Raw`/`Stream`（hyperoute_core 里定义），那时各自走专门的分流。
const TEXT_RETURN_TYPES: &[&str] = &["&'staticstr", "&str", "String"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnKind {
    Text,
    Json,
    /// hyperoute_core::Raw：一次性攒好的二进制 body + 自定义 header
    Raw,
    /// hyperoute_core::Stream：边读边发的流式 body + 自定义 header
    Stream,
}

#[derive(Debug, Clone)]
enum PathSegment {
    Literal(String),
    /// `{name}`
    Param(String),
}

/// 参数 / 字段类型在生成代码里需要的信息。
#[derive(Debug, Clone)]
struct TypeInfo {
    /// 签名里的类型原文，例如 "i64"；只用于日志（ParamError::expected_type）
    rust_type: String,
    /// 中性的类型描述，例如 "integer"；出现在默认错误文案里
    expected: Option<&'static str>,
    /// 是否是 `&str`：这种参数直接借用原始字符串，不做 `.parse()`
    is_str_ref: bool,
}

/// Body 参数结构体单个字段该按什么方式从 multipart 里取值。
/// 只有能在同一份源文件里找到 struct 定义时才会算出这个，用来支持 multipart/form-data。
#[derive(Debug, Clone)]
enum FieldKind {
    /// 普通值类型（String/数字等），当文本字段处理
    Plain(TypeInfo),
    /// `Vec<u8>`：整个文件内容直接放进这个字段
    RawBytes,
    /// `&Path`：文件内容写到临时文件，字段拿到这个临时文件的路径
    TempPath,
    /// `MultipartFile<Vec<u8>>` 或 `MultipartFile<&Path>`
    MultipartFile { inner_is_path: bool },
}

#[derive(Debug, Clone)]
struct BodyStructInfo {
    /// struct 的完整可调用路径，例如 "crate::controller::index::model::SampleInForm"
    full_path: String,
    /// 按声明顺序排列的 (字段名, 取值方式, 字段类型原文)
    fields: Vec<(String, FieldKind, String)>,
}

#[derive(Debug, Clone)]
enum ParamSource {
    /// 值来自路径段，绑定名与路径里的 `{name}` 一致
    Path(String, TypeInfo),
    /// 值来自 URL 查询串（`?name=value`），按参数名取值后 `.parse()`
    Query(String, TypeInfo),
    /// 值来自请求体；`struct_info` 是 Some 时才支持 multipart/form-data
    /// （要求这个类型的 struct 定义能在同一份源文件里找到，可以嵌套在 `mod {}` 里）
    Body {
        name: String,
        rust_type: String,
        struct_info: Option<BodyStructInfo>,
    },
    /// 值来自请求 extensions（过滤器放进去的数据）：参数类型是 `Ext<T>` 或 `Option<Ext<T>>`
    Ext { name: String, optional: bool },
    /// `#[WEBSOCKET]` 路由里的 `WebSocket` 参数：握手完成后由框架传入
    WebSocket { name: String },
    /// `Query<T>`：整个查询串解析到结构体 T（不论请求方法）；`inner_type` 只用于日志
    QueryStruct { name: String, inner_type: String },
}

impl ParamSource {
    fn name(&self) -> &str {
        match self {
            ParamSource::Path(name, _)
            | ParamSource::Query(name, _)
            | ParamSource::Body { name, .. }
            | ParamSource::Ext { name, .. }
            | ParamSource::WebSocket { name }
            | ParamSource::QueryStruct { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouteInfo {
    /// 例如 "GET /path_test/{id}"，写进 ParamError::route 和日志
    label: String,
    method: String,
    segments: Vec<PathSegment>,
    params: Vec<ParamSource>,
    fn_path: String,
    /// handler 是否返回 `Result<T, E>`；true 时按 Ok/Err 分别生成响应
    is_result: bool,
    /// 成功值（Result 时是 T，否则是整个返回类型）该按文本/JSON/二进制哪种方式建响应
    return_kind: ReturnKind,
    /// `#[WEBSOCKET]` 路由：握手后在后台调用 handler，返回值不进响应
    websocket: bool,
}

/// `#[PARAM_ERROR]` 标注的参数错误响应函数。
#[derive(Debug, Clone)]
struct ParamErrorHandler {
    fn_path: String,
    is_async: bool,
}

/// `#[FILTER(...)]` 标注的过滤器。
#[derive(Debug, Clone)]
struct FilterInfo {
    fn_path: String,
    /// 越小越先执行；相同时按 fn_path 字典序，保证每次编译顺序一致
    order: i64,
    /// 作用的路径模式；为空表示全部路径
    include: Vec<String>,
    exclude: Vec<String>,
    /// 是否返回 `Result<Response, E>`；Err 用 IntoErrorResponse 转成响应
    is_result: bool,
}

/// 扫描结果：所有路由、过滤器（已排好序）+ 可选的参数错误响应函数。
#[derive(Debug, Clone, Default)]
pub struct RouteTable {
    routes: Vec<RouteInfo>,
    filters: Vec<FilterInfo>,
    param_error_handler: Option<ParamErrorHandler>,
    statics: Vec<static_dir::StaticInfo>,
}

/// 扫描过程中收集到的原始结果。
#[derive(Default)]
struct Collected {
    routes: Vec<RouteInfo>,
    filters: Vec<FilterInfo>,
    param_error_handlers: Vec<ParamErrorHandler>,
}

/// 扫描一个或多个源码目录（例如 controller、filter），收集路由、过滤器和 `#[PARAM_ERROR]` 函数。
///
/// ```ignore
/// let routes = hyperoute_codegen::Scanner::new()
///     .dir(src.join("controller"), "crate::controller")
///     .dir(src.join("filter"), "crate::filter")
///     .scan();
/// ```
#[derive(Debug, Clone, Default)]
pub struct Scanner {
    dirs: Vec<(PathBuf, String)>,
    statics: Vec<StaticDir>,
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一个扫描目录；`module` 是这个目录对应的模块路径，例如 "crate::filter"。
    pub fn dir(mut self, dir: impl Into<PathBuf>, module: &str) -> Self {
        self.dirs.push((dir.into(), module.to_string()));
        self
    }

    /// 挂载一个静态资源目录，可以挂多个；没有动态路由匹配时才查找静态资源。
    pub fn static_dir(mut self, dir: StaticDir) -> Self {
        self.statics.push(dir);
        self
    }

    /// 执行扫描。`#[PARAM_ERROR]` 多于一个、`#[FILTER]` 写法不对时直接 panic，让 build.rs 报错。
    pub fn scan(&self) -> RouteTable {
        let mut collected = Collected::default();
        for (dir, module) in &self.dirs {
            scan_dir(dir, module, &mut collected);
        }

        let mut handlers = collected.param_error_handlers;
        if handlers.len() > 1 {
            let mut paths: Vec<&str> = handlers.iter().map(|h| h.fn_path.as_str()).collect();
            paths.sort();
            panic!(
                "#[PARAM_ERROR] 最多只能标注一个函数，实际找到 {} 个：{}",
                handlers.len(),
                paths.join(", ")
            );
        }

        let mut filters = collected.filters;
        filters.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.fn_path.cmp(&b.fn_path)));

        RouteTable {
            routes: collected.routes,
            filters,
            param_error_handler: handlers.pop(),
            statics: static_dir::prepare(&self.statics),
        }
    }
}

/// 只扫描一个目录的简写，等价于 `Scanner::new().dir(root, root_mod).scan()`。
/// `root_mod` 是 `root` 目录对应的模块路径前缀，例如 "crate::controller"。
pub fn scan_routes(root: &Path, root_mod: &str) -> RouteTable {
    Scanner::new().dir(root, root_mod).scan()
}

fn scan_dir(dir: &Path, mod_path: &str, collected: &mut Collected) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let child_mod = format!("{mod_path}::{name}");
            scan_dir(&path, &child_mod, collected);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            if stem == "mod" {
                continue; // mod.rs 本身不含 handler
            }
            let file_mod = format!("{mod_path}::{stem}");
            scan_file(&path, &file_mod, collected);
        }
    }
}

fn scan_file(path: &Path, mod_path: &str, collected: &mut Collected) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let Ok(file) = syn::parse_file(&content) else {
        return;
    };

    for item in &file.items {
        let Item::Fn(item_fn) = item else { continue };
        let fn_path = format!("{mod_path}::{}", item_fn.sig.ident);

        for attr in &item_fn.attrs {
            let Some(ident) = attr.path().get_ident() else {
                continue;
            };
            let method = ident.to_string();
            if method == "PARAM_ERROR" {
                collected.param_error_handlers.push(ParamErrorHandler {
                    fn_path: fn_path.clone(),
                    is_async: item_fn.sig.asyncness.is_some(),
                });
                continue;
            }
            if method == "FILTER" {
                collected.filters.push(parse_filter(attr, item_fn, &fn_path));
                continue;
            }
            let websocket = method == "WEBSOCKET";
            if !websocket && !METHODS.contains(&method.as_str()) {
                continue;
            }
            let Ok(lit) = attr.parse_args::<syn::LitStr>() else {
                continue;
            };

            let segments = parse_path_template(&lit.value());
            let params = classify_params(item_fn, &segments, &file, mod_path);
            let (is_result, return_kind) = classify_return(&item_fn.sig.output);
            validate_websocket_params(&fn_path, websocket, item_fn, &params);

            collected.routes.push(RouteInfo {
                label: format!("{method} {}", lit.value()),
                // WebSocket 握手是 GET 请求
                method: if websocket { "GET".to_string() } else { method },
                segments,
                params,
                fn_path: fn_path.clone(),
                is_result,
                return_kind,
                websocket,
            });
        }
    }
}

/// WebSocket 相关的签名检查，写法不对直接 panic（build.rs 编译失败）：
/// - `#[WEBSOCKET]` 函数必须是 async、恰好一个 `WebSocket` 参数、不能有请求体参数
///   （握手是 GET，没有 body）、不能有 `&str` 参数（handler 在后台任务里运行，参数必须是自有数据）；
/// - 普通路由不能有 `WebSocket` 参数。
fn validate_websocket_params(fn_path: &str, websocket: bool, item_fn: &ItemFn, params: &[ParamSource]) {
    let socket_count = params.iter().filter(|p| matches!(p, ParamSource::WebSocket { .. })).count();
    if !websocket {
        if socket_count > 0 {
            panic!("WebSocket 参数只能用在 #[WEBSOCKET] 路由上：{fn_path}");
        }
        return;
    }

    if item_fn.sig.asyncness.is_none() {
        panic!("#[WEBSOCKET] 函数必须是 async fn：{fn_path}");
    }
    if socket_count != 1 {
        panic!("#[WEBSOCKET] 函数必须恰好有一个 hyperoute_core::WebSocket 参数，实际 {socket_count} 个：{fn_path}");
    }
    for param in params {
        match param {
            ParamSource::Body { name, .. } => {
                panic!("#[WEBSOCKET] 函数不能有请求体参数 `{name}`（握手是 GET 请求，没有 body）：{fn_path}")
            }
            ParamSource::Path(name, ty) | ParamSource::Query(name, ty) if ty.is_str_ref => {
                panic!("#[WEBSOCKET] 函数的参数 `{name}` 不能是 &str，请改用 String（handler 在后台任务里运行）：{fn_path}")
            }
            _ => {}
        }
    }
}

/// 解析 `#[FILTER]` / `#[FILTER(order = 10, include = ["/api/**"], exclude = "/api/login")]`，
/// 并检查函数签名的基本形态；写法不对直接 panic（build.rs 编译失败），不静默忽略。
fn parse_filter(attr: &syn::Attribute, item_fn: &ItemFn, fn_path: &str) -> FilterInfo {
    if item_fn.sig.asyncness.is_none() {
        panic!("#[FILTER] 函数必须是 async fn：{fn_path}");
    }
    if item_fn.sig.inputs.len() != 2 {
        panic!("#[FILTER] 函数必须恰好有两个参数 (req: hyperoute_core::Request, next: hyperoute_core::Next)：{fn_path}");
    }

    let mut order = 0;
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    if !matches!(attr.meta, syn::Meta::Path(_)) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("order") {
                order = parse_int_expr(&meta.value()?.parse()?)?;
            } else if meta.path.is_ident("include") {
                include = parse_pattern_list(&meta.value()?.parse()?)?;
            } else if meta.path.is_ident("exclude") {
                exclude = parse_pattern_list(&meta.value()?.parse()?)?;
            } else {
                return Err(meta.error("未知参数，只支持 order / include / exclude"));
            }
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{fn_path} 上的 #[FILTER(...)] 写法有误：{e}"));
    }

    let (is_result, _) = classify_return(&item_fn.sig.output);
    FilterInfo {
        fn_path: fn_path.to_string(),
        order,
        include,
        exclude,
        is_result,
    }
}

/// 整数字面量，允许负数（`order = -10`）。
fn parse_int_expr(expr: &syn::Expr) -> syn::Result<i64> {
    match expr {
        syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(int), .. }) => int.base10_parse(),
        syn::Expr::Unary(syn::ExprUnary { op: syn::UnOp::Neg(_), expr, .. }) => parse_int_expr(expr).map(|v| -v),
        other => Err(syn::Error::new_spanned(other, "order 必须是整数字面量")),
    }
}

/// 单个字符串或字符串数组：`"/api/**"` / `["/api/**", "/admin/**"]`；每个模式必须以 `/` 开头。
fn parse_pattern_list(expr: &syn::Expr) -> syn::Result<Vec<String>> {
    let elems: Vec<&syn::Expr> = match expr {
        syn::Expr::Array(array) => array.elems.iter().collect(),
        other => vec![other],
    };
    elems
        .into_iter()
        .map(|e| match e {
            syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) if s.value().starts_with('/') => Ok(s.value()),
            other => Err(syn::Error::new_spanned(other, "路径模式必须是以 / 开头的字符串字面量")),
        })
        .collect()
}

fn parse_path_template(path: &str) -> Vec<PathSegment> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(name) => PathSegment::Param(name.to_string()),
            None => PathSegment::Literal(seg.to_string()),
        })
        .collect()
}

/// 按函数签名参数顺序，判断每个参数是来自路径段还是请求体；
/// 请求体参数额外尝试在同一份文件里找到它的 struct 定义，供 multipart 用。
fn classify_params(
    item_fn: &ItemFn,
    segments: &[PathSegment],
    file: &syn::File,
    file_mod_path: &str,
) -> Vec<ParamSource> {
    let path_names: Vec<&str> = segments
        .iter()
        .filter_map(|s| match s {
            PathSegment::Param(name) => Some(name.as_str()),
            PathSegment::Literal(_) => None,
        })
        .collect();

    item_fn
        .sig
        .inputs
        .iter()
        .filter_map(|arg| {
            let FnArg::Typed(pat_type) = arg else {
                return None;
            };
            let Pat::Ident(pat_ident) = &*pat_type.pat else {
                return None;
            };
            let name = pat_ident.ident.to_string();
            if type_last_ident_is(&pat_type.ty, "WebSocket") {
                Some(ParamSource::WebSocket { name })
            } else if type_last_ident_is(&pat_type.ty, "Query") {
                let inner_type = single_generic_arg(&pat_type.ty)
                    .map(|inner| type_to_string(&inner))
                    .unwrap_or_else(|| type_to_string(&pat_type.ty));
                Some(ParamSource::QueryStruct { name, inner_type })
            } else if let Some(optional) = ext_param_kind(&pat_type.ty) {
                Some(ParamSource::Ext { name, optional })
            } else if path_names.contains(&name.as_str()) {
                Some(ParamSource::Path(name, type_info(&pat_type.ty)))
            } else if is_scalar_type(&pat_type.ty) {
                // 标量类型（数字/布尔/字符串）且不是路径参数 => 来自 URL 查询串 `?name=value`
                Some(ParamSource::Query(name, type_info(&pat_type.ty)))
            } else {
                let struct_info = resolve_body_struct(file, file_mod_path, &pat_type.ty);
                Some(ParamSource::Body {
                    name,
                    rust_type: type_to_string(&pat_type.ty),
                    struct_info,
                })
            }
        })
        .collect()
}

/// 所有被当作查询参数标量的类型名（不含泛型/引用外壳）。
const SCALAR_TYPE_NAMES: &[&str] = &[
    "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize", "f32",
    "f64", "bool", "char", "String", "str",
];

/// 判断参数类型是否是"标量"（数字/布尔/字符串，含 `&str`），
/// 这类参数如果不是路径段，就按查询参数（`?name=value`）处理，而不是请求体。
fn is_scalar_type(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::Reference(type_ref) => is_scalar_type(&type_ref.elem),
        syn::Type::Path(type_path) => type_path
            .path
            .segments
            .last()
            .is_some_and(|seg| SCALAR_TYPE_NAMES.contains(&seg.ident.to_string().as_str())),
        _ => false,
    }
}

/// `Ext<T>` → Some(false)，`Option<Ext<T>>` → Some(true)，其它 → None。
fn ext_param_kind(ty: &syn::Type) -> Option<bool> {
    if type_last_ident_is(ty, "Ext") {
        return Some(false);
    }
    if type_last_ident_is(ty, "Option") && single_generic_arg(ty).is_some_and(|inner| type_last_ident_is(&inner, "Ext")) {
        return Some(true);
    }
    None
}

fn type_info(ty: &syn::Type) -> TypeInfo {
    TypeInfo {
        rust_type: type_to_string(ty),
        expected: neutral_type_name(ty),
        is_str_ref: matches!(ty, syn::Type::Reference(r) if type_last_ident_is(&r.elem, "str")),
    }
}

/// 把 Rust 类型映射成不暴露语言的中性描述，用在默认错误文案里（例如 i64 → "integer"）；
/// 无法中性描述的类型（自定义类型等）返回 None，文案退化为 "is invalid"。
fn neutral_type_name(ty: &syn::Type) -> Option<&'static str> {
    let ty = match ty {
        syn::Type::Reference(type_ref) => &*type_ref.elem,
        other => other,
    };
    let syn::Type::Path(type_path) = ty else {
        return None;
    };
    let ident = type_path.path.segments.last()?.ident.to_string();
    match ident.as_str() {
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64" | "u128" | "usize" => {
            Some("integer")
        }
        "f32" | "f64" => Some("number"),
        "bool" => Some("boolean"),
        "char" => Some("single character"),
        "String" | "str" => Some("string"),
        _ => None,
    }
}

/// 类型的源码文本，去掉 quote 插入的多余空格，例如 `& 'a std :: path :: Path` → `&'a std::path::Path`。
fn type_to_string(ty: &syn::Type) -> String {
    quote::quote!(#ty)
        .to_string()
        .replace(" :: ", "::")
        .replace(":: ", "::")
        .replace(" <", "<")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace(" ,", ",")
        .replace("& ", "&")
}

/// 在同一份文件里（可以嵌套在 `mod {}` 里）找 `ty` 对应的 struct 定义，
/// 拿到它的完整路径和字段列表。找不到就返回 None（那这个 body 参数就不支持 multipart）。
fn resolve_body_struct(file: &syn::File, file_mod_path: &str, ty: &syn::Type) -> Option<BodyStructInfo> {
    let syn::Type::Path(type_path) = ty else {
        return None;
    };
    let name = type_path.path.segments.last()?.ident.to_string();
    let (mod_suffix, fields_named) = find_struct(&file.items, "", &name)?;

    let full_path = format!("{file_mod_path}{mod_suffix}::{name}");
    let fields = fields_named
        .named
        .iter()
        .filter_map(|f| {
            let field_name = f.ident.as_ref()?.to_string();
            Some((field_name, classify_field_kind(&f.ty), type_to_string(&f.ty)))
        })
        .collect();

    Some(BodyStructInfo { full_path, fields })
}

fn find_struct<'a>(
    items: &'a [Item],
    mod_prefix: &str,
    name: &str,
) -> Option<(String, &'a FieldsNamed)> {
    for item in items {
        match item {
            Item::Struct(item_struct) if item_struct.ident == name => {
                if let syn::Fields::Named(named) = &item_struct.fields {
                    return Some((mod_prefix.to_string(), named));
                }
            }
            Item::Mod(item_mod) => {
                if let Some((_, content)) = &item_mod.content {
                    let child_prefix = format!("{mod_prefix}::{}", item_mod.ident);
                    if let Some(found) = find_struct(content, &child_prefix, name) {
                        return Some(found);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn classify_field_kind(ty: &syn::Type) -> FieldKind {
    if let syn::Type::Reference(type_ref) = ty {
        if type_last_ident_is(&type_ref.elem, "Path") {
            return FieldKind::TempPath;
        }
    }
    if type_last_ident_is(ty, "Vec") {
        if let Some(inner) = single_generic_arg(ty) {
            if type_last_ident_is(&inner, "u8") {
                return FieldKind::RawBytes;
            }
        }
    }
    if type_last_ident_is(ty, "MultipartFile") {
        let inner_is_path = single_generic_arg(ty).is_some_and(|inner| {
            matches!(&inner, syn::Type::Reference(r) if type_last_ident_is(&r.elem, "Path"))
        });
        return FieldKind::MultipartFile { inner_is_path };
    }
    FieldKind::Plain(type_info(ty))
}

fn single_generic_arg(ty: &syn::Type) -> Option<syn::Type> {
    let syn::Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    })
}

/// 返回 (是否是 `Result<T, E>`, 成功值该按哪种方式建响应)。
/// 是 Result 时按内部的 T 判断；否则按整个返回类型判断。
fn classify_return(output: &ReturnType) -> (bool, ReturnKind) {
    let ReturnType::Type(_, ty) = output else {
        return (false, ReturnKind::Json); // 无返回值（()），当 JSON 处理，属于极端情况
    };

    if let Some((ok_ty, _err_ty)) = extract_result_ok_err(ty) {
        (true, classify_value_type(&ok_ty))
    } else {
        (false, classify_value_type(ty))
    }
}

fn classify_value_type(ty: &syn::Type) -> ReturnKind {
    if type_last_ident_is(ty, "Raw") {
        return ReturnKind::Raw;
    }
    if type_last_ident_is(ty, "Stream") {
        return ReturnKind::Stream;
    }

    let tokens = quote::quote!(#ty).to_string();
    let normalized: String = tokens.chars().filter(|c| !c.is_whitespace()).collect();
    if TEXT_RETURN_TYPES.contains(&normalized.as_str()) {
        ReturnKind::Text
    } else {
        ReturnKind::Json
    }
}

/// 类型路径的最后一段是否等于给定名字，兼容裸名 `Raw` 和限定路径 `hyperoute_core::Raw`。
fn type_last_ident_is(ty: &syn::Type, name: &str) -> bool {
    let syn::Type::Path(type_path) = ty else {
        return false;
    };
    type_path
        .path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == name)
}

/// 如果 `ty` 是 `Result<T, E>`，返回 `(T, E)`。
fn extract_result_ok_err(ty: &syn::Type) -> Option<(syn::Type, syn::Type)> {
    let syn::Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    if segment.ident != "Result" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    let mut type_args = args.args.iter().filter_map(|arg| match arg {
        syn::GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    });
    let ok_ty = type_args.next()?;
    let err_ty = type_args.next()?;
    Some((ok_ty, err_ty))
}

/// 把扫描结果渲染成路由源码：入口 `router` → 过滤链 `__filter_chain` → 路由分发 `__dispatch`。
pub fn generate_router_source(table: &RouteTable) -> String {
    let mut arms = String::new();

    for route in &table.routes {
        write_route_arm(&mut arms, route);
    }

    let mut filter_arms = String::new();
    for (index, filter) in table.filters.iter().enumerate() {
        write_filter_arm(&mut filter_arms, index, filter);
    }

    // 有 #[PARAM_ERROR] 函数就交给它，否则用默认的 400 + 纯文本
    let param_error_response = match &table.param_error_handler {
        Some(handler) => format!(
            "hyperoute_core::error_response(&{fn_path}(errors){await_suffix})",
            fn_path = handler.fn_path,
            await_suffix = if handler.is_async { ".await" } else { "" },
        ),
        None => "hyperoute_core::error_response(&hyperoute_core::DefaultParamErrorResponse(errors))".to_string(),
    };

    let static_mounts = static_dir::render(&table.statics);

    format!(
        r#"// 本文件由 hyperoute-codegen 在编译期通过 build.rs 自动生成，请勿手动修改。
#[allow(unused_imports)]
use http_body_util::BodyExt;

{static_mounts}
/// 参数解析失败：交给 `__param_error` 生成响应并提前返回。
#[allow(unused_macros)]
macro_rules! __try_param {{
    ($route:expr, $result:expr) => {{
        match $result {{
            Ok(value) => value,
            Err(error) => return Ok(__param_error($route, error).await),
        }}
    }};
}}

/// 服务端自身出错（不是客户端参数的问题）：记日志，返回不带细节的 500。
#[allow(unused_macros)]
macro_rules! __try_internal {{
    ($route:expr, $result:expr) => {{
        match $result {{
            Ok(value) => value,
            Err(error) => {{
                eprintln!("[hyperoute] internal error on {{}}: {{}}", $route, error);
                return Ok(hyperoute_core::internal_error_response());
            }}
        }}
    }};
}}

#[allow(dead_code)]
async fn __param_error(
    route: &'static str,
    mut error: hyperoute_core::ParamError,
) -> hyper::Response<hyperoute_core::ResponseBody> {{
    error.route = route;
    // 原始错误（含 Rust 类型名、底层报错）只进服务端日志，不进默认响应
    eprintln!("[hyperoute] parameter error on {{route}}: {{error:?}}");
    let errors = hyperoute_core::ParamErrors::single(error);
    {param_error_response}
}}

/// 入口：把请求 body 装箱成统一类型，统一规范化路径（不合法直接 400），然后从第一个过滤器开始执行。
/// 过滤器匹配、动态路由、静态资源都使用这同一个规范化后的路径。
pub async fn router(
    req: hyper::Request<hyper::body::Incoming>,
) -> Result<hyperoute_core::Response, hyper::Error> {{
    hyperoute_core::static_init(__STATIC_MOUNTS);
    let mut req = req.map(hyperoute_core::box_request_body);
    if let Err(response) = hyperoute_core::normalize_request_path(&mut req) {{
        return Ok(response);
    }}
    Ok(__filter_chain(0, req).await)
}}

/// 从第 `index` 个过滤器开始执行剩余的过滤链；路径不匹配的过滤器直接跳过，
/// 所有过滤器之后是 `__dispatch`。过滤器按 (order, 函数路径) 排序。
fn __filter_chain(
    index: usize,
    req: hyperoute_core::Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = hyperoute_core::Response> + Send>> {{
    Box::pin(async move {{
        match index {{
{filter_arms}            _ => match __dispatch(req).await {{
                Ok(response) => response,
                Err(never) => match never {{}},
            }},
        }}
    }})
}}

#[allow(unused_assignments, unused_mut, non_snake_case)]
async fn __dispatch(
    mut req: hyperoute_core::Request,
) -> Result<hyperoute_core::Response, std::convert::Infallible> {{
    let method = req.method().clone();
    let path = hyperoute_core::request_path(&req).to_string();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let response = match (&method, segments.as_slice()) {{
{arms}        // 没有动态路由匹配：查静态资源，还没有就 404
        _ => match hyperoute_core::serve_static(__STATIC_MOUNTS, &req).await {{
            Some(response) => response,
            None => hyper::Response::builder()
                .status(hyper::StatusCode::NOT_FOUND)
                .body(http_body_util::Full::new(hyper::body::Bytes::from("Not Found")))
                .unwrap()
                .map(hyperoute_core::boxed_full),
        }},
    }};

    Ok(response)
}}
"#
    )
}

fn write_route_arm(out: &mut String, route: &RouteInfo) {
    let method_pattern = if route.method == "REQUEST" {
        "_".to_string()
    } else {
        format!("&hyper::Method::{}", route.method)
    };

    let segment_pattern = route
        .segments
        .iter()
        .map(|s| match s {
            PathSegment::Literal(lit) => format!("{lit:?}"),
            PathSegment::Param(name) => name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");

    let label = &route.label;
    let mut setup = String::new();

    // WebSocket 路由先校验握手请求头，不合法直接返回 400，不再解析其它参数
    if route.websocket {
        let _ = writeln!(
            setup,
            "            let __ws_upgrade = __try_param!({label:?}, hyperoute_core::WebSocketUpgrade::from_request(&mut req));"
        );
    }

    // 查询串只需要解析一次；查询串和 extensions 都必须在任何 `req.into_body()`（Body 参数会消费 req）
    // 之前取出，所以不管这两类参数在签名里排第几，都统一挪到最前面生成。
    if route.params.iter().any(|p| matches!(p, ParamSource::Query(..))) {
        let _ = writeln!(
            setup,
            "            let __query_map = hyperoute_core::query_map(req.uri().query());"
        );
    }
    for param in &route.params {
        if let ParamSource::QueryStruct { name, inner_type } = param {
            let _ = writeln!(
                setup,
                "            let {name} = __try_param!({label:?}, hyperoute_core::query_struct(req.uri().query(), {inner_type:?}));"
            );
        }
        if let ParamSource::Ext { name, optional } = param {
            if *optional {
                let _ = writeln!(
                    setup,
                    "            let {name} = hyperoute_core::take_ext_opt(req.extensions_mut());"
                );
            } else {
                // 缺数据说明过滤器没覆盖到这个路由，是服务端配置问题：记日志 + 500
                let _ = writeln!(
                    setup,
                    "            let {name} = __try_internal!({label:?}, hyperoute_core::take_ext(req.extensions_mut()));"
                );
            }
        }
    }

    for param in &route.params {
        match param {
            ParamSource::Ext { .. } | ParamSource::WebSocket { .. } | ParamSource::QueryStruct { .. } => {}
            ParamSource::Path(name, ty) => {
                if ty.is_str_ref {
                    // match 模式里绑定的是 `&&str`，这里解一层引用即可，不需要转换
                    let _ = writeln!(setup, "            let {name}: &str = {name};");
                } else {
                    let _ = writeln!(
                        setup,
                        "            let {name} = __try_param!({label:?}, hyperoute_core::parse_param(hyperoute_core::ParamLocation::Path, {name:?}, {name}, {expected}, {rust_type:?}));",
                        expected = expected_literal(ty.expected),
                        rust_type = ty.rust_type,
                    );
                }
            }
            ParamSource::Query(name, ty) => {
                if ty.is_str_ref {
                    let _ = writeln!(
                        setup,
                        "            let {name} = __try_param!({label:?}, hyperoute_core::query_param_str(&__query_map, {name:?}));"
                    );
                } else {
                    let _ = writeln!(
                        setup,
                        "            let {name} = __try_param!({label:?}, hyperoute_core::query_param(&__query_map, {name:?}, {expected}, {rust_type:?}));",
                        expected = expected_literal(ty.expected),
                        rust_type = ty.rust_type,
                    );
                }
            }
            ParamSource::Body {
                name,
                rust_type,
                struct_info,
            } => {
                write_body_setup(&mut setup, label, name, rust_type, struct_info.as_ref());
            }
        }
    }

    let call_args = route
        .params
        .iter()
        .map(ParamSource::name)
        .collect::<Vec<_>>()
        .join(", ");

    if route.websocket {
        write_websocket_arm(out, route, &method_pattern, &segment_pattern, &setup, &call_args);
        return;
    }

    let call_expr = if route.is_result {
        format!(
            r#"match {fn_path}({call_args}).await {{
                Ok(v) => v,
                Err(e) => return Ok(hyperoute_core::error_response(&e)),
            }}"#,
            fn_path = route.fn_path,
        )
    } else {
        format!("{fn_path}({call_args}).await", fn_path = route.fn_path)
    };

    let response_expr = match route.return_kind {
        ReturnKind::Text => r#"hyper::Response::builder()
                .status(hyper::StatusCode::OK)
                .body(http_body_util::Full::new(hyper::body::Bytes::from(__body.to_string())))
                .unwrap()
                .map(hyperoute_core::boxed_full)"#
            .to_string(),
        ReturnKind::Json => r#"hyper::Response::builder()
                .status(hyper::StatusCode::OK)
                .header("content-type", "application/json")
                .body(http_body_util::Full::new(hyper::body::Bytes::from(serde_json::to_string(&__body).unwrap())))
                .unwrap()
                .map(hyperoute_core::boxed_full)"#
            .to_string(),
        ReturnKind::Raw => r#"{
                let mut __builder = hyper::Response::builder().status(hyper::StatusCode::OK);
                for (__header_name, __header_value) in __body.headers.iter() {
                    __builder = __builder.header(*__header_name, __header_value);
                }
                __builder
                    .body(http_body_util::Full::new(hyper::body::Bytes::from(__body.body)))
                    .unwrap()
                    .map(hyperoute_core::boxed_full)
            }"#
            .to_string(),
        ReturnKind::Stream => r#"{
                let mut __builder = hyper::Response::builder().status(hyper::StatusCode::OK);
                for (__header_name, __header_value) in __body.headers.iter() {
                    __builder = __builder.header(*__header_name, __header_value);
                }
                __builder
                    .body(__body.body)
                    .unwrap()
            }"#
            .to_string(),
    };

    let _ = write!(
        out,
        r#"        ({method_pattern}, [{segment_pattern}]) => {{
{setup}            let __body = {call_expr};
            {response_expr}
        }}
"#,
    );
}

/// WebSocket 路由分支：参数都解析完后返回 101，连接升级完成后在后台任务里调用 handler。
/// handler 返回 `Result<_, E>` 时，Err 只记日志（E 需要实现 Display），此时响应早已发出。
fn write_websocket_arm(
    out: &mut String,
    route: &RouteInfo,
    method_pattern: &str,
    segment_pattern: &str,
    setup: &str,
    call_args: &str,
) {
    let label = &route.label;
    let socket_name = route
        .params
        .iter()
        .find_map(|p| match p {
            ParamSource::WebSocket { name } => Some(name.as_str()),
            _ => None,
        })
        .expect("validate_websocket_params 已保证恰好一个 WebSocket 参数");
    let fn_path = &route.fn_path;
    let call = if route.is_result {
        format!(
            "if let Err(error) = {fn_path}({call_args}).await {{\n                    eprintln!(\"[hyperoute] websocket handler error on {{}}: {{}}\", {label:?}, error);\n                }}"
        )
    } else {
        format!("{fn_path}({call_args}).await;")
    };

    let _ = write!(
        out,
        r#"        ({method_pattern}, [{segment_pattern}]) => {{
{setup}            __ws_upgrade.on_upgrade({label:?}, move |{socket_name}| async move {{
                {call}
            }})
        }}
"#,
    );
}

/// 过滤链里的一个分支：路径匹配就调用过滤器，并把 “从下一个过滤器继续” 作为 `Next` 传进去；
/// 不匹配就直接跳到下一个。
fn write_filter_arm(out: &mut String, index: usize, filter: &FilterInfo) {
    let include = render_str_slice(&filter.include);
    let exclude = render_str_slice(&filter.exclude);
    let call = format!("{}(req, __next).await", filter.fn_path);
    let call = if filter.is_result {
        format!(
            "match {call} {{\n                        Ok(response) => response,\n                        Err(error) => hyperoute_core::error_response(&error),\n                    }}"
        )
    } else {
        call
    };
    let next = index + 1;

    let _ = write!(
        out,
        r#"            {index} => {{
                if hyperoute_core::filter_applies({include}, {exclude}, hyperoute_core::request_path(&req)) {{
                    let __next = hyperoute_core::Next::new(|req| __filter_chain({next}, req));
                    {call}
                }} else {{
                    __filter_chain({next}, req).await
                }}
            }}
"#,
    );
}

/// `["/a", "/b"]` 渲染成源码里的 `&["/a", "/b"]`。
fn render_str_slice(items: &[String]) -> String {
    let items = items.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(", ");
    format!("&[{items}]")
}

/// `Option<&'static str>` 渲染成源码字面量：`Some("integer")` / `None`。
fn expected_literal(expected: Option<&'static str>) -> String {
    match expected {
        Some(e) => format!("Some({e:?})"),
        None => "None".to_string(),
    }
}

/// 生成请求体解析代码。
///
/// 关键约束：一个 `if/else` 表达式的每个分支哪怕运行时不会执行，编译期也必须类型检查通过——
/// 所以不能无脑对每个 body 参数都生成"JSON/urlencoded/multipart 三选一"：如果 struct 里有
/// 文件字段（`Vec<u8>`/`&Path`/`MultipartFile<T>`），这个类型根本不可能实现 `Deserialize`
/// （尤其是带生命周期的 `&Path` 字段），硬生成 serde 分支只会编译不过。
///
/// 所以按 struct 是否含文件字段分两条路：
/// - 含文件字段：这个类型注定是 multipart 专用的，只生成 multipart 分支，
///   其它 content-type 一律返回 UnsupportedContentType 参数错误，不生成 serde 调用。
/// - 不含文件字段（或者压根没找到 struct 定义）：按 content-type 在
///   JSON / multipart / urlencoded 之间三选一
///   （要求类型自己 derive 了 `Deserialize`，这是 Rust 编译器保证的，不用我们再检查）；
///   content-type 为空时按 urlencoded 处理，其它未知 content-type 返回 UnsupportedContentType。
fn write_body_setup(
    setup: &mut String,
    label: &str,
    name: &str,
    rust_type: &str,
    struct_info: Option<&BodyStructInfo>,
) {
    let _ = writeln!(
        setup,
        "            let __content_type = req.headers().get(\"content-type\").and_then(|v| v.to_str().ok()).unwrap_or(\"\").to_string();"
    );
    let _ = writeln!(
        setup,
        "            let __body_bytes = __try_param!({label:?}, http_body_util::BodyExt::collect(req.into_body()).await.map(|b| b.to_bytes()).map_err(hyperoute_core::body_read_error));"
    );

    let unsupported = format!(
        "                return Ok(__param_error({label:?}, hyperoute_core::unsupported_content_type(&__content_type, {rust_type:?})).await);\n"
    );

    let has_file_field = struct_info.is_some_and(|info| {
        info.fields.iter().any(|(_, kind, _)| !matches!(kind, FieldKind::Plain(_)))
    });

    if has_file_field {
        let (pre_decls, multipart_branch) = render_multipart_branch(label, struct_info.unwrap());
        setup.push_str(&pre_decls);
        let _ = writeln!(
            setup,
            "            let {name} = if __content_type.starts_with(\"multipart/form-data\") {{\n{multipart_branch}            }} else {{\n{unsupported}            }};"
        );
    } else {
        let multipart_branch = match struct_info {
            Some(info) => {
                let (pre_decls, branch) = render_multipart_branch(label, info);
                setup.push_str(&pre_decls);
                branch
            }
            // 找不到 struct 定义，不知道有哪些字段，没法支持 multipart
            None => unsupported.clone(),
        };

        let _ = writeln!(
            setup,
            "            let {name} = if __content_type.starts_with(\"application/json\") {{\n                __try_param!({label:?}, hyperoute_core::json_body(&__body_bytes, {rust_type:?}))\n            }} else if __content_type.starts_with(\"multipart/form-data\") {{\n{multipart_branch}            }} else if __content_type.is_empty() || __content_type.starts_with(\"application/x-www-form-urlencoded\") {{\n                __try_param!({label:?}, hyperoute_core::urlencoded_body(&__body_bytes, {rust_type:?}))\n            }} else {{\n{unsupported}            }};"
        );
    }
}

/// 生成 (需要提前挂在 `if/else` 外层的声明, multipart 分支内部的代码)。
///
/// 之所以要拆成两截：`&Path` 字段借用的临时文件路径，必须存在一个活得比
/// 整个 `if/else` 表达式还长的变量里——如果直接在分支内部 `let tmp = write_temp_file(...).await;`，
/// `tmp` 出了分支就被 drop 掉，构造出来的 `&Path` 就成悬空引用，编译器会直接拒绝。
/// 所以每个文件路径字段都在外层先声明一个 `Option<PathBuf>` 占位，分支内部只是把值填进去、再借用。
fn render_multipart_branch(label: &str, info: &BodyStructInfo) -> (String, String) {
    let mut pre_decls = String::new();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "                let mut __fields = __try_param!({label:?}, hyperoute_core::parse_multipart(__body_bytes, &__content_type).await);"
    );

    let mut ctor_fields = Vec::new();
    for (field_name, kind, rust_type) in &info.fields {
        ctor_fields.push(field_name.clone());
        let take = format!(
            "__try_param!({label:?}, hyperoute_core::multipart_take(&mut __fields, {field_name:?}, {rust_type:?}))"
        );
        match kind {
            FieldKind::Plain(ty) => {
                let _ = writeln!(
                    out,
                    "                let {field_name} = __try_param!({label:?}, hyperoute_core::multipart_text(&mut __fields, {field_name:?}, {expected}, {rust_type:?}));",
                    expected = expected_literal(ty.expected),
                );
            }
            FieldKind::RawBytes => {
                let _ = writeln!(out, "                let {field_name} = {take}.data;");
            }
            FieldKind::TempPath => {
                let _ = writeln!(
                    pre_decls,
                    "            let mut {field_name}__holder: Option<std::path::PathBuf> = None;"
                );
                let _ = writeln!(out, "                let {field_name}__raw = {take};");
                let _ = writeln!(
                    out,
                    "                {field_name}__holder = Some(__try_internal!({label:?}, hyperoute_core::write_temp_file(&{field_name}__raw.data).await));"
                );
                let _ = writeln!(
                    out,
                    "                let {field_name} = {field_name}__holder.as_ref().unwrap().as_path();"
                );
            }
            FieldKind::MultipartFile { inner_is_path: false } => {
                let _ = writeln!(
                    out,
                    "                let {field_name} = {{ let __f = {take}; hyperoute_core::MultipartFile {{ name: __f.file_name.unwrap_or_default(), content_type: __f.content_type, body: __f.data }} }};"
                );
            }
            FieldKind::MultipartFile { inner_is_path: true } => {
                let _ = writeln!(
                    pre_decls,
                    "            let mut {field_name}__holder: Option<std::path::PathBuf> = None;"
                );
                let _ = writeln!(out, "                let {field_name}__raw = {take};");
                let _ = writeln!(
                    out,
                    "                {field_name}__holder = Some(__try_internal!({label:?}, hyperoute_core::write_temp_file(&{field_name}__raw.data).await));"
                );
                let _ = writeln!(
                    out,
                    "                let {field_name} = hyperoute_core::MultipartFile {{ name: {field_name}__raw.file_name.unwrap_or_default(), content_type: {field_name}__raw.content_type, body: {field_name}__holder.as_ref().unwrap().as_path() }};"
                );
            }
        }
    }

    let _ = writeln!(
        out,
        "                {full_path} {{ {ctor_fields} }}",
        full_path = info.full_path,
        ctor_fields = ctor_fields.join(", "),
    );
    (pre_decls, out)
}
