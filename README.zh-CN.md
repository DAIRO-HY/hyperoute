# hyperoute

[English](README.md) | [简体中文](README.zh-CN.md)

**基于 [hyper](https://hyper.rs) 的注解式路由框架，路由代码在编译期生成。**

写法和 Spring MVC 类似：在 `async fn` 上加 `#[GET("/users/{id}")]`，路由匹配、参数提取和响应构建都由 hyperoute 负责。工作方式是这样的：
- `build.rs` 在编译期扫描源码，生成一个普通的、基于 `match` 的路由函数。
- 没有运行时反射，也不需要手写路由注册代码。
- 宏不会改写你的函数体。

```rust
#[GET("/users/{id}")]
pub async fn get_user(id: i64, verbose: bool) -> Result<User, ApiError> {
    // `id` 来自路径，`verbose` 来自 `?verbose=true`
    // `User` 序列化成 JSON，`ApiError` 转换成错误响应
}
```

> **状态：** 早期开发阶段（`0.0.x`），不同版本之间 API 可能还会变化。

## 功能

- **路由注解**：`#[GET]`、`#[POST]`、`#[PUT]`、`#[DELETE]`、`#[PATCH]`、`#[HEAD]`、`#[OPTIONS]`、`#[CONNECT]`、`#[TRACE]`，以及匹配任意方法的 `#[REQUEST]`。
- **按函数签名提取参数**：路径段、查询串、JSON / urlencoded / multipart 请求体、文件上传。
- **按返回类型构建响应**：纯文本、JSON、带自定义响应头的二进制、流式响应、`Result<T, E>`。
- **参数错误不会 panic**：参数格式不对时返回 `400`，并说明是哪个参数、为什么失败。
  - 响应格式可以自定义。
  - 默认文案不会暴露服务端的技术栈。
- **过滤器**（`#[FILTER]`）：环绕式中间件。
  - 支持执行顺序、include/exclude 路径模式、中断请求。
  - 可以把登录用户等数据传给 handler（`Ext<T>`）。
- **WebSocket**（`#[WEBSOCKET]`）：框架负责握手。handler 和普通 HTTP 路由一样能拿到路径参数、查询参数和过滤器传来的数据。
- **静态资源**：从磁盘读取或打包进二进制，提供 html / js / css / 图片，支持 ETag/304、可选 gzip、子目录和过滤器保护。
- **服务启动**：可以一行启动，也可以自己完全控制 accept 循环。

## Crate 组成

| Crate | 类型 | 作用 |
|---|---|---|
| [`hyperoute-macro`](hyperoute-macro) | proc-macro | 提供各种注解（`#[GET]`、`#[FILTER]`、`#[WEBSOCKET]`、`#[PARAM_ERROR]` 等）。它们只是标记，不改动函数本身。 |
| [`hyperoute-codegen`](hyperoute-codegen) | build-dependency | 在 `build.rs` 里扫描源码目录，生成路由代码。 |
| [`hyperoute-core`](hyperoute-core) | 运行时 | 业务代码和生成的路由代码用到的类型与工具函数。 |
| [`hyperoute-sample`](hyperoute-sample) | 示例项目 | 演示所有功能，不发布到 crates.io。 |

## 快速开始

**1. `Cargo.toml`**

```toml
[dependencies]
hyperoute-core = "0.0.5"
hyperoute-macro = "0.0.5"
# 生成的路由代码会直接用到下面这几个 crate：
hyper = { version = "1", features = ["full"] }
http-body-util = "0.1"
serde_json = "1"
tokio = { version = "1", features = ["full"] }
# 你自己的请求 / 响应类型需要：
serde = { version = "1", features = ["derive"] }

[build-dependencies]
hyperoute-codegen = "0.0.5"
```

**2. `build.rs`**：扫描 handler 所在目录，把生成的路由代码写到 `OUT_DIR`

```rust
use std::{env, fs, path::PathBuf};

fn main() {
    let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("src");

    let routes = hyperoute_codegen::Scanner::new()
        .dir(src.join("controller"), "crate::controller")
        .dir(src.join("filter"), "crate::filter") // 可选
        .scan();
    let code = hyperoute_codegen::generate_router_source(&routes);

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("routes.rs");
    fs::write(out, code).unwrap();

    println!("cargo:rerun-if-changed=src/controller");
    println!("cargo:rerun-if-changed=src/filter");
}
```

**3. `src/main.rs`**

```rust
mod controller;

mod routes {
    include!(concat!(env!("OUT_DIR"), "/routes.rs"));
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    hyperoute_core::serve("127.0.0.1:3000", routes::router).await
}
```

**4. `src/controller/mod.rs` 和 `src/controller/hello.rs`**

```rust
// mod.rs
pub mod hello;
```

```rust
// hello.rs
use hyperoute_macro::GET;

#[GET("/hello/{name}")]
pub async fn hello(name: String) -> String {
    format!("Hello, {name}!")
}
```

```console
$ cargo run
$ curl http://127.0.0.1:3000/hello/world
Hello, world!
```

## 扫描规则

`hyperoute-codegen` 在编译期用 `syn` 解析源码。要让它找到你的代码，需要遵守下面几条规则：

- **目录结构要和模块路径对应。** 例如 `src/controller/admin/user.rs` 对应 `crate::controller::admin::user`。`mod.rs` 会被跳过。模块声明（`pub mod admin;`）仍然要自己写，扫描过程不会替你声明。
- **只扫描文件顶层的函数。** `impl` 块和内联 `mod {}` 里的函数不会被扫描。
- **注解要先导入，再直接写名字。** 例如先写 `use hyperoute_macro::GET;`，再写 `#[GET("/x")]`。写成 `#[hyperoute_macro::GET("/x")]` 这种带路径的形式会被忽略。
- **handler 必须是 `async fn`，并且能从 crate 根访问到。** 最简单的做法是写成 `pub`。
- **违反 hyperoute 自身规则的写法会导致编译失败**，报错信息里会指出是哪个函数。例如标注了两个 `#[PARAM_ERROR]`，或者 `#[FILTER(...)]` 写法有误。

## 路由

- 路径模板按 `/` 分段：写成 `{name}` 的段是路径参数，其余段必须字面相等。
- 路径末尾的 `/` 会被忽略。
- 每个路径段在入口处统一 percent 解码一次，过滤器匹配、路由匹配和静态资源查找都使用这同一个解码后的路径。例如 `/hello/%E4%B8%AD` 的路径参数拿到的是 `中`。包含 `..`、编码后的 `/` 或 `\` 的路径直接返回 `400`。
- `#[REQUEST("/path")]` 匹配任意 HTTP 方法。
- 匹配不到任何路由时返回 `404 Not Found`。

## 请求参数

每个 handler 参数从哪里取值，由参数名和类型决定：

| 参数 | 来源 | 示例 |
|---|---|---|
| 参数名和路径里的 `{name}` 相同 | 路径段，用 `FromStr` 转换 | `#[GET("/users/{id}")] fn f(id: i64)` |
| 标量类型：整数、浮点数、`bool`、`char`、`String`、`&str` | 查询串 `?name=value` | `fn f(page: u32, keyword: String)` |
| `Query<T>` | 整个查询串，解析到结构体 `T`，不区分请求方法 | `fn f(q: Query<Search>)` |
| `Ext<T>` / `Option<Ext<T>>` | [过滤器](#过滤器)放进请求的数据 | `fn f(user: Ext<CurrentUser>)` |
| `WebSocket` | 只能用在 [`#[WEBSOCKET]`](#websocket) 路由 | `fn f(socket: WebSocket)` |
| 其它类型 | 请求体，按 `content-type` 解析 | `fn f(form: UserForm)` |

**查询串结构体（`Query<T>`）。** 可以把查询串绑定到一个结构体上，不用把每个参数都列成函数参数：
- `T` 需要实现 `Deserialize`，可选参数用 `Option<_>` 或 `#[serde(default)]`。
- 任何请求方法都能用。
- 可以和标量查询参数、请求体参数同时使用，同一个查询串可以同时提供给多个参数。

```rust
#[derive(Deserialize)]
pub struct UserInfo { pub name: String, pub age: u8, pub nickname: Option<String> }

// GET /users?id=10&name=AA&age=10
#[GET("/users")]
pub async fn find(id: i64, info: Query<UserInfo>) -> String {
    format!("{id} {} {}", info.name, info.age) // Query<T> 实现了 Deref，可以直接访问字段
}

// POST /users?name=AA&age=10，JSON / 表单请求体绑定到 `form`
#[POST("/users")]
pub async fn create(info: Query<UserInfo>, form: UserForm) -> String { /* ... */ }
```

解析失败时按查询参数报错，例如 `Query parameter "age" is invalid, got "x".`。同名参数出现多次（`?tag=a&tag=b`）暂时还不能绑定到 `Vec`。

**参数命名（前端用驼峰）。** 前端通常用驼峰命名（`pageSize`），Rust 用下划线命名（`page_size`）。各类参数的处理方式如下：

| 参数类型 | 名字怎么匹配 |
|---|---|
| JSON / urlencoded 请求体、`Query<T>`、JSON 响应 | 看结构体上的 serde 属性：`#[serde(rename_all = "camelCase")]`、`rename`、`alias` |
| 标量查询参数（`fn f(page_size: u32)`） | 看 `build.rs` 里的 `ParamNaming` 开关 |
| multipart 字段 | 结构体上有 `rename_all` / `rename` / `alias` 中的任何一个时，按 serde 属性，保证和 JSON 一致；否则按 `ParamNaming` 开关。含文件字段的结构体无法派生 serde，所以总是按开关 |

```rust
// build.rs
hyperoute_codegen::Scanner::new()
    .dir(src.join("controller"), "crate::controller")
    .param_naming(ParamNaming::CamelCase)   // 默认：ParamNaming::Exact
    .scan();
```

开启 `ParamNaming::CamelCase` 后：
- **两种写法都能接收**：`fn list(page_size: u32, sort_by: String)` 既能接收 `?pageSize=20&sortBy=name`，也能接收 `?page_size=20&sort_by=name`。两种写法同时出现时，以 Rust 原名为准。
- **驼峰名在编译期就算好**，运行时不做任何字符串转换。没有下划线的参数名（`id`、`name`）生成的代码和以前完全一样。
- **错误信息里显示驼峰名**，例如 `Query parameter "pageSize" is required.`。

想让整套接口都用驼峰：请求和响应的结构体都加上 `#[serde(rename_all = "camelCase")]`，标量参数再打开 `ParamNaming::CamelCase`。

**请求体**

| `content-type` | 解析方式 |
|---|---|
| `application/json` | `serde_json`（类型需要实现 `Deserialize`） |
| `application/x-www-form-urlencoded`，或没有 content-type | `serde_urlencoded` |
| `multipart/form-data` | 逐个字段解析，见下文 |
| 其它 | 返回 `400`，提示不支持的 content-type |

**multipart 和文件上传。** 要支持 multipart，请求体结构体必须和 handler 定义在**同一个文件**里，代码生成时才能看到它的字段。嵌套在 `mod {}` 里也可以。每个字段按类型取值：

| 字段类型 | 取到的值 |
|---|---|
| `String`、数字、`bool` 等 | 文本字段，用 `FromStr` 转换 |
| `Vec<u8>` | 文件内容，读进内存 |
| `&Path` | 文件内容写到临时文件，拿到临时文件路径 |
| `MultipartFile<Vec<u8>>` / `MultipartFile<&Path>` | 同上，额外带 `name`（文件名）和 `content_type` |

结构体里只要有文件字段，就只接受 `multipart/form-data`，也不需要实现 `Deserialize`。临时文件**不会**自动删除。

```rust
#[POST("/upload")]
pub async fn upload(form: UploadForm<'_>) -> String { /* ... */ }

pub struct UploadForm<'a> {
    pub title: String,
    pub avatar: MultipartFile<Vec<u8>>,
    pub attachment: &'a Path,
}
```

## 响应

返回类型决定响应的构建方式：

| 返回类型 | 响应 |
|---|---|
| `&str`、`String` | `200`，内容原样返回 |
| `hyperoute_core::Raw` | `200`，二进制内容加自己设置的响应头 |
| `hyperoute_core::Stream` | `200`，流式响应加自己设置的响应头 |
| 其它类型 | `200`，用 `serde_json` 序列化成 `application/json` |
| `Result<T, E>` | `Ok(T)` 按上面的规则处理；`Err(E)` 通过 `IntoErrorResponse` 转换 |

```rust
#[GET("/download")]
pub async fn download() -> Raw {
    Raw::new(bytes).header("content-type", "application/pdf").header("set-cookie", "a=1")
}

#[GET("/big-file")]
pub async fn big_file() -> Stream {
    let file = tokio::fs::File::open("big.bin").await.unwrap();
    Stream::new(tokio_util::io::ReaderStream::new(file)).header("content-type", "application/octet-stream")
}
```

**错误类型**需要实现 `IntoErrorResponse`。后面的示例都用这个 JSON 错误类型：

```rust
use hyper::StatusCode;
use hyperoute_core::IntoErrorResponse;
use serde_json::{Value, json};

pub struct ApiError { status: StatusCode, body: Value }

impl ApiError {
    pub fn new(status: StatusCode, body: Value) -> Self { Self { status, body } }
    pub fn unauthorized(message: &str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, json!({ "code": "UNAUTHORIZED", "message": message }))
    }
}

impl IntoErrorResponse for ApiError {
    fn status(&self) -> StatusCode { self.status }
    fn message(&self) -> String { self.body.to_string() }
    // 可选，默认是 `content-type: text/plain; charset=utf-8`
    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![("content-type", "application/json".into())]
    }
}
```

## 参数错误

路径参数、查询参数或请求体解析失败时，hyperoute 返回 `400`，不会 panic。遇到第一个错误就返回。默认响应是纯文本：

```text
Path parameter "id" must be an integer, got "abc".
Query parameter "name" is required.
Field "age" in request body is out of range, got "300".
Request body is malformed (invalid JSON at line 1, column 9).
Content-Type "application/xml" is not supported for this endpoint.
```

默认文案刻意不包含和实现相关的信息：
- **不出现 Rust 类型名**：写的是 "integer"，而不是 `i64`。
- **不出现标准库或 serde 的报错原文。**
- **不出现内部结构体名。**
- **超长的原始值会截断**到 64 个字符，控制字符会被转义。

完整信息仍然会打印到 stderr，方便排查问题，开头是 `[hyperoute] parameter error on GET /users/{id}: ...`。

**自定义格式。** 给一个函数标注 `#[PARAM_ERROR]`。它可以是同步的，也可以是 async，返回任意实现了 `IntoErrorResponse` 的类型：

```rust
use hyperoute_core::ParamErrors;
use hyperoute_macro::PARAM_ERROR;

#[PARAM_ERROR]
pub fn on_param_error(errors: ParamErrors) -> ApiError {
    let e = errors.first(); // 目前总是恰好一个错误
    ApiError::new(StatusCode::BAD_REQUEST, json!({
        "code": "INVALID_PARAM",
        "location": e.location.as_str(), // path | query | body | multipart | header
        "param": e.param,                // 例如 "id"、"items[2].age"；针对整个请求体时为 None
        "kind": e.kind.as_str(),         // missing | invalid | out_of_range | unsupported_content_type | malformed_body
        "message": e.to_string(),        // 中性的默认文案
    }))
}
```

`ParamError` 还提供这几个字段：
- `raw_value`：完整的原始值。
- `expected`：中性的类型描述，例如 "integer"、"boolean"。
- `expected_type`：Rust 类型，例如 `"i64"`。
- `reason`：底层错误原文。

后两个主要用于记日志，要不要返回给客户端由你决定。

## 过滤器

过滤器是一个 `async fn(Request, Next)`：
- `next.run(req).await` 之前的代码是 **before** 阶段。
- 之后的代码是 **after** 阶段。
- 不调用 `next` 直接返回，就是**中断**请求。

```rust
use hyperoute_core::{Next, Request, Response};
use hyperoute_macro::FILTER;

#[derive(Clone)]
pub struct CurrentUser { pub id: i64, pub name: String }

#[FILTER(order = 10, include = ["/api/**"], exclude = ["/api/login"])]
pub async fn auth(mut req: Request, next: Next) -> Result<Response, ApiError> {
    let token = req.headers().get("authorization")
        .ok_or_else(|| ApiError::unauthorized("missing token"))?; // 中断请求，返回 401
    let user: CurrentUser = verify(token).await?;     // 你自己的 token 校验逻辑
    req.extensions_mut().insert(user);                // 把数据交给 handler

    let mut response = next.run(req).await;           // 执行后面的过滤器和 handler
    response.headers_mut().insert("x-served-by", "hyperoute".parse().unwrap()); // after 阶段
    Ok(response)
}
```

handler 用 `Ext<T>` 接收过滤器传来的数据，`T` 需要满足 `Clone + Send + Sync + 'static`：

```rust
#[GET("/api/profile")]
pub async fn profile(user: Ext<CurrentUser>) -> String { format!("hi {}", user.name) }

#[GET("/home")]
pub async fn home(user: Option<Ext<CurrentUser>>) -> String { /* 登录与否都能访问 */ }
```

**`#[FILTER(...)]` 参数**，都可以省略：

| 参数 | 含义 |
|---|---|
| `order = 10` | 越小越先执行，也就是越靠外层。相同时按函数路径排序，保证每次编译顺序一致。默认 `0`。 |
| `include = ["/api/**"]` | 作用的路径，也可以写单个字符串。默认作用于所有路径。 |
| `exclude = ["/api/login"]` | 排除的路径，即使在 include 范围内也会跳过。 |

路径模式按段匹配：`*` 匹配恰好一段，`**` 匹配任意多段（包括零段）。
- `/api/**` 匹配 `/api` 和 `/api/a/b`。
- `/user/*` 匹配 `/user/1`，不匹配 `/user/1/edit`。
- 注意 `exclude = ["/api/login"]` 不会排除 `/api/login/x`，要排除子路径请写 `/api/login/**`。

**其它细节**

- **返回类型**：过滤器可以返回 `Response`，也可以返回 `Result<Response, E>`（`E: IntoErrorResponse`），后者可以使用 `?`。
- **过滤器在路由匹配之前执行**，和 Servlet Filter 一样：
  - 所有请求都会经过过滤器，包括最终返回 `404` 或参数错误 `400` 的请求。
  - 过滤器拿不到路径参数，路径参数要等路由匹配之后才知道。
- **读取请求体**：`hyperoute_core::read_body(&mut req).await` 会读出完整的请求体并放回请求，handler 之后照常可以读。适合做签名校验、记录请求报文。
- **缺少 `Ext<T>`**：handler 声明了必需的 `Ext<T>`，但没有过滤器放入这个数据时，返回 `500` 并记录日志。这是服务端配置问题，不是客户端的错。
- **`Stream` 响应**：对流式响应来说，after 阶段在响应头准备好时就执行了，这时响应体还在发送。

## WebSocket

```rust
use hyperoute_core::{Ext, Message, WebSocket, WebSocketError};
use hyperoute_macro::WEBSOCKET;

#[WEBSOCKET("/ws/chat/{room}")]
pub async fn chat(mut socket: WebSocket, room: String, user: Ext<CurrentUser>) -> Result<(), WebSocketError> {
    socket.send(Message::text(format!("{} joined {room}", user.name))).await?;
    while let Some(message) = socket.recv().await {
        match message? {
            Message::Text(text) => socket.send(Message::text(format!("{}: {text}", user.name))).await?,
            Message::Close(_) => break,
            _ => {} // ping/pong 由底层自动回复
        }
    }
    Ok(())
}
```

- **握手就是一个普通的 `GET` 请求。**
  - 握手会经过过滤器，路径参数、查询参数和 `Ext` 都在升级之前解析。
  - 任何一项不满足（没登录、参数错误、缺少 `Upgrade` 等握手请求头），客户端都会收到普通的 `401` / `400`，连接不会升级。
- **函数签名规则：**
  - 必须有且只有一个 `WebSocket` 参数。
  - 不能有请求体参数，也不能有 `&str` 参数。因为 handler 在后台任务里运行，参数必须是自有数据。
  - 返回 `Result<_, E>`（`E: Display`）时，错误会被记录到日志。
- **`WebSocket` 提供的方法**：`recv`、`send`、`close`、`split`。
  - `split` 拆出发送端和接收端，可以放到不同任务里，比如一个任务读客户端消息，另一个任务推送广播。
  - 对方发送 `Close` 时，框架会自动回复关闭帧，所以直接 `break` 就行。

## 静态资源

在 `build.rs` 里挂载 html / js / css / 图片等资源目录，子目录和 URL 路径一一对应：

```rust
use hyperoute_codegen::{Embed, StaticDir};

let routes = hyperoute_codegen::Scanner::new()
    .dir(src.join("controller"), "crate::controller")
    .static_dir(
        StaticDir::new("/static", manifest_dir.join("static")) // URL 前缀, 资源目录
            .embed(Embed::Release)          // Always | Never | Release（默认）
            .index("index.html")            // 访问目录时返回的文件（默认）
            .spa_fallback(false)            // 未知页面路由返回 index.html（Vue/React history 模式）
            .cache_control("no-cache")      // 可选的 Cache-Control 响应头
            .gzip(true)                     // 压缩文本类资源（默认关闭）
            .html_include(true),            // 把 {{template ...}} 片段合并进页面（默认关闭）
    )
    .scan();
```

按上面的配置，`static/img/icons/logo.png` 的访问地址是 `/static/img/icons/logo.png`。

**打包进二进制还是读磁盘？** 每个目录单独选择：

| `Embed` | 行为 |
|---|---|
| `Release`（默认） | debug 编译读磁盘，改了资源刷新就生效；release 编译打包进二进制 |
| `Always` | 始终打包。发布时只有一个二进制文件，只能访问编译时已存在的文件 |
| `Never` | 始终在运行时读磁盘，适合很大或经常变动的资源 |

- **打包模式**：文件表在编译期生成，运行时二分查找。每个请求直接返回二进制里的数据，没有 I/O，也不分配内存。资源有变化时会自动重新编译。
- **磁盘模式**：运行时按路径查找文件，大文件流式返回。根目录默认是配置的目录，也可以在运行时改成别的目录，例如 iOS 沙盒目录：

```rust
hyperoute_core::set_static_root("/static", "/path/known/only/at/runtime");
```

**请求处理**

- **优先级和方法**：动态路由永远优先，没有路由匹配时才查找静态资源。只处理 `GET` 和 `HEAD`。
- **目录**：`/static/docs/` 返回 `docs/index.html`；`/static/docs`（没有末尾 `/`）返回 `301`，跳转到 `/static/docs/`，保证页面里的相对路径能正确解析。
- **缓存**：`Content-Type` 按扩展名判断。支持 `ETag`，磁盘模式还支持 `Last-Modified`，文件没变时返回 `304 Not Modified`。
- **安全**：
  - 不提供以 `.` 开头的隐藏文件和目录，也不提供目录列表。
  - 磁盘模式下，符号链接不能跳出根目录。
  - 文件名严格区分大小写，即使在 macOS 这类不区分大小写的文件系统上也是如此，保证 debug（读磁盘）和 release（打包）行为一致。
- **同样经过过滤器**：静态资源请求和其它请求一样会经过过滤器。例如 `include = ["/static/private/**"]` 可以保护一个目录。

**gzip**（`.gzip(true)`）只压缩文本类资源：html、css、js、json、svg、xml、wasm、除 woff/woff2 之外的字体等。小于 1 KB 或压缩后省不到 10% 的文件不压缩。

- **打包模式**：编译期用最高级别压缩，二进制里**只存压缩版**，始终带 `Content-Encoding: gzip` 返回。浏览器会自动解压；用 curl 测试时要加 `--compressed`。
- **磁盘模式**：客户端声明支持 gzip（`Accept-Encoding: gzip`）时才压缩：
  - 优先返回旁边现成的 `app.js.gz`，前提是它不比原文件旧。
  - 否则第一次请求时压缩，结果缓存在磁盘上，文件变化后缓存自动失效。缓存目录可以用 `hyperoute_core::set_static_cache_dir(...)` 设置。
  - `.gzip_prewarm(true)` 会在第一个请求之后，在后台把所有文件预先压缩好。
  - 超过 16 MB 的文件不会在运行时压缩，可以在旁边放一个现成的 `.gz` 文件。

**HTML 模板合并**（`.html_include(true)`）。多个页面可以共用 `<head>`、导航栏这类片段。它是一个前置处理步骤：页面先合并，合并结果再和其它静态文件一样处理，ETag/304、gzip、Cache-Control 的行为完全相同。

```html
<!-- index.html -->
<head>{{template include/head}}</head>   <!-- 插入 include/head.tpl.html 的内容 -->
<body>{{template /include/top-bar}} ...  <!-- 以 / 开头：从静态资源根目录查找 -->
```

- **语法**：`{{template NAME}}` 插入 `NAME.tpl.html` 的内容。允许多余的空格，也允许给名字加引号（`{{ template "a b" }}`）。模板片段里还可以继续引用其它片段。不带 `template` 关键字的 `{{ ... }}` 原样保留，比如 Vue、Mustache 的 `{{ value }}`。
- **路径解析**：相对路径的 `NAME` 从当前文件所在的目录开始查找；以 `/` 开头的从静态资源根目录开始查找。模板片段必须位于静态资源根目录之内。
- **模板片段不对外提供**：`*.tpl.html` 只用于合并，直接访问返回 `404`。
- **打包模式**：编译期合并。引用的片段不存在或者出现循环引用时，**编译直接报错**，报错信息会显示完整的引用链。
- **磁盘模式**：请求时合并，结果作为文件存放在缓存目录里。修改页面本身，或者它直接、间接引用的任何片段，下次请求就会生效，不需要重启。引用的片段不存在时返回 `500` 并记录日志。
- **适用范围**：这里只做静态片段的拼接，不支持变量和循环。需要根据数据在服务端渲染页面时，请在 handler 里使用 askama、minijinja 这类模板引擎。

## 启动服务

```rust
// 一行启动：绑定地址、接收连接、每个连接一个任务
hyperoute_core::serve("0.0.0.0:3000", routes::router).await?;
```

需要完全控制的场景，比如 TLS、连接数限制、socket 参数、优雅停机，可以自己写 accept 循环，把每条连接交给 `serve_connection`：

```rust
let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
loop {
    let (stream, _) = listener.accept().await?;
    tokio::spawn(hyperoute_core::serve_connection(stream, routes::router));
}
```

两者都使用 HTTP/1.1，并开启了协议升级。如果你直接用 hyper 驱动 `routes::router`，记得在连接上调用 `.with_upgrades()`，否则 WebSocket 无法升级。

## 示例项目

[`hyperoute-sample`](hyperoute-sample) 演示了所有功能：

```console
$ cargo run -p hyperoute-sample
$ curl "http://127.0.0.1:3000/params/query?id=1&name=alice"
$ curl -H 'Authorization: Bearer demo-token' http://127.0.0.1:3000/api/profile
```

| 文件 | 演示内容 |
|---|---|
| `controller/hello.rs` | 基础路由、`#[REQUEST]` |
| `controller/params.rs` | 路径参数、查询参数、JSON / urlencoded / multipart 请求体、文件上传 |
| `controller/responses.rs` | 文本、JSON、`Result`、`Raw`、`Stream` |
| `controller/param_error.rs` | 自定义参数错误格式（`#[PARAM_ERROR]`） |
| `controller/user.rs` | 用 `Ext<T>` / `Option<Ext<T>>` 接收过滤器传来的数据 |
| `controller/websocket.rs` | WebSocket 回显、需要登录的聊天室 |
| `filter/auth.rs` | Bearer token 鉴权过滤器：before 阶段、中断请求、传递数据 |
| `filter/access_log.rs` | 计时过滤器：after 阶段 |
| `error.rs` | 统一的 JSON 错误类型 |
| `server.rs` | `serve`，以及用 `serve_connection` 自己写 accept 循环 |
| `static/` + `build.rs` | 挂在 `/static` 的静态资源：子目录、gzip、ETag/304、HTML 模板合并（`static/include/header.tpl.html`），`static/private/` 受 auth 过滤器保护 |

每个 handler 的文档注释里都附了对应的 `curl` 命令。

## 最低支持的 Rust 版本

Rust **1.85**（edition 2024）。

## 许可证

本项目采用以下任一许可证，由你选择：

- Apache License, Version 2.0（[LICENSE-APACHE](LICENSE-APACHE)）
- MIT license（[LICENSE-MIT](LICENSE-MIT)）

除非你明确声明，否则你有意提交并被纳入本项目的任何贡献（按 Apache-2.0 许可证的定义），都按上述双许可证授权，不附加任何其它条款或条件。
