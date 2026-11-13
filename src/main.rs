//! atlas_prime v1 —— headless 编排入口（TUI 已随 _deprecated_src/ 退役）。
//!
//! CLI 表面（clap；两旗标互斥，`--help` 全量列出）：
//! - `atlas_prime`                守护模式（下述启动顺序；flock 拿到后附带 runit 状态提示）
//! - `atlas_prime --sv-install`   一次性：安装/刷新 runit 服务（复制二进制到 $PREFIX/bin +
//!   内嵌服务资产落 $SVDIR + sv-enable），成功 exit 0（逻辑在 [`v1::service`]）
//! - `atlas_prime --sv-uninstall` 一次性：卸载 runit 服务（二进制与日志历史有意保留），exit 0
//!
//! 启动顺序：0 单例锁（flock 数据目录 atlas_prime.lock，进程退出——含崩溃/kill——自动释放；
//! 二启检测到锁被占用直接退出 1）→ 任务规格装载（autostart 项即拉）→ 操作器（唯一自主
//! 驱动数据的模块，周期采样入 sqlite）→ ntex API。
//! Ctrl+C 及 SIGTERM 优雅收尾：停操作器、停全部受管任务（systemd/监督器可终止）。

mod v1;

use std::env;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use clap::Parser;

use v1::operator::Operator;
use v1::task::{Phase, Task};
use v1::api::Web;

/// CLI 表面：守护模式（无旗标）+ runit 安装/卸载（互斥两旗标，一次性退出不取单例锁）。
#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    /// 安装/刷新 runit 服务：复制二进制到 $PREFIX/bin、内嵌资产落 $SVDIR、sv-enable
    #[arg(long, conflicts_with = "sv_uninstall")]
    sv_install: bool,
    /// 卸载 runit 服务：sv-disable + 删服务目录（二进制与日志历史有意保留）
    #[arg(long)]
    sv_uninstall: bool,
}

/// 一次性服务命令统一出口：Ok → exit 0；Err(msg) → [atlas] 前缀打印后 exit 1。
fn one_shot(r: Result<(), String>) -> ! {
    match r {
        Ok(()) => std::process::exit(0),
        Err(msg) => {
            eprintln!("[atlas] {msg}");
            std::process::exit(1)
        }
    }
}

/// 项目数据目录（统一走 [`v1::prelude`]：directories 三元组 + create_dir_all）。
fn data_dir() -> Option<PathBuf> {
    v1::prelude::data_dir().map(PathBuf::from)
}

/// 对 `path` 施加非阻塞独占 flock 并写入当前 pid。
/// 拿不到锁（EWOULDBLOCK 忙或任何错误）返回 None；成功返回持有句柄——
/// flock 是 fd 级语义，句柄（即进程）存续期间持锁，进程终止自动释放，无需残留文件判活。
fn lock_file_on(path: &Path) -> Option<std::fs::File> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return None;
    }
    // 锁文件记录当前持有者 pid（写失败无所谓，锁本身不依赖文件内容）
    let _ = f.set_len(0);
    let _ = f.write_all(format!("{}", std::process::id()).as_bytes());
    Some(f)
}

/// 单例锁：flock 数据目录下 atlas_prime.lock。返回 None = 已有实例在跑。
fn single_instance() -> Option<std::fs::File> {
    let dir = data_dir()?;
    lock_file_on(&dir.join("atlas_prime.lock"))
}

fn main() {
    // 一次性服务命令：不取单例锁（服务在跑时亦可安装/卸载），执行完直接退
    let cli = Cli::parse();
    if cli.sv_install {
        one_shot(v1::service::install());
    }
    if cli.sv_uninstall {
        one_shot(v1::service::uninstall());
    }

    // 0. 单例锁：绑定贯穿整个 main，锁持至进程结束
    let _lock = match single_instance() {
        Some(f) => f,
        None => {
            let lock_path = data_dir()
                .map(|d| d.join("atlas_prime.lock").display().to_string())
                .unwrap_or_else(|| "<数据目录不可用>".to_string());
            eprintln!("[atlas] 已有实例在运行（{lock_path} 被 flock 占用），单例退出");
            std::process::exit(1)
        }
    };
    // runit 状态提示（单次 stat，任何 IO 状况静默，绝不打断启动）
    v1::service::status_hint();

    // 1. 任务监督：重扫配置，autostart 项拉起
    let specs = Task::update();
    eprintln!("[atlas] task specs loaded: {}", specs.len());
    for s in &specs {
        eprintln!("[atlas]   · {:<20} [{}] {:?}", s.name, s.group, s.phase);
    }

    // 2. 操作器：程序内唯一自主驱动数据的模块，2 分钟一轮采样，滚动 20000 轮
    if let Err(e) = Operator::start() {
        eprintln!("[atlas] operator start failed: {e}");
    }

    // 3. Web API：后台线程内建 ntex System（句柄保活至进程结束）
    let bind = env::var("ATLAS_BIND").unwrap_or_else(|_| "0.0.0.0:1025".to_string());
    let _web = Web::spawn(bind.clone());
    eprintln!("[atlas] api listening on http://{bind}");

    // 4. 阻塞至 Ctrl+C 或 SIGTERM（自带最小 runtime，不污染 v1 各域）
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("ctrl-c runtime");
    rt.block_on(async {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("[atlas] SIGTERM handler 注册失败，降级为仅 SIGINT: {e}");
                None
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match term.as_mut() {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    });
    eprintln!("[atlas] shutting down");

    // 5. 收尾：停操作器采样线程，向全部在跑任务发停止意图
    Operator::stop();
    for s in Task::status() {
        if matches!(s.phase, Phase::Starting | Phase::Running | Phase::Stopping) {
            let _ = Task::stop(&s.id);
        }
    }
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod singleton_tests {
    use super::*;

    /// flock 排他性闭环（独立锁文件 atlas_prime.testlock，与生产锁互不干扰）：
    /// 首锁成功 → 同路径二次 open 再锁必忙（flock 按 open file description 判，进程内同样互斥）
    /// → drop 首锁后三锁应恢复成功。
    #[test]
    fn flock_is_exclusive_per_open_file_description() {
        let dir = data_dir().expect("项目数据目录");
        let file = dir.join("atlas_prime.testlock");
        let _ = std::fs::remove_file(&file);
        let first = lock_file_on(&file).expect("首锁应成功");
        assert!(
            lock_file_on(&file).is_none(),
            "同路径二次加锁应返回 None（忙）"
        );
        drop(first);
        assert!(
            lock_file_on(&file).is_some(),
            "释放后重新加锁应成功（Some 句柄随即 drop）"
        );
        let _ = std::fs::remove_file(&file);
    }
}
