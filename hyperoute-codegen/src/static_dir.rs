//! 静态资源目录配置（build.rs 里用）+ 编译期处理（遍历、打包、gzip、ETag）+ 生成挂载表源码。

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::html_include;

/// 是否把静态资源打包进二进制。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Embed {
    /// 始终打包
    Always,
    /// 始终运行时读磁盘
    Never,
    /// release 编译打包，debug 编译读磁盘（改资源刷新就生效）。默认值
    Release,
}

/// 一个静态资源目录的配置，交给 [`crate::Scanner::static_dir`]。
///
/// ```ignore
/// StaticDir::new("/static", manifest_dir.join("static"))
///     .embed(Embed::Release)
///     .index("index.html")
///     .spa_fallback(false)
///     .cache_control("public, max-age=3600")
///     .gzip(true)
/// ```
#[derive(Debug, Clone)]
pub struct StaticDir {
    prefix: String,
    dir: PathBuf,
    embed: Embed,
    index: Option<String>,
    spa_fallback: bool,
    cache_control: Option<String>,
    gzip: bool,
    gzip_prewarm: bool,
    html_include: bool,
}

impl StaticDir {
    /// `prefix` 是 URL 前缀（例如 `/static`，`/` 表示根路径），`dir` 是资源目录（建议用绝对路径，
    /// 例如 `CARGO_MANIFEST_DIR` 拼出来的路径；相对路径按 `CARGO_MANIFEST_DIR` 解析）。
    pub fn new(prefix: &str, dir: impl Into<PathBuf>) -> Self {
        let trimmed = prefix.trim_matches('/');
        Self {
            prefix: if trimmed.is_empty() { "/".to_string() } else { format!("/{trimmed}") },
            dir: dir.into(),
            embed: Embed::Release,
            index: Some("index.html".to_string()),
            spa_fallback: false,
            cache_control: None,
            gzip: false,
            gzip_prewarm: false,
            html_include: false,
        }
    }

    pub fn embed(mut self, embed: Embed) -> Self {
        self.embed = embed;
        self
    }

    /// 访问目录（以 `/` 结尾的路径）时返回的文件，默认 `index.html`。
    pub fn index(mut self, file_name: &str) -> Self {
        self.index = Some(file_name.to_string());
        self
    }

    /// 访问目录时不返回任何 index 文件（404）。
    pub fn no_index(mut self) -> Self {
        self.index = None;
        self
    }

    /// 找不到文件、且路径最后一段没有扩展名时，返回根目录的 index 文件（Vue / React 的 history 路由）。
    pub fn spa_fallback(mut self, enabled: bool) -> Self {
        self.spa_fallback = enabled;
        self
    }

    /// 所有文件统一加的 `Cache-Control` 响应头。
    pub fn cache_control(mut self, value: &str) -> Self {
        self.cache_control = Some(value.to_string());
        self
    }

    /// 开启 gzip：只压缩文本类资源（html / css / js / json / svg ...），小于 1KB 或压缩收益不足 10% 的不压缩。
    /// - 打包模式：编译期压缩，二进制里只存压缩版，始终以 gzip 返回；
    /// - 磁盘模式：客户端支持 gzip 时，优先返回旁边现成的 `xxx.gz`，否则首次访问时压缩并缓存到磁盘。
    pub fn gzip(mut self, enabled: bool) -> Self {
        self.gzip = enabled;
        self
    }

    /// 磁盘模式 + gzip：收到第一个请求时在后台把所有可压缩文件预先压缩进缓存。
    pub fn gzip_prewarm(mut self, enabled: bool) -> Self {
        self.gzip_prewarm = enabled;
        self
    }

    /// html 模板合并：页面里的 `{{template include/head}}` 替换成 `include/head.tpl.html` 的内容（可嵌套）；
    /// 不以 `/` 开头的路径相对当前文件所在目录，以 `/` 开头的相对资源根目录。
    /// 合并之后的页面按普通静态文件处理（ETag / gzip / 304 照常）；`.tpl.html` 模板片段不对外提供。
    /// 打包模式在编译期合并（模板不存在或循环引用时编译报错）；磁盘模式在请求时合并并缓存，模板变化后自动更新。
    pub fn html_include(mut self, enabled: bool) -> Self {
        self.html_include = enabled;
        self
    }
}

