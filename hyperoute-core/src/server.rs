//! 服务启动。
//!
//! - [`serve`]：一行启动，监听地址、接收连接、每个连接一个任务，适合大多数场景。
//! - [`serve_connection`]：只负责处理一条已建立的连接。需要自己控制监听和接收
//!   （TLS、连接数限制、自定义 socket 参数、优雅停机等）时，自己写 accept 循环并调用它。
//! - [`serve_connection_with_addr`]：同上，另外把客户端地址传进去，handler 用 [`RemoteAddr`] 参数取出。
//!   [`serve`] 内部用的就是它。
//!
//! 两者都会开启 HTTP upgrade，`#[WEBSOCKET]` 路由依赖它；自己用 hyper 启动服务时
//! 务必也调用 `.with_upgrades()`，否则 WebSocket 握手返回 101 后连接无法升级。

use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::ops::Deref;
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
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                // 文件描述符耗尽等错误通常是暂时的：记日志、稍等再继续，不让整个服务退出
                eprintln!("[hyperoute] accept failed: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        let router = router.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_connection_with_addr(stream, peer, router).await {
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

/// 用 HTTP/1.1 处理一条连接，并把客户端地址放进每个请求的 extensions（handler 用 [`RemoteAddr`] 参数取出，
/// 过滤器用 `req.extensions().get::<RemoteAddr>()`）。
pub async fn serve_connection_with_addr<I, F, Fut>(io: I, peer: SocketAddr, router: F) -> Result<(), hyper::Error>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Fn(hyper::Request<Incoming>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Response, hyper::Error>> + Send + 'static,
{
    serve_connection(io, move |mut req: hyper::Request<Incoming>| {
        req.extensions_mut().insert(RemoteAddr(peer));
        router(req)
    })
    .await
}

/// handler 参数：客户端地址（TCP 对端）。经过反向代理时这是代理的地址，真实地址需要自己看 `X-Forwarded-For`。
///
/// - `RemoteAddr`：必须存在；用 [`serve`] / [`serve_connection_with_addr`] 启动时总是有。
///   用 [`serve_connection`] 启动时没有，属于服务端配置问题，返回 500。
/// - `Option<RemoteAddr>`：没有时为 `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RemoteAddr(pub SocketAddr);

impl RemoteAddr {
    /// 从请求 extensions 里取出（过滤器里也可以用）。
    pub fn from_request<B>(req: &hyper::Request<B>) -> Option<RemoteAddr> {
        req.extensions().get::<RemoteAddr>().copied()
    }

    /// 生成代码调用：`RemoteAddr` 参数取不到时返回错误（记日志 + 500）。
    pub fn require<B>(req: &hyper::Request<B>) -> Result<RemoteAddr, MissingRemoteAddr> {
        Self::from_request(req).ok_or(MissingRemoteAddr)
    }
}

impl Deref for RemoteAddr {
    type Target = SocketAddr;

    fn deref(&self) -> &SocketAddr {
        &self.0
    }
}

impl fmt::Display for RemoteAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// [`RemoteAddr::require`] 取不到地址时的错误；只进服务端日志。
#[derive(Debug)]
pub struct MissingRemoteAddr;

impl fmt::Display for MissingRemoteAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("remote address not available; start the server with serve() or serve_connection_with_addr()")
    }
}

impl std::error::Error for MissingRemoteAddr {}
