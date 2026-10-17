//! html 模板合并：把 `{{template include/head}}` 替换成 `include/head.tpl.html` 的内容（可以嵌套）。
//!
//! 这是静态资源的前置处理：先合并，再按普通静态文件处理（ETag / gzip / 304 都照常）。
//! 打包模式在编译期调用（hyperoute-codegen），磁盘模式在运行时调用（hyperoute-core）。
//!
//! 注意：hyperoute-core/src/html_include.rs 与 hyperoute-codegen/src/html_include.rs 必须保持完全一致
//! （只依赖标准库，两个 crate 各放一份；hyperoute-core 的测试会检查两份是否相同）。

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 模板片段的文件后缀；这类文件只用于合并，不对外提供。
pub const TEMPLATE_SUFFIX: &str = ".tpl.html";

/// 最大嵌套层数（配合循环检测，正常不会触发）。
const MAX_DEPTH: usize = 16;

/// 合并结果。
#[derive(Debug)]
pub struct Merged {
    /// 合并后的内容；`changed` 为 false 时就是原文
    pub content: String,
    /// 是否有模板调用被替换
    pub changed: bool,
    /// 用到的所有模板文件（含间接引用，已去重），用于判断合并结果是否过期
    pub dependencies: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum IncludeError {
    /// 引用的模板不存在
    NotFound { chain: Vec<PathBuf>, name: String, resolved: PathBuf },
    /// 模板路径跳出了资源根目录
    OutsideRoot { chain: Vec<PathBuf>, name: String },
    /// 循环引用
    Cycle { chain: Vec<PathBuf> },
    /// 嵌套过深
    TooDeep { chain: Vec<PathBuf> },
    Io { path: PathBuf, error: io::Error },
}

impl fmt::Display for IncludeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let chain = |chain: &[PathBuf]| chain.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" -> ");
        match self {
            IncludeError::NotFound { chain: c, name, resolved } => {
                write!(f, "template `{name}` not found ({}), referenced by {}", resolved.display(), chain(c))
            }
            IncludeError::OutsideRoot { chain: c, name } => {
                write!(f, "template `{name}` is outside the static root, referenced by {}", chain(c))
            }
            IncludeError::Cycle { chain: c } => write!(f, "circular template include: {}", chain(c)),
            IncludeError::TooDeep { chain: c } => write!(f, "template include nested deeper than {MAX_DEPTH}: {}", chain(c)),
            IncludeError::Io { path, error } => write!(f, "failed to read {}: {error}", path.display()),
        }
    }
}

impl std::error::Error for IncludeError {}

/// 是否是需要做模板合并的页面（`.html` / `.htm`，模板片段本身除外）。
pub fn is_html_page(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    (lower.ends_with(".html") || lower.ends_with(".htm")) && !lower.ends_with(TEMPLATE_SUFFIX)
}

/// 是否是模板片段文件。
pub fn is_template(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(TEMPLATE_SUFFIX)
}

/// 合并 `file` 中的模板调用。`root` 是静态资源根目录：
/// - `{{template include/head}}`：相对当前文件所在目录；
/// - `{{template /include/head}}`：相对 `root`。
///
/// 模板文件必须位于 `root` 之内。
pub fn merge(root: &Path, file: &Path) -> Result<Merged, IncludeError> {
    let root = root.canonicalize().map_err(|error| IncludeError::Io { path: root.to_path_buf(), error })?;
    let file = file.canonicalize().map_err(|error| IncludeError::Io { path: file.to_path_buf(), error })?;
    let mut dependencies = Vec::new();
    let mut chain = vec![file.clone()];
    let (content, changed) = merge_file(&root, &file, &mut chain, &mut dependencies)?;
    Ok(Merged { content, changed, dependencies })
}

fn merge_file(
    root: &Path,
    file: &Path,
    chain: &mut Vec<PathBuf>,
    dependencies: &mut Vec<PathBuf>,
) -> Result<(String, bool), IncludeError> {
    if chain.len() > MAX_DEPTH {
        return Err(IncludeError::TooDeep { chain: chain.clone() });
    }
    let text = fs::read_to_string(file).map_err(|error| IncludeError::Io { path: file.to_path_buf(), error })?;
    let calls = find_calls(&text);
    if calls.is_empty() {
        return Ok((text, false));
    }

    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for call in calls {
        out.push_str(&text[last..call.start]);
        let resolved = resolve(root, file, &call.name);
        let template = match resolved.canonicalize() {
            Ok(path) => path,
            Err(_) => {
                return Err(IncludeError::NotFound { chain: chain.clone(), name: call.name, resolved });
            }
        };
        if !template.starts_with(root) {
            return Err(IncludeError::OutsideRoot { chain: chain.clone(), name: call.name });
        }
        if chain.contains(&template) {
            let mut cycle = chain.clone();
            cycle.push(template);
            return Err(IncludeError::Cycle { chain: cycle });
        }
        if !dependencies.contains(&template) {
            dependencies.push(template.clone());
        }
        chain.push(template.clone());
        let (content, _) = merge_file(root, &template, chain, dependencies)?;
        chain.pop();
        out.push_str(&content);
        last = call.end;
    }
    out.push_str(&text[last..]);
    Ok((out, true))
}

