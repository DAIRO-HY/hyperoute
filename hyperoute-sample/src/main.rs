mod compat;
mod controller;
mod error;
mod filter;
mod server;
#[cfg(test)]
mod tests;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // 绑定端口失败（例如端口被占用）时错误会返回给 main，进程以非 0 状态退出并打印原因
    server::start().await
}
