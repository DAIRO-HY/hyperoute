use hyperoute_core::{Next, Request, Response};
use hyperoute_macro::FILTER;

use crate::error::ApiError;

/// 过滤器放进请求、controller 用 `Ext<CurrentUser>` 取出的登录信息。
/// `req.extensions_mut().insert` 要求 `Clone + Send + Sync + 'static`。
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub name: String,
}

/// 演示 before 处理、中断请求、向 controller 传数据：
/// `/api/**` 下除了 `/api/login` 都要求 `Authorization: Bearer <token>`。
#[FILTER(order = 10, include = ["/api/**"], exclude = ["/api/login"])]
pub async fn auth(mut req: Request, next: Next) -> Result<Response, ApiError> {
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;

    // 演示用：真实场景这里查 session / 校验 JWT
    let user = match token {
        "demo-token" => CurrentUser {
            id: 1,
            name: "alice".to_string(),
        },
        _ => return Err(ApiError::unauthorized("invalid token")),
    };

    req.extensions_mut().insert(user);
    Ok(next.run(req).await)
}
