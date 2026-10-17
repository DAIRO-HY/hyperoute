//! 服务启动。
//!
//! - [`serve`]：一行启动，监听地址、接收连接、每个连接一个任务，适合大多数场景。
//! - [`serve_connection`]：只负责处理一条已建立的连接。需要自己控制监听和接收
//!   （TLS、连接数限制、自定义 socket 参数、优雅停机等）时，自己写 accept 循环并调用它。
//!
//! 两者都会开启 HTTP upgrade，`#[WEBSOCKET]` 路由依赖它；自己用 hyper 启动服务时
//! 务必也调用 `.with_upgrades()`，否则 WebSocket 握手返回 101 后连接无法升级。

use std::future::Future;
use std::time::Duration;

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, ToSocketAddrs};

use crate::Response;

/// 监听 `addr` 并一直运行；只有绑定地址失败时才返回错误。
/// `router` 一般就是生成代码里的 `router` 函数。
pub async fn serve<A, F, Fut>(addr: A, router: F) -> std::io::Result<()>
where
    A: ToSocketAddrs,
    F: Fn(hyper::Request<Incoming>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<Response, hyper::Error>> + Send + 'static,
{
    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", listener.local_addr()?);

    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                // 文件描述符耗尽等错误通常是暂时的：记日志、稍等再继续，不让整个服务退出
                eprintln!("[hyperoute] accept failed: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        let router = router.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, router).await {
                eprintln!("[hyperoute] connection error: {error:?}");
            }
        });
    }
}

/// 用 HTTP/1.1 处理一条连接（开启 upgrade，支持 WebSocket），直到连接关闭。
pub async fn serve_connection<I, F, Fut>(io: I, router: F) -> Result<(), hyper::Error>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Fn(hyper::Request<Incoming>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Response, hyper::Error>> + Send + 'static,
{
    http1::Builder::new()
        .serve_connection(TokioIo::new(io), service_fn(router))
        .with_upgrades()
        .await
}
