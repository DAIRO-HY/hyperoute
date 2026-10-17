//! 静态资源磁盘模式的端到端行为（不经过 HTTP 连接，直接调用 serve_static）。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use http_body_util::BodyExt;
use hyperoute_core::{Request, Response, StaticMount, StaticSource};

/// 每个测试一个独立的资源目录；gzip 缓存目录是全局设置，所有测试共用一个。
fn temp_root(name: &str) -> PathBuf {
    static CACHE: OnceLock<()> = OnceLock::new();
    let base = std::env::temp_dir().join(format!("hyperoute-static-test-{}", std::process::id()));
    CACHE.get_or_init(|| hyperoute_core::set_static_cache_dir(base.join("cache")));
    let root = base.join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn mount(prefix: &'static str, root: &Path, gzip: bool, spa: bool) -> &'static [&'static StaticMount] {
    mount_with(prefix, root, gzip, spa, false)
}

fn mount_with(prefix: &'static str, root: &Path, gzip: bool, spa: bool, html_include: bool) -> &'static [&'static StaticMount] {
    let root: &'static str = Box::leak(root.to_string_lossy().into_owned().into_boxed_str());
    let mount: &'static StaticMount = Box::leak(Box::new(StaticMount {
        prefix,
        source: StaticSource::Disk { default_root: root },
        index: Some("index.html"),
        spa_fallback: spa,
        cache_control: Some("public, max-age=60"),
        gzip,
        gzip_prewarm: false,
        html_include,
    }));
    Box::leak(vec![mount].into_boxed_slice())
}

fn request(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request {
    let mut builder = hyper::Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut req = builder.body(hyperoute_core::full_request_body("")).unwrap();
    hyperoute_core::normalize_request_path(&mut req).unwrap();
    req
}

async fn get(mounts: &'static [&'static StaticMount], uri: &str, headers: &[(&str, &str)]) -> Option<Response> {
    hyperoute_core::serve_static(mounts, &request("GET", uri, headers)).await
}

async fn body(response: Response) -> Vec<u8> {
    response.into_body().collect().await.unwrap().to_bytes().to_vec()
}

fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response.headers().get(name).map(|v| v.to_str().unwrap())
}

fn gunzip(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data).read_to_end(&mut out).unwrap();
    out
}

