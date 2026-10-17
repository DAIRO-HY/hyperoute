use proc_macro::TokenStream;

// 这些属性宏本身不改写函数体，只是让 `#[GET("/path")]` 这类写法成为合法语法，
// 真正的路由信息由 hyperoute-codegen 在 build.rs 里用 syn 重新解析源码来提取。
macro_rules! define_method_attribute {
    ($name:ident) => {
        #[proc_macro_attribute]
        #[allow(non_snake_case)]
        pub fn $name(_attr: TokenStream, item: TokenStream) -> TokenStream {
            item
        }
    };
}

define_method_attribute!(GET);
define_method_attribute!(POST);
define_method_attribute!(PUT);
define_method_attribute!(DELETE);
define_method_attribute!(PATCH);
define_method_attribute!(HEAD);
define_method_attribute!(OPTIONS);
define_method_attribute!(CONNECT);
define_method_attribute!(TRACE);
// 匹配所有 HTTP 方法
define_method_attribute!(REQUEST);
// 标注参数解析失败时的错误响应函数：`fn(hyperoute_core::ParamErrors) -> impl IntoErrorResponse`，
// 整个 controller 目录下最多一个；没有则使用 hyperoute_core::DefaultParamErrorResponse。
define_method_attribute!(PARAM_ERROR);
// 标注过滤器：`async fn(hyperoute_core::Request, hyperoute_core::Next) -> Response 或 Result<Response, E>`，
// 例如 `#[FILTER(order = 10, include = ["/api/**"], exclude = ["/api/login"])]`，参数都可省略。
define_method_attribute!(FILTER);
// 标注 WebSocket 路由：`#[WEBSOCKET("/ws/chat/{room}")] async fn chat(socket: WebSocket, room: String, ...)`，
// 握手由框架完成，函数在连接建立后被调用；其它参数（路径 / 查询 / Ext）和普通路由一样解析。
define_method_attribute!(WEBSOCKET);
