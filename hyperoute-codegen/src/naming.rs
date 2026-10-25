//! 参数命名：前端参数名（例如驼峰 `pageSize`）和 Rust 名字（`page_size`）的对应关系，全部在编译期算好。
//!
//! - 标量查询参数：按 [`ParamNaming`] 开关决定接受哪些名字；
//! - multipart 结构体字段：结构体带 serde 改名属性（`rename_all` / `rename` / `alias`）时按 serde 的规则，
//!   保证和 JSON / urlencoded 一致；否则按 [`ParamNaming`] 开关。
//!
//! 生成的代码里直接写死要查找的名字，运行时不做任何字符串转换。

/// 标量查询参数和 multipart 字段（没有 serde 改名属性时）接受哪些前端参数名。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParamNaming {
    /// 只接受和 Rust 名字完全相同的参数名（默认）
    #[default]
    Exact,
    /// 同时接受原名和驼峰名（`page_size` 也接受 `pageSize`），两者都出现时原名优先
    CamelCase,
}

/// 一个参数 / 字段在请求里的名字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireName {
    /// 按优先级排列的候选名
    pub lookup: Vec<String>,
    /// 错误信息里给前端看的名字
    pub display: String,
}

impl WireName {
    fn exact(name: &str) -> Self {
        Self { lookup: vec![name.to_string()], display: name.to_string() }
    }

    /// 是否就是 Rust 名字本身（生成代码时可以沿用原来的单名字版本，和不开启时完全一样）
    pub fn is_plain(&self, rust_name: &str) -> bool {
        self.lookup.len() == 1 && self.lookup[0] == rust_name && self.display == rust_name
    }

    /// 渲染成源码里的 `&["a", "b"]`
    pub fn lookup_literal(&self) -> String {
        let items = self.lookup.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(", ");
        format!("&[{items}]")
    }
}

/// 按开关得到标量参数（或没有 serde 改名属性的 multipart 字段）的名字。
pub(crate) fn for_param(rust_name: &str, naming: ParamNaming) -> WireName {
    match naming {
        ParamNaming::Exact => WireName::exact(rust_name),
        ParamNaming::CamelCase => {
            let camel = param_camel_case(rust_name);
            if camel == rust_name {
                // 没有下划线的名字驼峰写法和原名相同：只查一次，和不开启时一样
                WireName::exact(rust_name)
            } else {
                WireName { lookup: vec![rust_name.to_string(), camel.clone()], display: camel }
            }
        }
    }
}

/// `ParamNaming::CamelCase` 的驼峰名：`page_size` → `pageSize`，前导下划线保留（`_client_flag` → `_clientFlag`）。
/// 和 serde 的 `rename_all = "camelCase"` 不同（serde 会去掉前导下划线）；和 hyperoute-core 的 `param_camel_case` 一致。
pub(crate) fn param_camel_case(name: &str) -> String {
    let rest = name.trim_start_matches('_');
    let mut out = String::with_capacity(name.len());
    out.push_str(&name[..name.len() - rest.len()]);
    let mut capitalize = false;
    for ch in rest.chars() {
        if ch == '_' {
            capitalize = true;
        } else if capitalize {
            out.extend(ch.to_uppercase());
            capitalize = false;
        } else {
            out.push(ch);
        }
    }
    out
}

/// serde `rename_all` 对字段名的转换（和 serde_derive 的规则一致）；不认识的取值返回 None。
pub(crate) fn rename_case(case: &str, field: &str) -> Option<String> {
    let pascal = || {
        let mut out = String::new();
        let mut capitalize = true;
        for ch in field.chars() {
            if ch == '_' {
                capitalize = true;
            } else if capitalize {
                out.extend(ch.to_uppercase());
                capitalize = false;
            } else {
                out.push(ch);
            }
        }
        out
    };
    Some(match case {
        "lowercase" => field.to_ascii_lowercase(),
        "UPPERCASE" => field.to_ascii_uppercase(),
        "PascalCase" => pascal(),
        "camelCase" => {
            let pascal = pascal();
            let mut chars = pascal.chars();
            match chars.next() {
                Some(first) => first.to_lowercase().chain(chars).collect(),
                None => String::new(),
            }
        }
        "snake_case" => field.to_string(),
        "SCREAMING_SNAKE_CASE" => field.to_ascii_uppercase(),
        "kebab-case" => field.replace('_', "-"),
        "SCREAMING-KEBAB-CASE" => field.to_ascii_uppercase().replace('_', "-"),
        _ => return None,
    })
}

