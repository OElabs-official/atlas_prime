//! atlas_prime v1 —— headless 编排入口（TUI 已随 _deprecated_src/ 退役）。
//!
//! CLI 表面（clap；两旗标互斥，`--help` 全量列出）：
//! - `atlas_prime`                守护模式（下述启动顺序；flock 拿到后附带 runit 状态提示）
//! - `--host ADDR`                仅守护模式：监听地址。ADDR 含 ':' 即 host:port 原样使用，
//!   裸 host 补默认端口 1025；未给则退 ATLAS_BIND 环境变量，最终默认 127.0.0.1:1025
//!   （loopback——旧默认 0.0.0.0:1025 已收窄，优先级逻辑见 [`resolve_bind`]）
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
use std::time::Duration;

use clap::Parser;

use v1::operator::Operator;
use v1::task::Task;
use v1::api::Web;

/// CLI 表面：守护模式（无旗标）+ runit 安装/卸载（互斥两旗标，一次性退出不取单例锁）
/// + 守护模式监听地址覆盖（--host，与两旗标正交，仅守护模式生效）。
#[derive(Parser)]
#[command(
    author,
    version,
    about,
    long_about = "atlas_prime —— headless 系统监控守护进程：周期采样硬件/网络写入本地 sqlite，\
                  经 HTTP/JSON API 服务面板（守护模式）；另提供一次性服务命令 \
                  --sv-install / --sv-uninstall（runit 安装/卸载，执行完即退）。",
    after_help = "监听（仅守护模式）：默认 127.0.0.1:1025（loopback）。\n  \
        --host 0.0.0.0          全网卡监听，端口自动补 1025（ADDR 接受 host 或 host:port，含 ':' 原样使用）\n  \
        --host 0.0.0.0:8123     全网卡 + 自定义端口\n  \
        ATLAS_BIND=host:port    环境变量兜底（--host 未给时生效，值原样使用）\n  \
        建议由 APG atlas_network 承担对外 HTTPS 反向代理（监听高位端口），守护进程保持 loopback。"
)]
struct Cli {
    /// 安装/刷新 runit 服务：复制二进制到 $PREFIX/bin、内嵌资产落 $SVDIR、sv-enable
    #[arg(long, conflicts_with = "sv_uninstall")]
    sv_install: bool,
    /// 卸载 runit 服务：sv-disable + 删服务目录（二进制与日志历史有意保留）
    #[arg(long)]
    sv_uninstall: bool,
    /// 守护模式监听地址（仅守护模式生效）：ADDR 含 ':' 按 host:port 原样使用，裸 host 补端口 1025；
    /// 未给退 ATLAS_BIND 环境变量，均缺省则 127.0.0.1:1025（loopback）
    #[arg(long, value_name = "ADDR")]
    host: Option<String>,
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

/// API 监听地址解析（仅守护模式；纯函数，单测见 bind_tests）。优先级：
/// 1. `--host ADDR`：含 ':' 即 host:port，原样使用；不含则视为裸 host，补默认端口 1025。
///    空串按未给处理——「未给」以 None 编码（clap 未传该旗标时字段即 None，不会传 Some("")）。
/// 2. 否则 ATLAS_BIND 环境变量非空值，host:port 原样使用。
/// 3. 否则默认 `127.0.0.1:1025`——loopback。旧默认 0.0.0.0:1025 全网卡监听，现已收窄；
///    对外暴露建议由 APG atlas_network 承担 HTTPS 反代（高位端口），或显式 --host / ATLAS_BIND。
fn resolve_bind(host_flag: Option<&str>, atlas_bind: Option<&str>) -> String {
    const DEFAULT_PORT: u16 = 1025;
    if let Some(addr) = host_flag.filter(|a| !a.is_empty()) {
        return if addr.contains(':') {
            addr.to_string()
        } else {
            format!("{addr}:{DEFAULT_PORT}")
        };
    }
    if let Some(v) = atlas_bind.filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    format!("127.0.0.1:{DEFAULT_PORT}")
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

    // 3. Web API：后台线程内建 ntex System（句柄保活至进程结束）；
    // 监听地址 = --host > ATLAS_BIND > 默认 127.0.0.1:1025（loopback），见 resolve_bind
    let bind = resolve_bind(cli.host.as_deref(), env::var("ATLAS_BIND").ok().as_deref());
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

    // 5. 收尾：先停操作器采样线程，再带升级地停净全部受管任务——daemon 级宽限 5s
    // （SIGTERM 整组 → 宽限 → SIGKILL，升级在进程返回前必然走完，不留孤儿/僵尸；
    // 等待与摘除在 Task::shutdown_all 内，不重扫规格、autostart 不复活）
    Operator::stop();
    Task::shutdown_all(Duration::from_secs(5));
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

#[cfg(test)]
mod bind_tests {
    use super::resolve_bind;

    /// 旗标带端口：含 ':' 即 host:port，原样使用（env 同样在也优先旗标）
    #[test]
    fn host_flag_with_port_is_used_verbatim() {
        assert_eq!(resolve_bind(Some("0.0.0.0:8123"), None), "0.0.0.0:8123");
        assert_eq!(
            resolve_bind(Some("[::]:8123"), Some("10.0.0.1:9999")),
            "[::]:8123"
        );
    }

    /// 旗标裸 host：补默认端口 1025（即便 env 在，旗标优先级更高）
    #[test]
    fn host_flag_bare_address_gets_default_port() {
        assert_eq!(resolve_bind(Some("0.0.0.0"), None), "0.0.0.0:1025");
        assert_eq!(
            resolve_bind(Some("127.0.0.1"), Some("10.0.0.1:9999")),
            "127.0.0.1:1025"
        );
    }

    /// env 兜底：旗标未给时 ATLAS_BIND 原样使用
    #[test]
    fn env_fallback_used_verbatim_when_flag_absent() {
        assert_eq!(resolve_bind(None, Some("10.0.0.1:8080")), "10.0.0.1:8080");
    }

    /// 双缺省：默认 loopback（旧默认 0.0.0.0:1025 已收窄）
    #[test]
    fn default_is_loopback_when_nothing_given() {
        assert_eq!(resolve_bind(None, None), "127.0.0.1:1025");
    }

    /// 旗标压过 env（两种旗标形态各验一次）+ 空串按未给处理：
    /// 空旗标退到 env、空 env 退到默认——「未给」以 None/空编码，绝不算合法地址
    #[test]
    fn flag_beats_env_and_empty_strings_count_as_absent() {
        assert_eq!(
            resolve_bind(Some("192.168.1.5:1234"), Some("10.0.0.1:8080")),
            "192.168.1.5:1234"
        );
        assert_eq!(resolve_bind(Some(""), Some("10.0.0.1:8080")), "10.0.0.1:8080");
        assert_eq!(resolve_bind(None, Some("")), "127.0.0.1:1025");
    }
}
