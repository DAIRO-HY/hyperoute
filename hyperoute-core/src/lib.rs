//! 运行时依赖：给业务代码的错误类型实现，供生成的路由代码在 `Err` 分支里调用，
//! 从而把 handler 返回的 `Result<T, E>` 里的 E 映射成具体的 HTTP 状态码 + 响应体。

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::{Stream as FutureStream, StreamExt};
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::StatusCode;
use hyper::body::{Bytes, Frame};

mod extract;
mod filter;
mod param;
mod request_path;
mod server;
mod static_files;
mod websocket;

pub use extract::{
    body_read_error, json_body, multipart_take, multipart_text, parse_param, query_map, query_param,
    query_param_str, query_struct, unsupported_content_type, urlencoded_body,
};
pub use filter::{
    BoxError, Ext, MissingExt, Next, Query, Request, RequestBody, Response, box_request_body, filter_applies,
    full_request_body, path_matches, read_body, take_ext, take_ext_opt,
};
pub use request_path::{RequestPath, normalize_path, normalize_request_path, request_path};
pub use server::{serve, serve_connection};
pub use static_files::{
    EmbeddedFile, GZIP_MAX_RUNTIME_SIZE, GZIP_MIN_SIZE, StaticMount, StaticSource, content_type, is_compressible,
    serve_static, set_static_cache_dir, set_static_root, static_init,
};
pub use websocket::{
    CloseFrame, Message, WebSocket, WebSocketError, WebSocketReceiver, WebSocketSender, WebSocketUpgrade,
};
pub use param::{
    DefaultParamErrorResponse, ParamError, ParamErrorKind, ParamErrors, ParamLocation, RAW_VALUE_DISPLAY_LIMIT,
};

/// 把错误映射成 HTTP 响应。handler 返回的 `Result<T, E>` 里的 E，
/// 以及 `#[PARAM_ERROR]` 函数的返回值，都要实现它。
pub trait IntoErrorResponse {
    fn status(&self) -> StatusCode;
    fn message(&self) -> String;

    /// 响应头。默认只有纯文本的 content-type；返回 JSON / XML 时覆盖它，
    /// 注意覆盖后需要自己带上 content-type。
    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![("content-type", "text/plain; charset=utf-8".to_string())]
    }
}

/// 用 [`IntoErrorResponse`] 的三个方法建响应。
pub fn error_response<E: IntoErrorResponse + ?Sized>(error: &E) -> hyper::Response<ResponseBody> {
    let mut builder = hyper::Response::builder().status(error.status());
    for (name, value) in error.headers() {
        builder = builder.header(name, value);
    }
    builder
        .body(boxed_full(Full::new(Bytes::from(error.message()))))
        .unwrap()
}

/// 服务端自身出错（不是客户端参数的问题）时的通用 500 响应，不带任何内部细节。
pub fn internal_error_response() -> hyper::Response<ResponseBody> {
    hyper::Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header("content-type", "text/plain; charset=utf-8")
        .body(boxed_full(Full::new(Bytes::from("Internal Server Error"))))
        .unwrap()
}

/// 所有路由统一使用的响应 body 类型：一个类型擦除后的 body，
/// 既能装下一次性攒好的 `Full<Bytes>`（Text/JSON/Raw），也能装下边读边发的流式 body。
pub type ResponseBody = BoxBody<Bytes, std::io::Error>;

/// 把 `Full<Bytes>`（错误类型是 `Infallible`，不可能真的出错）装箱成 [`ResponseBody`]。
/// hyperoute-codegen 在 Text/JSON/Raw 分支的末尾都会调用它，统一到同一个 body 类型。
pub fn boxed_full(body: Full<Bytes>) -> ResponseBody {
    body.map_err(|never: Infallible| match never {}).boxed()
}

