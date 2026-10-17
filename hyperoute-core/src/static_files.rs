//! 静态资源（html / js / css / 图片等）。
//!
//! 每个挂载点（[`StaticMount`]）由 hyperoute-codegen 在编译期根据 build.rs 里的 `StaticDir` 配置生成，
//! 文件来源有两种：
//! - **打包**（[`StaticSource::Embedded`]）：编译期把文件打进二进制，文件表按路径排好序，运行时二分查找；
//!   开启 gzip 时可压缩的文件只存压缩版，无论客户端是否声明支持都直接返回 gzip。
//! - **磁盘**（[`StaticSource::Disk`]）：运行时按路径读文件，根目录可用 [`set_static_root`] 在运行时指定。
//!   开启 gzip 时优先使用旁边现成的 `xxx.gz`，否则首次访问时压缩并缓存到磁盘，文件变化后自动失效。
//!
//! 开启 `html_include` 时，html 页面先做模板合并（见 `html_include` 模块），合并结果再按普通静态文件处理：
//! 打包模式在编译期合并后打包；磁盘模式把合并结果存成缓存目录里的真实文件，页面或任何被引用的模板变化后
//! 自动重新合并。`.tpl.html` 模板片段不对外提供。
//!
//! 静态资源只在没有动态路由匹配时才查找；请求同样经过过滤器（入口已统一规范化路径）。

use std::collections::HashMap;
use std::fs::Metadata;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper::header::{self, HeaderValue};
use hyper::{Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::html_include::{self, IncludeError};
use crate::request_path::request_path;
use crate::{Request, Response, ResponseBody};

/// 小于这个大小的文件不压缩（压缩收益太小）。
pub const GZIP_MIN_SIZE: u64 = 1024;
/// 磁盘模式运行时压缩的文件大小上限，超过的直接返回原文件（避免一次性读进内存压缩）。
/// 更大的文件可以由前端构建工具生成 `xxx.gz` 放在旁边。
pub const GZIP_MAX_RUNTIME_SIZE: u64 = 16 * 1024 * 1024;

/// 打包进二进制的一个文件（由生成代码构造）。
#[derive(Debug)]
pub struct EmbeddedFile {
    /// 相对挂载目录的路径，用 `/` 分隔，例如 `img/logo.png`
    pub path: &'static str,
    /// 文件内容；`gzip` 为 true 时是 gzip 压缩后的内容
    pub data: &'static [u8],
    pub gzip: bool,
    /// 编译期根据原始内容计算的 ETag（含引号）
    pub etag: &'static str,
}

#[derive(Debug)]
pub enum StaticSource {
    /// 打包进二进制的文件表，按 `path` 字节序排好
    Embedded(&'static [EmbeddedFile]),
    /// 运行时读磁盘；`default_root` 是编译期配置的目录（绝对路径），可被 [`set_static_root`] 覆盖
    Disk { default_root: &'static str },
}

/// 一个静态资源挂载点（由生成代码构造）。
#[derive(Debug)]
pub struct StaticMount {
    /// URL 前缀，例如 `/static`；`/` 表示挂在根路径
    pub prefix: &'static str,
    pub source: StaticSource,
    /// 访问目录时返回的文件，例如 `index.html`
    pub index: Option<&'static str>,
    /// 找不到文件、且路径最后一段没有扩展名时返回根目录的 index 文件（前端 history 路由）
    pub spa_fallback: bool,
    pub cache_control: Option<&'static str>,
    pub gzip: bool,
    /// 磁盘模式：首次请求时在后台把可压缩文件预先压缩进缓存
    pub gzip_prewarm: bool,
    /// html 模板合并（`{{template include/head}}`）；`.tpl.html` 不对外提供
    pub html_include: bool,
}

static ROOT_OVERRIDES: LazyLock<RwLock<HashMap<String, PathBuf>>> = LazyLock::new(Default::default);
static CACHE_DIR: LazyLock<RwLock<Option<PathBuf>>> = LazyLock::new(Default::default);

/// 运行时指定某个挂载点（按 URL 前缀，例如 `"/static"`）的磁盘根目录。
/// 用于只有运行时才知道目录的场景（例如 iOS 沙盒），或部署目录和编译时不同。
/// 只对磁盘模式的挂载点生效；应在启动服务前调用。
pub fn set_static_root(prefix: &str, root: impl Into<PathBuf>) {
    ROOT_OVERRIDES
        .write()
        .unwrap()
        .insert(normalize_prefix(prefix), root.into());
}

/// 运行时指定 gzip 压缩缓存目录（磁盘模式），默认是系统临时目录下的 `hyperoute-static-cache`。
/// iOS 上建议设为 App 的 Caches 目录。
pub fn set_static_cache_dir(dir: impl Into<PathBuf>) {
    *CACHE_DIR.write().unwrap() = Some(dir.into());
}

fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() { "/".to_string() } else { format!("/{trimmed}") }
}

fn root_for(mount: &StaticMount, default_root: &str) -> PathBuf {
    ROOT_OVERRIDES
        .read()
        .unwrap()
        .get(mount.prefix)
        .cloned()
        .unwrap_or_else(|| PathBuf::from(default_root))
}

fn cache_dir() -> PathBuf {
    CACHE_DIR
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("hyperoute-static-cache"))
}

/// 按扩展名得到 content-type；文本类型带 `charset=utf-8`。
pub fn content_type(path: &str) -> &'static str {
    match extension(path).as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some("md") => "text/markdown; charset=utf-8",
        Some("xml") => "application/xml",
        Some("svg") => "image/svg+xml",
        Some("wasm") => "application/wasm",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("bmp") => "image/bmp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("eot") => "application/vnd.ms-fontobject",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// 适合 gzip 的文本类资源。图片 / 音视频 / woff2 / zip 等本身已压缩，不在此列。
/// 注意：hyperoute-codegen 里有一份相同的列表（编译期压缩用），修改时两边保持一致。
pub fn is_compressible(path: &str) -> bool {
    matches!(
        extension(path).as_deref(),
        Some("html" | "htm" | "css" | "js" | "mjs" | "json" | "map" | "txt" | "csv" | "md" | "xml" | "svg" | "wasm" | "ttf" | "otf" | "eot")
    )
}

fn extension(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next()?;
    let (_, ext) = name.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

/// 生成代码在路由入口调用：对开启了 `gzip_prewarm` 的磁盘挂载点，第一次请求时在后台预压缩一次。
pub fn static_init(mounts: &'static [&'static StaticMount]) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    for mount in mounts {
        if let StaticSource::Disk { default_root } = mount.source {
            if mount.gzip && mount.gzip_prewarm {
                let root = root_for(mount, default_root);
                tokio::task::spawn_blocking(move || prewarm(&root));
            }
        }
    }
}

