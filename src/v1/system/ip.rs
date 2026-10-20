//! v1/ip — 本机网卡地址枚举，按 回环 / 局域网 / 公网 三档分类。
//!
//! # 公开接口用法
//!
//! 对外只有一个结构体 [`Ip`]（唯一公开关联函数 [`Ip::get`]，一次采集）：
//!
//! ```ignore
//! // crate 内调用路径：crate::v1::system::ip
//! let net = crate::v1::system::ip::Ip::get();
//!
//! net.loopback;   // Vec<(网卡名, 地址)>  回环：127.x / ::1（通常只有 lo）
//! net.lan;        // Vec<(网卡名, 地址)>  局域网：RFC1918、链路本地、ULA（v4/v6 混装）
//! net.public_v4;  // Option<String>       出口公网 IPv4（探测结果 60s TTL + SWR 缓存）
//! net.public_v6;  // Vec<String>          本机全局单播 IPv6（本地即可判定，无需出网）
//!
//! // 惯用判网：有任一非回环地址 = 有网
//! let online = !net.lan.is_empty() || net.public_v4.is_some();
//! ```
//!
//! 内部策略：
//! - 本地枚举走 `local_ip_address::list_afinet_netifas()`（libc `getifaddrs`），
//!   **不 fork `ip`/`ifconfig` 子进程**；分类按地址本身（`is_loopback` /
//!   `is_private` / `is_unique_local` / `is_unicast_link_local`），与网卡名无关；
//! - 注意：运营商 CGNAT 的 rmnet 地址（10/8）按 RFC 落入 `lan` 桶，
//!   真实出口地址看 `public_v4`；接口直挂公网 IPv4（罕见）也并入 `lan` 桶展示；
//! - `public_v4` 主源为**明文 HTTP 回显**（std 裸 TCP:80 GET，ipip/ifconfig/ip.sb
//!   三站顺试）：语义是「当前网络路径上回显服务看到的源地址」——本机挂 SSR 时
//!   实测为 VPS 出口 202.182.121.227（代理视角），裸线路 IP 需旁路代理复测，
//!   详见 [`probe_public_v4`] 的观测视角备忘；兜底走 UDP:53 DNS TXT（Google
//!   `o-o.myaddr` 主、Cloudflare 备，扫描全部 TXT 记录取首个合法 IP——Google
//!   会随机夹带 `edns0-client-subnet` 副记录）；
//!   各请求 1.5s 超时，零新增依赖、不起 TLS 栈；结果含失败均缓存 60s（负缓存）；
//! - `public_v4` 缓存读取 = **stale-while-revalidate + 单飞（single-flight）**：
//!   TTL 内直接回缓存（含负缓存 None）；过期先**立即回旧值**，同时踢一个后台
//!   刷新线程（`REFRESH` 原子标志保证任意时刻至多一个在飞，刷新线程探完写缓存
//!   后释放标志）；从未探过（缓存为空）时在专用 `PROBE_LOCK` 下内联探测并
//!   双重检查——首个调用必回真实答案，并发首探只出网一次；
//! - `public_v6` = 本机枚举出的非 ULA/链路本地 v6（隐私扩展临时地址同样
//!   公网可达，一并列入）；纯 v4 NAT 环境下为空。

use serde::Serialize;
use std::net::{IpAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// 一次本机地址枚举 + 公网探测结果（本模块唯一对外类型）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Ip {
    /// 回环地址：`(网卡名, 地址)`，127.x / ::1
    pub loopback: Vec<(String, String)>,
    /// 局域网地址：`(网卡名, 地址)`，私网 + 链路本地 + ULA（v4/v6 混装，按地址格式区分）
    pub lan: Vec<(String, String)>,
    /// 出口公网 IPv4；无外网或探测超时为 `None`
    pub public_v4: Option<String>,
    /// 本机全局单播 IPv6（公网可直达）；纯 v4 环境为空
    pub public_v6: Vec<String>,
}

