//! 给 `build.rs` 用的路由扫描 + 代码生成逻辑。
//! 不是 proc-macro（那部分在 hyperoute-macro 里），是普通 lib，
//! 由使用方的 build.rs 作为 build-dependency 调用。

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use syn::{FnArg, Item, ItemFn, Pat, ReturnType};

mod html_include;
mod naming;
mod options;
mod static_dir;

pub use naming::ParamNaming;
pub use options::{ParamBinding, ParamFallback};
use options::ScanOptions;
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
    /// `{*name}`：剩下的 1 个或多个路径段，只能是最后一段
    Rest(String),
}

/// 标量参数 / 表单字段的形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `T`
    One,
    /// `Option<T>`
    Opt,
    /// `Vec<T>`
    Many,
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
    /// 普通值类型（String/数字等），当文本字段处理；TypeInfo 是去掉 Option / Vec 外壳后的类型
    Plain(TypeInfo, Shape),
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
    /// 按声明顺序排列的 (字段名, 取值方式, 字段类型原文, multipart 里的名字)
    fields: Vec<(String, FieldKind, String, naming::WireName)>,
}

#[derive(Debug, Clone)]
enum ParamSource {
    /// 值来自路径段，绑定名与路径里的 `{name}` 一致
    Path(String, TypeInfo),
    /// `{*name}`：剩下的路径段用 `/` 拼起来；`index` 是它在模板里的段序号
    RestPath { name: String, ty: TypeInfo, index: usize },
    /// 标量参数（`T` / `Option<T>` / `Vec<T>`）：默认从查询串取值，合并模式从合并后的表单取值；
    /// `ty` 是去掉 Option / Vec 外壳后的类型，`wire` 是请求里的名字（受 ParamNaming 影响）
    Query { name: String, ty: TypeInfo, wire: naming::WireName, shape: Shape },
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
    /// `HeaderMap`（复制一份）或 `&HeaderMap`
    Headers { name: String, by_ref: bool },
    /// `RemoteAddr` 或 `Option<RemoteAddr>`
    RemoteAddr { name: String, optional: bool },
    /// `BodyStream`：未缓冲的请求体
    BodyStream { name: String },
    /// handler 直接写的 multipart 文件参数：`MultipartFile<Vec<u8>>` / `MultipartFile<&Path>`
    File { name: String, kind: FieldKind, rust_type: String, wire: naming::WireName },
}

impl ParamSource {
    fn name(&self) -> &str {
        match self {
            ParamSource::Path(name, _)
            | ParamSource::RestPath { name, .. }
            | ParamSource::Query { name, .. }
            | ParamSource::Body { name, .. }
            | ParamSource::Ext { name, .. }
            | ParamSource::WebSocket { name }
            | ParamSource::QueryStruct { name, .. }
            | ParamSource::Headers { name, .. }
            | ParamSource::RemoteAddr { name, .. }
            | ParamSource::BodyStream { name }
            | ParamSource::File { name, .. } => name,
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
    /// `ParamBinding::Merged`
    merged: bool,
    /// 参数缺失 / 转换失败时用默认值（全局 `ParamFallback` 或 handler 上的属性）
    lenient: bool,
    /// `ParamNaming::CamelCase`（表单结构体在运行时也接受字段的驼峰名）
    camel: bool,
}

impl RouteInfo {
    /// 带 `{*name}` 的路由排在普通路由后面，固定段多的优先。
    fn rest_index(&self) -> Option<usize> {
        self.segments.iter().position(|s| matches!(s, PathSegment::Rest(_)))
    }
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
    options: ScanOptions,
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

    /// 标量查询参数（`fn f(page_size: u32)`）和没有 serde 改名属性的 multipart 字段接受哪些参数名，
    /// 默认 [`ParamNaming::Exact`]。`ParamNaming::CamelCase` 时 `page_size` 也接受前端的 `pageSize`。
    /// 驼峰名在编译期算好，运行时不做字符串转换。JSON / urlencoded / `Query<T>` 由 serde 决定，不受影响。
    pub fn param_naming(mut self, naming: ParamNaming) -> Self {
        self.options.naming = naming;
        self
    }

    /// 标量参数和表单结构体从哪里取值，默认 [`ParamBinding::Separate`]（标量只看查询串）。
    /// [`ParamBinding::Merged`] 时和 Go 的 `request.Form` 一致，查询串和表单请求体合并后取值。
    pub fn param_binding(mut self, binding: ParamBinding) -> Self {
        self.options.binding = binding;
        self
    }

    /// 参数缺失或转换失败时的处理，默认 [`ParamFallback::Strict`]（返回参数错误）。
    /// handler 上的 `#[PARAM_STRICT]` / `#[PARAM_LENIENT]` 覆盖这个设置。
    pub fn param_fallback(mut self, fallback: ParamFallback) -> Self {
        self.options.fallback = fallback;
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
            scan_dir(dir, module, &mut collected, self.options);
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

fn scan_dir(dir: &Path, mod_path: &str, collected: &mut Collected, options: ScanOptions) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let child_mod = format!("{mod_path}::{name}");
            scan_dir(&path, &child_mod, collected, options);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            if stem == "mod" {
                continue; // mod.rs 本身不含 handler
            }
            let file_mod = format!("{mod_path}::{stem}");
            scan_file(&path, &file_mod, collected, options);
        }
    }
}

fn scan_file(path: &Path, mod_path: &str, collected: &mut Collected, options: ScanOptions) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let Ok(file) = syn::parse_file(&content) else {
        return;
    };

