# hyperoute

[English](README.md) | [简体中文](README.zh-CN.md)

**Annotation-driven routing for [hyper](https://hyper.rs), generated at build time.**

Write handlers the way you would in Spring MVC: put `#[GET("/users/{id}")]` on an `async fn`, and hyperoute takes care of routing, parameter extraction and response building. A `build.rs` step scans your source files and generates a plain `match`-based router, so there is no runtime reflection, no route registration code and no macro magic hiding your function bodies.

```rust
#[GET("/users/{id}")]
pub async fn get_user(id: i64, verbose: bool) -> Result<User, ApiError> {
    // `id` comes from the path, `verbose` from `?verbose=true`
    // `User` is serialized to JSON, `ApiError` becomes an error response
}
```

> **Status:** early development (`0.0.x`). The API may still change between releases.

## Features

- **Route attributes**: `#[GET]`, `#[POST]`, `#[PUT]`, `#[DELETE]`, `#[PATCH]`, `#[HEAD]`, `#[OPTIONS]`, `#[CONNECT]`, `#[TRACE]`, and `#[REQUEST]` (any method).
- **Parameter extraction by signature**: path segments (including a trailing `{*path}` wildcard), query string, `Option` / `Vec` parameters, JSON / urlencoded / multipart bodies, file uploads, request headers, the client address, and unbuffered streaming bodies.
- **Go / Spring style form binding** (optional): merge the query string and form body like Go's `request.Form`, with an optional lenient mode where missing or malformed parameters become default values.
- **Responses by return type**: plain text, JSON, raw bytes with custom headers, streaming bodies, and `Result<T, E>`. `Stream::file` serves files with `Range` (resumable downloads, video seeking), `If-Range`, ETag/304 and `Content-Disposition`.
- **Parameter errors that never panic**: a malformed parameter returns `400` with a message saying which parameter failed and why. The response format is yours to define. Default messages don't reveal the server's tech stack.
- **Filters** (`#[FILTER]`): "around"-style middleware with ordering, include/exclude path patterns, short-circuiting, and passing data such as the logged-in user to handlers (`Ext<T>`).
- **WebSocket** (`#[WEBSOCKET]`): the handshake is handled for you. Handlers get path, query and filter data just like HTTP routes.
- **Static files**: serve html / js / css / images from disk or embedded in the binary, with ETag/304, optional gzip, sub-directories and filter protection.
- **Server helpers**: start with one line, or keep full control of the accept loop.

## Crates

| Crate | Kind | Purpose |
|---|---|---|
| [`hyperoute-macro`](hyperoute-macro) | proc-macro | The attributes (`#[GET]`, `#[FILTER]`, `#[WEBSOCKET]`, `#[PARAM_ERROR]`, ...). They are markers only and leave your functions untouched. |
| [`hyperoute-codegen`](hyperoute-codegen) | build-dependency | Scans source directories from `build.rs` and generates the router. |
| [`hyperoute-core`](hyperoute-core) | runtime | Types and helpers used by your code and by the generated router. |
| [`hyperoute-sample`](hyperoute-sample) | example app | Demonstrates every feature. Not published. |

## Quick start

**1. `Cargo.toml`**

```toml
[dependencies]
hyperoute-core = "0.0.6"
hyperoute-macro = "0.0.6"
# The generated router uses these crates directly:
hyper = { version = "1", features = ["full"] }
http-body-util = "0.1"
serde_json = "1"
tokio = { version = "1", features = ["full"] }
# For your own request / response types:
serde = { version = "1", features = ["derive"] }

[build-dependencies]
hyperoute-codegen = "0.0.6"
```

**2. `build.rs`**: scan your handler directories and write the router into `OUT_DIR`

```rust
use std::{env, fs, path::PathBuf};

fn main() {
    let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("src");

    let routes = hyperoute_codegen::Scanner::new()
        .dir(src.join("controller"), "crate::controller")
        .dir(src.join("filter"), "crate::filter") // optional
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

**4. `src/controller/mod.rs` and `src/controller/hello.rs`**

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

## How scanning works

`hyperoute-codegen` parses your source files with `syn` at build time. Follow these rules so it can find your code:

- **Directory and module paths must line up.** `src/controller/admin/user.rs` is treated as `crate::controller::admin::user`. `mod.rs` files are skipped. You still declare the modules yourself (`pub mod admin;`), because scanning does not create them.
- **Only top-level functions are scanned.** Functions inside `impl` blocks or inline `mod {}` blocks are not.
- **Write attributes by their bare name and import them**: `use hyperoute_macro::GET;` then `#[GET("/x")]`. A qualified path such as `#[hyperoute_macro::GET("/x")]` is ignored.
- **Handlers must be `async fn` and reachable from the crate root** (`pub` is simplest).
- **Mistakes in hyperoute's own rules fail the build** with a message naming the function. Examples: two `#[PARAM_ERROR]` functions, or a malformed `#[FILTER(...)]`.

## Routing

The path template is split on `/`. A segment written as `{name}` is a path parameter, and every other segment must match literally. The last segment may be a wildcard `{*name}`, which matches the remaining one or more segments and joins them with `/` (each segment decoded). Routes with a wildcard have lower priority than routes without one, so `#[GET("/d/{id}/info")]` wins over `#[GET("/d/{id}/{*path}")]` for `/d/1/info`, and `/d/1/info/x` goes to the wildcard route. Trailing slashes are ignored. Each path segment is percent-decoded once at the entry point, and the same decoded path is used for filter matching, routing and static files. For example, a path parameter in `/hello/%E4%B8%AD` receives `中`. Paths containing `..` or an encoded `/` or `\` are rejected with `400`. `#[REQUEST("/path")]` matches any HTTP method. Requests that match no route get `404 Not Found`.

## Request parameters

Each handler parameter's source is decided from its name and type:

| Parameter | Source | Example |
|---|---|---|
| Name matches a `{name}` in the path | Path segment, converted with `FromStr` | `#[GET("/users/{id}")] fn f(id: i64)` |
| Scalar type: integers, floats, `bool`, `char`, `String`, `&str` | Query string `?name=value` (merged form, see [form binding](#form-binding-go--spring-style)) | `fn f(page: u32, keyword: String)` |
| `Option<scalar>` / `Vec<scalar>` | Same as above. `None` / empty `Vec` when absent; `Vec` takes every value of a repeated key (`?id=1&id=2`) | `fn f(tag: Option<String>, ids: Vec<i64>)` |
| `{*name}` in the path | The remaining path segments joined with `/` | `#[GET("/files/{*path}")] fn f(path: String)` |
| `HeaderMap` / `&HeaderMap` | Request headers (`hyper::HeaderMap`) | `fn f(headers: &HeaderMap)` |
| `RemoteAddr` / `Option<RemoteAddr>` | Client address, see [running the server](#running-the-server) | `fn f(addr: RemoteAddr)` |
| `BodyStream` | The unbuffered request body | `fn f(body: BodyStream)` |
| `MultipartFile<Vec<u8>>` / `MultipartFile<&Path>` | A file field of a multipart body, by parameter name | `fn f(folder: String, file: MultipartFile<&Path>)` |
| `Query<T>` | The whole query string, deserialized into struct `T`, for any HTTP method | `fn f(q: Query<Search>)` |
| `Ext<T>` / `Option<Ext<T>>` | Data inserted by a [filter](#filters) | `fn f(user: Ext<CurrentUser>)` |
| `WebSocket` | Only in [`#[WEBSOCKET]`](#websocket) routes | `fn f(socket: WebSocket)` |
| Any other type | Request body, chosen by `content-type` | `fn f(form: UserForm)` |

**Query struct (`Query<T>`).** Bind the query string to a struct instead of listing every parameter. `T` must implement `Deserialize`. Use `Option<_>` or `#[serde(default)]` for optional parameters. It works with any HTTP method, and it can be combined with scalar query parameters and a request body, since the same query string can feed several parameters:

```rust
#[derive(Deserialize)]
pub struct UserInfo { pub name: String, pub age: u8, pub nickname: Option<String> }

// GET /users?id=10&name=AA&age=10
#[GET("/users")]
pub async fn find(id: i64, info: Query<UserInfo>) -> String {
    format!("{id} {} {}", info.name, info.age) // Query<T> derefs to T
}

// POST /users?name=AA&age=10, with a JSON / form body bound to `form`
#[POST("/users")]
pub async fn create(info: Query<UserInfo>, form: UserForm) -> String { /* ... */ }
```

Errors are reported as query parameters, for example `Query parameter "age" is invalid, got "x".`. Inside `Query<T>`, a repeated key (`?tag=a&tag=b`) can't be bound to a `Vec` field; use a `Vec<String>` handler parameter instead.

**Parameter naming (camelCase front ends).** Front ends usually send camelCase names (`pageSize`), while Rust uses snake_case (`page_size`). How each kind of parameter handles this:

| Parameter | How names are matched |
|---|---|
| JSON / urlencoded body, `Query<T>`, JSON responses | serde attributes on the struct: `#[serde(rename_all = "camelCase")]`, `rename`, `alias` |
| Scalar query parameters (`fn f(page_size: u32)`) | The `ParamNaming` switch in `build.rs` |
| multipart fields | serde attributes if the struct has any `rename_all` / `rename` / `alias`, so JSON and multipart agree. Otherwise the `ParamNaming` switch. Structs with file fields can't derive serde, so they use the switch. |

```rust
// build.rs
hyperoute_codegen::Scanner::new()
    .dir(src.join("controller"), "crate::controller")
    .param_naming(ParamNaming::CamelCase)   // default: ParamNaming::Exact
    .scan();
```

With `ParamNaming::CamelCase`:
- **Both names are accepted.** `fn list(page_size: u32, sort_by: String)` accepts `?pageSize=20&sortBy=name` as well as `?page_size=20&sort_by=name`. If both appear, the Rust name wins.
- **The camelCase name is computed at build time**, so there is no string conversion at runtime. Names without an underscore (`id`, `name`) generate exactly the same code as before.
- **Error messages use the camelCase name**, e.g. `Query parameter "pageSize" is required.`.
- **Leading underscores are kept**: `_client_flag` also accepts `_clientFlag`. (This differs from serde's `rename_all = "camelCase"`, which drops them.)

To make a whole API camelCase, put `#[serde(rename_all = "camelCase")]` on request and response structs, and turn on `ParamNaming::CamelCase` for scalar parameters.

**Request body**

| `content-type` | Decoding |
|---|---|
| `application/json` | `serde_json` (the type must implement `Deserialize`) |
| `application/x-www-form-urlencoded`, or missing | `serde_urlencoded` |
| `multipart/form-data` | Field by field, see below |
| anything else | `400`, unsupported content type (with `ParamBinding::Merged`, a struct is bound from the query string instead) |

**Multipart and file uploads.** For multipart to work, the body struct must be defined **in the same file** as the handler (nesting inside a `mod {}` is fine), so the code generator can see its fields. Each field is filled according to its type:

| Field type | Value |
|---|---|
| `String`, numbers, `bool`, ... | Text field, parsed with `FromStr` |
| `Vec<u8>` | File content, in memory |
| `&Path` | File content written to a temp file; you get its path |
| `MultipartFile<Vec<u8>>` / `MultipartFile<&Path>` | As above, plus `name` (file name) and `content_type` |

A struct containing any file field accepts only `multipart/form-data` and doesn't need `Deserialize`.

**Uploads are parsed as they arrive.** A file field is kept in memory up to the [multipart memory limit](#body-limits) (1MB by default), then spilled to a temp file that the rest of the upload is streamed into. A multi-GB upload therefore uses only a few MB of memory. Field order doesn't matter: text fields sent after the file still bind normally.

**Temp files are deleted automatically** when the request has been handled, after the handler returns or when parameter parsing fails. To keep an upload, move it inside the handler with `tokio::fs::rename(path, target)` or copy it. `hyperoute_core::write_temp_file` still returns a file that is never deleted.

```rust
#[POST("/upload")]
pub async fn upload(form: UploadForm<'_>) -> String { /* ... */ }

pub struct UploadForm<'a> {
    pub title: String,
    pub avatar: MultipartFile<Vec<u8>>,
    pub attachment: &'a Path,
}
```

### Body limits

These settings are read by the framework itself from environment variables, so your project needs no code for them:

| Environment variable | Meaning | Default |
|---|---|---|
| `HYPEROUTE_MULTIPART_MEMORY_LIMIT` | A multipart file larger than this is spilled to a temp file. A text field larger than this answers `413`. | 1MB |
| `HYPEROUTE_MAX_BODY_SIZE` | Maximum request body size. Larger bodies answer `413`. | unlimited |
| `HYPEROUTE_UPLOAD_TEMP_DIR` | Directory for upload temp files. It's created if missing. If it can't be created or written, the request answers `500` and the reason is logged. | system temp dir |

- **Where to put temp files.** Put `HYPEROUTE_UPLOAD_TEMP_DIR` on the same file system as your storage directory, so handlers can `rename` uploads instead of copying them. This matters with Docker volumes, and when `/tmp` is a tmpfs held in memory.
- **Value format.** Sizes look like `65536`, `512KB`, `64MB` or `10GB` (powers of 1024). An invalid value is ignored, with a warning on stderr.
- **When they're read.** The variables are read once, on first use, so restart the process to change them.
- **What the size limit covers.** It applies to bodies hyperoute reads for you: JSON, urlencoded, multipart and merged forms. `Content-Length` is checked before reading, and bodies without one are counted while they're read.
- **`BodyStream` is not limited.** The handler decides, for example with `size_hint()`.
- **`413` bypasses `#[PARAM_ERROR]`.** It always uses the plain-text default response `Request body is too large.`.

For environments where you can't set environment variables (an app embedded on iOS, for example), the same values can be set in code. A value set in code takes precedence over the environment variable and applies to requests that arrive afterwards:

```rust
hyperoute_core::set_multipart_memory_limit(64 * 1024 * 1024);
hyperoute_core::set_max_body_size(Some(10 * 1024 * 1024 * 1024));
hyperoute_core::set_upload_temp_dir("/app/data/temp");
```

**Streaming request bodies.** A `BodyStream` parameter receives the body without buffering it, for uploads too large to hold in memory:

```rust
#[POST("/upload/{name}")]
pub async fn upload(name: String, mut body: BodyStream) -> Result<String, ApiError> {
    let mut file = tokio::fs::File::create(format!("/data/{name}")).await?;
    while let Some(chunk) = body.next_chunk().await {
        file.write_all(&chunk?).await?;
    }
    Ok("ok".into())
}
```

`BodyStream` also implements `futures::Stream`, and `size_hint()` returns the `Content-Length` if there is one. A handler can have at most one, and it can't be combined with a body struct or file parameters (the build fails). If a filter called `read_body`, the handler gets the copy the filter put back.

### Form binding (Go / Spring style)

By default, scalar parameters come only from the query string, and a struct parameter is decoded from the body. Projects migrating from Go's `net/http` (`request.Form`) or Spring can switch to merged binding in `build.rs`:

```rust
use hyperoute_codegen::{ParamBinding, ParamFallback};

hyperoute_codegen::Scanner::new()
    .dir(src.join("controller"), "crate::controller")
    .param_binding(ParamBinding::Merged)     // default: ParamBinding::Separate
    .param_fallback(ParamFallback::Default)  // optional, default: ParamFallback::Strict
    .scan();
```

With `ParamBinding::Merged`, the query string and form body are merged, in the same order as Go:

| Body | Order of values |
|---|---|
| `application/x-www-form-urlencoded` (or no content type) | body values first, then query values |
| `multipart/form-data` | query values first, then text fields |
| anything else (JSON, ...) | query values only |

- **Scalars take the first value, `Vec` takes all values.** Like Go's AutoWeb, a single value written as `"[a,b,c]"` is also split into a list: the brackets are stripped, the value is split on `,`, and `_2c` in each item turns back into `,`. A scalar takes the first item, so `?id=[5]` gives `5`. `"[]"` gives one empty string, as in Go.
- **Struct parameters are bound from the merged form by field name**, so a GET query string can fill a struct too. `Option` and `Vec` fields, unit enum variants and serde attributes (`rename`, `rename_all`, `alias`, `default`) work. JSON bodies are still decoded as JSON. With `ParamNaming::CamelCase`, fields also accept their camelCase name.
- **The body is read once**, so scalars and file parameters can share a handler:

```rust
#[POST("/upload")]
pub async fn upload(folder: String, content_type: String, file: MultipartFile<&Path>) -> String { /* ... */ }
```

- Routes with a `BodyStream` parameter, and WebSocket routes, don't read the body, so their parameters come from the query string only.
- When a parameter is missing in merged mode, the error says `Parameter "id" is required.` (location `form`).

**Lenient mode** (`ParamFallback::Default`). A missing or malformed scalar becomes `Default::default()` instead of a `400`. `Option` gets `None`, and malformed items in a `Vec` become the default value, matching Go, which ignores conversion errors. It also works without merged binding. Override it per handler with a marker attribute:

```rust
use hyperoute_macro::{GET, PARAM_LENIENT, PARAM_STRICT};

#[PARAM_STRICT]   // always 400 on bad parameters, whatever the global setting is
#[GET("/api/v2/items")]
pub async fn items(page: u32) -> String { /* ... */ }

#[PARAM_LENIENT]  // defaults instead of 400, even when the global setting is Strict
#[GET("/legacy/list")]
pub async fn legacy(page: u32, size: Option<u32>) -> String { /* ... */ }
```

- **Struct fields.** Missing fields and malformed values become defaults, and serde's `#[serde(default = ...)]` still takes precedence. Enum fields have no generic default, so a missing or unknown enum value is still an error; add `#[serde(default)]` to make it optional.
- **What stays strict.** Path parameters, file fields, JSON bodies and `Query<T>` are always strict.
- **Debug logging.** Every fallback is logged to stderr in debug builds, so you can find clients that send bad parameters.

## Responses

The return type decides how the response is built:

| Return type | Response |
|---|---|
| `&str`, `String` | `200`, body as-is |
| `hyperoute_core::Raw` | bytes + headers you set; status `200` unless you call `.status(...)` |
| `hyperoute_core::Stream` | streamed body + headers you set; status `200` unless you call `.status(...)` |
| Any other type | `200`, `application/json` via `serde_json` |
| `Result<T, E>` | `Ok(T)` follows the rules above. `Err(E)` is converted with `IntoErrorResponse`. |

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

**Files with Range support.** `Stream::file(path, &headers)` serves a file and handles the request headers for you:
- **Range.** It supports single ranges (`bytes=a-b`, `a-`, `-n`) and answers `206` with `Content-Range`.
- **Invalid ranges.** A range that can't be satisfied answers `416`. A multi-range or unrecognized header is ignored, and the whole file is sent with `200`.
- **If-Range.** If it doesn't match the current ETag or Last-Modified, the Range is ignored.
- **Conditional requests.** A matching `If-None-Match` or `If-Modified-Since` answers `304`.
- **Response headers.** It always sets `Accept-Ranges: bytes`, `Content-Length`, an `ETag` derived from size and mtime, and `Last-Modified`.

```rust
#[GET("/files/{*path}")]
pub async fn download(path: String, headers: &HeaderMap) -> Result<Stream, ApiError> {
    let stream = Stream::file(Path::new("/data").join(&path), headers).await
        .map_err(|_| ApiError::not_found())?;          // io::Error: missing, not a file, ...
    Ok(stream.attachment("报告.pdf"))                   // or .inline(name), .content_type("video/mp4")
}
```

- **Builder methods.**
  - `.attachment(name)` and `.inline(name)` set `Content-Disposition` with an RFC 5987 `filename*` for non-ASCII names.
  - `.content_type(..)` replaces the type guessed from the extension.
  - `.set_header(..)` replaces a header, while `.header(..)` appends one.
- **Static directories don't support Range.** Use a handler with `Stream::file` for downloads and video.

**Error types** implement `IntoErrorResponse`. The examples below share this JSON error type:

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
    // Optional; defaults to `content-type: text/plain; charset=utf-8`
    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![("content-type", "application/json".into())]
    }
}
```

## Parameter errors

When a path parameter, query parameter or body can't be parsed, hyperoute returns `400` instead of panicking. It stops at the first error. The default response is plain text:

```text
Path parameter "id" must be an integer, got "abc".
Query parameter "name" is required.
Field "age" in request body is out of range, got "300".
Request body is malformed (invalid JSON at line 1, column 9).
Content-Type "application/xml" is not supported for this endpoint.
```

The default messages deliberately avoid anything specific to the implementation:
- **No Rust type names.** Messages say "integer" rather than `i64`.
- **No raw `std` or `serde` error text.**
- **No internal struct names.**
- **Long values are truncated** to 64 characters, and control characters are escaped.

The full details still go to stderr for debugging, as a line starting with `[hyperoute] parameter error on GET /users/{id}: ...`.

**Custom format.** Mark one function with `#[PARAM_ERROR]`. It may be sync or async, and it returns any `IntoErrorResponse` type:

```rust
use hyperoute_core::ParamErrors;
use hyperoute_macro::PARAM_ERROR;

#[PARAM_ERROR]
pub fn on_param_error(errors: ParamErrors) -> ApiError {
    let e = errors.first(); // currently always exactly one error
    ApiError::new(StatusCode::BAD_REQUEST, json!({
        "code": "INVALID_PARAM",
        "location": e.location.as_str(), // path | query | body | multipart | header | form
        "param": e.param,                // e.g. "id", "items[2].age"; None for the whole body
        "kind": e.kind.as_str(),         // missing | invalid | out_of_range | unsupported_content_type | malformed_body
                                         // (payload_too_large never reaches this function: it always answers 413)
        "message": e.to_string(),        // the neutral default message
    }))
}
```

`ParamError` also exposes `raw_value` (the full original value), `expected` ("integer", "boolean", ...), `expected_type` (the Rust type, e.g. `"i64"`) and `reason` (the underlying error text). The last two are meant for logging. Expose them to clients only if you choose to.

## Filters

A filter is an `async fn(Request, Next)`. The code before `next.run(req).await` is the **before** phase, the code after it is the **after** phase, and returning without calling `next` **short-circuits** the request.

```rust
use hyperoute_core::{Next, Request, Response};
use hyperoute_macro::FILTER;

#[derive(Clone)]
pub struct CurrentUser { pub id: i64, pub name: String }

#[FILTER(order = 10, include = ["/api/**"], exclude = ["/api/login"])]
pub async fn auth(mut req: Request, next: Next) -> Result<Response, ApiError> {
    let token = req.headers().get("authorization")
        .ok_or_else(|| ApiError::unauthorized("missing token"))?; // short-circuit with 401
    let user: CurrentUser = verify(token).await?;     // your own token check
    req.extensions_mut().insert(user);                // hand data to the handler

    let mut response = next.run(req).await;           // run the next filters + the handler
    response.headers_mut().insert("x-served-by", "hyperoute".parse().unwrap()); // after phase
    Ok(response)
}
```

Handlers receive the data with `Ext<T>`. `T` must be `Clone + Send + Sync + 'static`.

```rust
#[GET("/api/profile")]
pub async fn profile(user: Ext<CurrentUser>) -> String { format!("hi {}", user.name) }

#[GET("/home")]
pub async fn home(user: Option<Ext<CurrentUser>>) -> String { /* logged in or not */ }
```

**`#[FILTER(...)]` arguments.** All are optional.

| Argument | Meaning |
|---|---|
| `order = 10` | Lower runs first, i.e. outermost. Ties are broken by function path, so the order is stable across builds. Default `0`. |
| `include = ["/api/**"]` | Paths the filter applies to. A single string is also accepted. Default: all paths. |
| `exclude = ["/api/login"]` | Paths to skip, even if included. |

Path patterns match segment by segment. `*` matches exactly one segment and `**` matches any number of segments, including none. So `/api/**` matches `/api` and `/api/a/b`, and `/user/*` matches `/user/1` but not `/user/1/edit`. Note that `exclude = ["/api/login"]` does not exclude `/api/login/x`. Use `/api/login/**` for that.

**More details**

- **Return type.** A filter returns either `Response` or `Result<Response, E>` where `E: IntoErrorResponse`, which lets you use `?`.
- **Filters run before routing**, like Servlet filters:
  - They see every request, including those that end up as `404` or a parameter-error `400`.
  - They can't see path parameters, which are only known after routing.
- **Reading the body.** `hyperoute_core::read_body(&mut req).await` reads the whole body and puts it back, so the handler can still read it. Use it for signature checks or request logging.
- **Missing `Ext<T>`.** If a handler requires `Ext<T>` but no filter inserted it, the response is `500` and the cause is logged. That's a server configuration problem, not a client error.
- **`Stream` responses.** For streaming responses, the after phase runs once the headers are ready, while the body is still being sent.

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
            _ => {} // ping/pong are answered automatically
        }
    }
    Ok(())
}
```

- **The handshake is a normal `GET` request.** It passes through filters, and path, query and `Ext` parameters are extracted before the upgrade. If anything is wrong (missing login, bad parameter, missing `Upgrade` headers), the client gets a normal `401`/`400` and the connection is not upgraded.
- **Signature rules.** The handler gets exactly one `WebSocket` parameter. It can't take a request body or `&str` parameters, because it runs in a background task and its parameters must be owned. If it returns `Result<_, E>` with `E: Display`, errors are logged.
- **The API.** `WebSocket` provides `recv`, `send`, `close` and `split`. `split` returns a sender and a receiver that can live in separate tasks, for example one task reading client messages while another pushes broadcasts. When the peer sends `Close`, hyperoute sends the close reply for you, so you can simply `break`.

## Static files

Mount directories of html / js / css / images in `build.rs`. Sub-directories map one-to-one to URL paths:

```rust
use hyperoute_codegen::{Embed, StaticDir};

let routes = hyperoute_codegen::Scanner::new()
    .dir(src.join("controller"), "crate::controller")
    .static_dir(
        StaticDir::new("/static", manifest_dir.join("static")) // URL prefix, directory
            .embed(Embed::Release)          // Always | Never | Release (default)
            .index("index.html")            // served for directory requests (default)
            .spa_fallback(false)            // serve index.html for unknown page routes (Vue/React history mode)
            .cache_control("no-cache")      // optional Cache-Control header
            .gzip(true)                     // compress text assets (default: off)
            .html_include(true),            // merge {{template ...}} fragments into pages (default: off)
    )
    .scan();
```

With this setup, `static/img/icons/logo.png` is served at `/static/img/icons/logo.png`.

**Embedded or read from disk?** You choose per directory:

| `Embed` | Behavior |
|---|---|
| `Release` (default) | Debug builds read from disk, so edits show up on refresh. Release builds embed the files into the binary. |
| `Always` | Always embed. Ships as a single binary and can only serve files known at build time. |
| `Never` | Always read from disk at runtime, for large or frequently changing assets. |

- **Embedded mode**: the file table is generated at build time and looked up with a binary search. Each request serves the bytes straight from the binary, with no I/O and no allocation. Changing a file triggers a rebuild.
- **Disk mode**: files are resolved at runtime and large files are streamed. The root directory defaults to the configured path and can be changed at runtime, for example to an iOS sandbox directory:

```rust
hyperoute_core::set_static_root("/static", "/path/known/only/at/runtime");
```

**Request handling**

- **Priority and methods**: dynamic routes always win, and static files are only looked up when no route matches. Only `GET` and `HEAD` are served.
- **Directories**: `/static/docs/` serves `docs/index.html`. `/static/docs` (no trailing slash) answers `301` to `/static/docs/`, so relative links inside the page resolve correctly.
- **Caching**: `Content-Type` comes from the file extension. `ETag` is supported, plus `Last-Modified` in disk mode, and unchanged files answer `304 Not Modified`.
- **Security**:
  - Hidden files and directories (names starting with `.`) are never served, and there are no directory listings.
  - In disk mode, symlinks can't escape the root.
  - File names are matched case-sensitively, even on case-insensitive file systems such as macOS, so debug (disk) and release (embedded) behave the same.
- **Filters apply**: static requests pass through filters like any other request. For example, `include = ["/static/private/**"]` protects a folder.

**gzip** (`.gzip(true)`) applies only to text assets: html, css, js, json, svg, xml, wasm, fonts other than woff/woff2, and similar. Files under 1 KB, or that shrink by less than 10%, stay uncompressed.

- **Embedded mode**: files are compressed at build time with the best level, and **only the compressed version is stored**. It is always served with `Content-Encoding: gzip`, because browsers decompress automatically. Use `curl --compressed` when testing.
- **Disk mode**: gzip is used only when the client sends `Accept-Encoding: gzip`. hyperoute first looks for a ready-made `app.js.gz` next to the file and serves it if it isn't older than the original. Otherwise it compresses on the first request and caches the result on disk. The cache is invalidated automatically when the file changes. You can set the cache directory with `hyperoute_core::set_static_cache_dir(...)`. `.gzip_prewarm(true)` compresses everything in the background after the first request. Files over 16 MB are never compressed at runtime; ship a `.gz` next to them instead.

**HTML includes** (`.html_include(true)`). Pages can share fragments such as a `<head>` block or a nav bar. This is a pre-processing step: pages are merged first, and the merged result is then served exactly like any other static file, with the same ETag/304, gzip and Cache-Control behavior.

```html
<!-- index.html -->
<head>{{template include/head}}</head>   <!-- inserts include/head.tpl.html -->
<body>{{template /include/top-bar}} ...  <!-- a leading / resolves from the static root -->
```

- **Syntax.** `{{template NAME}}` inserts `NAME.tpl.html`. Extra spaces and quotes (`{{ template "a b" }}`) are allowed, and fragments can include other fragments. A `{{ ... }}` without the `template` keyword, such as Vue or Mustache `{{ value }}`, is left untouched.
- **Path resolution.** A relative `NAME` resolves from the directory of the file that contains it. A leading `/` resolves from the static root. A fragment must stay inside the static root.
- **Fragments are never served.** `*.tpl.html` files are only used for merging and answer `404`.
- **Embedded mode** merges at build time. A missing fragment or a circular include **fails the build**, and the message shows the include chain.
- **Disk mode** merges on request and stores the result as a file in the cache directory. Editing a page or any fragment it uses (directly or indirectly) is picked up on the next request without a restart. A missing fragment answers `500` and is logged.
- **Scope.** This is static fragment composition only, with no variables or loops. For server-side rendering with data, use a template engine such as askama or minijinja in a handler.

## Running the server

```rust
// One line: bind, accept, one task per connection
hyperoute_core::serve("0.0.0.0:3000", routes::router).await?;
```

For full control (TLS, connection limits, socket options, graceful shutdown), write the accept loop yourself and hand each connection to `serve_connection`:

```rust
let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
loop {
    let (stream, peer) = listener.accept().await?;
    tokio::spawn(hyperoute_core::serve_connection_with_addr(stream, peer, routes::router));
}
```

Use `serve_connection_with_addr(stream, peer, routes::router)` instead to make the client address available. Handlers get it with a `RemoteAddr` parameter, and filters with `RemoteAddr::from_request(&req)`. `serve` always provides it. With plain `serve_connection`, a `RemoteAddr` parameter answers `500` (logged), and `Option<RemoteAddr>` gets `None`. Behind a reverse proxy this is the proxy's address; read `X-Forwarded-For` from the headers for the original client.

Both use HTTP/1.1 with upgrades enabled. If you drive `routes::router` with hyper directly, call `.with_upgrades()` on the connection, or WebSocket upgrades will not work.

## Example project

[`hyperoute-sample`](hyperoute-sample) shows every feature:

```console
$ cargo run -p hyperoute-sample
$ curl "http://127.0.0.1:3000/params/query?id=1&name=alice"
$ curl -H 'Authorization: Bearer demo-token' http://127.0.0.1:3000/api/profile
```

| File | Shows |
|---|---|
| `controller/hello.rs` | Basic routes, `#[REQUEST]` |
| `controller/params.rs` | Path, query, `Option` / `Vec`, `#[PARAM_LENIENT]`, JSON / urlencoded / multipart bodies, file uploads |
| `controller/files.rs` | `{*path}` wildcard, `Stream::file` with Range, `&HeaderMap`, `RemoteAddr`, `BodyStream` uploads |
| `compat/dfs.rs` | Go-compatible binding: `ParamBinding::Merged` + `ParamFallback::Default`, `"[a,b]"` lists, scalars + file in one handler, `#[PARAM_STRICT]`. `build.rs` scans `src/compat` with a second `Scanner`, and `server.rs` routes `/compat/*` to it. |
| `controller/responses.rs` | Text, JSON, `Result`, `Raw`, `Stream` |
| `controller/param_error.rs` | Custom parameter error format (`#[PARAM_ERROR]`) |
| `controller/user.rs` | Receiving filter data with `Ext<T>` / `Option<Ext<T>>` |
| `controller/websocket.rs` | WebSocket echo and an authenticated chat room |
| `filter/auth.rs` | Bearer-token auth filter: before phase, short-circuit, passing data |
| `filter/access_log.rs` | Timing filter: after phase |
| `error.rs` | A shared JSON error type |
| `server.rs` | `serve` and a custom accept loop with `serve_connection_with_addr` |
| `tests.rs` | End-to-end tests over real TCP (`cargo test -p hyperoute-sample`) |
| `static/` + `build.rs` | Static files at `/static`: sub-directories, gzip, ETag/304, HTML includes (`static/include/header.tpl.html`), and `static/private/` protected by the auth filter |

Every handler has a `curl` command in its doc comment.

## Minimum supported Rust version

Rust **1.85** (edition 2024).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