impl Ip {
    /// 【唯一公开关联函数】枚举本机地址并分桶；`public_v4` 走 60s TTL + SWR 缓存。
    pub fn get() -> Self {
        let mut out = Self::default();
        let Ok(ips) = local_enumeration() else {
            return out;
        };
        for (name, ip) in ips {
            let entry = (name, ip.to_string());
            match classify(&ip) {
                Bucket::Loopback => out.loopback.push(entry),
                Bucket::Lan | Bucket::GlobalV4 => out.lan.push(entry),
                Bucket::GlobalV6 => out.public_v6.push(entry.1),
            }
        }
        out.public_v4 = public_v4_cached();
        out
    }
}

// ---------------- 以下为内部黑箱 ----------------

#[derive(Debug, PartialEq, Eq)]
enum Bucket {
    Loopback,
    Lan,
    GlobalV4,
    GlobalV6,
}

/// 按地址属性归类（与网卡名无关，跨平台稳定）
fn classify(ip: &IpAddr) -> Bucket {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                Bucket::Loopback
            } else if v4.is_private() {
                Bucket::Lan
            } else {
                Bucket::GlobalV4
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                Bucket::Loopback
            } else if v6.is_unique_local() || v6.is_unicast_link_local() {
                Bucket::Lan
            } else {
                Bucket::GlobalV6
            }
        }
    }
}

/// 本地枚举薄封装（独立出来便于单测，不掺公网逻辑）
fn local_enumeration() -> Result<Vec<(String, IpAddr)>, String> {
    local_ip_address::list_afinet_netifas().map_err(|e| e.to_string())
}

/// 公网 IPv4 结果缓存：(取得时刻, 结果)，失败也缓存（负缓存防连环出网）
static PUB4: OnceLock<Mutex<Option<(Instant, Option<String>)>>> = OnceLock::new();
const PUB4_TTL: Duration = Duration::from_secs(60);

/// SWR 后台刷新的单飞标志：`swap(true)` 抢到 false 者才许 spawn，全程至多一个在飞
static REFRESH: AtomicBool = AtomicBool::new(false);
/// 首探专用锁：并发首调用在锁后双重检查去重（与 PUB4 锁不嵌套成环）
static PROBE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// SWR 读缓存：fresh（< TTL，含负缓存）直接回；过期立即回旧值并踢单飞后台刷新；
/// 从未探过在 `PROBE_LOCK` 下内联探测（首个调用必回真实答案）。
fn public_v4_cached() -> Option<String> {
    let lock = PUB4.get_or_init(|| Mutex::new(None));
    let cached = lock.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some((at, val)) = cached {
        if at.elapsed() >= PUB4_TTL {
            kick_refresh();
        }
        return val; // 过期也先回旧值（SWR），新值下次调用可见
    }
    // 空缓存：首个调用内联探测；并发首探由 PROBE_LOCK 去重，锁后双检
    let probe_lock = PROBE_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = probe_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, val)) = g.as_ref() {
        return val.clone(); // 等锁期间已被他人（含后台刷新）填充
    }
    let fresh = probe_public_v4();
    *g = Some((Instant::now(), fresh.clone()));
    fresh
}

/// 单飞后台刷新：抢 `REFRESH` 标志 → spawn 探测线程 → 写缓存 → 释放标志。
/// `probe_public_v4` 吞尽 IO 错误不会 panic；若线程 spawn 本身失败则标志滞留，
/// 影响仅为「本次不再后台刷新」（读侧仍回旧值）。
fn kick_refresh() {
    if REFRESH.swap(true, Ordering::AcqRel) {
        return; // 已有刷新在飞
    }
    std::thread::spawn(|| {
        let val = probe_public_v4();
        let lock = PUB4.get_or_init(|| Mutex::new(None));
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), val));
        REFRESH.store(false, Ordering::Release);
    });
}

