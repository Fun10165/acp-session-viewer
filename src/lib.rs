//! acp-view：用 ACP 协议只读查看各家 agent 的会话。
//!
//! 库部分按职责分层：
//!
//! * [`config`]：配置文件模型（服务端参数、agent 注册表、可见性策略）；
//! * [`policy`]：可见性判定规则（纯逻辑）；
//! * [`render`]：事件到文本的渲染与字符预算；
//! * [`acp`]：只读 ACP 客户端与长连接池；
//! * [`catalog`]：会话台账与判定记录（SQLite）；
//! * [`locate`]：会话文件定位，为文件条件提供事实；
//! * [`service`]：把上面几层组装成查询接口；
//! * [`server`] / [`client`]：HTTP 服务端与客户端；
//! * [`cli`]：命令行入口。

pub mod acl;
pub mod acp;
pub mod catalog;
pub mod cli;
pub mod client;
pub mod config;
pub mod locate;
pub mod policy;
pub mod render;
pub mod server;
pub mod service;
pub mod system;
