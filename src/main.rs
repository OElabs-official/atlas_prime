//! atlas_prime v1 —— headless 编排入口（TUI 已随 _deprecated_src/ 退役）。
//!
//! 启动顺序：0 单例锁（flock 数据目录 atlas_prime.lock，进程退出——含崩溃/kill——自动释放；
//! 二启检测到锁被占用直接退出 1）→ 任务规格装载（autostart 项即拉）→ 操作器（唯一自主
//! 驱动数据的模块，周期采样入 sqlite）→ ntex API。
//! Ctrl+C 优雅收尾：停操作器、停全部受管任务。

mod v1;

use std::env;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use v1::operator::Operator;
use v1::task::{Phase, Task};
use v1::web::Web;

/// 项目数据目录（与 v1/db 的 db_path 同一解析：directories 三元组 + create_dir_all）。
fn data_dir() -> Option<PathBuf> {
    ProjectDirs::from(v1::PROJECT[0], v1::PROJECT[1], v1::PROJECT[2]).map(|p| {
        let d = p.data_dir().to_path_buf();
        let _ = std::fs::create_dir_all(&d);
        d
    })
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

    // 4. 阻塞至 Ctrl+C（自带最小 runtime，不污染 v1 各域）
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("ctrl-c runtime");
    rt.block_on(async {
        let _ = tokio::signal::ctrl_c().await;
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
