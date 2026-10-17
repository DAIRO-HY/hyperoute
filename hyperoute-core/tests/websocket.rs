//! 端到端：真实 TCP 连接 + serve_connection + WebSocket 握手 / 收发 / 关闭。

use futures_util::{SinkExt, StreamExt};
use hyperoute_core::{Message, Response, WebSocketUpgrade};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;

async fn router(req: hyper::Request<hyper::body::Incoming>) -> Result<Response, hyper::Error> {
    let mut req = req.map(hyperoute_core::box_request_body);
    let upgrade = match WebSocketUpgrade::from_request(&mut req) {
        Ok(upgrade) => upgrade,
        Err(error) => {
            return Ok(hyperoute_core::error_response(&hyperoute_core::DefaultParamErrorResponse(
                hyperoute_core::ParamErrors::single(error),
            )));
        }
    };
    Ok(upgrade.on_upgrade("TEST /ws", |socket| async move {
        // 用 split 验证收发两端可以分开使用
        let (mut tx, mut rx) = socket.split();
        tx.send(Message::text("welcome")).await.unwrap();
        while let Some(Ok(message)) = rx.recv().await {
            match message {
                Message::Text(text) => tx.send(Message::text(format!("echo: {text}"))).await.unwrap(),
                Message::Binary(data) => tx.send(Message::Binary(data)).await.unwrap(),
                Message::Close(_) => break,
                _ => {}
            }
        }
    }))
}

async fn start_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(hyperoute_core::serve_connection(stream, router));
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_roundtrip() {
    let addr = start_server().await;
    let (mut client, response) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
    assert_eq!(response.status(), 101);

    let welcome = client.next().await.unwrap().unwrap();
    assert_eq!(welcome, tungstenite::Message::text("welcome"));

    client.send(tungstenite::Message::text("你好")).await.unwrap();
    assert_eq!(client.next().await.unwrap().unwrap(), tungstenite::Message::text("echo: 你好"));

    client.send(tungstenite::Message::binary(vec![1u8, 2, 3])).await.unwrap();
    assert_eq!(client.next().await.unwrap().unwrap(), tungstenite::Message::binary(vec![1u8, 2, 3]));

    // 客户端发起关闭：服务端 handler 收到 Close 后直接 break，框架仍保证回复关闭帧
    client.close(None).await.unwrap();
    match client.next().await {
        Some(Ok(tungstenite::Message::Close(_))) => {}
        other => panic!("expected close reply, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_http_request_is_rejected() {
    let addr = start_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(b"GET /ws HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(response.ends_with(r#"Header "upgrade" is required."#), "{response}");
}