/// 探测出口公网 IPv4：明文 HTTP 回显（TCP 视角）优先，
/// DNS TXT（Google 主、Cloudflare 备）兜底。
///
/// ⚠️ 观测视角备忘（本机真机验证，2026-10；待用户以 SSR 开/关对照复测）：
/// - 实测返回 `202.182.121.227` 为**用户 VPS 出口**而非运营商手机号——本机跑着
///   SSR（tun/透明代理会接管 TCP:80），HTTP 回显看到的其实是 VPS 的出站点；
/// - 对照组：DNS TXT 走被 Private DNS TLS 化的链路时，回声是解析器出口
///   （74.125/172.253 等 Google 段，每次轮换），响应内 edns0-client-subnet
///   字段倒是暴露了真线路段（120.240.178.0/24，联通）；
/// - 结论：`public_v4` 语义 =「当前网络路径上、回显服务看到的源地址」，挂了
///   代理时它就是代理出口（这往往正是想看的）；若想要裸线路 IP，需旁路 SSR
///   后复测，或改走 UDP 观测点（EDNS client-subnet 只能给到 /24 段）。
fn probe_public_v4() -> Option<String> {
    for (host, path) in [
        ("myip.ipip.net", "/"),
        ("ifconfig.me", "/ip"),
        ("api.ip.sb", "/ip"),
    ] {
        if let Some(ip) = http_echo_ip(host, path) {
            return Some(ip);
        }
    }
    for (server, qname) in [
        ("8.8.8.8:53", "o-o.myaddr.l.google.com"),
        ("8.8.4.4:53", "o-o.myaddr.l.google.com"),
        ("1.1.1.1:53", "whoami.cloudflare"),
    ] {
        if let Some(ip) = dns_txt(server, qname) {
            let ip = ip.trim_matches('"').trim();
            if ip.parse::<IpAddr>().is_ok() {
                return Some(ip.to_owned());
            }
        }
    }
    None
}

/// std 裸 TCP + 最小 HTTP/1.0 GET，从响应正文提取首个合法 IPv4（不复活 ureq）
fn http_echo_ip(host: &str, path: &str) -> Option<String> {
    use std::io::{Read, Write};
    let addr = (host, 80u16).to_socket_addrs().ok()?.next()?;
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_millis(1500)).ok()?;
    sock.set_read_timeout(Some(Duration::from_millis(1500)))
        .ok()?;
    write!(
        sock,
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: atlas-v1\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).ok()?;
    if buf.len() > 8192 {
        return None; // 回显服务不该有正文膨胀，防呆
    }
    let text = String::from_utf8_lossy(&buf);
    let body = text.split_once("\r\n\r\n")?.1;
    body.split_whitespace()
        .map(|t| t.trim_matches(|c| c == '"' || c == ','))
        .find(|t| t.parse::<std::net::Ipv4Addr>().is_ok())
        .map(str::to_owned)
}

/// 单发 UDP DNS TXT 查询（std 手搓报文，无新增依赖）
fn dns_txt(server: &str, qname: &str) -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.set_read_timeout(Some(Duration::from_millis(1500)))
        .ok()?;
    // 简易随机 ID（纳秒时间戳低位）
    let id = (SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .subsec_nanos() as u16)
        | 1;
    let packet = build_txt_query(id, qname);
    sock.send_to(&packet, server).ok()?;
    let mut buf = [0u8; 1024];
    let (n, _) = sock.recv_from(&mut buf).ok()?;
    parse_txt_answer(&buf[..n])
}

/// 组装仅含一个问题（QTYPE=TXT）的 DNS 查询报文
fn build_txt_query(id: u16, qname: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(64);
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&0x0100u16.to_be_bytes()); // 标准查询，RD=1
    p.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // QDCOUNT=1，其余 0
    for label in qname.split('.') {
        p.push(label.len() as u8);
        p.extend_from_slice(label.as_bytes());
    }
    p.push(0);
    p.extend_from_slice(&16u16.to_be_bytes()); // QTYPE = TXT
    p.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
    p
}