/// 模板名 → 模板文件路径（补上 `.tpl.html` 后缀）。
fn resolve(root: &Path, file: &Path, name: &str) -> PathBuf {
    let file_name = format!("{name}{TEMPLATE_SUFFIX}");
    match file_name.strip_prefix('/') {
        Some(from_root) => root.join(from_root),
        None => file.parent().unwrap_or(root).join(file_name),
    }
}

struct Call {
    start: usize,
    end: usize,
    name: String,
}

/// 找出所有 `{{ template NAME }}`（NAME 可以加单 / 双引号）。不符合这个格式的 `{{ ... }}` 原样保留，
/// 所以页面里 Vue / Mustache 的 `{{ value }}` 不受影响。
fn find_calls(text: &str) -> Vec<Call> {
    let bytes = text.as_bytes();
    let mut calls = Vec::new();
    let mut i = 0;
    while let Some(offset) = text[i..].find("{{") {
        let start = i + offset;
        match parse_call(text, start + 2) {
            Some((name, end)) => {
                calls.push(Call { start, end, name });
                i = end;
            }
            None => i = start + 2,
        }
        if i >= bytes.len() {
            break;
        }
    }
    calls
}

/// 从 `{{` 之后开始解析，成功返回 (模板名, 结束位置——`}}` 之后)。
fn parse_call(text: &str, mut i: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let skip_ws = |i: &mut usize| {
        while *i < bytes.len() && bytes[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };

    skip_ws(&mut i);
    let keyword = "template";
    if !text[i..].starts_with(keyword) {
        return None;
    }
    i += keyword.len();
    // 关键字后至少一个空白
    if i >= bytes.len() || !bytes[i].is_ascii_whitespace() {
        return None;
    }
    skip_ws(&mut i);

    let name = match bytes.get(i)? {
        quote @ (b'"' | b'\'') => {
            let close = text[i + 1..].find(*quote as char)? + i + 1;
            let name = &text[i + 1..close];
            i = close + 1;
            name
        }
        _ => {
            let begin = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'}' {
                i += 1;
            }
            &text[begin..i]
        }
    };
    if name.is_empty() || name.contains(['\n', '\r', '{', '}']) {
        return None;
    }

    skip_ws(&mut i);
    if !text[i..].starts_with("}}") {
        return None;
    }
    Some((name.to_string(), i + 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("hyperoute-include-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn parses_calls() {
        let names = |text: &str| find_calls(text).into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names("a{{template include/head}}b"), ["include/head"]);
        assert_eq!(names("{{  template   include/head  }}"), ["include/head"]);
        assert_eq!(names(r#"{{ template "x y/z" }} {{template 'q'}}"#), ["x y/z", "q"]);
        assert_eq!(names("{{template /include/head}}"), ["/include/head"]);
        // 不是模板调用的 {{ }} 原样保留
        assert!(names("{{ name }} {{templateX a}} {{template}} {{template a b}} {{template a").is_empty());
    }

    #[test]
    fn merges_relative_absolute_and_nested() {
        let root = temp_root("merge");
        write(&root, "include/head.tpl.html", "<meta>{{template inner}}");
        write(&root, "include/inner.tpl.html", "<inner $1 \\0>");
        write(&root, "index.html", "<head>{{template include/head}}</head>{{ vue }}");
        write(&root, "docs/page.html", "{{template /include/head}}|{{template ../include/inner}}");

        let merged = merge(&root, &root.join("index.html")).unwrap();
        assert!(merged.changed);
        assert_eq!(merged.content, "<head><meta><inner $1 \\0></head>{{ vue }}");
        assert_eq!(merged.dependencies.len(), 2);

        let merged = merge(&root, &root.join("docs/page.html")).unwrap();
        assert_eq!(merged.content, "<meta><inner $1 \\0>|<inner $1 \\0>");

        write(&root, "plain.html", "<p>no templates</p>");
        let merged = merge(&root, &root.join("plain.html")).unwrap();
        assert!(!merged.changed && merged.dependencies.is_empty());
    }

    #[test]
    fn reports_errors() {
        let root = temp_root("errors");
        write(&root, "missing.html", "{{template include/nope}}");
        assert!(matches!(merge(&root, &root.join("missing.html")), Err(IncludeError::NotFound { .. })));

        write(&root, "a.tpl.html", "{{template b}}");
        write(&root, "b.tpl.html", "{{template a}}");
        write(&root, "cycle.html", "{{template a}}");
        let error = merge(&root, &root.join("cycle.html")).unwrap_err();
        assert!(matches!(error, IncludeError::Cycle { .. }));
        assert!(error.to_string().contains("a.tpl.html -> "), "{error}");

        let outside = temp_root("errors-outside");
        write(&outside, "secret.tpl.html", "secret");
        let rel = format!("../{}/secret", outside.file_name().unwrap().to_string_lossy());
        write(&root, "escape.html", &format!("{{{{template {rel}}}}}"));
        assert!(matches!(merge(&root, &root.join("escape.html")), Err(IncludeError::OutsideRoot { .. })));
    }

    #[test]
    fn classifies_files() {
        assert!(is_html_page("a/index.html") && is_html_page("A.HTM"));
        assert!(!is_html_page("include/head.tpl.html") && !is_html_page("app.js"));
        assert!(is_template("include/HEAD.TPL.HTML"));
    }
}