/// 生成代码在没有动态路由匹配时调用；不是静态资源请求时返回 None（由调用方返回 404）。
pub async fn serve_static(mounts: &'static [&'static StaticMount], req: &Request) -> Option<Response> {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return None;
    }
    let path = request_path(req);
    let (mount, rel) = find_mount(mounts, path)?;
    // 以 `.` 开头的隐藏文件 / 目录（.env、.git ...）一律不提供
    if rel.iter().any(|segment| segment.starts_with('.')) {
        return None;
    }
    // 模板片段只用于合并，不对外提供
    if mount.html_include && rel.last().is_some_and(|last| html_include::is_template(last)) {
        return None;
    }
    let dir_request = path.ends_with('/');

    match mount.source {
        StaticSource::Embedded(files) => serve_embedded(mount, files, req, &rel, dir_request),
        StaticSource::Disk { default_root } => serve_disk(mount, &root_for(mount, default_root), req, &rel, dir_request).await,
    }
}

/// 最长前缀匹配：返回挂载点和前缀之后的路径段。
fn find_mount<'a>(mounts: &'static [&'static StaticMount], path: &'a str) -> Option<(&'static StaticMount, Vec<&'a str>)> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    mounts
        .iter()
        .filter_map(|mount| {
            let prefix: Vec<&str> = mount.prefix.split('/').filter(|s| !s.is_empty()).collect();
            segments.starts_with(&prefix).then(|| (*mount, prefix.len()))
        })
        .max_by_key(|(_, len)| *len)
        .map(|(mount, len)| (mount, segments[len..].to_vec()))
}

// ---------------------------------------------------------------------------------------------
// 打包模式

