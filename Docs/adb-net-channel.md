# 网卡流量采集通道：回环 adb（Android 16 实战记录）

> 适用：SM-S9380 / Android 16 (SDK 36) / One UI 8 / 无 root / Termux(glibc 环境)。
> 结论先行：**普通 Termux 进程读不到任何网卡流量计数器**，唯一免 root 通路是
> **回环 adb（shell uid=2000）**；无线调试由人工开启、由「无线配对」任务维护端口，
> Shizuku 已退役（弃用理由见 §4 历史备注）。

## 1. 问题：Android 16 全面封锁流量计数器

以普通 app uid 尝试的全部内核读数路径（2026-10 实测）：

| 路径 | 结果 |
|---|---|
| `/proc/net/dev`、`/proc/net/snmp`、`/proc/net/netstat`、`/proc/net/wireless` | EACCES |
| `/sys/class/net/*`（目录列举）及 `statistics/rx_bytes`（精确路径） | EACCES |
| netlink（`ip -s link`、`ss`）| bind 即 Permission denied |
| 老梗 `/proc/net/xt_qtaguid/stats`、`xt_traffic_stats` | 不存在/EACCES |
| `su`（termux 桩）| 设备无 root |

无 root 时 `adb shell`（uid=2000、`u:r:shell:s0`）是唯一能读上述文件的身份。

## 2. 方案架构（无 Shizuku）

```
adbd（系统服务，随「无线调试」开关存亡；回环端口见 adb_port 文件）
  ▲  adb -s localhost:<port> shell cat /proc/net/dev   ~46ms/次，1Hz 无压力
  │
atlas (system::network 快照源链：直读 → EACCES → adb 兜底)
  │        │
  │        └─ operator 1Hz 差分 → 缓存(Operator::net_speeds) + db kind=net
  │
adb-watch（~/.local/bin/adb-watch，任务引擎常驻 autostart）
     探活 = 真数据通道本身：adb shell cat /proc/net/dev 须回含 wlan0 的正文
     （10s 超时）；死 → adb connect 重试，不拉起任何东西——adbd 是系统服务，
     无线调试开着它就在；连续失败 ≥3 次发通知（30 分钟节流）。
```

通道来源就两条腿：

- **人工**：开发者选项 → 无线调试 → 开；
- **「无线配对」任务**（控制台 SERVICE 组按钮，`atlas_task.json` id=`wireless_pair`）：
  `~/.local/bin/adb-pair` 用 termux-float 悬浮窗收 pair_port / pin / connect_port
  → `adb pair` → `adb connect` → 实读 `/proc/net/dev` 验证 → 把 connect_port
  写入 adb_port 并通知「无线通道已恢复」。每步失败通知 adb stderr 首行，退出码分环节。

关键文件：

- `~/.local/share/atlas_prime/adb_port` —— 连接端口，单行数字。**重启手机/关无线调试后端口会变**：跑一次「无线配对」任务，或人工 `echo <新端口> > ~/.local/share/atlas_prime/adb_port`（看门狗与 atlas 每拍现读该文件，改完即生效、无需重启任何进程）。
- `~/.local/bin/adb-watch` —— 通道看门狗（探的是读数能力本身，不是某个进程活没活）。
- `~/.local/bin/adb-pair` —— 配对/换端口流程（需悬浮窗权限 + Termux:API app）。

手机端保命符：无线调试保持常开；开发者选项开「撤销 USB 调试授权永不超时」
（防 adb 授权过期）；Termux 电池无限制 + 最近任务锁定（保 atlas 与看门狗）。

## 3. 一次性布置（已在本机执行）

```sh
adb pair localhost:<配对端口> <6位码>     # 无线调试→使用配对码配对设备（此后走「无线配对」任务即可）
adb connect localhost:<连接端口>          # 无线调试主页 IP:端口
echo <连接端口> > ~/.local/share/atlas_prime/adb_port
```

（历史上的 `pm grant moe.shizuku.privileged.api …` 四条授权命令随 Shizuku 退役一并作废。）

## 4. 历史备注（Shizuku 路线为何弃用）

最初链路是 Shizuku 兜底：给 Shizuku app 授 `WRITE_SECURE_SETTINGS` 等权限，让它
开机自动重开无线调试并自启 `shizuku_server`，读数走 rish 通道。实际运行后弃用，
理由：atlas 数据面早已只依赖原生 `adb` 客户端（无需 Java、无需 dex），Shizuku
提供的只剩「无线调试自动回活」这一层保险，而这层保险自身的维护成本（下述三坑 +
常驻 app + 保命符）超过了它省下的人工——重启后反正也要人工跑一次「无线配对」刷新
端口，保险便不再值钱。三连坑留档为教训：

1. **ART 拒载可写 dex**：`rish-shizuku.dex` 必须 `chmod 400`（连 600 都不行，
   "Writable dex file is not allowed"，logcat 里见 SecurityException）；apt 升级包
   会重置权限。教训：Android 运行时对可执行资产的权限比对功能要求还严。
2. **app_process 被 glibc 环境污染**：Termux 的 `LD_*` 变量会炸 bionic 启动
   （SIGABRT），必须 `env -i` 净化后再 exec。教训：跨 libc 边界 exec 必须清环境。
3. **13.5.4 的 rish Java 加载器被新 ART 拒**："Unsupported class loader:
   BaseDexClassLoader" + 需从 APK 手挖 `librish.so`；官方 13.6 改用原生 starter
   二进制才规避。教训：绑死在私有 ART 接口上的桥，每次系统升级都是雷。

## 5. 故障速查

| 症状 | 处置 |
|---|---|
| 双环不显示 / `speed:{}` | `cat ~/.local/share/atlas_prime/adb_port` 核对端口 → `adb connect localhost:<port>` → `adb -s localhost:<port> shell cat /proc/net/dev`；端口没把握就**跑「无线配对」任务** |
| 收到「ATLAS · 无线通道失联」通知 / 刚重启过手机 | 无线调试被关或端口已变 → **跑「无线配对」任务**（或把新连接端口写入 `adb_port` 文件）；无线调试若整个被关，先在开发者选项里打开 |
| 「无线配对」报配对失败 | 配对码/配对端口过期（配对窗口只有一分多钟）：回无线调试页重新弹码、立即重跑任务 |
| adb connect 报 offline | `adb disconnect && adb connect`；还不行就无线调试页重开开关，再跑「无线配对」 |
| 「无线配对」弹不出悬浮窗 | 检查 Termux:API app 已装 + 悬浮窗权限已授（`adb-pair` 退出码 1） |
| rish / shizuku_server 残影 | 历史遗留，可直接卸载 Shizuku app，通道不依赖它 |

## 6. 已知残余限制

- 无线调试的**新端口无法被看门狗自动发现**（Termux 无 mDNS 客户端）——重启/关开
  无线调试后需人工跑一次「无线配对」任务（或手改端口文件）；Shizuku 退役后
  「无线调试被系统杀掉」也无人自动回活，保命符（§2）要做全。
- adb shell 读到的计数器仍是全量网卡；无 per-app 拆分。
- 自 rev 2026-10-20.28 起数据面按**组**聚合计量与落库：`wlan*`→`wifi`、`rmnet_data*`/`rmnet_link*`/`rmnet[0-9]*`→`cellular`、有流量的其他网卡（usb0/eth*/rndis*…）自组；db `kind=net` 历史键相应改为组级 `<组>.rx_bps`/`<组>.tx_bps`（旧 per-iface 行不迁移，随 ≈8h 窗口自然出清）。
