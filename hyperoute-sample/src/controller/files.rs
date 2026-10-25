//! 文件下载（Range：断点下载、视频拖动）、尾部通配路径参数 `{*path}`、请求头、客户端地址、流式请求体。

use std::path::Path;

use hyper::{HeaderMap, StatusCode};
use hyperoute_core::{BodyStream, RemoteAddr, Stream};
use hyperoute_macro::{GET, POST};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::error::ApiError;

/// 示例直接用 static 目录里的文件。
const FILE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/static");

/// `{*path}` 匹配剩下的 1 个或多个路径段（每段解码后用 `/` 拼起来）；路径已经规范化过，不会有 `..`。
/// `&HeaderMap` 拿到请求头，`Stream::file` 按它处理 Range / If-Range / 304。
/// curl -i http://127.0.0.1:3000/files/css/app.css
/// curl -i -H 'Range: bytes=0-9' http://127.0.0.1:3000/files/css/app.css      → 206
/// curl -i -H 'Range: bytes=99999-' http://127.0.0.1:3000/files/css/app.css   → 416
/// curl -OJ "http://127.0.0.1:3000/files/img/logo.svg?download=true"        → Content-Disposition: attachment
#[GET("/files/{*path}")]
pub async fn download(path: String, download: Option<bool>, headers: &HeaderMap) -> Result<Stream, ApiError> {
    let full = Path::new(FILE_ROOT).join(&path);
    let stream = Stream::file(&full, headers)
        .await
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "NOT_FOUND", "file not found"))?;
    if download == Some(true) {
        let file_name = full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(stream.attachment(&file_name))
    } else {
        Ok(stream)
    }
}

/// 普通 `{name}` 路由的优先级高于 `{*path}`：`/d/abc/info` 走这里，`/d/abc/info/x` 走下面那个。
/// curl http://127.0.0.1:3000/d/abc/info
#[GET("/d/{url_path}/info")]
pub async fn dfs_info(url_path: String) -> String {
    format!("info of {url_path}")
}

/// curl http://127.0.0.1:3000/d/abc/folder/%E4%B8%AD%E6%96%87.txt   → url_path = abc, path = folder/中文.txt
#[GET("/d/{url_path}/{*path}")]
pub async fn dfs_download(url_path: String, path: String) -> String {
    format!("url_path = {url_path}, path = {path}")
}

/// `RemoteAddr` 是 TCP 对端地址（经过反向代理时是代理的地址，真实地址看 `X-Forwarded-For`）。
/// curl -H 'X-Forwarded-For: 10.0.0.1' http://127.0.0.1:3000/whoami
#[GET("/whoami")]
pub async fn whoami(addr: RemoteAddr, headers: &HeaderMap) -> Value {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    json!({
        "addr": addr.to_string(),
        "ip": addr.ip().to_string(),
        "userAgent": header("user-agent"),
        "xForwardedFor": header("x-forwarded-for"),
    })
}

/// `BodyStream` 拿到未缓冲的请求体，边收边写，几十 GB 的文件也不会读进内存。
/// curl --data-binary @Cargo.toml http://127.0.0.1:3000/upload/stream/demo.bin
#[POST("/upload/stream/{name}")]
pub async fn upload_stream(name: String, mut body: BodyStream) -> Result<Value, ApiError> {
    let internal = |_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", "write failed");
    let path = std::env::temp_dir().join(format!("hyperoute-stream-{name}"));
    let mut file = tokio::fs::File::create(&path).await.map_err(internal)?;
    let mut total = 0u64;
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk.map_err(|_| ApiError::bad_request("upload interrupted"))?;
        file.write_all(&chunk).await.map_err(internal)?;
        total += chunk.len() as u64;
    }
    file.flush().await.map_err(internal)?;
    Ok(json!({ "name": name, "bytes": total, "savedTo": path.to_string_lossy() }))
}
