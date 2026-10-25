//! 合并模式 + 宽松模式的示例（开关见 compat/mod.rs）。

use std::path::Path;

use hyperoute_core::MultipartFile;
use hyperoute_macro::{GET, PARAM_STRICT, POST, REQUEST};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct Property {
    pub paths: Vec<String>,
    pub client_flag: i32,
    pub version: Option<i32>,
}

/// 标量 / `Option` / `Vec` 参数从合并后的表单取值，GET 的查询串、POST 的 urlencoded / multipart 都可以。
/// `Vec` 取全部值，兼容同名 key 重复和 `"[a,b]"` 写法（`_2c` 还原成逗号）；`_client_flag` 接受 `_clientFlag`。
/// curl "http://127.0.0.1:3000/compat/files/get_property?paths=[/a,/b_2cc]&_clientFlag=1"
/// urlencoded 请求体的值排在查询串前面，这里得到 ["/a", "/b", "/q"]：
/// curl -d 'paths=/a&paths=/b&_version=2' "http://127.0.0.1:3000/compat/files/get_property?paths=/q"
/// 宽松模式：`_clientFlag=x` 转换失败得到 0，`_version` 没传得到 None：
/// curl "http://127.0.0.1:3000/compat/files/get_property?_clientFlag=x"
#[REQUEST("/compat/files/get_property")]
pub async fn get_property(paths: Vec<String>, _client_flag: i32, _version: Option<i32>) -> Property {
    Property { paths, client_flag: _client_flag, version: _version }
}

#[derive(Serialize)]
pub struct Uploaded {
    pub folder: String,
    pub content_type: String,
    pub file_name: String,
    pub size: u64,
    pub temp_path: String,
}

/// 标量参数和文件参数写在同一个 handler 里，请求体只读一次（multipart：查询串在前、文本字段在后，
/// 文件字段排在文本字段前面也没关系）。
/// multipart 边收边解析：文件超过 `HYPEROUTE_MULTIPART_MEMORY_LIMIT`（默认 1MB）就转存到临时文件，
/// 几 GB 的上传也只占用有限的内存。临时文件在请求处理完后自动删除，要保留就在 handler 里 rename / copy 走。
/// curl -F file=@Cargo.toml -F folder=/docs -F contentType=text/plain http://127.0.0.1:3000/compat/upload
#[POST("/compat/upload")]
pub async fn upload(folder: String, content_type: String, file: MultipartFile<&Path>) -> Uploaded {
    let size = std::fs::metadata(file.body).map(|m| m.len()).unwrap_or(0);
    let temp_path = file.body.to_string_lossy().into_owned();
    Uploaded { folder, content_type, file_name: file.name, size, temp_path }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SaveForm {
    pub name: String,
    pub age: u8,
    pub tags: Vec<String>,
    pub remark: Option<String>,
}

/// 表单结构体也从合并后的表单按字段名取值：GET 的查询串也能绑定到结构体；JSON 请求体照旧按 JSON 解析。
/// 宽松模式：`age=x` 转换失败得到 0，缺少的 `name` 得到空字符串。
/// curl "http://127.0.0.1:3000/compat/user/save?name=alice&age=20&tags=[a,b]"
/// curl -d 'age=x&tags=a&tags=b' http://127.0.0.1:3000/compat/user/save
/// curl -H 'content-type: application/json' -d '{"name":"bob","age":3,"tags":[]}' http://127.0.0.1:3000/compat/user/save
#[REQUEST("/compat/user/save")]
pub async fn save(form: SaveForm) -> SaveForm {
    form
}

/// `#[PARAM_STRICT]` 覆盖全局的宽松模式：新写的接口仍然可以严格校验。
/// curl -i http://127.0.0.1:3000/compat/strict        → 400 Parameter "id" is required.
/// curl -i "http://127.0.0.1:3000/compat/strict?id=7"
#[PARAM_STRICT]
#[GET("/compat/strict")]
pub async fn strict(id: i64) -> String {
    format!("id = {id}")
}