fn serve_embedded(
    mount: &StaticMount,
    files: &'static [EmbeddedFile],
    req: &Request,
    rel: &[&str],
    dir_request: bool,
) -> Option<Response> {
    let find = |path: &str| files.binary_search_by(|f| f.path.cmp(path)).ok().map(|i| &files[i]);
    let rel_path = rel.join("/");

    let file = if dir_request || rel.is_empty() {
        if !dir_request {
            return Some(redirect_to_dir(req));
        }
        mount.index.and_then(|index| find(&join(&rel_path, index)))
    } else if let Some(file) = find(&rel_path) {
        Some(file)
    } else if embedded_dir_exists(files, &rel_path) {
        return Some(redirect_to_dir(req));
    } else {
        None
    };

    let file = match file {
        Some(file) => file,
        None if spa_applies(mount, rel) => find(mount.index?)?,
        None => return None,
    };

    let mut builder = file_headers(mount, file.path, Some(file.etag), None, file.gzip);
    if not_modified(req, &[file.etag], None) {
        return Some(builder.status(StatusCode::NOT_MODIFIED).body(empty()).unwrap());
    }
    builder = builder.header(header::CONTENT_LENGTH, file.data.len());
    let body = if req.method() == Method::HEAD {
        empty()
    } else {
        Full::new(Bytes::from_static(file.data)).map_err(|never| match never {}).boxed()
    };
    Some(builder.body(body).unwrap())
}

fn embedded_dir_exists(files: &[EmbeddedFile], dir: &str) -> bool {
    if dir.is_empty() {
        return true;
    }
    let prefix = format!("{dir}/");
    let i = files.partition_point(|f| f.path < prefix.as_str());
    files.get(i).is_some_and(|f| f.path.starts_with(&prefix))
}

// ---------------------------------------------------------------------------------------------
// 磁盘模式

enum DiskHit {
    File { path: PathBuf, meta: Metadata },
    Redirect,
    NotFound,
    /// 模板合并失败（引用的模板不存在、循环引用等），属于服务端问题
    Error,
}

async fn serve_disk(mount: &StaticMount, root: &Path, req: &Request, rel: &[&str], dir_request: bool) -> Option<Response> {
    let root = root.to_path_buf();
    let rel_owned: Vec<String> = rel.iter().map(|s| s.to_string()).collect();
    let index = mount.index;
    let spa = spa_applies(mount, rel);
    let include = mount.html_include;

    let hit = tokio::task::spawn_blocking(move || {
        let rel: Vec<&str> = rel_owned.iter().map(String::as_str).collect();
        let hit = match resolve_disk(&root, &rel, dir_request, index) {
            DiskHit::NotFound if spa => resolve_disk(&root, &[], true, index),
            other => other,
        };
        match hit {
            // 先合并模板，再把合并结果当成普通文件交给后面的逻辑
            DiskHit::File { path, meta } if include && html_include::is_html_page(&path.to_string_lossy()) => {
                match materialize_html(&root, &path, &meta) {
                    Ok(Some((merged, merged_meta))) => DiskHit::File { path: merged, meta: merged_meta },
                    Ok(None) => DiskHit::File { path, meta },
                    Err(error) => {
                        eprintln!("[hyperoute] html include error for {}: {error}", path.display());
                        DiskHit::Error
                    }
                }
            }
            other => other,
        }
    })
    .await
    .ok()?;

    match hit {
        DiskHit::NotFound => None,
        DiskHit::Redirect => Some(redirect_to_dir(req)),
        DiskHit::Error => Some(crate::internal_error_response()),
        DiskHit::File { path, meta } => Some(serve_disk_file(mount, req, path, meta).await),
    }
}

/// 解析磁盘路径（阻塞，在 spawn_blocking 里调用），做目录 / index / 越界 / 大小写检查。
fn resolve_disk(root: &Path, rel: &[&str], dir_request: bool, index: Option<&str>) -> DiskHit {
    let Ok(root) = root.canonicalize() else {
        return DiskHit::NotFound;
    };
    let mut path = root.clone();
    let mut components: Vec<&str> = rel.to_vec();
    path.extend(rel);

    let Ok(mut meta) = std::fs::metadata(&path) else {
        return DiskHit::NotFound;
    };
    if meta.is_dir() {
        if !dir_request {
            return DiskHit::Redirect;
        }
        let Some(index) = index else {
            return DiskHit::NotFound;
        };
        path.push(index);
        components.push(index);
        match std::fs::metadata(&path) {
            Ok(m) => meta = m,
            Err(_) => return DiskHit::NotFound,
        }
    } else if dir_request {
        return DiskHit::NotFound; // `/file.txt/`
    }
    if !meta.is_file() {
        return DiskHit::NotFound;
    }

    // 符号链接不能跳出根目录
    match path.canonicalize() {
        Ok(real) if real.starts_with(&root) => {}
        _ => return DiskHit::NotFound,
    }
    // 大小写不敏感的文件系统上，严格按大小写匹配，保证和打包模式行为一致
    if !exact_case(&root, &components) {
        return DiskHit::NotFound;
    }
    DiskHit::File { path, meta }
}

