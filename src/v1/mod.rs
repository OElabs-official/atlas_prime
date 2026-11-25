//! v1: 采集/管理层根模块，按域分子目录：[`prelude`] 通用数据底座（版本号、
//! org.oelabs.* 标准路径、配置加载；_std_general.md 规范 mod），
//! [`system`] 硬件/系统黑箱采集，[`task`] 任务监督器，
//! [`api`] ntex HTTP API（路由 ↔ 模块一一对应），
//! [`operator`] 操作器（程序内唯一自主驱动数据的模块：周期采样），
//! [`db`] sqlite 被动存储（open/schema/insert/history，永不自发产数据），
//! [`notify`] 被动出站（termux-notification 薄桥，零状态零策略），
//! [`service`] runit 服务内置安装/卸载（CLI 一次性调用的被动模块，零常驻状态）。
//! 除 [`prelude`]（标准路径三元组等通用数据）外零共享：六个驱动/采集域彼此隔离，
//! 被动工具模块 [`service`]（CLI 一次性调用）同样零状态零共享。

pub mod prelude;

pub mod system;
pub mod task;
pub mod api;

pub mod db;
pub mod notify;
pub mod operator;
pub mod service;
