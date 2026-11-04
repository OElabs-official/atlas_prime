# 网卡流量采集通道

> 适用：Termux / Android（atlas 以 untrusted_app 身份常驻）。
> 结论先行：快照源链四级回退 **直读 → su 桥 → sysfs → netlink**；root 设备（Magisk 等）走 su 桥长期稳定，
> 无 root 设备干净降级（速度组为空、双环空闲）。adb 回环桥与 Shizuku 均已退役，见 §4 历史备注。

## 1. 架构（四级回退）

```
operator 1Hz 每拍：
  ① 直读     read /proc/net/dev（自身 uid）
  ② su 桥    su -c "cat /proc/net/dev"（1.5s 超时钳）
  ③ sysfs    /sys/class/net/*/statistics/{rx,tx}_bytes
  ④ netlink  RTM_GETLINK（`ip -s link` 同源）
        │
        └─ 差分聚合 → 缓存(Operator::net_speeds) + db kind=net（组级 <组>.rx_bps / <组>.tx_bps）
```

| 级 | 通道 | 适用域 |
|---|---|---|
| ① 直读 | `/proc/net/dev` 自身 uid | 老内核/放行 ROM 可直读，代价最低；Android 16+ 对 untrusted_app 一律 EACCES，落到下一级 |
| ② su 桥 | `su -c "cat /proc/net/dev"` | root 设备（Magisk 已实测；KSU/APatch 同路径）。当前主力级 |
| ③ sysfs | `statistics/rx_bytes` 等 | `/proc/net/dev` 被封但 sysfs 未封的部分 ROM |
| ④ netlink | RTM_GETLINK | 末位兜底；新 Android 非特权 bind 即 EACCES |

采集参数：**每拍现采**（operator 1Hz 循环）、su 子进程 **1.5s 超时钳**，超时弃本拍下拍重试，不阻塞。
计量分组：`wlan*`→`wifi`、`rmnet_data*`/`rmnet_link*`/`rmnet[0-9]*`→`cellular`、有流量的其他网卡（usb0/eth*/rndis*…）自组。

无 root 设备：① EACCES → ② su spawn **ENOENT 瞬时失败**（不等超时，记 `NetError::NoSu`）→ ③④ 亦被封锁
→ 速度组为空、面板双环空闲——**干净降级，无虚假读数、无挂起**。

## 2. su 要求与验证

| 项 | 说明 |
|---|---|
| 命令 | 固定 `su -c "cat /proc/net/dev"`，每拍 spawn 一次，无 shell 嵌套 |
| 要求 | 任意 root 管理器；Magisk 实测：untrusted_app 下 `su -c id` 回 `uid=0 context=u:r:magisk:s0`，`/proc/net/dev` 全量正文（wlan0/rmnet_* 行齐） |
| 手动验证 | 终端跑 `su -c id`，应回 `uid=0`；否则 atlas 无法走 ② |
| 授权持久化 | Magisk 设置 → 应用列表 → 终端（com.termux）root 权限设为**允许/永久**，避免每次 spawn 弹框 |
| 首次授权 | Magisk 首弹框可能让**第一拍超时**（1.5s 钳到点）——预期行为，授权后下一拍自愈，无需重启 atlas |

## 3. 故障速查

| 症状 | 处置 |
|---|---|
| 双环全 0（无 root 设备） | 预期：四级皆不可读，空闲即正确状态 |
| 双环全 0（root 设备） | 终端 `su -c id` 确认回 uid=0 → 查 Magisk 授权记录/拒绝名单是否挡了终端 |
| 单环 0 | 查对应网卡组：wifi→`wlan*`、cellular→`rmnet*`；网卡本身未走流量则为 0，属正常 |
| 装机/首次授权后第一拍超时 | Magisk 弹框阻塞，1.5s 钳到——预期，下一拍自愈 |

## 4. 历史备注

### adb 回环桥（2026-10 移除）