/// 从 DNS 响应中提取公网 IP 所在的 TXT 记录。
/// Google 响应会随机顺序夹带 `edns0-client-subnet x.x.x.0/24` 等副 TXT，
/// 因此扫描全部 TXT 记录、返回首个可解析为 IP 的内容（去引号前原样返回）。
fn parse_txt_answer(msg: &[u8]) -> Option<String> {
    if msg.len() < 12 {
        return None;
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]) as usize;
    let an = u16::from_be_bytes([msg[6], msg[7]]) as usize;
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(msg, i)? + 4; // 问题段：QNAME + QTYPE + QCLASS
    }
    let mut fallback: Option<String> = None;
    for _ in 0..an {
        i = skip_name(msg, i)?;
        if i + 10 > msg.len() {
            return fallback;
        }
        let rtype = u16::from_be_bytes([msg[i], msg[i + 1]]);
        let rdlen = u16::from_be_bytes([msg[i + 8], msg[i + 9]]) as usize;
        let rd_start = i + 10;
        let rd_end = rd_start.checked_add(rdlen)?;
        let rd = msg.get(rd_start..rd_end)?;
        i = rd_end;
        if rtype == 16 {
            if let Some(txt) = read_txt_strings(rd) {
                if txt.trim_matches('"').trim().parse::<IpAddr>().is_ok() {
                    return Some(txt); // 命中真 IP 记录，立即采用
                }
                fallback.get_or_insert(txt); // 副产物（client-subnet 等）暂存
            }
        }
    }
    fallback
}

/// 跳过 DNS 名称（含 0xC0 压缩指针，直接停在指针后）
fn skip_name(m: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let l = *m.get(i)?;
        if l == 0 {
            return Some(i + 1);
        }
        if l & 0xC0 == 0xC0 {
            return Some(i + 2);
        }
        i += 1 + l as usize;
    }
}