/// 计算 multipart 结构体每个具名字段的名字（顺序与字段声明顺序一致）。
/// `struct_path` 只用于报错信息。
pub(crate) fn struct_field_names(item: &syn::ItemStruct, naming: ParamNaming, struct_path: &str) -> Vec<WireName> {
    let syn::Fields::Named(fields) = &item.fields else {
        return Vec::new();
    };
    let struct_attrs = serde_attrs(&item.attrs, struct_path);
    let field_attrs: Vec<SerdeAttrs> = fields
        .named
        .iter()
        .map(|f| serde_attrs(&f.attrs, struct_path))
        .collect();
    let serde_named = struct_attrs.rename_all.is_some()
        || field_attrs.iter().any(|a| a.rename.is_some() || !a.aliases.is_empty());

    fields
        .named
        .iter()
        .zip(field_attrs)
        .map(|(field, attrs)| {
            let rust_name = field.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
            if !serde_named {
                return for_param(&rust_name, naming);
            }
            // 和 serde 一致：字段 rename 优先，其次 rename_all，最后原名；alias 作为额外候选
            let primary = match (&attrs.rename, &struct_attrs.rename_all) {
                (Some(rename), _) => rename.clone(),
                (None, Some(case)) => rename_case(case, &rust_name)
                    .unwrap_or_else(|| panic!("{struct_path}: 不支持的 serde rename_all 取值 {case:?}")),
                (None, None) => rust_name.clone(),
            };
            let mut lookup = vec![primary.clone()];
            for alias in attrs.aliases {
                if !lookup.contains(&alias) {
                    lookup.push(alias);
                }
            }
            WireName { lookup, display: primary }
        })
        .collect()
}

#[derive(Default)]
struct SerdeAttrs {
    rename_all: Option<String>,
    rename: Option<String>,
    aliases: Vec<String>,
}

/// 读取 `#[serde(...)]` 里和命名有关的部分：`rename_all`、`rename`（含 `(deserialize = "..")` 形式）、`alias`。
/// 其它 serde 参数原样跳过。
fn serde_attrs(attrs: &[syn::Attribute], struct_path: &str) -> SerdeAttrs {
    let mut out = SerdeAttrs::default();
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                if let Some(value) = deserialize_value(&meta)? {
                    out.rename_all = Some(value);
                }
            } else if meta.path.is_ident("rename") {
                if let Some(value) = deserialize_value(&meta)? {
                    out.rename = Some(value);
                }
            } else if meta.path.is_ident("alias") {
                out.aliases.push(meta.value()?.parse::<syn::LitStr>()?.value());
            } else {
                skip_meta(&meta)?;
            }
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{struct_path}: 无法解析 #[serde(...)] 属性：{e}"));
    }
    out
}

/// `key = "v"` 返回 v；`key(deserialize = "v", serialize = "..")` 返回 deserialize 的 v。
fn deserialize_value(meta: &syn::meta::ParseNestedMeta) -> syn::Result<Option<String>> {
    if meta.input.peek(syn::Token![=]) {
        return Ok(Some(meta.value()?.parse::<syn::LitStr>()?.value()));
    }
    let mut found = None;
    meta.parse_nested_meta(|inner| {
        if inner.path.is_ident("deserialize") {
            found = Some(inner.value()?.parse::<syn::LitStr>()?.value());
        } else {
            skip_meta(&inner)?;
        }
        Ok(())
    })?;
    Ok(found)
}

