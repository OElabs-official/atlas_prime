🚀 硬件监控模块 (Hardware Monitor)

由ai生成，可能会包含错误。

该模块负责实时采集系统资源数据，并采用异步任务与事件驱动架构，将数据流推送至 UI 组件进行渲染。
📋 核心特性

    跨平台深度适配：

        通用 (Windows/Linux/macOS)：通过 sysinfo 采集基础的 CPU 负载、内存 (RAM) 与交换空间 (Swap) 使用情况。

        Android (Termux/Root)：支持通过 /sys/class/thermal 采集核心温度，通过 /proc 采集各核频率，并集成 termux-api 获取电池状态（电量、温度、充电状态）。

    多周期同步逻辑 (Multi-Tick Strategy)： 为了平衡“实时性”与“系统负载”，监控任务采用了分级采样：

        短周期 (Short-term)：每 1s 采集一次，用于波形图（Sparkline）的实时跳动。

        中周期 (Mid-term)：每 10s 采集一次，用于磁盘空间、IP 地址等变动较慢的数据。

        长周期 (Long-term)：每 30s 采集一次，用于绘制长趋势图表，帮助分析资源随时间的演变过程。

    高性能异步架构：

        使用 tokio::spawn 隔离采集任务，确保 IO 读取不阻塞 UI 渲染循环。

        数据对齐优化：长短周期共享同一份采样负载（Payload），消除因重复读取导致的趋势偏移。

        预热机制：启动即进行全量同步采集，杜绝 TUI 启动时的空白期。

🛠 架构设计

模块主要由以下几个部分组成：

    spawn_monitor_task：主入口函数，管理 tokio::time::interval 循环。

    数据采集器 (Collectors)：

        collect_android_cpu(): 针对移动端的频率与热传感器读取。

        collect_disks(): 磁盘挂载点与空间统计。

    事件总线 (Event Bus)：通过 GlobSend 将封装好的 DynamicPayload 发送至全局事件分发中心。

📊 数据结构

监控数据封装在 GlobalEvent::Data 中，使用 Arc<T> 确保跨线程传输的低开销：
键名 (Key)	数据内容	采样频率
MEM_SWAP	(UsedMem, UsedSwap)	1s
ANDROID_CPU	(Freqs, Temp0, Temp7)	1s
DISK_IP	(Vec<DiskInfo>, Vec<String>)	10s
ANDROID_BAT	(Percentage, Status, Temp)	30s
📦 依赖项

    sysinfo: 跨平台硬件信息获取。

    tokio: 异步运行时调度。

    termux-api (Optional): 仅在 Android 环境下用于电池信息采集。

💡 如何扩展

如果你想增加新的监控项（例如 GPU 占用）：

    在子函数中实现采集逻辑。

    在 spawn_monitor_task 循环中选择合适的 tick_count 频率。

    定义新的 key 常量并发送。

    