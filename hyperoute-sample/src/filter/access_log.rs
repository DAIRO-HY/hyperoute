use std::time::Instant;

use hyperoute_core::{Next, Request, Response};
use hyperoute_macro::FILTER;

/// 演示 after 处理：记录每个请求的耗时，并加到响应头里。
/// order 最小，最先进入、最后退出，所以统计的是包括其它过滤器在内的总耗时。
/// 注意：Stream 响应在这里只能统计到响应头准备好为止，body 还在后台继续发送。
#[FILTER(order = -100)]
pub async fn access_log(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let mut response = next.run(req).await;

    let elapsed = start.elapsed();
    println!("{method} {path} -> {} ({elapsed:?})", response.status().as_u16());
    response
        .headers_mut()
        .insert("x-response-time-us", (elapsed.as_micros() as u64).into());
    response
}
