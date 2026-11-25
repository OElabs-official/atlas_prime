//! v1 通用数据底座（_std_general.md 规范的 `mod prelude`）：
//! 版本号、`org.oelabs.*` 标准路径（directories）、配置文件加载（std `OnceLock`——
//! 本机 Termux std 构建无 `sync::OnceCell`，`OnceLock` 为等价超集，见各域同款注释）。
//!
//! # 消费方（谁在用本模块）
//! - `main.rs`：单例锁路径（[`data_dir`]）；
//! - `v1::db`：sqlite 库文件落点（[`data_dir`]，`atlas_history.db`）；
//! - `v1::task`：任务规格目录（[`data_dir`]，`atlas_task.json` + `script/*.ts`）；
//! - `v1::api`：面板热替换覆盖文件（[`data_dir`]，`dashboard.html`）；
//! - `v1::system::disk`：路径汇报（[`project_dirs`] 全套四目录）。
//!
//! [`PROJECT`] 三元组是六域唯一共享数据（经本模块单例函数取用，各域不再自解析）。
//!
//! # 配置加载约定
//! [`config`] 读取 preference_dir 下 `config.json`（进程内只加载一次）。
//! 当前无消费方，作为统一配置的预留入口；接入方在本节补记模块名。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use directories::ProjectDirs;

/// 标准路径三元组（organization, vendor/application）+ 版本槽（随 Cargo.toml
/// 走，`env!` 编译期注入，永不手改；消费侧只取 [0..3]，版本槽留审计）。
pub const PROJECT: &[&str] = &["org", "oelabs", "atlas_prime", env!("CARGO_PKG_VERSION")];

/// `org.oelabs.atlas_prime` 的 [`ProjectDirs`]（进程内单例；平台无标准路径时 None）。
pub fn project_dirs() -> Option<&'static ProjectDirs> {
    static DIRS: OnceLock<Option<ProjectDirs>> = OnceLock::new();
    DIRS.get_or_init(|| ProjectDirs::from(PROJECT[0], PROJECT[1], PROJECT[2]))
        .as_ref()
}

/// 项目数据目录（解析即 `create_dir_all` 保活，返回可直接 join 的已存在目录）。
pub fn data_dir() -> Option<&'static Path> {
    static DATA: OnceLock<Option<PathBuf>> = OnceLock::new();
    DATA.get_or_init(|| {
        project_dirs().map(|p| {
            let d = p.data_dir().to_path_buf();
            let _ = std::fs::create_dir_all(&d);
            d
        })
    })
    .as_deref()
}

/// 统一配置入口：preference_dir 下 `config.json` 一次加载；文件缺失/坏损
/// 回落空对象（永不 panic）。当前无消费方（预留），接入时更新模块头文档。
#[allow(dead_code)]
pub fn config() -> &'static serde_json::Value {
    static CFG: OnceLock<serde_json::Value> = OnceLock::new();
    CFG.get_or_init(|| {
        project_dirs()
            .map(|p| p.preference_dir().join("config.json"))
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    })
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod prelude_tests {
    use super::*;

    /// 三元组与 Cargo.toml 版本槽对齐；路径解析进程内幂等（同一内存块）。
    #[test]
    fn triple_and_dirs_are_stable() {
        assert_eq!(&PROJECT[0..3], &["org", "oelabs", "atlas_prime"]);
        assert_eq!(PROJECT[3], env!("CARGO_PKG_VERSION"));
        if let Some(d) = data_dir() {
            assert!(d.exists(), "data_dir 解析即建目录，应存在");
            assert!(std::ptr::eq(d, data_dir().unwrap()), "OnceLock 应返回同一块内存");
        }
        assert!(config().is_object(), "配置缺省应回落空对象");
    }
}