#[cfg(any(target_os = "macos", windows))]
fn exact_case(root: &Path, components: &[&str]) -> bool {
    let mut dir = root.to_path_buf();
    for name in components {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return false;
        };
        if !entries.flatten().any(|e| e.file_name() == *name) {
            return false;
        }
        dir.push(name);
    }
    true
}

#[cfg(not(any(target_os = "macos", windows)))]
fn exact_case(_root: &Path, _components: &[&str]) -> bool {
    true // 大小写敏感的文件系统本身就保证了这一点
}

async fn serve_disk_file(mount: &StaticMount, req: &Request, path: PathBuf, meta: Metadata) -> Response {
    let path_str = path.to_string_lossy().into_owned();
    let mtime = mtime_nanos(&meta);
    let raw_etag = format!("\"{:x}-{:x}\"", meta.len(), mtime);
    let gz_etag = format!("\"{:x}-{:x}-gz\"", meta.len(), mtime);
    let last_modified = meta.modified().ok();
    let compressible = mount.gzip && is_compressible(&path_str) && meta.len() >= GZIP_MIN_SIZE;
    let use_gzip = compressible && accepts_gzip(req);

    // 先用不需要 I/O 的 ETag 判断 304
    let candidates: Vec<&str> = if use_gzip { vec![gz_etag.as_str(), raw_etag.as_str()] } else { vec![raw_etag.as_str()] };
    if not_modified(req, &candidates, last_modified) {
        let etag = matched_etag(req, &candidates).unwrap_or(candidates[0]);
        let gzip = use_gzip && etag == gz_etag;
        let mut builder = file_headers(mount, &path_str, Some(etag), last_modified, gzip);
        if compressible && !gzip {
            builder = builder.header(header::VARY, "accept-encoding");
        }
        return builder.status(StatusCode::NOT_MODIFIED).body(empty()).unwrap();
    }

    if use_gzip {
        if let Some(gz) = gzip_variant(&path, &meta).await {
            let builder = file_headers(mount, &path_str, Some(&gz_etag), last_modified, true);
            return stream_file(builder, req, &gz.path, gz.offset, gz.len).await;
        }
    }
    let mut builder = file_headers(mount, &path_str, Some(&raw_etag), last_modified, false);
    if compressible {
        builder = builder.header(header::VARY, "accept-encoding");
    }
    stream_file(builder, req, &path, 0, meta.len()).await
}

// 合并结果缓存：缓存目录下 `html/<路径哈希>.html`（合并后的页面）+ `html/<路径哈希>.inc`（依赖清单）。
// 清单格式（文本，每行一项）：
//   HINC1
//   <页面大小> <页面修改时间纳秒>
//   merged | plain            （plain 表示页面没有模板调用，直接用原文件）
//   <模板大小> <模板修改时间纳秒> <模板路径>   （每个依赖一行）
const INCLUDE_MANIFEST_MAGIC: &str = "HINC1";

