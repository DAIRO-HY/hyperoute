//! 参数来源：路径段、查询串、请求体（JSON / urlencoded / multipart）。
//! 解析失败时返回 400，格式由 `param_error.rs` 里的 `#[PARAM_ERROR]` 函数决定。

use std::path::Path;

use hyperoute_core::MultipartFile;
use hyperoute_macro::{GET, POST};
use serde::{Deserialize, Serialize};

/// 参数名和路径里的 `{id}` 同名 → 取路径段。
/// curl http://127.0.0.1:3000/params/path/42
#[GET("/params/path/{id}")]
pub async fn path_param(id: i64) -> String {
    format!("id = {id}")
}

/// 标量类型（数字 / bool / char / String / &str）且不是路径参数 → 取查询串。
/// curl "http://127.0.0.1:3000/params/query?id=1&name=alice"
#[GET("/params/query")]
pub async fn query_params(id: i64, name: String) -> String {
    format!("id = {id}, name = {name}")
}

/// 其它类型 → 请求体，按 content-type 选择 JSON / urlencoded / multipart。
/// curl -X POST -H 'content-type: application/json' -d '{"id":1,"name":"alice","age":30}' http://127.0.0.1:3000/params/form
/// curl -X POST -d 'id=1&name=alice&age=30' http://127.0.0.1:3000/params/form
/// curl -F id=1 -F name=alice -F age=30 http://127.0.0.1:3000/params/form
#[POST("/params/form")]
pub async fn submit_form(form: UserForm) -> String {
    format!("form = {form:?}")
}

/// 含文件字段的结构体只支持 multipart/form-data。
/// curl -F title=report -F raw_file=@Cargo.toml -F temp_file=@Cargo.toml -F detailed_file=@Cargo.toml http://127.0.0.1:3000/params/upload
#[POST("/params/upload")]
pub async fn upload(form: UploadForm<'_>) -> String {
    let temp_file_size = std::fs::metadata(form.temp_file).map(|m| m.len()).unwrap_or(0);
    format!(
        "title = {:?}, raw_file = {} bytes, temp_file = {:?} ({temp_file_size} bytes), detailed_file = {:?} ({:?}, {} bytes)",
        form.title,
        form.raw_file.len(),
        form.temp_file,
        form.detailed_file.name,
        form.detailed_file.content_type,
        form.detailed_file.body.len(),
    )
}

// 请求体结构体要和使用它的 handler 定义在同一个文件里（可以嵌套在 mod 里），
// 代码生成时才能看到它的字段，从而支持 multipart/form-data。

#[derive(Debug, Serialize, Deserialize)]
pub struct UserForm {
    pub id: i64,
    pub name: String,
    pub age: u8,
}

/// 只走 multipart/form-data，不需要 Serialize / Deserialize：
/// 每个字段怎么取值由代码生成按字段类型决定，不经过 serde。
#[derive(Debug)]
pub struct UploadForm<'a> {
    /// 普通文本字段
    pub title: String,
    /// `Vec<u8>`：文件内容直接读进内存
    pub raw_file: Vec<u8>,
    /// `&Path`：文件内容写到临时文件，这里拿到临时文件路径（框架不会自动删除）
    pub temp_file: &'a Path,
    /// `MultipartFile<T>`：同时需要文件名和 content-type 时使用；T 可以是 `Vec<u8>` 或 `&Path`
    pub detailed_file: MultipartFile<Vec<u8>>,
}
