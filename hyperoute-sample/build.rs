use std::env;
use std::fs;
use std::path::PathBuf;

use hyperoute_codegen::{Embed, ParamBinding, ParamFallback, ParamNaming, StaticDir};

/// 扫描 src/controller、src/filter 下的注解并挂载 static 目录，生成 `$OUT_DIR/routes.rs`；
/// 另外用一套兼容 Go 版 AutoWeb 约定的开关扫描 src/compat，生成 `$OUT_DIR/routes_compat.rs`。
/// 两份都由 src/server.rs `include!` 进来，按路径前缀分发。
fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest_dir.join("src");

    let routes = hyperoute_codegen::Scanner::new()
        .dir(src_dir.join("controller"), "crate::controller")
        .dir(src_dir.join("filter"), "crate::filter")
        // 标量查询参数和 multipart 字段同时接受驼峰写法：fn f(page_size: u32) 也能收到 ?pageSize=20
        .param_naming(ParamNaming::CamelCase)
        // static/ 挂在 /static 下：debug 编译读磁盘（改了刷新就生效），release 编译打包进二进制
        .static_dir(
            StaticDir::new("/static", manifest_dir.join("static"))
                .embed(Embed::Release)
                .gzip(true)
                // html 里的 {{template include/header}} 先合并成完整页面，再按普通静态文件处理
                .html_include(true)
                // no-cache：浏览器每次都带 ETag 来验证，没变就是 304
                .cache_control("no-cache"),
        )
        .scan();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let code = hyperoute_codegen::generate_router_source(&routes);
    fs::write(out_dir.join("routes.rs"), code).expect("failed to write generated routes");

    // 迁移老项目（例如 Go 版 AutoWeb）时的写法：查询串和表单请求体合并取值、参数缺失或转换失败时用默认值。
    // 一般一个项目只用一套开关；这里为了同时演示两种模式，单独扫描 src/compat。
    let compat = hyperoute_codegen::Scanner::new()
        .dir(src_dir.join("compat"), "crate::compat")
        .param_naming(ParamNaming::CamelCase)
        .param_binding(ParamBinding::Merged)
        .param_fallback(ParamFallback::Default)
        .scan();
    let code = hyperoute_codegen::generate_router_source(&compat);
    fs::write(out_dir.join("routes_compat.rs"), code).expect("failed to write generated routes");

    println!("cargo:rerun-if-changed=src/controller");
    println!("cargo:rerun-if-changed=src/filter");
    println!("cargo:rerun-if-changed=src/compat");
}
