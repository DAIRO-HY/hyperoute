use tokio::net::TcpListener;

// build.rs 扫描 src/controller、src/filter 下的 #[GET]、#[FILTER] 等注解后生成的路由代码。
mod routes {
    include!(concat!(env!("OUT_DIR"), "/routes.rs"));
}

const ADDR: &str = "127.0.0.1:3000";

/// 一行启动：监听、接收连接、每个连接一个任务都由框架处理。
pub async fn start() -> std::io::Result<()> {
    hyperoute_core::serve(ADDR, routes::router).await
}

/// 自己控制监听和接收连接的写法（TLS、连接数限制、自定义 socket 参数、优雅停机等场景），
/// 每条连接交给 `serve_connection` 处理（已开启 upgrade，WebSocket 可用）。
/// 把 main.rs 里的 `start()` 换成 `start_custom()` 即可使用。
#[allow(dead_code)]
pub async fn start_custom() -> std::io::Result<()> {
    let listener = TcpListener::bind(ADDR).await?;
    println!("Listening on http://{ADDR}");

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(error) = hyperoute_core::serve_connection(stream, routes::router).await {
                eprintln!("connection error: {error:?}");
            }
        });
    }
}
