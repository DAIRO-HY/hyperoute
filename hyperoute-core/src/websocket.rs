//! `#[WEBSOCKET("/path")]` 路由的运行时支持。
//!
//! 生成代码的流程：校验握手请求头（[`WebSocketUpgrade::from_request`]）→ 解析路径 / 查询 / `Ext` 参数
//! → 返回 101 并在后台任务里等连接升级完成，再把 [`WebSocket`] 交给用户函数（[`WebSocketUpgrade::on_upgrade`]）。
//! 握手是普通的 GET 请求，会完整经过过滤链；握手头不合法时返回 400 参数错误，不会升级。
//!
//! 对外的 [`Message`] / [`CloseFrame`] / [`WebSocketError`] 是框架自己的类型，不直接暴露 tungstenite，
//! 以后换底层实现不影响业务代码。

use std::fmt;
use std::future::Future;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Empty};
use hyper::StatusCode;
use hyper::body::Bytes;
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::param::{ParamError, ParamErrorKind, ParamLocation};
use crate::{Request, Response};

type Stream = WebSocketStream<TokioIo<Upgraded>>;

/// 一条 WebSocket 消息。
///
/// 收到 Ping 时底层会自动回 Pong，业务代码一般不需要处理 Ping / Pong。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    /// 对方发起关闭；收到后应结束收发循环
    Close(Option<CloseFrame>),
}

impl Message {
    pub fn text(text: impl Into<String>) -> Self {
        Message::Text(text.into())
    }

    pub fn binary(data: impl Into<Bytes>) -> Self {
        Message::Binary(data.into())
    }
}

/// 关闭帧：状态码（例如 1000 正常关闭）+ 原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    pub code: u16,
    pub reason: String,
}

impl From<Message> for tungstenite::Message {
    fn from(message: Message) -> Self {
        match message {
            Message::Text(text) => tungstenite::Message::Text(text.into()),
            Message::Binary(data) => tungstenite::Message::Binary(data),
            Message::Ping(data) => tungstenite::Message::Ping(data),
            Message::Pong(data) => tungstenite::Message::Pong(data),
            Message::Close(frame) => tungstenite::Message::Close(frame.map(|f| {
                tungstenite::protocol::CloseFrame {
                    code: f.code.into(),
                    reason: f.reason.into(),
                }
            })),
        }
    }
}

/// 底层的原始帧（`Frame`）只在手动构造时出现，读取时不会收到，统一返回 None 跳过。
fn from_tungstenite(message: tungstenite::Message) -> Option<Message> {
    Some(match message {
        tungstenite::Message::Text(text) => Message::Text(text.as_str().to_string()),
        tungstenite::Message::Binary(data) => Message::Binary(data),
        tungstenite::Message::Ping(data) => Message::Ping(data),
        tungstenite::Message::Pong(data) => Message::Pong(data),
        tungstenite::Message::Close(frame) => Message::Close(frame.map(|f| CloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().to_string(),
        })),
        tungstenite::Message::Frame(_) => return None,
    })
}

/// 收发过程中的错误（连接断开、协议错误、消息过大等）。
#[derive(Debug)]
pub struct WebSocketError(tungstenite::Error);

impl fmt::Display for WebSocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for WebSocketError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<tungstenite::Error> for WebSocketError {
    fn from(error: tungstenite::Error) -> Self {
        Self(error)
    }
}

/// 已建立的 WebSocket 连接，作为 `#[WEBSOCKET]` 函数的参数传入。
pub struct WebSocket {
    inner: Stream,
}

impl WebSocket {
    /// 等下一条消息；连接已关闭时返回 None。
    pub async fn recv(&mut self) -> Option<Result<Message, WebSocketError>> {
        next_message(&mut self.inner).await
    }

    pub async fn send(&mut self, message: Message) -> Result<(), WebSocketError> {
        Ok(self.inner.send(message.into()).await?)
    }

    /// 主动发起关闭。
    pub async fn close(&mut self, frame: Option<CloseFrame>) -> Result<(), WebSocketError> {
        self.send(Message::Close(frame)).await
    }

    /// 拆成发送端和接收端，可以分别放进不同的任务里同时收发
    /// （例如一个任务读客户端消息，另一个任务把广播消息推给客户端）。
    pub fn split(self) -> (WebSocketSender, WebSocketReceiver) {
        let (sink, stream) = self.inner.split();
        (WebSocketSender { inner: sink }, WebSocketReceiver { inner: stream })
    }
}