#[tokio::test]
async fn files_directories_and_redirects() {
    let root = temp_root("basic");
    write(&root, "index.html", "<h1>home</h1>");
    write(&root, "docs/index.html", "<h1>docs</h1>");
    write(&root, "img/中文 logo.svg", "<svg/>");
    write(&root, ".env", "SECRET=1");
    let mounts = mount("/s", &root, false, false);

    let response = get(mounts, "/s/docs/", &[]).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(header(&response, "cache-control"), Some("public, max-age=60"));
    assert!(header(&response, "etag").is_some() && header(&response, "last-modified").is_some());
    assert_eq!(body(response).await, b"<h1>docs</h1>");

    // 目录缺少末尾 `/` → 301；挂载根路径同理；查询串保留
    let response = get(mounts, "/s/docs?x=1", &[]).await.unwrap();
    assert_eq!((response.status().as_u16(), header(&response, "location")), (301, Some("/s/docs/?x=1")));
    assert_eq!(get(mounts, "/s", &[]).await.unwrap().status(), 301);
    assert_eq!(body(get(mounts, "/s/", &[]).await.unwrap()).await, b"<h1>home</h1>");

    // 子目录 + percent 编码的中文 / 空格文件名
    let response = get(mounts, "/s/img/%E4%B8%AD%E6%96%87%20logo.svg", &[]).await.unwrap();
    assert_eq!(header(&response, "content-type"), Some("image/svg+xml"));

    // 隐藏文件、大小写不一致、文件后加 `/`、不存在、前缀外
    assert!(get(mounts, "/s/.env", &[]).await.is_none());
    assert!(get(mounts, "/s/Index.html", &[]).await.is_none());
    assert!(get(mounts, "/s/index.html/", &[]).await.is_none());
    assert!(get(mounts, "/s/missing.txt", &[]).await.is_none());
    assert!(get(mounts, "/other/index.html", &[]).await.is_none());

    // 只处理 GET / HEAD；HEAD 有 content-length 没有 body
    assert!(hyperoute_core::serve_static(mounts, &request("POST", "/s/", &[])).await.is_none());
    let response = hyperoute_core::serve_static(mounts, &request("HEAD", "/s/", &[])).await.unwrap();
    assert_eq!(header(&response, "content-length"), Some("13"));
    assert!(body(response).await.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_cannot_escape_root() {
    let root = temp_root("symlink");
    let outside = temp_root("symlink-outside");
    write(&outside, "secret.txt", "secret");
    std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();
    write(&root, "real.txt", "ok");
    std::os::unix::fs::symlink(root.join("real.txt"), root.join("inside-link.txt")).unwrap();
    let mounts = mount("/", &root, false, false);

    assert!(get(mounts, "/link.txt", &[]).await.is_none());
    assert_eq!(body(get(mounts, "/inside-link.txt", &[]).await.unwrap()).await, b"ok");
}

#[tokio::test]
async fn conditional_requests() {
    let root = temp_root("conditional");
    write(&root, "a.txt", "hello");
    let mounts = mount("/", &root, false, false);

    let first = get(mounts, "/a.txt", &[]).await.unwrap();
    let etag = header(&first, "etag").unwrap().to_string();
    let last_modified = header(&first, "last-modified").unwrap().to_string();

    let response = get(mounts, "/a.txt", &[("if-none-match", &etag)]).await.unwrap();
    assert_eq!(response.status(), 304);
    assert!(body(response).await.is_empty());
    assert_eq!(get(mounts, "/a.txt", &[("if-modified-since", &last_modified)]).await.unwrap().status(), 304);
    assert_eq!(get(mounts, "/a.txt", &[("if-none-match", "\"other\"")]).await.unwrap().status(), 200);
}

#[tokio::test]
async fn gzip_cache_and_invalidation() {
    let root = temp_root("gzip");
    let v1 = "console.log('v1');\n".repeat(200);
    write(&root, "app.js", &v1);
    write(&root, "tiny.js", "1");
    write(&root, "photo.jpg", vec![7u8; 4096]);
    let mounts = mount("/", &root, true, false);
    let gzip = [("accept-encoding", "gzip, br")];

    // 首次：压缩并写缓存
    let response = get(mounts, "/app.js", &gzip).await.unwrap();
    assert_eq!(header(&response, "content-encoding"), Some("gzip"));
    assert_eq!(header(&response, "vary"), Some("accept-encoding"));
    let gz_etag = header(&response, "etag").unwrap().to_string();
    assert!(gz_etag.ends_with("-gz\""));
    let compressed = body(response).await;
    assert!(compressed.len() < v1.len() / 5);
    assert_eq!(gunzip(&compressed), v1.as_bytes());

    // 再次：命中缓存，内容一致；带压缩版 ETag 的条件请求 → 304
    assert_eq!(body(get(mounts, "/app.js", &gzip).await.unwrap()).await, compressed);
    assert_eq!(get(mounts, "/app.js", &[gzip[0], ("if-none-match", &gz_etag)]).await.unwrap().status(), 304);

    // 文件修改后缓存失效
    std::thread::sleep(std::time::Duration::from_millis(20));
    let v2 = "console.log('v2');\n".repeat(300);
    write(&root, "app.js", &v2);
    let response = get(mounts, "/app.js", &gzip).await.unwrap();
    assert_ne!(header(&response, "etag").unwrap(), gz_etag);
    assert_eq!(gunzip(&body(response).await), v2.as_bytes());

    // 客户端不支持 gzip → 原文件，但仍带 Vary
    let response = get(mounts, "/app.js", &[]).await.unwrap();
    assert_eq!((header(&response, "content-encoding"), header(&response, "vary")), (None, Some("accept-encoding")));
    assert_eq!(body(response).await, v2.as_bytes());

    // 太小的文件、本身已压缩的格式不压缩
    assert_eq!(header(&get(mounts, "/tiny.js", &gzip).await.unwrap(), "content-encoding"), None);
    assert_eq!(header(&get(mounts, "/photo.jpg", &gzip).await.unwrap(), "content-encoding"), None);
}

#[tokio::test]
async fn sidecar_gz_is_preferred() {
    let root = temp_root("sidecar");
    let original = "body { color: red; }\n".repeat(100);
    write(&root, "style.css", &original);
    std::thread::sleep(std::time::Duration::from_millis(20));
    // 故意放一个内容不同的 .gz，确认返回的是它而不是运行时压缩的结果
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut encoder, b"from-sidecar").unwrap();
    write(&root, "style.css.gz", encoder.finish().unwrap());
    let mounts = mount("/", &root, true, false);

    let response = get(mounts, "/style.css", &[("accept-encoding", "gzip")]).await.unwrap();
    assert_eq!(header(&response, "content-type"), Some("text/css; charset=utf-8"));
    assert_eq!(gunzip(&body(response).await), b"from-sidecar");
}

#[tokio::test]
async fn spa_fallback_only_for_page_routes() {
    let root = temp_root("spa");
    write(&root, "index.html", "<app/>");
    let mounts = mount("/app", &root, false, true);

    assert_eq!(body(get(mounts, "/app/users/42", &[]).await.unwrap()).await, b"<app/>");
    assert!(get(mounts, "/app/missing.js", &[]).await.is_none());
}

#[tokio::test]
async fn html_include_merges_then_serves_as_static_file() {
    let root = temp_root("include");
    write(&root, "include/head.tpl.html", "<meta charset=utf-8>{{template nav}}");
    write(&root, "include/nav.tpl.html", "<nav>v1</nav>");
    write(&root, "index.html", "<head>{{template include/head}}</head><p>{{ vue }}</p>");
    write(&root, "docs/index.html", "{{template /include/nav}}");
    write(&root, "plain.html", "<p>plain</p>");
    write(&root, "big.html", &format!("{{{{template include/nav}}}}{}", "<li>row</li>".repeat(300)));
    let mounts = mount_with("/", &root, true, false, true);

    // 合并后的内容；`{{ vue }}` 这类非模板调用原样保留
    let response = get(mounts, "/", &[]).await.unwrap();
    assert_eq!(header(&response, "content-type"), Some("text/html; charset=utf-8"));
    let etag_v1 = header(&response, "etag").unwrap().to_string();
    assert_eq!(body(response).await, b"<head><meta charset=utf-8><nav>v1</nav></head><p>{{ vue }}</p>");
    // 子目录里用 `/` 开头的路径引用根目录下的模板
    assert_eq!(body(get(mounts, "/docs/", &[]).await.unwrap()).await, b"<nav>v1</nav>");
    // 没有模板调用的页面原样返回
    assert_eq!(body(get(mounts, "/plain.html", &[]).await.unwrap()).await, b"<p>plain</p>");

    // 模板片段不对外提供
    assert!(get(mounts, "/include/head.tpl.html", &[]).await.is_none());
    assert!(get(mounts, "/include/NAV.TPL.HTML", &[]).await.is_none());

    // 合并结果按普通静态文件处理：304 / gzip 照常
    assert_eq!(get(mounts, "/", &[("if-none-match", &etag_v1)]).await.unwrap().status(), 304);
    let response = get(mounts, "/big.html", &[("accept-encoding", "gzip")]).await.unwrap();
    assert_eq!(header(&response, "content-encoding"), Some("gzip"));
    assert!(gunzip(&body(response).await).starts_with(b"<nav>v1</nav><li>row</li>"));

    // 被间接引用的模板变化后自动重新合并，ETag 随之变化
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(&root, "include/nav.tpl.html", "<nav>v2</nav>");
    let response = get(mounts, "/", &[("if-none-match", &etag_v1)]).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_ne!(header(&response, "etag").unwrap(), etag_v1);
    assert_eq!(body(response).await, b"<head><meta charset=utf-8><nav>v2</nav></head><p>{{ vue }}</p>");
    let response = get(mounts, "/big.html", &[("accept-encoding", "gzip")]).await.unwrap();
    assert!(gunzip(&body(response).await).starts_with(b"<nav>v2</nav>"));

    // 引用的模板不存在：服务端问题，500
    write(&root, "broken.html", "{{template include/missing}}");
    assert_eq!(get(mounts, "/broken.html", &[]).await.unwrap().status(), 500);

    // 不开启 html_include 时，模板调用原样返回，.tpl.html 也是普通文件
    let plain_mounts = mount("/", &root, false, false);
    assert_eq!(body(get(plain_mounts, "/docs/", &[]).await.unwrap()).await, b"{{template /include/nav}}");
    assert!(get(plain_mounts, "/include/nav.tpl.html", &[]).await.is_some());
}

/// hyperoute-core 和 hyperoute-codegen 各有一份 html_include.rs，必须完全一致。
/// （打包发布后另一份不在包里，测试自动跳过。）
#[test]
fn html_include_copies_are_identical() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let other = manifest.join("../hyperoute-codegen/src/html_include.rs");
    if !other.exists() {
        return;
    }
    let mine = std::fs::read_to_string(manifest.join("src/html_include.rs")).unwrap();
    assert_eq!(mine, std::fs::read_to_string(other).unwrap(), "两份 html_include.rs 不一致，请同步修改");
}
