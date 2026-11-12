//! yi 的核心库：配置、会话、供应商、对话循环。
//!
//! 二进制只是一层薄壳；这里的一切都可以被集成测试直接驱动。

pub mod agent;
pub mod cancel;
pub mod crash;
pub mod config;
pub mod lsp;
pub mod provider;
pub mod render;
pub mod session;
pub mod skills;
pub mod tools;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    Http(String),
    Api(String),
    Config(String),
    /// 用户按 Ctrl-C 要求中断本轮。
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "读写失败：{e}"),
            Error::Json(e) => write!(f, "JSON 解析失败：{e}"),
            Error::Http(e) => write!(f, "网络错误：{e}"),
            Error::Api(e) => write!(f, "接口返回错误：{e}"),
            Error::Config(e) => write!(f, "{e}"),
            Error::Cancelled => write!(f, "已取消这一轮"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}
