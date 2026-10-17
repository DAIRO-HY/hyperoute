//! 返回值类型决定响应格式：
//! - `&str` / `String` → 纯文本
//! - `Raw` → 二进制 + 自定义响应头；`Stream` → 流式响应 + 自定义响应头
//! - 其它类型 → JSON（要求实现 `Serialize`）
//! - `Result<T, E>` → Ok 按上面的规则处理，Err 通过 `IntoErrorResponse` 转换成响应

use hyperoute_core::{Raw, Stream};
use hyperoute_macro::GET;
use serde::Serialize;

use crate::error::ApiError;

#[derive(Serialize)]
pub struct User {
    pub id: i64,
    pub name: String,
}

/// curl http://127.0.0.1:3000/responses/text
#[GET("/responses/text")]
pub async fn text() -> String {
    "plain text".to_string()
}

/// curl http://127.0.0.1:3000/responses/json
#[GET("/responses/json")]
pub async fn json() -> User {
    User {
        id: 1,
        name: "alice".to_string(),
    }
}

/// curl http://127.0.0.1:3000/responses/result/1   → 200 + JSON
/// curl http://127.0.0.1:3000/responses/result/0   → 400 + ApiError
#[GET("/responses/result/{id}")]
pub async fn result(id: i64) -> Result<User, ApiError> {
    if id > 0 {
        Ok(User {
            id,
            name: "alice".to_string(),
        })
    } else {
        Err(ApiError::bad_request(format!("id must be positive, got {id}")))
    }
}

/// 同名响应头可以追加多次（例如 set-cookie）。
/// curl -i http://127.0.0.1:3000/responses/raw
#[GET("/responses/raw")]
pub async fn raw() -> Raw {
    Raw::new(vec![0xDE, 0xAD, 0xBE, 0xEF])
        .header("content-type", "application/octet-stream")
        .header("set-cookie", "a=1")
        .header("set-cookie", "b=2")
}

/// 边读边发：这里流式返回本项目的 Cargo.toml，真实场景可以是大文件或转发其它接口的字节流。
/// curl http://127.0.0.1:3000/responses/stream
#[GET("/responses/stream")]
pub async fn stream() -> Result<Stream, ApiError> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| ApiError::new(hyper::StatusCode::NOT_FOUND, "NOT_FOUND", "file not found"))?;

    Ok(Stream::new(tokio_util::io::ReaderStream::new(file)).header("content-type", "text/plain; charset=utf-8"))
}
