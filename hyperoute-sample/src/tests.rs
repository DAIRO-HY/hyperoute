//! 端到端测试：在随机端口上启动示例服务（和 `server::start` 一样带客户端地址），用原始 HTTP/1.1 请求验证。

use std::collections::HashMap;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::server;

/// 测试用的限制：文件超过 4KB 就转存到临时文件，请求体上限 8MB。
/// 两个设置都是进程级的，所有测试共用同一组值。
const MEMORY_LIMIT: usize = 4096;
const MAX_BODY: usize = 8 * 1024 * 1024;

/// 上传临时目录：一个还不存在的子目录，验证设置生效、目录自动创建。
fn upload_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hyperoute-sample-test-{}/uploads", std::process::id()))
}

async fn start() -> SocketAddr {
    hyperoute_core::set_multipart_memory_limit(MEMORY_LIMIT as u64);
    hyperoute_core::set_max_body_size(Some(MAX_BODY as u64));
    hyperoute_core::set_upload_temp_dir(upload_dir());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            tokio::spawn(hyperoute_core::serve_connection_with_addr(stream, peer, server::router));
        }
    });
    addr
}

struct Resp {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Resp {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.text()))
    }
}

/// 发一个请求（自动补上 Host / Connection: close / Content-Length）。
async fn request(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
    let mut raw = format!("{method} {path} HTTP/1.1\r\nhost: test\r\nconnection: close\r\ncontent-length: {}\r\n", body.len());
    for (name, value) in headers {
        raw.push_str(&format!("{name}: {value}\r\n"));
    }
    raw.push_str("\r\n");
    let mut bytes = raw.into_bytes();
    bytes.extend_from_slice(body);

    // 服务端可能不读完请求体就提前响应（例如 413）并关闭连接：后台写，写失败不算错，读到多少算多少
    let (mut reader, mut writer) = TcpStream::connect(addr).await.unwrap().into_split();
    let write = tokio::spawn(async move {
        let _ = writer.write_all(&bytes).await;
        writer
    });
    let mut response = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
        }
    }
    drop(write.await);

    let split = response.windows(4).position(|w| w == b"\r\n\r\n").expect("no header end");
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let mut body = response[split + 4..].to_vec();
    if headers.get("transfer-encoding").is_some_and(|v| v == "chunked") {
        body = dechunk(&body);
    }
    Resp { status, headers, body }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = data.windows(2).position(|w| w == b"\r\n").unwrap();
        let size = usize::from_str_radix(std::str::from_utf8(&data[..line_end]).unwrap().trim(), 16).unwrap();
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[line_end + 2..line_end + 2 + size]);
        data = &data[line_end + 2 + size + 2..];
    }
}

async fn get(addr: SocketAddr, path: &str) -> Resp {
    request(addr, "GET", path, &[], b"").await
}

const FORM: (&str, &str) = ("content-type", "application/x-www-form-urlencoded");