/// handler 想返回二进制数据、并自己控制响应 header（含 content-type）时用这个包装类型；
/// hyperoute-codegen 按类型名 `Raw` 识别它，直接用这几个字段建响应，
/// 不走文本/JSON 那套自动分流。
///
/// `headers` 用 `Vec` 而不是 `HashMap`：允许同名 header 出现多次（例如 `Set-Cookie`），
/// 并保留调用方追加时的顺序。
pub struct Raw {
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Raw {
    pub fn new(body: impl Into<Vec<u8>>) -> Self {
        Self {
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// 链式追加一个 header，例如 `.header("content-type", "image/png")`。
    /// 同名 header 可以多次调用，每次都会追加一条，而不是覆盖。
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

/// handler 想返回一个边读边发的流式响应（大文件、转发别的接口的字节流等）时用这个包装类型；
/// hyperoute-codegen 按类型名 `Stream` 识别它，header API 跟 [`Raw`] 保持一致。
pub struct Stream {
    pub headers: Vec<(&'static str, String)>,
    pub body: ResponseBody,
}

impl Stream {
    /// `chunks` 产出的每一项要么是一块数据，要么是读取过程中的 I/O 错误。
    /// 常见来源：`tokio_util::io::ReaderStream::new(file)`（读大文件）、
    /// 或者把别的 HTTP 客户端返回的字节流 `.map_err(std::io::Error::other)` 转一下类型。
    pub fn new<S>(chunks: S) -> Self
    where
        S: FutureStream<Item = std::io::Result<Bytes>> + Send + Sync + 'static,
    {
        let frames = chunks.map(|chunk| chunk.map(Frame::data));
        let body = BodyExt::boxed(StreamBody::new(frames));
        Self {
            headers: Vec::new(),
            body,
        }
    }

    /// 链式追加一个 header，用法和 [`Raw::header`] 一致。
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

#[derive(Debug)]
pub struct MultipartFile<T> {

    //文件名
    pub name: String,

    //文件类型
    pub content_type: Option<String>,

    //数据；T为Vec<u8>时，则是文件二进制数据；T为&Path时，则是临时文件目录；
    pub body: T,
}

/// `multipart/form-data` 里解析出的一个字段，交给 hyperoute-codegen 生成的代码
/// 按 struct 字段类型分别处理（普通值 / `Vec<u8>` / `&Path` / `MultipartFile<T>`）。
pub struct MultipartField {
    pub file_name: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

/// 解析 `multipart/form-data` 请求体，按字段名建索引。
/// 注意：同名字段目前只保留最后一个，不支持同名多文件。
pub async fn parse_multipart(
    body: Bytes,
    content_type: &str,
) -> Result<HashMap<String, MultipartField>, ParamError> {
    let malformed = |reason: multer::Error| {
        ParamError::new(ParamLocation::Body, None, ParamErrorKind::MalformedBody)
            .with_detail("invalid multipart/form-data")
            .with_reason(reason)
    };

    let boundary = multer::parse_boundary(content_type).map_err(malformed)?;
    // 请求体已经整个收集到内存里了，包装成一个只产出一次的 stream 喂给 multer。
    let stream = futures_util::stream::once(async move { Ok::<Bytes, Infallible>(body) });
    let mut multipart = multer::Multipart::new(stream, boundary);

    let mut fields = HashMap::new();
    while let Some(field) = multipart.next_field().await.map_err(malformed)? {
        let Some(name) = field.name().map(|s| s.to_string()) else {
            continue;
        };
        let file_name = field.file_name().map(|s| s.to_string());
        let field_content_type = field.content_type().map(|m| m.to_string());
        let data = field.bytes().await.map_err(malformed)?.to_vec();
        fields.insert(
            name,
            MultipartField {
                file_name,
                content_type: field_content_type,
                data,
            },
        );
    }
    Ok(fields)
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 把上传的文件内容写到系统临时目录下的一个新文件里，返回它的路径。
/// 注意：不会自动清理——长期运行的服务如果大量走这条路径，需要自己加一个清理策略。
/// 写入失败是服务端的问题，生成代码会返回 [`internal_error_response`]，而不是参数错误。
pub async fn write_temp_file(data: &[u8]) -> std::io::Result<PathBuf> {
    let n = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("hyperoute-upload-{}-{n}", std::process::id()));
    tokio::fs::write(&path, data).await?;
    Ok(path)
}