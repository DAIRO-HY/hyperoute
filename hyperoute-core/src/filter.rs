//! 过滤器（`#[FILTER]`）的运行时支持。
//!
//! 过滤器是“环绕式”的：一个 `async fn(Request, Next) -> Response` 同时承担 before 和 after，
//! 调用 `next.run(req).await` 之前的代码是 before，之后的是 after，不调用就是中断请求。
//! 过滤器往 `req.extensions_mut()` 里放的数据，controller 用 [`Ext<T>`] 参数取出。

use std::any::type_name;
use std::fmt;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;

use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::body::Bytes;

use crate::ResponseBody;

/// 请求体读取过程中可能出现的错误（类型擦除，过滤器替换 body 时不受具体错误类型限制）。
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 所有过滤器和路由统一使用的请求 body 类型：类型擦除后的 body，
/// 过滤器读完原始 body 后可以换成一个 `Full<Bytes>` 放回去（见 [`read_body`]）。
pub type RequestBody = BoxBody<Bytes, BoxError>;

pub type Request = hyper::Request<RequestBody>;
pub type Response = hyper::Response<ResponseBody>;

/// 把 hyper 收到的原始请求 body 装箱成 [`RequestBody`]，生成代码在入口处调用。
pub fn box_request_body<B>(body: B) -> RequestBody
where
    B: hyper::body::Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: Into<BoxError>,
{
    body.map_err(Into::into).boxed()
}

/// 用一段已经在内存里的数据构造请求 body。
pub fn full_request_body(bytes: impl Into<Bytes>) -> RequestBody {
    Full::new(bytes.into()).map_err(|never| match never {}).boxed()
}

/// 读出完整的请求体，并把同样的内容放回请求里，后面的过滤器和 controller 还能照常读取。
/// 用于签名校验、记录请求报文等需要看 body 的过滤器。
pub async fn read_body(req: &mut Request) -> Result<Bytes, BoxError> {
    let empty = Empty::<Bytes>::new().map_err(|never| match never {}).boxed();
    let body = std::mem::replace(req.body_mut(), empty);
    let bytes = body.collect().await?.to_bytes();
    *req.body_mut() = full_request_body(bytes.clone());
    Ok(bytes)
}

type BoxFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

/// 过滤链中“剩下的部分”（后面的过滤器 + controller）。只能运行一次。
pub struct Next {
    inner: Box<dyn FnOnce(Request) -> BoxFuture + Send>,
}

impl Next {
    /// 由生成代码构造；业务代码一般不需要直接调用。
    pub fn new<F>(f: F) -> Self
    where
        F: FnOnce(Request) -> BoxFuture + Send + 'static,
    {
        Self { inner: Box::new(f) }
    }

    pub async fn run(self, req: Request) -> Response {
        (self.inner)(req).await
    }
}

/// controller 参数：取出过滤器放进请求 extensions 里的 `T`。
///
/// - `Ext<T>`：必须存在；不存在说明过滤器没覆盖到这个路由，属于服务端配置问题，返回 500。
/// - `Option<Ext<T>>`：可选，不存在时为 `None`。
///
/// 注意 `http::Extensions::insert` 要求 `T: Clone + Send + Sync + 'static`。
#[derive(Debug, Clone)]
pub struct Ext<T>(pub T);

impl<T> Ext<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Deref for Ext<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for Ext<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// controller 参数：把查询串（`?a=1&b=2`）解析到结构体 `T`，不论请求方法（GET / POST / ...）。
/// `T` 需要实现 `serde::Deserialize`，可选参数用 `Option<_>` 或 `#[serde(default)]`。
/// 可以和标量查询参数（`id: i64`）、请求体参数同时使用。
#[derive(Debug, Clone)]
pub struct Query<T>(pub T);

impl<T> Query<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Deref for Query<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for Query<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// [`take_ext`] 找不到数据时的错误；只进服务端日志，不会返回给客户端。
#[derive(Debug)]
pub struct MissingExt {
    type_name: &'static str,
}

impl fmt::Display for MissingExt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "request extension `{}` not found; check that a filter inserting it covers this route",
            self.type_name
        )
    }
}

impl std::error::Error for MissingExt {}

pub fn take_ext<T: Send + Sync + 'static>(extensions: &mut hyper::http::Extensions) -> Result<Ext<T>, MissingExt> {
    extensions.remove::<T>().map(Ext).ok_or(MissingExt {
        type_name: type_name::<T>(),
    })
}

pub fn take_ext_opt<T: Send + Sync + 'static>(extensions: &mut hyper::http::Extensions) -> Option<Ext<T>> {
    extensions.remove::<T>().map(Ext)
}

/// 路径是否匹配模式：按 `/` 分段比较，`*` 匹配恰好一段，`**` 匹配任意多段（含零段），其余按字面相等。
/// 例如 `/api/**` 匹配 `/api`、`/api/user/1`；`/user/*` 匹配 `/user/1`，不匹配 `/user/1/edit`。
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        Some((&seg, rest)) => match path.split_first() {
            Some((&first, path_rest)) => (seg == "*" || seg == first) && segments_match(rest, path_rest),
            None => false,
        },
    }
}

/// 过滤器是否作用于这个路径：`include` 为空表示全部路径，再排除命中 `exclude` 的。
pub fn filter_applies(include: &[&str], exclude: &[&str], path: &str) -> bool {
    (include.is_empty() || include.iter().any(|p| path_matches(p, path)))
        && !exclude.iter().any(|p| path_matches(p, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_patterns() {
        assert!(path_matches("/api/**", "/api"));
        assert!(path_matches("/api/**", "/api/user/1"));
        assert!(!path_matches("/api/**", "/apix/user"));
        assert!(path_matches("/user/*", "/user/1"));
        assert!(!path_matches("/user/*", "/user/1/edit"));
        assert!(!path_matches("/user/*", "/user"));
        assert!(path_matches("/**/edit", "/user/1/edit"));
        assert!(path_matches("/index", "/index/"));
        assert!(path_matches("/**", "/"));
    }

    #[test]
    fn include_and_exclude() {
        assert!(filter_applies(&[], &[], "/anything"));
        assert!(filter_applies(&["/api/**"], &["/api/login"], "/api/profile"));
        assert!(!filter_applies(&["/api/**"], &["/api/login"], "/api/login"));
        assert!(!filter_applies(&["/api/**"], &[], "/index"));
    }

    #[test]
    fn ext_take() {
        let mut ext = hyper::http::Extensions::new();
        ext.insert(42_i64);
        assert_eq!(*take_ext::<i64>(&mut ext).unwrap(), 42);
        assert!(take_ext_opt::<i64>(&mut ext).is_none());
        assert!(take_ext::<i64>(&mut ext).unwrap_err().to_string().contains("i64"));
    }

    #[tokio::test]
    async fn read_body_puts_content_back() {
        let mut req: Request = hyper::Request::new(full_request_body("hello"));
        assert_eq!(read_body(&mut req).await.unwrap(), "hello");
        let again = req.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(again, "hello");
    }
}
