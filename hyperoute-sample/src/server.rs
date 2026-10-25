use hyper::body::Incoming;
use hyperoute_core::Response;
use tokio::net::TcpListener;

// build.rs 扫描 src/controller、src/filter 下的 #[GET]、#[FILTER] 等注解后生成的路由代码。
mod routes {
    include!(concat!(env!("OUT_DIR"), "/routes.rs"));
}

// build.rs 用 Go 兼容开关（合并取值 + 宽松模式）扫描 src/compat 生成的路由代码。
mod routes_compat {
    include!(concat!(env!("OUT_DIR"), "/routes_compat.rs"));
}

const ADDR: &str = "0.0.0.0:3000";

/// 按路径前缀分给两套生成的路由。一般项目只有一套，直接把 `routes::router` 交给 `serve` 即可。
pub async fn router(req: hyper::Request<Incoming>) -> Result<Response, hyper::Error> {
    if req.uri().path().starts_with("/compat/") {
        routes_compat::router(req).await
    } else {
        routes::router(req).await
    }
}

/// 一行启动：监听、接收连接、每个连接一个任务都由框架处理（handler 可以用 `RemoteAddr` 拿到客户端地址）。
pub async fn start() -> std::io::Result<()> {
    hyperoute_core::serve(ADDR, router).await
}

/// 自己控制监听和接收连接的写法（TLS、连接数限制、自定义 socket 参数、优雅停机等场景），
/// 每条连接交给 `serve_connection_with_addr` 处理（已开启 upgrade，WebSocket 可用；
/// 不需要 `RemoteAddr` 时也可以用 `serve_connection(stream, router)`）。
/// 把 main.rs 里的 `start()` 换成 `start_custom()` 即可使用。
#[allow(dead_code)]
pub async fn start_custom() -> std::io::Result<()> {
    let listener = TcpListener::bind(ADDR).await?;
    println!("Listening on http://{ADDR}");

    loop {
        let (stream, peer) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(error) = hyperoute_core::serve_connection_with_addr(stream, peer, router).await {
                eprintln!("connection error: {error:?}");
            }
        });
    }
}
