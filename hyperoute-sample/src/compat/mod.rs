//! 兼容 Go 版 AutoWeb 约定的接口。build.rs 用另一个 Scanner 扫描这个目录，开启了：
//! - `ParamBinding::Merged`：查询串和表单请求体合并取值（和 Go 的 `request.Form` 一致）；
//! - `ParamFallback::Default`：参数缺失或转换失败时用默认值；
//! - `ParamNaming::CamelCase`：`_client_flag` 也接受 `_clientFlag`。
pub mod dfs;