/// 编译期处理后的挂载点，用于生成源码。
#[derive(Debug, Clone)]
pub(crate) struct StaticInfo {
    prefix: String,
    source: SourceInfo,
    index: Option<String>,
    spa_fallback: bool,
    cache_control: Option<String>,
    gzip: bool,
    gzip_prewarm: bool,
    html_include: bool,
}

#[derive(Debug, Clone)]
enum SourceInfo {
    Embedded(Vec<EmbeddedEntry>),
    Disk { default_root: String },
}

#[derive(Debug, Clone)]
struct EmbeddedEntry {
    /// 相对路径，`/` 分隔
    path: String,
    /// `include_bytes!` 的绝对路径（原文件，或 OUT_DIR 里的 .gz）
    include: String,
    gzip: bool,
    etag: String,
}

/// 打包资源超过这个总大小时给出编译警告。
const EMBED_WARN_BYTES: u64 = 50 * 1024 * 1024;

/// 和 hyperoute-core 的 `is_compressible` 保持一致。
fn is_compressible(path: &str) -> bool {
    let ext = path.rsplit('/').next().and_then(|name| name.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some("html" | "htm" | "css" | "js" | "mjs" | "json" | "map" | "txt" | "csv" | "md" | "xml" | "svg" | "wasm" | "ttf" | "otf" | "eot")
    )
}

const GZIP_MIN_SIZE: usize = 1024;

pub(crate) fn prepare(dirs: &[StaticDir]) -> Vec<StaticInfo> {
    let mut seen = std::collections::HashSet::new();
    let mut infos: Vec<StaticInfo> = dirs
        .iter()
        .enumerate()
        .map(|(mount_index, dir)| {
            if !seen.insert(dir.prefix.clone()) {
                panic!("静态资源前缀 {} 重复配置", dir.prefix);
            }
            prepare_one(mount_index, dir)
        })
        .collect();
    // 前缀段数多的排前面（运行时还会做最长前缀匹配，这里只是让生成的表更直观）
    infos.sort_by_key(|info| std::cmp::Reverse(info.prefix.split('/').filter(|s| !s.is_empty()).count()));
    infos
}

fn prepare_one(mount_index: usize, config: &StaticDir) -> StaticInfo {
    let dir = absolute(&config.dir);
    let embed = match config.embed {
        Embed::Always => true,
        Embed::Never => false,
        Embed::Release => std::env::var("PROFILE").as_deref() == Ok("release"),
    };

    let source = if embed {
        if !dir.is_dir() {
            panic!("静态资源目录不存在：{}（{} 配置为打包进二进制）", dir.display(), config.prefix);
        }
        // 资源变化时重新编译（cargo 会递归检查目录下的所有文件）
        println!("cargo:rerun-if-changed={}", dir.display());
        SourceInfo::Embedded(embed_files(mount_index, &config.prefix, &dir, config.gzip, config.html_include))
    } else {
        SourceInfo::Disk {
            default_root: dir.to_string_lossy().into_owned(),
        }
    };

    StaticInfo {
        prefix: config.prefix.clone(),
        source,
        index: config.index.clone(),
        spa_fallback: config.spa_fallback,
        cache_control: config.cache_control.clone(),
        gzip: config.gzip,
        gzip_prewarm: config.gzip_prewarm,
        html_include: config.html_include,
    }
}

fn absolute(dir: &Path) -> PathBuf {
    let dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        let base = std::env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap());
        base.join(dir)
    };
    dir.canonicalize().unwrap_or(dir)
}

/// 递归收集文件（跳过以 `.` 开头的隐藏文件 / 目录），按相对路径字节序排序。
fn collect_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("读取静态资源目录失败 {}: {e}", dir.display()));
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let rel_path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            match fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => stack.push((path, rel_path)),
                Ok(meta) if meta.is_file() => files.push((rel_path, path)),
                _ => {}
            }
        }
    }
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    files
}

