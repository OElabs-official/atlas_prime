//! network/netlink — RTM_GETLINK dump 读取网卡累计收发字节（speed 的第四数据源）。
//!
//! 真机（骁龙8Gen2 / Android，Termux 主域 untrusted_app）实测：`/proc/net/dev`、
//! `/proc/self/net/*`、`/sys/class/net` 一律 EPERM；netlink route socket 可创建，
//! 但显式 bind 与 RTM_GETLINK sendto 均被内核 nlaccess 拒（EPERM，nlaccess 按
//! 消息类型过滤——type 0 能过、GETLINK 不能），SO_PEEK_OFF 亦不受支持（ENOTSUP）；
//! SOCK_DIAG/GENERIC 连创建都不行。即本域内三级数据源全部封死，快照为空是
//! **正确行为**。root / adb shell / 自定制 ROM 等放行域下，本路径即出数。
//!
//! IFLA_STATS 载荷为内核序列化的 legacy `rtnl_link_stats`（arm64 内核字长 8B：
//! `rx_bytes@0`、`tx_bytes@80`；32 位内核的载荷仅 72B，被长度闸挡下 = 不出数
//! 而非出错）。零新增依赖：std + 既有 libc。

use super::speed::Counters;

const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const IFLA_IFNAME: u16 = 3;
const IFLA_STATS: u16 = 7;
/// legacy rtnl_link_stats 中 tx_bytes 的偏移（arm64 内核：第 11 个 8B 字段）
const TX_BYTES_OFF: usize = 80;
/// rx_bytes@0 + tx_bytes@80 可读的最小载荷长度
const STATS_MIN_LEN: usize = TX_BYTES_OFF + 8;

