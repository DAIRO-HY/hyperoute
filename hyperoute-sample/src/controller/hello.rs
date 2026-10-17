use hyperoute_macro::{GET, REQUEST};

#[GET("/")]
pub async fn hello() -> &'static str {
    "Hello, hyperoute!"
}

/// `#[REQUEST]` 匹配任意 HTTP 方法。
#[REQUEST("/echo")]
pub async fn echo() -> &'static str {
    "this route accepts any HTTP method"
}