fn multipart(parts: &[(&str, Option<&str>, &[u8])]) -> (String, Vec<u8>) {
    let boundary = "XhyperouteBoundary";
    let mut body = Vec::new();
    for (name, file_name, data) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match file_name {
            Some(file) => body.extend_from_slice(
                format!("content-disposition: form-data; name=\"{name}\"; filename=\"{file}\"\r\ncontent-type: text/plain\r\n\r\n").as_bytes(),
            ),
            None => body.extend_from_slice(format!("content-disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes()),
        }
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

// ---- 1. 合并取值 + 宽松模式（src/compat）----

#[tokio::test]
async fn merged_scalars_vec_and_brackets() {
    let addr = start().await;
    let r = get(addr, "/compat/files/get_property?paths=[/a,/b_2cc]&_clientFlag=1").await;
    assert_eq!(r.json(), serde_json::json!({"paths": ["/a", "/b,c"], "client_flag": 1, "version": null}));

    // urlencoded 请求体在前、查询串在后
    let r = request(addr, "POST", "/compat/files/get_property?paths=/q&_version=9", &[FORM], b"paths=/a&paths=/b&_version=2").await;
    assert_eq!(r.json(), serde_json::json!({"paths": ["/a", "/b", "/q"], "client_flag": 0, "version": 2}));

    // multipart 是查询串在前、文本字段在后
    let (ct, body) = multipart(&[("paths", None, b"/m"), ("_clientFlag", None, b"5")]);
    let r = request(addr, "POST", "/compat/files/get_property?paths=/q", &[("content-type", &ct)], &body).await;
    assert_eq!(r.json(), serde_json::json!({"paths": ["/q", "/m"], "client_flag": 5, "version": null}));

    // 宽松模式：转换失败 → 默认值，Option 转换失败 → None
    let r = get(addr, "/compat/files/get_property?_clientFlag=x&_version=y").await;
    assert_eq!(r.json(), serde_json::json!({"paths": [], "client_flag": 0, "version": null}));
}

#[tokio::test]
async fn merged_upload_with_scalars_and_file() {
    let addr = start().await;
    let (ct, body) = multipart(&[("folder", None, b"/docs"), ("contentType", None, b"text/plain"), ("file", Some("a.txt"), b"hello")]);
    let r = request(addr, "POST", "/compat/upload", &[("content-type", &ct)], &body).await;
    let json = r.json();
    assert_eq!((json["folder"].as_str(), json["content_type"].as_str(), json["file_name"].as_str(), json["size"].as_u64()), (Some("/docs"), Some("text/plain"), Some("a.txt"), Some(5)));
    // 临时文件在请求处理完后自动删除
    assert!(!std::path::Path::new(json["temp_path"].as_str().unwrap()).exists());

    // 文本字段也可以放在查询串里
    let (ct, body) = multipart(&[("file", Some("b.txt"), b"hi")]);
    let r = request(addr, "POST", "/compat/upload?folder=/q", &[("content-type", &ct)], &body).await;
    assert_eq!(r.json()["folder"], "/q");

    // 文件字段不受宽松模式影响：没传仍然是 400
    let (ct, body) = multipart(&[("folder", None, b"/docs")]);
    let r = request(addr, "POST", "/compat/upload", &[("content-type", &ct)], &body).await;
    assert_eq!(r.status, 400, "{}", r.text());
}

#[tokio::test]
async fn merged_struct_and_strict_override() {
    let addr = start().await;
    let r = get(addr, "/compat/user/save?name=alice&age=20&tags=[a,b]").await;
    assert_eq!(r.json(), serde_json::json!({"name": "alice", "age": 20, "tags": ["a", "b"], "remark": null}));

    let r = request(addr, "POST", "/compat/user/save", &[FORM], b"age=x&tags=a&tags=b&remark=hi").await;
    assert_eq!(r.json(), serde_json::json!({"name": "", "age": 0, "tags": ["a", "b"], "remark": "hi"}));

    let r = request(addr, "POST", "/compat/user/save", &[("content-type", "application/json")], br#"{"name":"bob","age":3,"tags":[]}"#).await;
    assert_eq!(r.json()["name"], "bob");

    let r = get(addr, "/compat/strict").await;
    assert_eq!((r.status, r.text().as_str()), (400, r#"Parameter "id" is required."#));
    assert_eq!(get(addr, "/compat/strict?id=7").await.text(), "id = 7");
}

// ---- 默认模式下的 Vec / Option / #[PARAM_LENIENT] ----

#[tokio::test]
async fn separate_mode_vec_option_and_lenient() {
    let addr = start().await;
    assert_eq!(get(addr, "/params/list?ids=1&ids=2&tag=x").await.text(), "ids = [1, 2], tag = Some(\"x\")");
    // 默认模式不拆 "[...]"，也不读请求体
    let r = get(addr, "/params/list?ids=[1,2]").await;
    assert_eq!(r.status, 400);
    assert_eq!(get(addr, "/params/list").await.text(), "ids = [], tag = None");

    assert_eq!(get(addr, "/params/lenient?page=x").await.text(), "page = 0, size = None");
    assert_eq!(get(addr, "/params/lenient?page=3&size=9").await.text(), "page = 3, size = Some(9)");
    // 其它接口仍然是严格模式
    assert_eq!(get(addr, "/params/query?id=x&name=a").await.status, 400);
}

// ---- 2. 尾部通配路径参数 ----

#[tokio::test]
async fn rest_path_param() {
    let addr = start().await;
    assert_eq!(
        get(addr, "/d/abc/folder/%E4%B8%AD%E6%96%87.txt").await.text(),
        "url_path = abc, path = folder/中文.txt"
    );
    assert_eq!(get(addr, "/d/abc/x").await.text(), "url_path = abc, path = x");
    // 普通路由优先
    assert_eq!(get(addr, "/d/abc/info").await.text(), "info of abc");
    assert_eq!(get(addr, "/d/abc/info/x").await.text(), "url_path = abc, path = info/x");
    // {*path} 至少一段
    assert_eq!(get(addr, "/d/abc").await.status, 404);
    // 编码的 / 仍然是 400
    assert_eq!(get(addr, "/d/abc/a%2Fb").await.status, 400);
}

// ---- 3 / 4. 请求头和客户端地址 ----

#[tokio::test]
async fn headers_and_remote_addr() {
    let addr = start().await;
    let r = request(addr, "GET", "/whoami", &[("user-agent", "test-agent"), ("x-forwarded-for", "10.0.0.1")], b"").await;
    let json = r.json();
    assert_eq!(json["ip"], "127.0.0.1");
    assert!(json["addr"].as_str().unwrap().starts_with("127.0.0.1:"));
    assert_eq!(json["userAgent"], "test-agent");
    assert_eq!(json["xForwardedFor"], "10.0.0.1");
}

// ---- 5. 流式请求体 ----

#[tokio::test]
async fn streaming_request_body() {
    let addr = start().await;
    let data = vec![7u8; 3 * 1024 * 1024 + 17];
    let r = request(addr, "POST", "/upload/stream/test-upload.bin", &[], &data).await;
    let json = r.json();
    assert_eq!(json["bytes"], data.len() as u64);
    let saved = json["savedTo"].as_str().unwrap().to_string();
    assert_eq!(std::fs::read(&saved).unwrap(), data);
    std::fs::remove_file(saved).unwrap();
}

// ---- 6. Range 文件响应 ----

#[tokio::test]
async fn file_ranges() {
    let addr = start().await;
    let content = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/static/css/app.css")).unwrap();
    let size = content.len();

    let full = get(addr, "/files/css/app.css").await;
    assert_eq!(full.status, 200);
    assert_eq!(full.body, content);
    assert_eq!(full.header("accept-ranges"), Some("bytes"));
    assert_eq!(full.header("content-length"), Some(size.to_string().as_str()));
    assert!(full.header("content-type").unwrap().starts_with("text/css"));
    let etag = full.header("etag").unwrap().to_string();
    let last_modified = full.header("last-modified").unwrap().to_string();

    let r = request(addr, "GET", "/files/css/app.css", &[("range", "bytes=0-9")], b"").await;
    assert_eq!(r.status, 206);
    assert_eq!(r.body, &content[..10]);
    assert_eq!(r.header("content-range"), Some(format!("bytes 0-9/{size}").as_str()));

    let r = request(addr, "GET", "/files/css/app.css", &[("range", "bytes=-5")], b"").await;
    assert_eq!((r.status, r.body.as_slice()), (206, &content[size - 5..]));
    let r = request(addr, "GET", "/files/css/app.css", &[("range", "bytes=10-")], b"").await;
    assert_eq!((r.status, r.body.as_slice()), (206, &content[10..]));

    let r = request(addr, "GET", "/files/css/app.css", &[("range", "bytes=999999-")], b"").await;
    assert_eq!(r.status, 416);
    assert_eq!(r.header("content-range"), Some(format!("bytes */{size}").as_str()));

    // If-Range：ETag / 日期一致才按 Range 返回，否则返回整个文件
    let r = request(addr, "GET", "/files/css/app.css", &[("range", "bytes=0-9"), ("if-range", &etag)], b"").await;
    assert_eq!(r.status, 206);
    let r = request(addr, "GET", "/files/css/app.css", &[("range", "bytes=0-9"), ("if-range", &last_modified)], b"").await;
    assert_eq!(r.status, 206);
    let r = request(addr, "GET", "/files/css/app.css", &[("range", "bytes=0-9"), ("if-range", "\"old\"")], b"").await;
    assert_eq!((r.status, r.body.len()), (200, size));

    let r = request(addr, "GET", "/files/css/app.css", &[("if-none-match", &etag)], b"").await;
    assert_eq!((r.status, r.body.len()), (304, 0));

    let r = get(addr, "/files/img/logo.svg?download=true").await;
    assert_eq!(r.header("content-disposition"), Some(r#"attachment; filename="logo.svg""#));
    assert_eq!(get(addr, "/files/nope.txt").await.status, 404);
}

// ---- multipart 边收边解析、请求体大小上限 ----

#[tokio::test]
async fn large_multipart_file_is_spooled_and_later_fields_bind() {
    let addr = start().await;
    // 和 DFS 网页端一样：file 在前，token / folder 在后；文件远大于内存阈值
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let (ct, body) = multipart(&[("file", Some("big.bin"), &data), ("token", None, b"t"), ("folder", None, b"/big")]);
    let r = request(addr, "POST", "/compat/upload", &[("content-type", &ct)], &body).await;
    let json = r.json();
    assert_eq!(json["folder"], "/big");
    assert_eq!(json["size"], data.len() as u64);
    let temp_path = std::path::PathBuf::from(json["temp_path"].as_str().unwrap());
    // 临时文件写在配置的目录里（目录是自动创建的），请求处理完就删除
    assert!(temp_path.starts_with(upload_dir()), "{temp_path:?}");
    assert!(!temp_path.exists());

    // 默认模式的 multipart 结构体同样边收边解析：Vec<u8>、&Path、MultipartFile 字段都能拿到完整内容
    let (ct, body) = multipart(&[
        ("raw_file", Some("a.bin"), &data),
        ("temp_file", Some("b.bin"), &data),
        ("detailed_file", Some("c.bin"), &data[..10]),
        ("title", None, b"report"),
    ]);
    let r = request(addr, "POST", "/params/upload", &[("content-type", &ct)], &body).await;
    let text = r.text();
    assert!(text.contains(&format!("raw_file = {} bytes", data.len())), "{text}");
    assert!(text.contains(&format!("({} bytes), detailed_file", data.len())), "{text}");
    assert!(text.contains("\"c.bin\""), "{text}");
}

#[tokio::test]
async fn oversized_bodies_get_413() {
    let addr = start().await;
    // 文本字段不落盘：超过内存阈值返回 413
    let long = vec![b'x'; MEMORY_LIMIT + 1];
    let (ct, body) = multipart(&[("folder", None, &long), ("file", Some("a.txt"), b"x")]);
    let r = request(addr, "POST", "/compat/upload", &[("content-type", &ct)], &body).await;
    assert_eq!((r.status, r.text().as_str()), (413, "Request body is too large."));

    // 请求体总大小超过上限：multipart、urlencoded、JSON 都是 413，而且不经过 #[PARAM_ERROR]（那里返回 JSON 400）
    let big = vec![b'a'; MAX_BODY + 1];
    let (ct, body) = multipart(&[("file", Some("a.bin"), &big)]);
    assert_eq!(request(addr, "POST", "/compat/upload", &[("content-type", &ct)], &body).await.status, 413);
    let r = request(addr, "POST", "/params/form", &[FORM], &big).await;
    assert_eq!((r.status, r.header("content-type")), (413, Some("text/plain; charset=utf-8")));
    assert_eq!(request(addr, "POST", "/compat/user/save", &[("content-type", "application/json")], &big).await.status, 413);

    // BodyStream 不受限制，由 handler 自己判断
    let r = request(addr, "POST", "/upload/stream/test-limit.bin", &[], &big).await;
    let saved = r.json()["savedTo"].as_str().unwrap().to_string();
    std::fs::remove_file(saved).unwrap();
}