    for item in &file.items {
        let Item::Fn(item_fn) = item else { continue };
        let fn_path = format!("{mod_path}::{}", item_fn.sig.ident);
        let lenient = route_lenient(item_fn, &fn_path, options.fallback);

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

            let segments = parse_path_template(&lit.value(), &fn_path);
            let params = classify_params(item_fn, &segments, &file, mod_path, options.naming);
            let (is_result, return_kind) = classify_return(&item_fn.sig.output);
            validate_websocket_params(&fn_path, websocket, item_fn, &params);
            validate_body_params(&fn_path, options.binding, &params);

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
                merged: options.binding == ParamBinding::Merged,
                lenient,
                camel: options.naming == ParamNaming::CamelCase,
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
            ParamSource::Path(name, ty) | ParamSource::RestPath { name, ty, .. } | ParamSource::Query { name, ty, .. }
                if ty.is_str_ref =>
            {
                panic!("#[WEBSOCKET] 函数的参数 `{name}` 不能是 &str，请改用 String（handler 在后台任务里运行）：{fn_path}")
            }
            ParamSource::Headers { name, by_ref: true } => {
                panic!("#[WEBSOCKET] 函数的参数 `{name}` 不能是 &HeaderMap，请改用 HeaderMap（handler 在后台任务里运行）：{fn_path}")
            }
            ParamSource::File { name, .. } | ParamSource::BodyStream { name } => {
                panic!("#[WEBSOCKET] 函数不能有请求体参数 `{name}`（握手是 GET 请求，没有 body）：{fn_path}")
            }
            _ => {}
        }
    }
}

/// 请求体相关参数的组合检查：
/// - `BodyStream` 最多一个，不能和请求体结构体、文件参数同时出现（请求体只能读一次）；
/// - 默认模式下文件参数不能和请求体结构体同时出现（各自读取请求体）；合并模式下请求体只读一次，可以同时出现。
fn validate_body_params(fn_path: &str, binding: ParamBinding, params: &[ParamSource]) {
    let streams = params.iter().filter(|p| matches!(p, ParamSource::BodyStream { .. })).count();
    let has_body = params.iter().any(|p| matches!(p, ParamSource::Body { .. }));
    let has_file = params.iter().any(|p| matches!(p, ParamSource::File { .. }));
    if streams > 1 {
        panic!("一个 handler 最多只能有一个 BodyStream 参数：{fn_path}");
    }
    if streams == 1 && (has_body || has_file) {
        panic!("BodyStream 参数不能和请求体结构体 / multipart 文件参数同时使用（请求体只能读一次）：{fn_path}");
    }
    if binding == ParamBinding::Separate && has_body && has_file {
        panic!(
            "默认模式下 multipart 文件参数不能和请求体结构体参数同时使用，请把文件字段放进结构体，或者开启 ParamBinding::Merged：{fn_path}"
        );
    }
}

/// handler 上的 `#[PARAM_STRICT]` / `#[PARAM_LENIENT]` 覆盖全局的 `ParamFallback`。
fn route_lenient(item_fn: &ItemFn, fn_path: &str, fallback: ParamFallback) -> bool {
    let strict = item_fn.attrs.iter().any(|a| a.path().is_ident("PARAM_STRICT"));
    let lenient = item_fn.attrs.iter().any(|a| a.path().is_ident("PARAM_LENIENT"));
    match (strict, lenient) {
        (true, true) => panic!("#[PARAM_STRICT] 和 #[PARAM_LENIENT] 不能同时使用：{fn_path}"),
        (true, false) => false,
        (false, true) => true,
        (false, false) => fallback == ParamFallback::Default,
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

fn parse_path_template(path: &str, fn_path: &str) -> Vec<PathSegment> {
    let segments: Vec<PathSegment> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(name) => match name.strip_prefix('*') {
                Some(rest) => PathSegment::Rest(rest.to_string()),
                None => PathSegment::Param(name.to_string()),
            },
            None => PathSegment::Literal(seg.to_string()),
        })
        .collect();
    if let Some(index) = segments.iter().position(|s| matches!(s, PathSegment::Rest(_))) {
        if index + 1 != segments.len() {
            panic!("尾部通配参数 {{*name}} 只能写在路径模板的最后一段：{path}（{fn_path}）");
        }
    }
    segments
}

