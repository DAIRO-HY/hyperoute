use hyperoute_core::ParamErrors;
use hyperoute_macro::PARAM_ERROR;

use crate::error::ApiError;

/// 参数解析失败时的响应格式。整个项目最多一个 `#[PARAM_ERROR]` 函数；
/// 不写的话框架默认返回 400 + 纯文本。
/// curl http://127.0.0.1:3000/params/path/abc
/// → {"code":"INVALID_PARAM","message":"Path parameter \"id\" must be an integer, got \"abc\".","location":"path","param":"id","kind":"invalid"}
#[PARAM_ERROR]
pub fn on_param_error(errors: ParamErrors) -> ApiError {
    let error = errors.first();
    ApiError::new(hyper::StatusCode::BAD_REQUEST, "INVALID_PARAM", error.to_string())
        .with("location", error.location.as_str())
        .with("param", error.param.clone())
        .with("kind", error.kind.as_str())
}