/// 磁盘模式下的模板合并：返回合并后的缓存文件；页面没有模板调用时返回 None（直接用原文件）。
/// 页面本身或任何被引用的模板变化后自动重新合并。
fn materialize_html(root: &Path, page: &Path, meta: &Metadata) -> Result<Option<(PathBuf, Metadata)>, String> {
    let dir = cache_dir().join("html");
    let key = format!("{:016x}", fnv1a(page.to_string_lossy().as_bytes()));
    let merged_path = dir.join(format!("{key}.html"));
    let manifest_path = dir.join(format!("{key}.inc"));
    let stamp = |m: &Metadata| format!("{} {}", m.len(), mtime_nanos(m));

    // 清单有效（页面和所有依赖都没变）就直接用上次的结果
    if let Ok(manifest) = std::fs::read_to_string(&manifest_path) {
        let mut lines = manifest.lines();
        let fresh = lines.next() == Some(INCLUDE_MANIFEST_MAGIC) && lines.next() == Some(stamp(meta).as_str());
        let kind = lines.next();
        let deps_fresh = fresh
            && lines.all(|line| {
                let mut parts = line.splitn(3, ' ');
                let (Some(len), Some(mtime), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
                    return false;
                };
                std::fs::metadata(path).is_ok_and(|m| stamp(&m) == format!("{len} {mtime}"))
            });
        if deps_fresh {
            match kind {
                Some("plain") => return Ok(None),
                Some("merged") => {
                    if let Ok(merged_meta) = std::fs::metadata(&merged_path) {
                        return Ok(Some((merged_path, merged_meta)));
                    }
                }
                _ => {}
            }
        }
    }

    let merged = html_include::merge(root, page).map_err(|e: IncludeError| e.to_string())?;
    let mut manifest = format!("{INCLUDE_MANIFEST_MAGIC}\n{}\n{}\n", stamp(meta), if merged.changed { "merged" } else { "plain" });
    for dep in &merged.dependencies {
        let dep_meta = std::fs::metadata(dep).map_err(|e| format!("{}: {e}", dep.display()))?;
        manifest.push_str(&format!("{} {}\n", stamp(&dep_meta), dep.display()));
    }

    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if merged.changed {
        write_atomic(&dir, &merged_path, merged.content.as_bytes()).map_err(|e| e.to_string())?;
    }
    write_atomic(&dir, &manifest_path, manifest.as_bytes()).map_err(|e| e.to_string())?;

    if !merged.changed {
        return Ok(None);
    }
    let merged_meta = std::fs::metadata(&merged_path).map_err(|e| e.to_string())?;
    Ok(Some((merged_path, merged_meta)))
}

/// 先写临时文件再 rename，保证并发请求不会读到写了一半的文件。
fn write_atomic(dir: &Path, target: &Path, data: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, target)
}

struct GzipFile {
    path: PathBuf,
    offset: u64,
    len: u64,
}

/// 现成的 `xxx.gz`（不比原文件旧）优先；否则查 / 建压缩缓存。不值得压缩或出错时返回 None。
async fn gzip_variant(path: &Path, meta: &Metadata) -> Option<GzipFile> {
    let path = path.to_path_buf();
    let meta = meta.clone();
    tokio::task::spawn_blocking(move || {
        let mut sidecar = path.clone().into_os_string();
        sidecar.push(".gz");
        let sidecar = PathBuf::from(sidecar);
        if let Ok(side_meta) = std::fs::metadata(&sidecar) {
            let fresh = match (side_meta.modified(), meta.modified()) {
                (Ok(side), Ok(orig)) => side >= orig,
                _ => false,
            };
            if side_meta.is_file() && fresh {
                return Some(GzipFile { path: sidecar, offset: 0, len: side_meta.len() });
            }
        }
        match ensure_gzip_cache(&path, &meta) {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("[hyperoute] gzip cache error for {}: {error}", path.display());
                None
            }
        }
    })
    .await
    .ok()
    .flatten()
}

// 缓存文件格式：24 字节文件头 + gzip 数据。
// 文件头：magic "HGZ1"(4) | 是否值得压缩(1) | 填充(3) | 原文件大小 u64 LE(8) | 原文件修改时间纳秒 u64 LE(8)。
// 每个源文件只对应一个缓存文件（按路径哈希命名），文件变化后原地覆盖，不会无限增长。
const CACHE_MAGIC: &[u8; 4] = b"HGZ1";
const CACHE_HEADER_LEN: u64 = 24;

