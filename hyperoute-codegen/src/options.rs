//! Scanner 上和参数绑定有关的开关。

use crate::naming::ParamNaming;

/// 标量参数（`id: i64`、`Option<T>`、`Vec<T>`）和表单结构体参数从哪里取值。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParamBinding {
    /// 默认：标量参数只从查询串取值；请求体结构体按 content-type 解析请求体。
    #[default]
    Separate,
    /// 和 Go 的 `request.Form` / Spring 一致：查询串和表单请求体合并后取值。
    /// urlencoded 请求体的值在前、查询串在后；multipart 是查询串在前、文本字段在后。
    /// 标量取第一个值，`Vec` 取全部值，并兼容 `"[a,b,c]"` 写法（每项里的 `_2c` 还原成 `,`）；
    /// 表单结构体参数也从合并后的表单取值（GET 请求的 query 也能绑定到结构体），JSON 请求体照旧按 JSON 解析。
    Merged,
}

/// 标量参数和表单结构体字段缺失或转换失败时怎么办。handler 上的 `#[PARAM_STRICT]` / `#[PARAM_LENIENT]` 可以覆盖。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParamFallback {
    /// 默认：返回参数错误（400，或 `#[PARAM_ERROR]` 自定义的响应）。
    #[default]
    Strict,
    /// 和 Go 一致：得到 `Default::default()`（`Option` 得到 `None`）；debug 构建里记一条日志。
    /// 路径参数、文件字段、JSON 请求体、`Query<T>` 不受影响。
    Default,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ScanOptions {
    pub naming: ParamNaming,
    pub binding: ParamBinding,
    pub fallback: ParamFallback,
}