/// 按函数签名参数顺序，判断每个参数是来自路径段还是请求体；
/// 请求体参数额外尝试在同一份文件里找到它的 struct 定义，供 multipart 用。
fn classify_params(
    item_fn: &ItemFn,
    segments: &[PathSegment],
    file: &syn::File,
    file_mod_path: &str,
    naming: ParamNaming,
) -> Vec<ParamSource> {
    let path_names: Vec<&str> = segments
        .iter()
        .filter_map(|s| match s {
            PathSegment::Param(name) => Some(name.as_str()),
            PathSegment::Literal(_) | PathSegment::Rest(_) => None,
        })
        .collect();
    let rest = segments.iter().enumerate().find_map(|(index, s)| match s {
        PathSegment::Rest(name) => Some((name.as_str(), index)),
        _ => None,
    });

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
            } else if let Some(by_ref) = header_map_kind(&pat_type.ty) {
                Some(ParamSource::Headers { name, by_ref })
            } else if let Some(optional) = wrapped_kind(&pat_type.ty, "RemoteAddr") {
                Some(ParamSource::RemoteAddr { name, optional })
            } else if type_last_ident_is(&pat_type.ty, "BodyStream") {
                Some(ParamSource::BodyStream { name })
            } else if path_names.contains(&name.as_str()) {
                Some(ParamSource::Path(name, type_info(&pat_type.ty)))
            } else if let Some((_, index)) = rest.filter(|(rest_name, _)| *rest_name == name) {
                Some(ParamSource::RestPath { ty: type_info(&pat_type.ty), name, index })
            } else if let Some((shape, inner)) = scalar_shape(&pat_type.ty) {
                // 标量类型（数字/布尔/字符串，可以套 Option / Vec）且不是路径参数 => 来自查询串（合并模式下是合并后的表单）
                if shape == Shape::Many && matches!(inner, syn::Type::Reference(_)) {
                    panic!("Vec<&str> 参数不支持，请改用 Vec<String>：参数 `{name}`（{}）", item_fn.sig.ident);
                }
                let wire = naming::for_param(&name, naming);
                Some(ParamSource::Query { name, ty: type_info(&inner), wire, shape })
            } else if type_last_ident_is(&pat_type.ty, "MultipartFile") {
                let wire = naming::for_param(&name, naming);
                Some(ParamSource::File {
                    name,
                    kind: classify_field_kind(&pat_type.ty),
                    rust_type: type_to_string(&pat_type.ty),
                    wire,
                })
            } else {
                let struct_info = resolve_body_struct(file, file_mod_path, &pat_type.ty, naming);
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
    wrapped_kind(ty, "Ext")
}

/// `Name` → Some(false)，`Option<Name>` → Some(true)，其它 → None（按类型路径最后一段判断）。
fn wrapped_kind(ty: &syn::Type, name: &str) -> Option<bool> {
    if type_last_ident_is(ty, name) {
        return Some(false);
    }
    if type_last_ident_is(ty, "Option") && single_generic_arg(ty).is_some_and(|inner| type_last_ident_is(&inner, name)) {
        return Some(true);
    }
    None
}

/// `HeaderMap` → Some(false)，`&HeaderMap` → Some(true)。
fn header_map_kind(ty: &syn::Type) -> Option<bool> {
    match ty {
        syn::Type::Reference(r) if type_last_ident_is(&r.elem, "HeaderMap") => Some(true),
        other if type_last_ident_is(other, "HeaderMap") => Some(false),
        _ => None,
    }
}