fn ensure_gzip_cache(path: &Path, meta: &Metadata) -> io::Result<Option<GzipFile>> {
    let dir = cache_dir();
    let cache_path = dir.join(format!("{:016x}.hgz", fnv1a(path.to_string_lossy().as_bytes())));
    let mtime = mtime_nanos(meta);

    if let Ok(mut file) = std::fs::File::open(&cache_path) {
        let mut header = [0u8; CACHE_HEADER_LEN as usize];
        if file.read_exact(&mut header).is_ok()
            && &header[0..4] == CACHE_MAGIC
            && u64::from_le_bytes(header[8..16].try_into().unwrap()) == meta.len()
            && u64::from_le_bytes(header[16..24].try_into().unwrap()) == mtime
        {
            let worth = header[4] == 1;
            let len = file.metadata()?.len() - CACHE_HEADER_LEN;
            return Ok(worth.then(|| GzipFile { path: cache_path, offset: CACHE_HEADER_LEN, len }));
        }
    }

    if meta.len() > GZIP_MAX_RUNTIME_SIZE {
        return Ok(None);
    }
    let original = std::fs::read(path)?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&original)?;
    let compressed = encoder.finish()?;
    let worth = compressed.len() * 10 < original.len() * 9; // 至少省 10%

    let mut header = [0u8; CACHE_HEADER_LEN as usize];
    header[0..4].copy_from_slice(CACHE_MAGIC);
    header[4] = worth as u8;
    header[8..16].copy_from_slice(&meta.len().to_le_bytes());
    header[16..24].copy_from_slice(&mtime.to_le_bytes());

    std::fs::create_dir_all(&dir)?;
    let mut data = header.to_vec();
    if worth {
        data.extend_from_slice(&compressed);
    }
    write_atomic(&dir, &cache_path, &data)?;

    Ok(worth.then(|| GzipFile { path: cache_path, offset: CACHE_HEADER_LEN, len: compressed.len() as u64 }))
}

/// 后台预压缩：遍历根目录，把可压缩、没有现成 `.gz` 的文件压进缓存。
fn prewarm(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file()
                && is_compressible(&path.to_string_lossy())
                && (GZIP_MIN_SIZE..=GZIP_MAX_RUNTIME_SIZE).contains(&meta.len())
            {
                let mut sidecar = path.clone().into_os_string();
                sidecar.push(".gz");
                if Path::new(&sidecar).is_file() {
                    continue;
                }
                if let Err(error) = ensure_gzip_cache(&path, &meta) {
                    eprintln!("[hyperoute] gzip prewarm error for {}: {error}", path.display());
                }
            }
        }
    }
}

async fn stream_file(mut builder: hyper::http::response::Builder, req: &Request, path: &Path, offset: u64, len: u64) -> Response {
    builder = builder.header(header::CONTENT_LENGTH, len);
    if req.method() == Method::HEAD {
        return builder.body(empty()).unwrap();
    }
    let file = match tokio::fs::File::open(path).await {
        Ok(mut file) => match file.seek(io::SeekFrom::Start(offset)).await {
            Ok(_) => file,
            Err(error) => return read_error(path, error),
        },
        Err(error) => return read_error(path, error),
    };
    let stream = tokio_util::io::ReaderStream::new(file.take(len));
    builder.body(crate::Stream::new(stream).body).unwrap()
}

fn read_error(path: &Path, error: io::Error) -> Response {
    eprintln!("[hyperoute] static file read error for {}: {error}", path.display());
    crate::internal_error_response()
}

// ---------------------------------------------------------------------------------------------
// 公共部分

fn file_headers(
    mount: &StaticMount,
    path: &str,
    etag: Option<&str>,
    last_modified: Option<SystemTime>,
    gzip: bool,
) -> hyper::http::response::Builder {
    let mut builder = hyper::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(path));
    if let Some(cache_control) = mount.cache_control {
        builder = builder.header(header::CACHE_CONTROL, cache_control);
    }
    if let Some(etag) = etag {
        builder = builder.header(header::ETAG, etag);
    }
    if let Some(modified) = last_modified {
        builder = builder.header(header::LAST_MODIFIED, httpdate::fmt_http_date(modified));
    }
    if gzip {
        builder = builder
            .header(header::CONTENT_ENCODING, "gzip")
            .header(header::VARY, "accept-encoding");
    }
    builder
}

/// If-None-Match 优先；没有它时才看 If-Modified-Since（精确到秒）。
fn not_modified(req: &Request, etags: &[&str], last_modified: Option<SystemTime>) -> bool {
    if req.headers().contains_key(header::IF_NONE_MATCH) {
        return matched_etag(req, etags).is_some();
    }
    let (Some(modified), Some(since)) = (
        last_modified,
        req.headers()
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok()),
    ) else {
        return false;
    };
    let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    secs(modified) <= secs(since)
}