fn embed_files(mount_index: usize, prefix: &str, root: &Path, gzip: bool, html_include: bool) -> Vec<EmbeddedEntry> {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("打包静态资源需要在 build.rs 里调用（缺少 OUT_DIR）"))
        .join("hyperoute_static")
        .join(mount_index.to_string());
    fs::create_dir_all(&out_dir).expect("创建静态资源输出目录失败");

    let mut total: u64 = 0;
    let entries = collect_files(root)
        .into_iter()
        // 模板片段只用于合并，不打包、不对外提供
        .filter(|(rel, _)| !(html_include && html_include::is_template(rel)))
        .enumerate()
        .map(|(file_index, (rel, path))| {
            let mut include = path.to_string_lossy().into_owned();
            // 先合并模板，合并结果写到 OUT_DIR，之后按普通文件处理（ETag / gzip 都基于合并后的内容）
            let raw = if html_include && html_include::is_html_page(&rel) {
                let merged = html_include::merge(root, &path)
                    .unwrap_or_else(|e| panic!("静态资源 {prefix} 模板合并失败：{e}"));
                for dependency in &merged.dependencies {
                    println!("cargo:rerun-if-changed={}", dependency.display());
                }
                if merged.changed {
                    let merged_path = out_dir.join(format!("{file_index}.merged.html"));
                    fs::write(&merged_path, &merged.content).expect("写入合并后的 html 失败");
                    include = merged_path.to_string_lossy().into_owned();
                }
                merged.content.into_bytes()
            } else {
                fs::read(&path).unwrap_or_else(|e| panic!("读取静态资源失败 {}: {e}", path.display()))
            };
            let hash = fnv1a(&raw);
            let mut entry = EmbeddedEntry {
                path: rel.clone(),
                include,
                gzip: false,
                etag: format!("\"{hash:016x}\""),
            };
            if gzip && is_compressible(&rel) && raw.len() >= GZIP_MIN_SIZE {
                let compressed = gzip_best(&raw);
                if compressed.len() * 10 < raw.len() * 9 {
                    let gz_path = out_dir.join(format!("{file_index}.gz"));
                    fs::write(&gz_path, &compressed).expect("写入压缩后的静态资源失败");
                    entry.include = gz_path.to_string_lossy().into_owned();
                    entry.gzip = true;
                    entry.etag = format!("\"{hash:016x}-gz\"");
                    total += compressed.len() as u64;
                    return entry;
                }
            }
            total += raw.len() as u64;
            entry
        })
        .collect();

    if total > EMBED_WARN_BYTES {
        println!(
            "cargo:warning=静态资源 {prefix} 打包进二进制的总大小 {:.1} MB，资源较大时建议使用 Embed::Never（运行时读磁盘）",
            total as f64 / 1024.0 / 1024.0
        );
    }
    entries
}

fn gzip_best(data: &[u8]) -> Vec<u8> {
    // GzBuilder 默认 mtime = 0，保证同样的输入每次编译产出相同的字节（可复现构建）
    let mut encoder = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::best());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// 生成挂载表源码：`static __STATIC_MOUNTS: &[&hyperoute_core::StaticMount]`。
pub(crate) fn render(infos: &[StaticInfo]) -> String {
    let mut out = String::new();
    let mut mount_names = Vec::new();

    for (i, info) in infos.iter().enumerate() {
        let source = match &info.source {
            SourceInfo::Embedded(entries) => {
                let _ = writeln!(out, "static __STATIC_FILES_{i}: &[hyperoute_core::EmbeddedFile] = &[");
                for entry in entries {
                    let _ = writeln!(
                        out,
                        "    hyperoute_core::EmbeddedFile {{ path: {:?}, data: include_bytes!({:?}), gzip: {}, etag: {:?} }},",
                        entry.path, entry.include, entry.gzip, entry.etag
                    );
                }
                let _ = writeln!(out, "];");
                format!("hyperoute_core::StaticSource::Embedded(__STATIC_FILES_{i})")
            }
            SourceInfo::Disk { default_root } => {
                format!("hyperoute_core::StaticSource::Disk {{ default_root: {default_root:?} }}")
            }
        };
        let _ = writeln!(
            out,
            "static __STATIC_MOUNT_{i}: hyperoute_core::StaticMount = hyperoute_core::StaticMount {{ prefix: {:?}, source: {source}, index: {}, spa_fallback: {}, cache_control: {}, gzip: {}, gzip_prewarm: {}, html_include: {} }};",
            info.prefix,
            option_literal(info.index.as_deref()),
            info.spa_fallback,
            option_literal(info.cache_control.as_deref()),
            info.gzip,
            info.gzip_prewarm,
            info.html_include,
        );
        mount_names.push(format!("&__STATIC_MOUNT_{i}"));
    }

    let _ = writeln!(
        out,
        "static __STATIC_MOUNTS: &[&hyperoute_core::StaticMount] = &[{}];",
        mount_names.join(", ")
    );
    out
}

fn option_literal(value: Option<&str>) -> String {
    match value {
        Some(v) => format!("Some({v:?})"),
        None => "None".to_string(),
    }
}