/// 跳过不关心的 serde 参数（`default`、`deny_unknown_fields`、`with = "..."`、`bound(...)` 等）。
fn skip_meta(meta: &syn::meta::ParseNestedMeta) -> syn::Result<()> {
    if meta.input.peek(syn::Token![=]) {
        meta.value()?.parse::<syn::Expr>()?;
    } else if meta.input.peek(syn::token::Paren) {
        meta.parse_nested_meta(|inner| skip_meta(&inner))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_names() {
        assert_eq!(for_param("page_size", ParamNaming::Exact), WireName::exact("page_size"));
        let camel = for_param("page_size", ParamNaming::CamelCase);
        assert_eq!(camel.lookup, ["page_size", "pageSize"]);
        assert_eq!(camel.display, "pageSize");
        assert!(for_param("id", ParamNaming::CamelCase).is_plain("id"));
        assert_eq!(for_param("user_id2", ParamNaming::CamelCase).display, "userId2");
        // 前导下划线保留（DFS 的 `_clientFlag`）
        assert_eq!(for_param("_client_flag", ParamNaming::CamelCase).lookup, ["_client_flag", "_clientFlag"]);
        assert!(for_param("_token", ParamNaming::CamelCase).is_plain("_token"));
    }

    #[test]
    fn rename_all_cases_match_serde() {
        let f = "user_full_name";
        assert_eq!(rename_case("camelCase", f).unwrap(), "userFullName");
        assert_eq!(rename_case("PascalCase", f).unwrap(), "UserFullName");
        assert_eq!(rename_case("kebab-case", f).unwrap(), "user-full-name");
        assert_eq!(rename_case("SCREAMING_SNAKE_CASE", f).unwrap(), "USER_FULL_NAME");
        assert_eq!(rename_case("SCREAMING-KEBAB-CASE", f).unwrap(), "USER-FULL-NAME");
        assert_eq!(rename_case("lowercase", f).unwrap(), "user_full_name");
        assert_eq!(rename_case("UPPERCASE", f).unwrap(), "USER_FULL_NAME");
        assert_eq!(rename_case("snake_case", f).unwrap(), "user_full_name");
        assert!(rename_case("camelcase", f).is_none());
    }

    fn names(src: &str, naming: ParamNaming) -> Vec<(Vec<String>, String)> {
        let item: syn::ItemStruct = syn::parse_str(src).unwrap();
        struct_field_names(&item, naming, "T").into_iter().map(|w| (w.lookup, w.display)).collect()
    }

    #[test]
    fn struct_fields_follow_serde_first() {
        // 有 serde 改名属性：按 serde，不受开关影响
        let got = names(
            r#"#[derive(Deserialize)] #[serde(rename_all = "camelCase", deny_unknown_fields)]
               struct T { user_id: i64, #[serde(rename = "nick", default)] nick_name: String, #[serde(alias = "pwd")] pass_word: String }"#,
            ParamNaming::Exact,
        );
        assert_eq!(got[0], (vec!["userId".to_string()], "userId".to_string()));
        assert_eq!(got[1], (vec!["nick".to_string()], "nick".to_string()));
        assert_eq!(got[2], (vec!["passWord".to_string(), "pwd".to_string()], "passWord".to_string()));

        // rename(deserialize = ..) 形式；只有字段级 rename 时其它字段保持原名（和 serde 一致）
        let got = names(
            r#"struct T { #[serde(rename(serialize = "a", deserialize = "b"))] x_y: i64, page_size: u32 }"#,
            ParamNaming::CamelCase,
        );
        assert_eq!(got[0].0, ["b"]);
        assert_eq!(got[1].0, ["page_size"]);

        // 没有 serde 改名属性：按开关
        let got = names("struct T { page_size: u32, title: String }", ParamNaming::CamelCase);
        assert_eq!(got[0], (vec!["page_size".to_string(), "pageSize".to_string()], "pageSize".to_string()));
        assert_eq!(got[1], (vec!["title".to_string()], "title".to_string()));
    }
}