fn matched_etag<'a>(req: &Request, etags: &[&'a str]) -> Option<&'a str> {
    let value = req.headers().get(header::IF_NONE_MATCH)?.to_str().ok()?;
    for tag in value.split(',').map(str::trim) {
        if tag == "*" {
            return etags.first().copied();
        }
        let tag = tag.strip_prefix("W/").unwrap_or(tag);
        if let Some(found) = etags.iter().find(|e| **e == tag) {
            return Some(found);
        }
    }
    None
}

/// `Accept-Encoding` 里有 gzip（或 `*`）且 q 不为 0。
fn accepts_gzip(req: &Request) -> bool {
    let Some(value) = req.headers().get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    value.split(',').any(|item| {
        let mut parts = item.split(';').map(str::trim);
        let coding = parts.next().unwrap_or("");
        let q_zero = parts.any(|p| {
            p.strip_prefix("q=")
                .and_then(|q| q.parse::<f32>().ok())
                .is_some_and(|q| q == 0.0)
        });
        (coding.eq_ignore_ascii_case("gzip") || coding == "*") && !q_zero
    })
}

/// SPA 回退只对“看起来是页面路由”的路径生效：最后一段没有扩展名（`/app.js` 缺失仍然是 404）。
fn spa_applies(mount: &StaticMount, rel: &[&str]) -> bool {
    mount.spa_fallback && mount.index.is_some() && rel.last().is_none_or(|last| !last.contains('.'))
}

/// 访问目录但缺少末尾 `/` 时 301 到带 `/` 的地址，保证 index.html 里的相对路径解析正确。
fn redirect_to_dir(req: &Request) -> Response {
    let mut location = format!("{}/", req.uri().path());
    if let Some(query) = req.uri().query() {
        location.push('?');
        location.push_str(query);
    }
    hyper::Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, HeaderValue::from_str(&location).unwrap_or(HeaderValue::from_static("/")))
        .body(empty())
        .unwrap()
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

fn empty() -> ResponseBody {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

fn mtime_nanos(meta: &Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(headers: &[(&str, &str)]) -> Request {
        let mut builder = hyper::Request::get("/");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(crate::full_request_body("")).unwrap()
    }

    #[test]
    fn accept_encoding_parsing() {
        assert!(accepts_gzip(&request(&[("accept-encoding", "gzip, deflate, br")])));
        assert!(accepts_gzip(&request(&[("accept-encoding", "br;q=1.0, GZIP;q=0.5")])));
        assert!(accepts_gzip(&request(&[("accept-encoding", "*")])));
        assert!(!accepts_gzip(&request(&[("accept-encoding", "gzip;q=0")])));
        assert!(!accepts_gzip(&request(&[("accept-encoding", "br")])));
        assert!(!accepts_gzip(&request(&[])));
    }

    #[test]
    fn etag_matching() {
        let req = request(&[("if-none-match", r#"W/"abc", "def-gz""#)]);
        assert_eq!(matched_etag(&req, &[r#""def-gz""#, r#""def""#]), Some(r#""def-gz""#));
        assert_eq!(matched_etag(&req, &[r#""abc""#]), Some(r#""abc""#));
        assert_eq!(matched_etag(&req, &[r#""zzz""#]), None);
        assert!(not_modified(&request(&[("if-none-match", "*")]), &[r#""x""#], None));
    }

    #[test]
    fn content_types_and_compressibility() {
        assert_eq!(content_type("a/b/INDEX.HTML"), "text/html; charset=utf-8");
        assert_eq!(content_type("logo.png"), "image/png");
        assert_eq!(content_type("noext"), "application/octet-stream");
        assert!(is_compressible("app.js"));
        assert!(!is_compressible("photo.jpg"));
        assert!(!is_compressible("font.woff2"));
    }

    #[test]
    fn embedded_dir_lookup() {
        static FILES: &[EmbeddedFile] = &[
            EmbeddedFile { path: "docs/index.html", data: b"", gzip: false, etag: "\"1\"" },
            EmbeddedFile { path: "docsx.txt", data: b"", gzip: false, etag: "\"2\"" },
            EmbeddedFile { path: "index.html", data: b"", gzip: false, etag: "\"3\"" },
        ];
        assert!(embedded_dir_exists(FILES, "docs"));
        assert!(!embedded_dir_exists(FILES, "doc"));
        assert!(!embedded_dir_exists(FILES, "docsx.txt"));
    }
}
