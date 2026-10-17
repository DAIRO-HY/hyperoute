use hyper::StatusCode;
use hyperoute_core::IntoErrorResponse;
use serde_json::{Map, Value, json};

/// 示例项目统一的错误类型：业务错误（handler 返回的 `Err`）、参数错误、鉴权失败
/// 都转换成同一种 JSON 格式 `{"code": "...", "message": "...", ...}`。
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    body: Map<String, Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        let mut body = Map::new();
        body.insert("code".into(), json!(code));
        body.insert("message".into(), json!(message.into()));
        Self { status, body }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BAD_REQUEST", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", message)
    }

    /// 追加一个字段到 JSON 响应体里。
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.body.insert(key.into(), value.into());
        self
    }
}

impl IntoErrorResponse for ApiError {
    fn status(&self) -> StatusCode {
        self.status
    }

    fn message(&self) -> String {
        Value::Object(self.body.clone()).to_string()
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![("content-type", "application/json".to_string())]
    }
}
