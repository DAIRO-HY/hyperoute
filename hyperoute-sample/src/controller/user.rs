//! 过滤器传数据给 controller：`filter/auth.rs` 把登录用户放进请求，这里用 `Ext<T>` 取出。

use hyperoute_core::Ext;
use hyperoute_macro::GET;

use crate::filter::auth::CurrentUser;

/// 被 auth 过滤器排除，不需要登录。
/// curl http://127.0.0.1:3000/api/login
#[GET("/api/login")]
pub async fn login() -> &'static str {
    "use header: Authorization: Bearer demo-token"
}

/// 需要登录。
/// curl -H 'Authorization: Bearer demo-token' http://127.0.0.1:3000/api/profile
#[GET("/api/profile")]
pub async fn profile(user: Ext<CurrentUser>) -> String {
    format!("id = {}, name = {}", user.id, user.name)
}

/// `Ext` 可以和路径参数、查询参数混用。
/// curl -H 'Authorization: Bearer demo-token' "http://127.0.0.1:3000/api/orders/7?verbose=true"
#[GET("/api/orders/{order_id}")]
pub async fn order(order_id: i64, user: Ext<CurrentUser>, verbose: bool) -> String {
    format!("user {} reads order {order_id} (verbose = {verbose})", user.id)
}

/// 可选登录：`Option<Ext<T>>` 在没有数据时为 None。
/// auth 过滤器不覆盖这个路径，所以这里总是 guest。
/// curl http://127.0.0.1:3000/home
#[GET("/home")]
pub async fn home(user: Option<Ext<CurrentUser>>) -> String {
    match user {
        Some(user) => format!("hello, {}", user.name),
        None => "hello, guest".to_string(),
    }
}
