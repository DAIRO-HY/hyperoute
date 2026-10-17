//! build.rs 会扫描这个目录下的所有 .rs 文件（mod.rs 除外），收集路由注解。
pub mod hello;
pub mod param_error;
pub mod params;
pub mod responses;
pub mod user;
pub mod websocket;
