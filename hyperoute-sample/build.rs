use std::env;
use std::fs;
use std::path::PathBuf;

use hyperoute_codegen::{Embed, StaticDir};

/// 扫描 src/controller、src/filter 下的注解并挂载 static 目录，生成 `$OUT_DIR/routes.rs`，
/// 由 src/server.rs `include!` 进来。
fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest_dir.join("src");

    let routes = hyperoute_codegen::Scanner::new()
        .dir(src_dir.join("controller"), "crate::controller")
        .dir(src_dir.join("filter"), "crate::filter")
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
    let code = hyperoute_codegen::generate_router_source(&routes);

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("routes.rs");
    fs::write(&out_path, code).expect("failed to write generated routes");

    println!("cargo:rerun-if-changed=src/controller");
    println!("cargo:rerun-if-changed=src/filter");
}
