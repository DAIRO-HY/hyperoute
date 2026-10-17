use hyperoute_core::{Ext, Message, WebSocket, WebSocketError};
use hyperoute_macro::WEBSOCKET;

use crate::filter::auth::CurrentUser;

/// 最简单的 WebSocket：收到什么回什么。
/// 测试：websocat ws://127.0.0.1:3000/ws/echo
#[WEBSOCKET("/ws/echo")]
pub async fn echo(mut socket: WebSocket) {
    while let Some(Ok(message)) = socket.recv().await {
        let reply = match message {
            Message::Text(text) => Message::text(format!("echo: {text}")),
            Message::Binary(data) => Message::Binary(data),
            Message::Close(_) => break,
            _ => continue, // Ping / Pong 由底层自动处理
        };
        if socket.send(reply).await.is_err() {
            break;
        }
    }
}

/// 和普通路由一样可以用路径参数、查询参数、过滤器传来的数据：
/// `/api/**` 被 auth 过滤器覆盖，没登录在握手阶段就会被拒绝（401），不会升级。
/// 返回 Result 时可以用 `?`，Err 由框架记日志。
/// 测试：websocat -H 'Authorization: Bearer demo-token' "ws://127.0.0.1:3000/api/ws/chat/rust?nickname=alice"
#[WEBSOCKET("/api/ws/chat/{room}")]
pub async fn chat(
    socket: WebSocket,
    room: String,
    user: Ext<CurrentUser>,
    nickname: String,
) -> Result<(), WebSocketError> {
    // 拆成发送端和接收端；真实的聊天室里通常把发送端交给另一个任务，用来推送广播消息
    let (mut sender, mut receiver) = socket.split();
    sender
        .send(Message::text(format!("{nickname} (user {}) joined room {room}", user.id)))
        .await?;

    while let Some(message) = receiver.recv().await {
        match message? {
            Message::Text(text) => sender.send(Message::text(format!("[{room}] {nickname}: {text}"))).await?,
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}