/// 全部网卡的累计计数器 `(iface, rx_bytes, tx_bytes)`（不做数据面过滤，交给调用侧）。
/// socket/bind/send/recv 任一环节失败即空表——本函数是安全黑箱，unsafe 不外溢。
pub(super) fn link_stats() -> Counters {
    // RTM_GETLINK dump 请求：nlmsghdr(16B) + 全零 ifinfomsg(16B) = 转储全部链路
    let mut req = Vec::with_capacity(32);
    req.extend_from_slice(&32u32.to_ne_bytes()); // nlmsg_len
    req.extend_from_slice(&RTM_GETLINK.to_ne_bytes());
    req.extend_from_slice(&0x0301u16.to_ne_bytes()); // NLM_F_REQUEST | NLM_F_DUMP
    req.extend_from_slice(&1u32.to_ne_bytes()); // nlmsg_seq
    req.extend_from_slice(&0u32.to_ne_bytes()); // nlmsg_pid=0：bind 时内核自动分配
    req.extend_from_slice(&[0u8; 16]);

    // SAFETY: socket() 三个实参均为合法常量；失败返回 -1 由下方分支消化
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE) };
    if fd < 0 {
        return Counters::new();
    }
    let _guard = NlFd(fd);
    // SAFETY: sockaddr_nl 是 plain-data C 结构体，全零 + 覆写 family 即合法
    // （nl_pad 在 libc 中是不透明的 Padding 类型，只能 zeroed 构造）
    let mut dest: libc::sockaddr_nl = unsafe { core::mem::zeroed() };
    dest.nl_family = libc::AF_NETLINK as u16; // pid=0 内核即目的地（核），groups=0 不收组播
    let mut out = Counters::new();
    let mut buf = vec![0u8; 32 * 1024];
    // 不发 bind()：nlaccess 拒显式 bind 的域里，sendto 隐式 autobind 是仅存通道
    // （bionic getifaddrs 同款走法）；放行域下 sendto 同样正确。
    // SAFETY: sockaddr_nl 是 plain-data C 结构体指针+长度配对正确；fd 由 _guard
    // 持有且存活期覆盖全部后续 syscall；sendto/recv 缓冲区在本函数堆上且长度如实
    let sr = unsafe {
        libc::sendto(
            fd,
            req.as_ptr().cast(),
            req.len(),
            0,
            (&dest as *const libc::sockaddr_nl).cast::<libc::sockaddr>(),
            core::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if sr < 0 {
        return Counters::new();
    }
    // dump 以 NLMSG_DONE 收尾；recv 返回 <=0 视为截断，拿已收部分
    loop {
        // SAFETY: recv 写入范围 [0, buf.len()) 由返回值界定，切片以 n 为界
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n <= 0 {
            break;
        }
        if !collect_newlinks(&buf[..n as usize], &mut out) {
            break;
        }
    }
    out
}

/// 自动关闭 netlink fd（socket 生命周期仅此函数内，RAII 保证异常路径也释放）
struct NlFd(i32);

impl Drop for NlFd {
    fn drop(&mut self) {
        // SAFETY: fd 是本模块 socket() 返回且未被关闭的有效描述符
        unsafe { libc::close(self.0) };
    }
}

/// 解析一批 netlink 报文：逐 nlmsghdr 步进，RTM_NEWLINK 抽取入 out；
/// 见 NLMSG_DONE/NLMSG_ERROR 返回 false（流结束）。纯函数，fixture 直测。
fn collect_newlinks(batch: &[u8], out: &mut Counters) -> bool {
    let mut i = 0;
    while i + 16 <= batch.len() {
        let mlen = u32::from_ne_bytes(batch[i..i + 4].try_into().unwrap()) as usize;
        let mtype = u16::from_ne_bytes(batch[i + 4..i + 6].try_into().unwrap());
        if mlen < 16 || i + mlen > batch.len() {
            break; // 截断/对齐填充
        }
        match mtype {
            NLMSG_DONE | NLMSG_ERROR => return false,
            RTM_NEWLINK => parse_one_link(&batch[i..i + mlen], out),
            _ => {}
        }
        i += (mlen + 3) & !3;
    }
    true
}

/// 单条 RTM_NEWLINK：nlmsghdr(16B) + ifinfomsg(16B) + rtattr 流
fn parse_one_link(msg: &[u8], out: &mut Counters) {
    if msg.len() < 32 {
        return;
    }
    let mut name: Option<String> = None;
    let mut stats: Option<(u64, u64)> = None;
    let mut a = &msg[32..];
    loop {
        if a.len() < 4 {
            break;
        }
        let alen = u16::from_ne_bytes(a[0..2].try_into().unwrap()) as usize;
        let atype = u16::from_ne_bytes(a[2..4].try_into().unwrap());
        if alen < 4 || alen > a.len() {
            break;
        }
        let data = &a[4..alen];
        match atype {
            IFLA_IFNAME => {
                if let Ok(s) = std::str::from_utf8(data) {
                    name = Some(s.trim_end_matches('\0').to_owned());
                }
            }
            IFLA_STATS => {
                if data.len() >= STATS_MIN_LEN {
                    stats = Some((rd_u64(data, 0), rd_u64(data, TX_BYTES_OFF)));
                }
            }
            _ => {}
        }
        let step = (alen + 3) & !3;
        if step > a.len() {
            break;
        }
        a = &a[step..];
    }
    if let (Some(n), Some((rx, tx))) = (name, stats) {
        out.push((n, rx, tx));
    }
}

fn rd_u64(b: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::{Counters, collect_newlinks, IFLA_IFNAME, IFLA_STATS};

    /// 组一条 nlmsg（type + 载荷，4B 对齐）
    fn nlmsg(mtype: u16, payload: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        let len = (16 + payload.len() + 3) & !3;
        m.extend_from_slice(&(len as u32).to_ne_bytes());
        m.extend_from_slice(&mtype.to_ne_bytes());
        m.extend_from_slice(&0u16.to_ne_bytes()); // flags
        m.extend_from_slice(&1u32.to_ne_bytes()); // seq
        m.extend_from_slice(&0u32.to_ne_bytes()); // pid
        m.extend_from_slice(payload);
        m.resize(len, 0);
        m
    }

    /// 组一条 rtattr（4B 对齐）
    fn rtattr(atype: u16, data: &[u8]) -> Vec<u8> {
        let mut a = Vec::new();
        let len = (4 + data.len() + 3) & !3;
        a.extend_from_slice(&(len as u16).to_ne_bytes());
        a.extend_from_slice(&atype.to_ne_bytes());
        a.extend_from_slice(data);
        a.resize(len, 0);
        a
    }

    /// 单链路消息体：全零 ifinfomsg + 名称 + 144B legacy stats（rx@0, tx@80）
    fn link(name: &str, rx: u64, tx: u64) -> Vec<u8> {
        let mut p = vec![0u8; 16];
        p.extend_from_slice(&rtattr(IFLA_IFNAME, name.as_bytes())); // NUL 由对齐填充提供
        let mut st = vec![0u8; 144];
        st[0..8].copy_from_slice(&rx.to_ne_bytes());
        st[80..88].copy_from_slice(&tx.to_ne_bytes());
        p.extend_from_slice(&rtattr(IFLA_STATS, &st));
        nlmsg(16, &p)
    }

    #[test]
    fn dumps_links_from_crafted_batch() {
        let mut batch = link("lo", 111, 222);
        batch.extend_from_slice(&link("wlan0", 9_000_000_000, 1_200_000_000));
        batch.extend_from_slice(&nlmsg(3, &[0, 0, 0, 0])); // NLMSG_DONE
        batch.extend_from_slice(&link("ignored_after_done", 1, 1));
        let mut got: Counters = Counters::new();
        assert!(
            !collect_newlinks(&batch, &mut got),
            "见 DONE 必须报流结束（false）"
        );
        assert_eq!(
            got,
            vec![
                ("lo".to_string(), 111, 222),
                ("wlan0".to_string(), 9_000_000_000, 1_200_000_000),
            ],
            "DONE 后的消息不得收集；字段/偏移须准确"
        );
    }

    /// 真机 SELinux 面诊断（默认跳过）：本域对 netlink 各协议 socket 的
    /// create/bind/sendto 到底放行什么，并打印 link_stats 实收结果。
    /// `cargo test v1::system::network::netlink -- --ignored --test-threads=1 --nocapture`
    #[test]
    #[ignore]
    fn live_nlaccess_probe() {
        for (name, proto) in [
            ("NETLINK_ROUTE", 0i32),
            ("NETLINK_SOCK_DIAG", 4),
            ("NETLINK_GENERIC", 16),
        ] {
            let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, proto) };
            if fd < 0 {
                println!("{name}: create EPERM/err");
                continue;
            }
            let mut addr: libc::sockaddr_nl = unsafe { core::mem::zeroed() };
            addr.nl_family = libc::AF_NETLINK as u16;
            addr.nl_groups = 1; // MCGRP_RTNL_LINK：join 即置 NETLINK_F_LISTEN
            let br = unsafe {
                libc::bind(
                    fd,
                    (&addr as *const libc::sockaddr_nl).cast::<libc::sockaddr>(),
                    core::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
                )
            };
            let mut dest: libc::sockaddr_nl = unsafe { core::mem::zeroed() };
            dest.nl_family = libc::AF_NETLINK as u16;
            let msg = [0u8; 16]; // 全零 nlmsghdr：只为触发 sendto 的 nlaccess 判定
            let sr = unsafe {
                libc::sendto(
                    fd,
                    msg.as_ptr().cast(),
                    16,
                    0,
                    (&dest as *const libc::sockaddr_nl).cast::<libc::sockaddr>(),
                    core::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
                )
            };
            println!(
                "{name}: bind(groups=1)={br} sendto(16B)={sr} errno={}",
                std::io::Error::last_os_error()
            );
            unsafe { libc::close(fd) };
        }
        println!("link_stats() = {:?}", super::link_stats());
    }

    #[test]
    fn truncated_or_statsless_links_are_skipped() {
        // 无 IFLA_STATS 的链路（只有名字）→ 跳过
        let mut body = vec![0u8; 16];
        body.extend_from_slice(&rtattr(IFLA_IFNAME, b"ifb0"));
        let mut batch = nlmsg(16, &body);
        batch.extend_from_slice(&[0xFF, 0xFF, 0x00, 0x00, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // 自称 65535B → 截断停
        let mut got: Counters = Counters::new();
        assert!(collect_newlinks(&batch, &mut got));
        assert!(got.is_empty(), "缺 stats/截断都不入表: {got:?}");
    }
}