/// `T` / `Option<T>` / `Vec<T>`（T 是标量）→ (形态, T)。
fn scalar_shape(ty: &syn::Type) -> Option<(Shape, syn::Type)> {
    if is_scalar_type(ty) {
        return Some((Shape::One, ty.clone()));
    }
    for (wrapper, shape) in [("Option", Shape::Opt), ("Vec", Shape::Many)] {
        if type_last_ident_is(ty, wrapper) {
            if let Some(inner) = single_generic_arg(ty).filter(is_scalar_type) {
                return Some((shape, inner));
            }
        }
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
fn resolve_body_struct(
    file: &syn::File,
    file_mod_path: &str,
    ty: &syn::Type,
    naming: ParamNaming,
) -> Option<BodyStructInfo> {
    let syn::Type::Path(type_path) = ty else {
        return None;
    };
    let name = type_path.path.segments.last()?.ident.to_string();
    let (mod_suffix, item_struct) = find_struct(&file.items, "", &name)?;
    let syn::Fields::Named(fields_named) = &item_struct.fields else {
        return None;
    };

    let full_path = format!("{file_mod_path}{mod_suffix}::{name}");
    // multipart 里的字段名：有 serde 改名属性按 serde，否则按 ParamNaming 开关
    let wire_names = naming::struct_field_names(item_struct, naming, &full_path);
    let fields = fields_named
        .named
        .iter()
        .zip(wire_names)
        .filter_map(|(f, wire)| {
            let field_name = f.ident.as_ref()?.to_string();
            Some((field_name, classify_field_kind(&f.ty), type_to_string(&f.ty), wire))
        })
        .collect();

    Some(BodyStructInfo { full_path, fields })
}

fn find_struct<'a>(
    items: &'a [Item],
    mod_prefix: &str,
    name: &str,
) -> Option<(String, &'a syn::ItemStruct)> {
    for item in items {
        match item {
            Item::Struct(item_struct) if item_struct.ident == name => {
                if let syn::Fields::Named(_) = &item_struct.fields {
                    return Some((mod_prefix.to_string(), item_struct));
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
    match scalar_shape(ty) {
        Some((shape, inner)) => FieldKind::Plain(type_info(&inner), shape),
        None => FieldKind::Plain(type_info(ty), Shape::One),
    }
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

    // 带 `{*name}` 的路由排在普通路由后面（优先级低于 `{name}`），固定段多的优先
    let mut routes: Vec<&RouteInfo> = table.routes.iter().collect();
    routes.sort_by_key(|r| r.rest_index().map_or(0, |index| usize::MAX - index));
    for route in routes {
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
    if error.kind == hyperoute_core::ParamErrorKind::Internal {{
        // 服务端自己的问题（例如上传临时目录不可写）：和 __try_internal! 一样记日志，返回 500
        eprintln!("[hyperoute] internal error on {{route}}: {{}}", error.reason.as_deref().unwrap_or("unknown"));
        return hyperoute_core::internal_error_response();
    }}
    // 原始错误（含 Rust 类型名、底层报错）只进服务端日志，不进默认响应
    eprintln!("[hyperoute] parameter error on {{route}}: {{error:?}}");
    let errors = hyperoute_core::ParamErrors::single(error);
    // 请求体太大不是参数格式的问题：固定返回 413，不交给 #[PARAM_ERROR]
    if errors.first().kind == hyperoute_core::ParamErrorKind::PayloadTooLarge {{
        return hyperoute_core::error_response(&hyperoute_core::DefaultParamErrorResponse(errors));
    }}
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

#[allow(unused_assignments, unused_mut, unused_variables, non_snake_case)]
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
            // 至少一段；值在下面用 `segments[index..]` 拼起来
            PathSegment::Rest(_) => "_, ..".to_string(),
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

    // 查询串、请求头、客户端地址、extensions 都必须在任何消费请求的操作（读取请求体）之前取出，
    // 所以不管这些参数在签名里排第几，都统一挪到最前面生成。
    for param in &route.params {
        match param {
            ParamSource::QueryStruct { name, inner_type } => {
                let _ = writeln!(
                    setup,
                    "            let {name} = __try_param!({label:?}, hyperoute_core::query_struct(req.uri().query(), {inner_type:?}));"
                );
            }
            ParamSource::Ext { name, optional: true } => {
                let _ = writeln!(setup, "            let {name} = hyperoute_core::take_ext_opt(req.extensions_mut());");
            }
            ParamSource::Ext { name, optional: false } => {
                // 缺数据说明过滤器没覆盖到这个路由，是服务端配置问题：记日志 + 500
                let _ = writeln!(
                    setup,
                    "            let {name} = __try_internal!({label:?}, hyperoute_core::take_ext(req.extensions_mut()));"
                );
            }
            ParamSource::Headers { name, by_ref: false } => {
                let _ = writeln!(setup, "            let {name} = req.headers().clone();");
            }
            ParamSource::Headers { name, by_ref: true } => {
                let _ = writeln!(setup, "            let {name}__headers = req.headers().clone();");
                let _ = writeln!(setup, "            let {name} = &{name}__headers;");
            }
            ParamSource::RemoteAddr { name, optional: true } => {
                let _ = writeln!(setup, "            let {name} = hyperoute_core::RemoteAddr::from_request(&req);");
            }
            ParamSource::RemoteAddr { name, optional: false } => {
                let _ = writeln!(
                    setup,
                    "            let {name} = __try_internal!({label:?}, hyperoute_core::RemoteAddr::require(&req));"
                );
            }
            _ => {}
        }
    }

    write_form_source(&mut setup, route);

    for param in &route.params {
        match param {
            ParamSource::Ext { .. }
            | ParamSource::WebSocket { .. }
            | ParamSource::QueryStruct { .. }
            | ParamSource::Headers { .. }
            | ParamSource::RemoteAddr { .. } => {}
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
            ParamSource::RestPath { name, ty, index } => {
                let _ = writeln!(setup, "            let {name}__rest = segments[{index}..].join(\"/\");");
                if ty.is_str_ref {
                    let _ = writeln!(setup, "            let {name}: &str = &{name}__rest;");
                } else if ty.rust_type.ends_with("String") {
                    let _ = writeln!(setup, "            let {name} = {name}__rest;");
                } else {
                    let _ = writeln!(
                        setup,
                        "            let {name} = __try_param!({label:?}, hyperoute_core::parse_param(hyperoute_core::ParamLocation::Path, {name:?}, &{name}__rest, {expected}, {rust_type:?}));",
                        expected = expected_literal(ty.expected),
                        rust_type = ty.rust_type,
                    );
                }
            }
            ParamSource::Query { name, ty, wire, shape } => {
                write_form_value(&mut setup, route, name, ty, wire, *shape, "            ");
            }
            ParamSource::Body {
                name,
                rust_type,
                struct_info,
            } => {
                if route.merged {
                    write_body_setup_merged(&mut setup, route, name, rust_type, struct_info.as_ref());
                } else {
                    write_body_setup(&mut setup, label, name, rust_type, struct_info.as_ref());
                }
            }
            ParamSource::File { name, kind, rust_type, wire } => {
                let take = multipart_take_expr(label, name, rust_type, wire);
                let (mut pre, mut stmts) = (String::new(), String::new());
                let value = file_value(label, name, kind, &take, &mut pre, &mut stmts, "            ");
                setup.push_str(&pre);
                setup.push_str(&stmts);
                let _ = writeln!(setup, "            let {name} = {value};");
            }
            ParamSource::BodyStream { name } => {
                let _ = writeln!(setup, "            let {name} = hyperoute_core::BodyStream::new(req.into_body());");
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
                let mut __builder = hyper::Response::builder().status(__body.status);
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
                let mut __builder = hyper::Response::builder().status(__body.status);
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
    // multipart 边收边解析（大文件转存到临时文件）；其它 content-type 整体读取（受请求体大小上限限制）。
    // 找不到 struct 定义时不支持 multipart，这种请求不读取请求体，直接在下面返回 unsupported。
    let multipart_read = if struct_info.is_some() {
        format!(
            "if __content_type.starts_with(\"multipart/form-data\") {{ __fields = __try_param!({label:?}, hyperoute_core::read_multipart_fields(req.into_body(), &__content_type).await); }} else "
        )
    } else {
        "if __content_type.starts_with(\"multipart/form-data\") {} else ".to_string()
    };
    let _ = writeln!(setup, "            let mut __body_bytes = hyper::body::Bytes::new();");
    let _ = writeln!(
        setup,
        "            let mut __fields: std::collections::HashMap<String, hyperoute_core::MultipartField> = std::collections::HashMap::new();"
    );
    let _ = writeln!(
        setup,
        "            {multipart_read}{{ __body_bytes = __try_param!({label:?}, hyperoute_core::read_body_limited(req.into_body()).await); }}"
    );

    let unsupported = format!(
        "                return Ok(__param_error({label:?}, hyperoute_core::unsupported_content_type(&__content_type, {rust_type:?})).await);\n"
    );

    let has_file_field = struct_info.is_some_and(|info| {
        info.fields.iter().any(|(_, kind, _, _)| !matches!(kind, FieldKind::Plain(..)))
    });

    if has_file_field {
        let (pre_decls, multipart_branch) = render_multipart_branch(label, struct_info.unwrap(), None);
        setup.push_str(&pre_decls);
        let _ = writeln!(
            setup,
            "            let {name} = if __content_type.starts_with(\"multipart/form-data\") {{\n{multipart_branch}            }} else {{\n{unsupported}            }};"
        );
    } else {
        let multipart_branch = match struct_info {
            Some(info) => {
                let (pre_decls, branch) = render_multipart_branch(label, info, None);
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
///
/// multipart 已经在外面边收边解析到 `__fields` 里了。`merged` 是 Some 时（合并模式），文本字段从合并后的表单取值。
fn render_multipart_branch(label: &str, info: &BodyStructInfo, merged: Option<&RouteInfo>) -> (String, String) {
    let mut pre_decls = String::new();
    let mut out = String::new();

    let mut ctor_fields = Vec::new();
    for (field_name, kind, rust_type, wire) in &info.fields {
        ctor_fields.push(field_name.clone());
        match (kind, merged) {
            (FieldKind::Plain(ty, shape), Some(route)) => {
                write_form_value(&mut out, route, field_name, ty, wire, *shape, "                ");
            }
            (FieldKind::Plain(ty, _), None) => {
                // 字段名就是 Rust 名字时沿用单名字版本；否则按编译期算好的候选名依次查找
                let (text_fn, keys) = if wire.is_plain(field_name) {
                    ("multipart_text", format!("{field_name:?}"))
                } else {
                    ("multipart_text_any", format!("{}, {:?}", wire.lookup_literal(), wire.display))
                };
                let _ = writeln!(
                    out,
                    "                let {field_name} = __try_param!({label:?}, hyperoute_core::{text_fn}(&mut __fields, {keys}, {expected}, {rust_type:?}));",
                    expected = expected_literal(ty.expected),
                );
            }
            (kind, _) => {
                let take = multipart_take_expr(label, field_name, rust_type, wire);
                let value = file_value(label, field_name, kind, &take, &mut pre_decls, &mut out, "                ");
                let _ = writeln!(out, "                let {field_name} = {value};");
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

/// 从 `__fields` 取出一个 multipart 字段的表达式（没传返回参数错误）。
fn multipart_take_expr(label: &str, name: &str, rust_type: &str, wire: &naming::WireName) -> String {
    let (take_fn, keys) = if wire.is_plain(name) {
        ("multipart_take", format!("{name:?}"))
    } else {
        ("multipart_take_any", format!("{}, {:?}", wire.lookup_literal(), wire.display))
    };
    format!("__try_param!({label:?}, hyperoute_core::{take_fn}(&mut __fields, {keys}, {rust_type:?}))")
}

/// 文件字段 / 文件参数的值表达式。`&Path` 用的临时文件占位声明写进 `pre`（外层作用域，活到 handler 返回之后，
/// 离开作用域时 `TempFile` 自动删除文件），取值语句写进 `out`。
fn file_value(label: &str, name: &str, kind: &FieldKind, take: &str, pre: &mut String, out: &mut String, indent: &str) -> String {
    match kind {
        FieldKind::RawBytes => format!("__try_internal!({label:?}, {take}.into_bytes().await)"),
        FieldKind::MultipartFile { inner_is_path: false } => format!(
            "{{ let mut __f = {take}; let __name = __f.file_name.take().unwrap_or_default(); let __content_type = __f.content_type.take(); hyperoute_core::MultipartFile {{ name: __name, content_type: __content_type, body: __try_internal!({label:?}, __f.into_bytes().await) }} }}"
        ),
        FieldKind::TempPath | FieldKind::MultipartFile { inner_is_path: true } => {
            let _ = writeln!(pre, "            let mut {name}__holder: Option<hyperoute_core::TempFile> = None;");
            let _ = writeln!(out, "{indent}let mut {name}__raw = {take};");
            let _ = writeln!(out, "{indent}let {name}__file_name = {name}__raw.file_name.take().unwrap_or_default();");
            let _ = writeln!(out, "{indent}let {name}__content_type = {name}__raw.content_type.take();");
            let _ = writeln!(
                out,
                "{indent}{name}__holder = Some(__try_internal!({label:?}, {name}__raw.into_temp_file().await));"
            );
            let path = format!("{name}__holder.as_ref().unwrap().as_path()");
            if matches!(kind, FieldKind::TempPath) {
                path
            } else {
                format!(
                    "hyperoute_core::MultipartFile {{ name: {name}__file_name, content_type: {name}__content_type, body: {path} }}"
                )
            }
        }
        FieldKind::Plain(..) => unreachable!("文本字段不走文件取值"),
    }
}

/// 准备标量参数 / 请求体用的数据源：
/// - 默认模式：`__form` 只包含查询串；有文件参数时另外按 multipart 读取请求体（`__fields`）。
/// - 合并模式：`FormRequest::read` 读一次请求体，得到合并后的 `__form`、`__content_type`、`__body_bytes`、`__fields`；
///   WebSocket 路由和带 `BodyStream` 参数的路由不读请求体，`__form` 只包含查询串。
fn write_form_source(setup: &mut String, route: &RouteInfo) {
    let label = &route.label;
    let has = |f: fn(&ParamSource) -> bool| route.params.iter().any(f);
    let has_query = has(|p| matches!(p, ParamSource::Query { .. }));
    let has_body = has(|p| matches!(p, ParamSource::Body { .. }));
    let has_stream = has(|p| matches!(p, ParamSource::BodyStream { .. }));
    let first_file = route.params.iter().find_map(|p| match p {
        ParamSource::File { rust_type, .. } => Some(rust_type.as_str()),
        _ => None,
    });

    if route.merged {
        if route.websocket || has_stream {
            if has_query {
                let _ = writeln!(setup, "            let __form = hyperoute_core::FormData::merged_query(req.uri().query());");
            }
        } else if has_query || has_body || first_file.is_some() {
            let _ = writeln!(
                setup,
                "            let hyperoute_core::FormRequest {{ form: __form, content_type: __content_type, body: __body_bytes, files: mut __fields }} = __try_param!({label:?}, hyperoute_core::FormRequest::read(req).await);"
            );
        }
        return;
    }

    if has_query {
        let _ = writeln!(setup, "            let __form = hyperoute_core::FormData::from_query(req.uri().query());");
    }
    if let Some(rust_type) = first_file {
        let _ = writeln!(
            setup,
            "            let hyperoute_core::FormRequest {{ files: mut __fields, .. }} = __try_param!({label:?}, hyperoute_core::FormRequest::read_multipart(req, {rust_type:?}).await);"
        );
    }
}

/// 一个标量参数（或合并模式下 multipart 结构体的文本字段）的取值语句：`let name = ...;`。
fn write_form_value(
    out: &mut String,
    route: &RouteInfo,
    name: &str,
    ty: &TypeInfo,
    wire: &naming::WireName,
    shape: Shape,
    indent: &str,
) {
    let label = &route.label;
    let keys = wire.lookup_literal();
    let display = &wire.display;
    let expected = expected_literal(ty.expected);
    let rust_type = &ty.rust_type;
    // 严格模式：失败时返回参数错误；宽松模式：失败时得到默认值 / None
    let one = |call: String| {
        if route.lenient { format!("hyperoute_core::lenient({label:?}, {call})") } else { format!("__try_param!({label:?}, {call})") }
    };
    let opt = |call: String| {
        if route.lenient { format!("hyperoute_core::lenient_opt({label:?}, {call})") } else { format!("__try_param!({label:?}, {call})") }
    };

    match shape {
        Shape::One if ty.is_str_ref && !route.merged => {
            let value = one(format!("hyperoute_core::form_str(&__form, {keys}, {display:?})"));
            let _ = writeln!(out, "{indent}let {name} = {value};");
        }
        Shape::One if ty.is_str_ref => {
            // 合并模式下的值可能是 "[a,b]" 拆开后新生成的字符串，借用不了，先取成 String
            let value = one(format!("hyperoute_core::form_value::<String>(&__form, {keys}, {display:?}, {expected}, \"&str\")"));
            let _ = writeln!(out, "{indent}let {name}__owned: String = {value};");
            let _ = writeln!(out, "{indent}let {name}: &str = &{name}__owned;");
        }
        Shape::One => {
            let value = one(format!("hyperoute_core::form_value(&__form, {keys}, {display:?}, {expected}, {rust_type:?})"));
            let _ = writeln!(out, "{indent}let {name} = {value};");
        }
        Shape::Opt if ty.is_str_ref && !route.merged => {
            let _ = writeln!(out, "{indent}let {name} = __try_param!({label:?}, hyperoute_core::form_opt_str(&__form, {keys}));");
        }
        Shape::Opt if ty.is_str_ref => {
            let value = opt(format!("hyperoute_core::form_opt::<String>(&__form, {keys}, {expected}, \"&str\")"));
            let _ = writeln!(out, "{indent}let {name}__owned: Option<String> = {value};");
            let _ = writeln!(out, "{indent}let {name} = {name}__owned.as_deref();");
        }
        Shape::Opt => {
            let value = opt(format!("hyperoute_core::form_opt(&__form, {keys}, {expected}, {rust_type:?})"));
            let _ = writeln!(out, "{indent}let {name} = {value};");
        }
        Shape::Many if route.lenient => {
            let _ = writeln!(
                out,
                "{indent}let {name} = hyperoute_core::form_vec_lenient({label:?}, &__form, {keys}, {expected}, {rust_type:?});"
            );
        }
        Shape::Many => {
            let _ = writeln!(
                out,
                "{indent}let {name} = __try_param!({label:?}, hyperoute_core::form_vec(&__form, {keys}, {expected}, {rust_type:?}));"
            );
        }
    }
}

/// 合并模式下的请求体结构体参数：
/// - 含文件字段：只支持 multipart，文本字段从合并后的表单取值，文件字段从 `__fields` 取；
/// - 不含文件字段：JSON 请求体按 JSON 解析，其它情况（urlencoded / multipart / 没有请求体的 GET）
///   从合并后的表单按字段名取值（`form_struct`，需要 `Deserialize`）。
fn write_body_setup_merged(
    setup: &mut String,
    route: &RouteInfo,
    name: &str,
    rust_type: &str,
    struct_info: Option<&BodyStructInfo>,
) {
    let label = &route.label;
    let has_file_field = struct_info.is_some_and(|info| {
        info.fields.iter().any(|(_, kind, _, _)| !matches!(kind, FieldKind::Plain(..)))
    });

    if has_file_field {
        let (pre_decls, multipart_branch) = render_multipart_branch(label, struct_info.unwrap(), Some(route));
        setup.push_str(&pre_decls);
        let _ = writeln!(
            setup,
            "            let {name} = if __content_type.starts_with(\"multipart/form-data\") {{\n{multipart_branch}            }} else {{\n                return Ok(__param_error({label:?}, hyperoute_core::unsupported_content_type(&__content_type, {rust_type:?})).await);\n            }};"
        );
        return;
    }

    let options = format!(
        "hyperoute_core::FormOptions {{ lenient: {}, camel: {} }}",
        route.lenient, route.camel
    );
    let _ = writeln!(
        setup,
        "            let {name} = if __content_type.starts_with(\"application/json\") {{\n                __try_param!({label:?}, hyperoute_core::json_body(&__body_bytes, {rust_type:?}))\n            }} else {{\n                __try_param!({label:?}, hyperoute_core::form_struct({label:?}, &__form, {rust_type:?}, {options}))\n            }};"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes(src: &str, options: ScanOptions) -> Vec<RouteInfo> {
        let file: syn::File = syn::parse_str(src).unwrap();
        let dir = std::env::temp_dir().join(format!("hyperoute-codegen-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("t{}.rs", src.len()));
        fs::write(&path, quote::quote!(#file).to_string()).unwrap();
        let mut collected = Collected::default();
        scan_file(&path, "crate::t", &mut collected, options);
        let _ = fs::remove_file(&path);
        collected.routes
    }

    #[test]
    fn classifies_new_param_kinds() {
        let r = routes(
            r#"#[GET("/d/{id}/{*path}")]
               async fn f(id: i64, path: String, headers: &HeaderMap, addr: Option<RemoteAddr>, ids: Vec<i64>, tag: Option<&str>, _client_flag: i32) -> String { todo!() }"#,
            ScanOptions { naming: ParamNaming::CamelCase, ..Default::default() },
        );
        let p = &r[0].params;
        assert!(matches!(&p[1], ParamSource::RestPath { index: 2, .. }));
        assert!(matches!(&p[2], ParamSource::Headers { by_ref: true, .. }));
        assert!(matches!(&p[3], ParamSource::RemoteAddr { optional: true, .. }));
        assert!(matches!(&p[4], ParamSource::Query { shape: Shape::Many, .. }));
        assert!(matches!(&p[5], ParamSource::Query { shape: Shape::Opt, ty, .. } if ty.is_str_ref));
        let ParamSource::Query { wire, .. } = &p[6] else { panic!() };
        assert_eq!(wire.lookup, ["_client_flag", "_clientFlag"]);
        assert_eq!(r[0].rest_index(), Some(2));
    }

    #[test]
    fn lenient_attribute_overrides_global() {
        let src = r#"#[PARAM_STRICT] #[GET("/a")] async fn a(id: i64) -> String { todo!() }
                     #[GET("/b")] async fn b(id: i64) -> String { todo!() }
                     #[PARAM_LENIENT] #[GET("/c")] async fn c(id: i64) -> String { todo!() }"#;
        let lenient = |o| routes(src, o).iter().map(|r| r.lenient).collect::<Vec<_>>();
        assert_eq!(lenient(ScanOptions { fallback: ParamFallback::Default, ..Default::default() }), [false, true, true]);
        assert_eq!(lenient(ScanOptions::default()), [false, false, true]);
    }

    #[test]
    #[should_panic(expected = "只能写在路径模板的最后一段")]
    fn rest_must_be_last() {
        parse_path_template("/d/{*path}/x", "f");
    }

    #[test]
    #[should_panic(expected = "BodyStream 参数不能和请求体结构体")]
    fn body_stream_conflicts_with_body() {
        routes(
            r#"#[POST("/u")] async fn u(body: BodyStream, form: Form) -> String { todo!() }"#,
            ScanOptions::default(),
        );
    }

    #[test]
    #[should_panic(expected = "默认模式下 multipart 文件参数不能和请求体结构体参数同时使用")]
    fn file_param_conflicts_with_body_in_separate_mode() {
        let src = r#"#[POST("/u")] async fn u(file: MultipartFile<Vec<u8>>, form: Form) -> String { todo!() }"#;
        // 合并模式下可以同时使用
        routes(src, ScanOptions { binding: ParamBinding::Merged, ..Default::default() });
        routes(src, ScanOptions::default());
    }
}