/// TXT rdata = 若干个 [len][bytes] 段，拼接返回
fn read_txt_strings(rd: &[u8]) -> Option<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < rd.len() {
        let l = rd[i] as usize;
        let part = rd.get(i + 1..i + 1 + l)?;
        out.push_str(std::str::from_utf8(part).ok()?);
        i += 1 + l;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{Bucket, classify, parse_txt_answer, read_txt_strings, skip_name};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn classifies_real_device_addresses() {
        assert_eq!(classify(&IpAddr::V4(Ipv4Addr::LOCALHOST)), Bucket::Loopback);
        assert_eq!(
            classify(&IpAddr::V4("192.168.1.123".parse().unwrap())), // wlan0 家庭路由
            Bucket::Lan
        );
        assert_eq!(
            classify(&IpAddr::V4("10.100.24.97".parse().unwrap())), // rmnet 运营商 CGNAT
            Bucket::Lan
        );
        assert_eq!(
            classify(&IpAddr::V4("8.8.8.8".parse().unwrap())),
            Bucket::GlobalV4
        );
        assert_eq!(classify(&IpAddr::V6(Ipv6Addr::LOCALHOST)), Bucket::Loopback);
        assert_eq!(
            classify(&IpAddr::V6("fe80::1".parse().unwrap())),
            Bucket::Lan
        );
        assert_eq!(
            classify(&IpAddr::V6("fd12:3456::1".parse().unwrap())), // ULA
            Bucket::Lan
        );
        assert_eq!(
            classify(&IpAddr::V6("2401:c1c:1234::5".parse().unwrap())), // 本机实测 v6 段
            Bucket::GlobalV6
        );
    }

    /// 手搓 Google 风格响应：问题段原文 + 两条 TXT 答案（副产物 subnet 记录在前、
    /// 真 IP 在后，复刻真机随机顺序坑）+ 答案段压缩指针(C0 0C)
    fn fixture_response() -> Vec<u8> {
        let mut m = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0]; // ANCOUNT=2
        m.extend_from_slice(&[6]); // "whoami"
        m.extend_from_slice(b"whoami");
        m.extend_from_slice(&[10]); // "cloudflare"（10 字符）
        m.extend_from_slice(b"cloudflare");
        m.extend_from_slice(&[0, 0, 16, 0, 1]); // 根标签 + QTYPE=TXT + QCLASS=IN
        m.extend_from_slice(&[0xC0, 0x0C]); // 答案1：指针名 + 副产物 TXT
        m.extend_from_slice(&[0, 16, 0, 1, 0, 0, 0, 60, 0, 31]); // RDLEN=31
        m.extend_from_slice(&[30]);
        m.extend_from_slice(b"edns0-client-subnet 1.2.3.0/24");
        m.extend_from_slice(&[0xC0, 0x0C]); // 答案2：指针名 + 真 IP TXT
        m.extend_from_slice(&[0, 16, 0, 1, 0, 0, 0, 60, 0, 10]); // RDLEN=10（=1字节段长+9字节文本）
        m.extend_from_slice(&[9]); // 单段 TXT，9 字节
        m.extend_from_slice(b"\"1.2.3.4\"");
        m
    }

    #[test]
    fn parses_txt_answer_with_pointer() {
        let m = fixture_response();
        // 12 + [6]+whoami(7) + [10]+cloudflare(11) + 根标签(1) = 31
        assert_eq!(skip_name(&m, 12), Some(31));
        // 副产物 TXT 在前也必须跳过，返回可解析为 IP 的那条
        assert_eq!(parse_txt_answer(&m).as_deref(), Some("\"1.2.3.4\""));
    }

    #[test]
    fn concatenates_multi_segment_txt_and_survives_garbage() {
        assert_eq!(
            read_txt_strings(&[3, b'a', b'b', b'c', 2, b'd', b'e']).as_deref(),
            Some("abcde")
        );
        assert_eq!(parse_txt_answer(&[0u8; 4]), None);
        assert_eq!(parse_txt_answer(&[0u8; 12][..]), None); // 零问题零答案
    }

    /// getifaddrs 在任何 Linux/Android 都可用：lo/127.0.0.1 必现
    #[test]
    fn enumerates_loopback() {
        let ips = super::local_enumeration().expect("getifaddrs 应可用");
        assert!(
            ips.iter()
                .any(|(n, a)| n == "lo" && *a == "127.0.0.1".parse::<IpAddr>().unwrap()),
            "应至少枚举到 lo/127.0.0.1，实际: {ips:?}"
        );
    }

    /// 真机出网探测（需外网），默认跳过：打印两个 DNS 探测器的原始返回
    #[test]
    #[ignore]
    fn live_public_probe_raw() {
        use super::{build_txt_query, parse_txt_answer};
        use std::net::UdpSocket;
        use std::time::{Duration, SystemTime};
        for (server, qname) in [
            ("1.1.1.1:53", "whoami.cloudflare"),
            ("8.8.8.8:53", "o-o.myaddr.l.google.com"),
        ] {
            let raw = (|| -> Option<Vec<u8>> {
                let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
                sock.set_read_timeout(Some(Duration::from_millis(1500)))
                    .ok()?;
                let id = (SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .ok()?
                    .subsec_nanos() as u16)
                    | 1;
                sock.send_to(&build_txt_query(id, qname), server).ok()?;
                let mut buf = [0u8; 1024];
                let (n, _) = sock.recv_from(&mut buf).ok()?;
                Some(buf[..n].to_vec())
            })();
            match raw {
                Some(b) => println!(
                    "{server} {qname}: {}B parsed={:?}",
                    b.len(),
                    parse_txt_answer(&b)
                ),
                None => println!("{server} {qname}: 无响应/发送失败"),
            }
        }
        println!("probe_public_v4() = {:?}", super::probe_public_v4());
    }
}
