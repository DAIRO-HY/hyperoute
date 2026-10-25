//! [`BodyStream`]：handler 直接拿到未缓冲的请求体，边收边处理（大文件上传、断点续传）。

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream as FutureStream;
use http_body_util::BodyExt;
use hyper::body::{Body, Bytes};

use crate::{BoxError, RequestBody};

/// handler 参数：未缓冲的请求体。用 [`BodyStream::next_chunk`] 逐块读取，或者当 `futures::Stream` 用。
///
/// 一个 handler 里最多一个，不能和请求体结构体 / multipart 文件参数同时出现；
/// 合并模式（`ParamBinding::Merged`）下这个路由的标量参数只从查询串取值。
/// 注意：如果过滤器调用过 `read_body`，这里拿到的是过滤器放回去的那份内存数据。
pub struct BodyStream {
    body: RequestBody,
}

impl BodyStream {
    /// 由生成代码构造。
    pub fn new(body: RequestBody) -> Self {
        Self { body }
    }

    /// 下一块数据；请求体读完时返回 `None`。trailers 会被跳过。
    pub async fn next_chunk(&mut self) -> Option<Result<Bytes, BoxError>> {
        loop {
            match self.body.frame().await? {
                Ok(frame) => {
                    if let Ok(data) = frame.into_data() {
                        return Some(Ok(data));
                    }
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }

    /// 客户端声明的请求体大小（`Content-Length`）；分块传输时为 `None`。
    pub fn size_hint(&self) -> Option<u64> {
        self.body.size_hint().exact()
    }

    /// 取出底层的 body。
    pub fn into_inner(self) -> RequestBody {
        self.body
    }
}

impl FutureStream for BodyStream {
    type Item = Result<Bytes, BoxError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        return Poll::Ready(Some(Ok(data)));
                    }
                }
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