前身链路：直读 → EACCES → 回环 adb（shell uid=2000、`u:r:shell:s0`）`cat /proc/net/dev`，
实测 ~46ms/读、1Hz 无压力。**移除理由：无线调试连接端口在切换网络/重启/关闭无线调试后必变**，
每次都要人工重新配对——为兜住此事养了「无线配对」任务（adb-pair 悬浮窗收码）+ `adb_port` 端口文件 +
adb-watch 看门狗三件套，本质仍是持续人工维护，体验差。su root 桥免维护：root 设备一次授权永久生效。
配套运行时已一并移除：`atlas_task.json` 删 `adb_watch`/`wireless_pair` 两任务，
`~/.local/bin/adb-watch`、`~/.local/bin/adb-pair`、`~/.local/share/atlas_prime/adb_port` 已删。

### Shizuku（更早弃用）

再早的兜底走 Shizuku（rish 通道，附带「无线调试自动回活」）。atlas 数据面彼时已只依赖原生 adb 客户端，
Shizuku 仅剩回活一层保险，自身维护成本超过收益。三坑留档：
① ART 拒载可写 dex（须 `chmod 400`，apt 升级会重置）；
② app_process 被 Termux glibc 环境 `LD_*` 污染（SIGABRT，须 `env -i` 净化）；
③ 13.5.4 rish 加载器被新 ART 拒（"Unsupported class loader"，须手挖 `librish.so`）。

## 5. GPU 频率/使用率（root 模式同源）

GPU 域与测速 ② 级同走 su 桥（`su -c` 单条 for 循环整抓、`@@@路径` 分段、1.5s 钳、每拍一次 fork；
老内核/放行 ROM 直读即中零 fork，链序与纪律见 `system/gpu.rs`）。

- **主源 kgsl**：`/sys/class/kgsl/kgsl-3d0/{gpu_busy_percentage,clock_mhz,max_gpuclk}`（使用率%、当前 MHz、封顶 Hz）；
- **兜底 devfreq**：`/sys/class/devfreq/{*gpu*,*kgsl*}/{cur_freq,max_freq}`（Hz 整型；实测 `load` 文件为空——空/缺席一律按「无使用率」，永不算错）；
- **差分备选**：`kgsl-3d0/gpubusy` 累计对 Δbusy/Δtotal×100 钳 0..=100，首读无使用率；
- 频率是锚：两源频率俱缺 → `available:false`。**无 root 卡片自动隐藏（前端契约）**，静默缺席不告警；
- 出口：web `GET /system/gpu`（普通桶，不可用亦 200）；落库 kind=`gpu`（`load_pct`/`freq_mhz`，仅 Some 值写行，2min/轮）；
- 与 thermal 的 `temp.gpu.N` 区分：那是 gpuss-* 温度（℃），本域是频率（MHz）/使用率（%）——温度≠频率/使用率。

## 6. 进程观测（GET /system/proc）

无 root 下 /proc 按 uid 遮蔽：目录枚举只列本 uid（Termux 自家）进程，本域只呈这一视野——非缺漏，域规则，无 su 桥腿。
CPU 账走 `/proc/<pid>/stat` 的 utime+stime（comm 可含空格括号，按最后一个 `)` 切野），内核用户时钟 **USER_HZ=100**
（ticks÷100=CPU 秒），跨拍差分 `Δticks÷USER_HZ÷Δ墙钟秒×100`（首见/pid 复用=0.0，基线每拍整表重建）。
节拍：操作器第三条回路 **5s 一轮**，`spawn_blocking` 现扫（/proc 批量小文件读），产物整拍换入缓存——
**不落库**（只观测不存储，与 1Hz/120s 两回路互不掺和）。出口：web `GET /system/proc` 纯缓存读、永 200、`procs` 按 rss 降序；
首轮前 `reason:"warming"`。实现见 `system/proc.rs`。
