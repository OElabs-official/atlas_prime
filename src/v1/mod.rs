//! v1: 采集/管理层根模块，按域分子目录：[`system`] 硬件/系统黑箱采集，
//! [`task`] 任务监督器，[`web`] ntex HTTP API（路由 ↔ 模块一一对应），
//! [`db`] sqlite 滚动历史采样。除标准路径三元组外四域零共享。

/// 标准路径三元组（organization, vendor/application, application version），
/// directories/sqlite/任务配置共用（原 constans::PROJECT，随 v1 自包含化迁入）。
pub const PROJECT: &[&str] = &["org", "oelabs", "atlas_prime", "0.2.0"];

#[allow(dead_code)]
pub mod system;
#[allow(dead_code)]
pub mod task;
#[allow(dead_code)]
pub mod web;

#[allow(dead_code)]
pub mod db;
