//! 参数来源：路径段、查询串、请求体（JSON / urlencoded / multipart）。
//! 解析失败时返回 400，格式由 `param_error.rs` 里的 `#[PARAM_ERROR]` 函数决定。

use std::path::Path;

use hyperoute_core::{MultipartFile, Query};
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

/// build.rs 里开启了 `ParamNaming::CamelCase`：snake_case 参数名同时接受前端的驼峰写法（原名优先），
/// 驼峰名在编译期算好，运行时没有额外的字符串转换。
/// curl "http://127.0.0.1:3000/params/naming?pageSize=20&sortBy=name"
/// curl "http://127.0.0.1:3000/params/naming?page_size=20&sort_by=name"
#[GET("/params/naming")]
pub async fn naming(page_size: u32, sort_by: String) -> String {
    format!("page_size = {page_size}, sort_by = {sort_by}")
}

/// `Query<T>` → 整个查询串解析到结构体（不论请求方法），可以和标量查询参数同时使用：
/// 这里 `id` 和 `info` 都从同一个查询串取值。可选参数用 `Option`。
/// curl "http://127.0.0.1:3000/params/query_struct?id=10&name=AA&age=10"
/// curl "http://127.0.0.1:3000/params/query_struct?id=10&name=AA&age=10&nickname=bob"
#[GET("/params/query_struct")]
pub async fn query_struct(id: i64, info: Query<UserInfo>) -> String {
    // Query<T> 实现了 Deref，可以直接访问字段
    format!("id = {id}, name = {}, age = {}, nickname = {:?}", info.name, info.age, info.nickname)
}

/// POST 请求同样可以用 `Query<T>` 接收查询串，同时用另一个参数接收请求体。
/// curl -X POST -d 'id=1&name=alice&age=30' "http://127.0.0.1:3000/params/form_with_query?name=AA&age=10"
#[POST("/params/form_with_query")]
pub async fn form_with_query(info: Query<UserInfo>, form: UserForm) -> String {
    format!("query = {:?}, form = {form:?}", *info)
}

/// 其它类型 → 请求体，按 content-type 选择 JSON / urlencoded / multipart。
/// curl -X POST -H 'content-type: application/json' -d '{"id":1,"name":"alice","age":30}' http://127.0.0.1:3000/params/form
/// curl -X POST -d 'id=1&name=alice&age=30' http://127.0.0.1:3000/params/form
/// curl -F id=1 -F name=alice -F age=30 http://127.0.0.1:3000/params/form
#[POST("/params/form")]
pub async fn submit_form(form: UserForm) -> String {
    format!("form = {form:?}")
}

/// 结构体带 serde 改名属性时，JSON / urlencoded / multipart 都按 serde 的规则取字段名（这里是驼峰），
/// 不受 build.rs 里 `ParamNaming` 开关的影响，保证同一个结构体在各种 content-type 下一致。
/// curl -F userId=1 -F nickName=alice http://127.0.0.1:3000/params/profile
/// curl -X POST -H 'content-type: application/json' -d '{"userId":1,"nickName":"alice"}' http://127.0.0.1:3000/params/profile
#[POST("/params/profile")]
pub async fn profile(form: ProfileForm) -> String {
    format!("user_id = {}, nick_name = {}", form.user_id, form.nick_name)
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

/// `Query<UserInfo>` 用的结构体，需要实现 `Deserialize`。
#[derive(Debug, Deserialize)]
pub struct UserInfo {
    pub name: String,
    pub age: u8,
    pub nickname: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserForm {
    pub id: i64,
    pub name: String,
    pub age: u8,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileForm {
    pub user_id: i64,
    pub nick_name: String,
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