pub struct WebSocketSender {
    inner: SplitSink<Stream, tungstenite::Message>,
}

impl WebSocketSender {
    pub async fn send(&mut self, message: Message) -> Result<(), WebSocketError> {
        Ok(self.inner.send(message.into()).await?)
    }

    pub async fn close(&mut self, frame: Option<CloseFrame>) -> Result<(), WebSocketError> {
        self.send(Message::Close(frame)).await
    }
}

pub struct WebSocketReceiver {
    inner: SplitStream<Stream>,
}

impl WebSocketReceiver {
    pub async fn recv(&mut self) -> Option<Result<Message, WebSocketError>> {
        next_message(&mut self.inner).await
    }
}

async fn next_message<S>(stream: &mut S) -> Option<Result<Message, WebSocketError>>
where
    S: futures_util::Stream<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await? {
            Ok(tungstenite::Message::Close(frame)) => {
                // 收到对方的 Close 时，底层已排队一个关闭回复，但要到下一次读取才会真正发出去。
                // 业务代码通常收到 Close 就 break 并丢弃连接，回复会丢失、对方收不到关闭确认；
                // 所以这里替它多读一次：发出回复后底层立即返回“连接已关闭”，不会阻塞。
                let _ = stream.next().await;
                return Some(Ok(from_tungstenite(tungstenite::Message::Close(frame))?));
            }
            Ok(message) => {
                if let Some(message) = from_tungstenite(message) {
                    return Some(Ok(message));
                }
            }
            Err(error) => return Some(Err(error.into())),
        }
    }
}

/// 握手阶段的中间状态，由生成代码使用。
pub struct WebSocketUpgrade {
    key: String,
    on_upgrade: OnUpgrade,
}

impl WebSocketUpgrade {
    /// 校验 WebSocket 握手请求头；不合法时返回位置为 [`ParamLocation::Header`] 的参数错误（400）。
    pub fn from_request(req: &mut Request) -> Result<Self, ParamError> {
        let headers = req.headers();

        let upgrade = header(headers, "upgrade")?;
        if !upgrade.eq_ignore_ascii_case("websocket") {
            return Err(invalid_header("upgrade", upgrade, "expected websocket"));
        }

        let connection = header(headers, "connection")?;
        if !connection.split(',').any(|token| token.trim().eq_ignore_ascii_case("upgrade")) {
            return Err(invalid_header("connection", connection, "expected upgrade"));
        }

        let version = header(headers, "sec-websocket-version")?;
        if version.trim() != "13" {
            return Err(invalid_header("sec-websocket-version", version, "expected 13"));
        }

        let key = header(headers, "sec-websocket-key")?.trim().to_string();
        if key.is_empty() {
            return Err(ParamError::missing(ParamLocation::Header, "sec-websocket-key"));
        }

        Ok(Self {
            key,
            on_upgrade: hyper::upgrade::on(req),
        })
    }

    /// 返回 101 响应；响应发出、连接升级完成后，在后台任务里调用 `callback`。
    /// 升级失败（例如服务端没开启 upgrades、客户端提前断开）只记日志。
    pub fn on_upgrade<F, Fut>(self, route: &'static str, callback: F) -> Response
    where
        F: FnOnce(WebSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let accept = tungstenite::handshake::derive_accept_key(self.key.as_bytes());
        let on_upgrade = self.on_upgrade;

        tokio::spawn(async move {
            match on_upgrade.await {
                Ok(upgraded) => {
                    let inner = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
                    callback(WebSocket { inner }).await;
                }
                Err(error) => eprintln!("[hyperoute] websocket upgrade failed on {route}: {error}"),
            }
        });

        hyper::Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header("upgrade", "websocket")
            .header("connection", "upgrade")
            .header("sec-websocket-accept", accept)
            .body(Empty::<Bytes>::new().map_err(|never| match never {}).boxed())
            .unwrap()
    }
}

fn header<'a>(headers: &'a hyper::HeaderMap, name: &'static str) -> Result<&'a str, ParamError> {
    let value = headers
        .get(name)
        .ok_or_else(|| ParamError::missing(ParamLocation::Header, name))?;
    value.to_str().map_err(|e| {
        ParamError::new(ParamLocation::Header, Some(name.to_string()), ParamErrorKind::Invalid).with_reason(e)
    })
}

fn invalid_header(name: &str, value: &str, detail: &str) -> ParamError {
    ParamError::new(ParamLocation::Header, Some(name.to_string()), ParamErrorKind::Invalid)
        .with_raw_value(value)
        .with_detail(detail)
}
