use std::env;
use std::fs;
use std::path::PathBuf;

/// 扫描 src/controller、src/filter 下的注解，生成 `$OUT_DIR/routes.rs`，由 src/server.rs `include!` 进来。
fn main() {
    let src_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("src");

    let routes = hyperoute_codegen::Scanner::new()
        .dir(src_dir.join("controller"), "crate::controller")
        .dir(src_dir.join("filter"), "crate::filter")
        .scan();
    let code = hyperoute_codegen::generate_router_source(&routes);

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("routes.rs");
    fs::write(&out_path, code).expect("failed to write generated routes");

    println!("cargo:rerun-if-changed=src/controller");
    println!("cargo:rerun-if-changed=src/filter");
}
