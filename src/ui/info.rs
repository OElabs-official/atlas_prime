use crate::{
    config::{AppColor, Config, SharedConfig}, constans::{
         HISTORY_CAP, INFO_UPDATE_INTERVAL_BASE, INFO_UPDATE_INTERVAL_SLOW_TIMES, INFO_UPDATE_INTERVAL_SLOWEST
    }, prelude::*, 
};
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent};
use directories::{BaseDirs, UserDirs};
use ratatui::{prelude::*, symbols::block, widgets::*};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use sysinfo::{Disks, System};
use tokio::sync::{broadcast, mpsc};
// use crate::db::Mongo;
use sqlx::{sqlite::SqliteRow, Row};


const COLL_NAME: &str = "telemetry_history"; // database collections


// 增加长周期数据 Key
const MEM_SWAP_LONG: &str = "mem_swap_long";
const ANDROID_CPU_LONG: &str = "android_cpu_long";
const ANDROID_BAT: &str = "android_bat";
pub type AndroidBatInfo = (u8, String, f64); // (电量百分比, 充放电状态String, 电池温度f32)
const ANDROID_CPU: &str = "android_cpu";
type CpuInfo = (Vec<f32>, f32, f32); // (各核心频率Vec<f32>, Zone0温度f32, Zone7温度f32)
const MEM_SWAP: &str = "mem_swap";
type MemSwapMB = (u64, u64);
const DISK_IP: &str = "disk_ip";
// 修改类型定义，将 IP 分为 (IPv4列表, IPv6列表)
type IPData = (Vec<String>, Vec<String>);
type DiskIP = (Vec<DiskInf>, IPData);
type DiskInf = (String, u64, u64, String);
pub struct InfoComponent {
    glob_recv: GlobRecv,

    // 数据存储
    mount_points: Vec<DiskInf>,
    dir_list: Vec<String>,
    ip_list: (Vec<String>, Vec<String>),

    // UI 状态
    focus_index: Option<usize>, // 0: Mounts, 1: Dirs, 2: IPs
    scroll_offsets: [u16; 3],

    total_mem_swap_mb: (u64, u64),
    mem_swap_history: VecDeque<(u64, u64)>,
    mem_swap_long_history: VecDeque<(u64, u64)>,
    // Android 专用数据存储
    bat_history: VecDeque<AndroidBatInfo>,
    cpu_info_history: VecDeque<CpuInfo>,
    cpu_info_long_history: VecDeque<CpuInfo>,

    system_info: String, // 例如: "Android 14"
}

impl InfoComponent // rende part uis
{
    fn render_ip_addresses(&self, f: &mut Frame, area: Rect) {
        let (v4, v6) = &self.ip_list;

        // 创建包裹容器
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" 🌐 IP Addresses (Left: v4 | Right: v6) ")
            .border_style(if self.focus_index == Some(2) {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            });

        let inner_area = block.inner(area);
        f.render_widget(block, area);

        // 在容器内部进行横向切分
        let chunks = Layout::horizontal([
            Constraint::Percentage(45), // v4 区域
            Constraint::Length(1),      // 分隔符
            Constraint::Percentage(54), // v6 区域
        ])
        .split(inner_area);

        // 渲染 IPv4
        f.render_widget(
            Paragraph::new(v4.join("\n"))
                .style(Style::default().fg(Color::Cyan))
                .scroll((self.scroll_offsets[2], 0)),
            chunks[0],
        );

        // 渲染中间分隔线
        f.render_widget(
            Paragraph::new("│\n".repeat(chunks[1].height as usize))
                .style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );

        // 渲染 IPv6
        f.render_widget(
            Paragraph::new(v6.join("\n"))
                .style(Style::default().fg(Color::LightGreen)) // v6 通常不常用，颜色调淡
                .scroll((self.scroll_offsets[2], 0)),
            chunks[2],
        );
    }

    fn render_mem_swap_status(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        let (mem_total, swap_total) = self.total_mem_swap_mb;
        // 获取最新数值用于标题展示
        let (mem_last, swap_last) = self.mem_swap_history.back().unwrap_or(&(0, 0));

        for (i, is_mem) in [true, false].iter().enumerate() {
            let inner_chunks =
                Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)])
                    .split(chunks[i]);

            // 关键：根据当前 UI 块的宽度决定采样点数
            let width = inner_chunks[0].width as usize;

            let title = if *is_mem { " 📟 RAM" } else { " 🔁 SWAP" };
            let last_val = if *is_mem { mem_last } else { swap_last };
            let total = if *is_mem { mem_total } else { swap_total };
            let color = if *is_mem { Color::Blue } else { Color::Magenta };

            // 1. 渲染短周期 (上) - 采样最新的数据
            let data_s: Vec<u64> = self
                .mem_swap_history
                .iter()
                .map(|(m, s)| {
                    let val = if *is_mem { *m } else { *s };
                    if total > 0 { val * 100 / total } else { 0 }
                })
                .rev()
                .take(width)
                .rev() // 只取最新可见部分
                .collect();

            f.render_widget(
                Sparkline::default()
                    .data(&data_s)
                    .max(100)
                    .style(Style::default().fg(color))
                    .block(
                        Block::default()
                            .title(format!(" {}: {}/{}MB  ", title, last_val, total))
                            .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP),
                    ),
                inner_chunks[0],
            );

            // 2. 渲染长周期 (下) - 采样最新的数据
            let data_l: Vec<u64> = self
                .mem_swap_long_history
                .iter()
                .map(|(m, s)| {
                    let val = if *is_mem { *m } else { *s };
                    if total > 0 { val * 100 / total } else { 0 }
                })
                .rev()
                .take(width)
                .rev() // 只取最新可见部分
                .collect();

            f.render_widget(
                Sparkline::default()
                    .data(&data_l)
                    .max(100)
                    .style(Style::default().fg(color).add_modifier(Modifier::DIM)) // 调暗颜色区分
                    .block(
                        Block::default()
                            .title(format!(" {} (Long Trend) ", title))
                            .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM),
                    ),
                inner_chunks[1],
            );
        }
    }

    fn render_cpu_status(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        // 获取最新数据点
        let default_cpu = (vec![0.0; 8], 0.0, 0.0);
        let (freqs, _z0, z7) = self.cpu_info_history.back().unwrap_or(&default_cpu);
        let width = chunks[0].width.saturating_sub(2) as usize;

        // --- 左侧：频率采集 (映射 5GHz -> 100) ---
        let left_chunks =
            Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)])
                .split(chunks[0]);
        let max_cur_freq = freqs.iter().cloned().fold(0.0, f32::max);

        let freq_data: Vec<u64> = self
            .cpu_info_history
            .iter()
            .map(|(fs, _, _)| {
                let max = fs.iter().cloned().fold(0.0, f32::max);
                ((max / 5.0) * 100.0) as u64 // 5.0GHz 映射为 100%
            })
            .rev()
            .take(width)
            .rev()
            .collect();

        f.render_widget(
            Sparkline::default()
                .data(&freq_data)
                .max(100)
                .style(Style::default().fg(Color::Yellow))
                .block(
                    Block::default()
                        .title(format!(" ⚡ CPU Freq: {:.1}GHz (Max) ", max_cur_freq))
                        .borders(Borders::LEFT | Borders::TOP | Borders::RIGHT),
                ),
            left_chunks[0],
        );

        // 底部文字显示所有核心频率
        let freqs_text = freqs
            .iter()
            .map(|f| format!("{:.1}", f))
            .collect::<Vec<_>>()
            .join("|");
        f.render_widget(
            Paragraph::new(freqs_text).block(
                Block::default()
                    .title(" All Cores ")
                    .borders(Borders::LEFT | Borders::BOTTOM | Borders::RIGHT),
            ),
            left_chunks[1],
        );

        // --- 右侧：温度采集 (映射 10°C-90°C -> 0-100) ---
        let right_chunks =
            Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)])
                .split(chunks[1]);
        let map_temp = |t: f32| (((t - 10.0) / (90.0 - 10.0)) * 100.0).clamp(0.0, 100.0) as u64;

        let temp_s: Vec<u64> = self
            .cpu_info_history
            .iter()
            .map(|(_, _, z)| map_temp(*z))
            .rev()
            .take(width)
            .rev()
            .collect();
        let temp_l: Vec<u64> = self
            .cpu_info_long_history
            .iter()
            .map(|(_, _, z)| map_temp(*z))
            .rev()
            .take(width)
            .rev()
            .collect();

        f.render_widget(
            Sparkline::default()
                .data(&temp_s)
                .max(100)
                .style(Style::default().fg(Color::Red))
                .block(
                    Block::default()
                        .title(format!(" 🌡️ Temp: {:.1}°C   ", z7))
                        .borders(Borders::LEFT | Borders::TOP | Borders::RIGHT),
                ),
            right_chunks[0],
        );

        f.render_widget(
            Sparkline::default()
                .data(&temp_l)
                .max(100)
                .style(Style::default().fg(Color::Red).add_modifier(Modifier::DIM))
                .block(
                    Block::default()
                        .title(" 🌡️  Temp (Long Trend) ")
                        .borders(Borders::LEFT | Borders::BOTTOM | Borders::RIGHT),
                ),
            right_chunks[1],
        );
    }

    fn render_battery_status(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        let width = chunks[0].width.saturating_sub(2) as usize;
        let default_bat = (0u8, String::from("N/A"), 0.0f64);
        let (pct, _status, temp) = self.bat_history.back().unwrap_or(&default_bat);

        // 左侧：剩余电量历史 (基于已存储的长周期 bat_history)
        let bat_data: Vec<u64> = self
            .bat_history
            .iter()
            .map(|(p, _, _)| *p as u64)
            .rev()
            .take(width)
            .rev()
            .collect();

        f.render_widget(
            Sparkline::default()
                .data(&bat_data)
                .max(100)
                .style(Style::default().fg(Color::Green))
                .block(
                    Block::default()
                        .title(format!(" 🔋 Battery: {}% ", pct,))
                        .borders(Borders::ALL),
                ),
            chunks[0],
        );

        // 右侧：电池温度历史 (映射 20°C-50°C 常用区间)
        let bat_temp_data: Vec<u64> = self
            .bat_history
            .iter()
            .map(|(_, _, t)| ((*t - 20.0).max(0.0) * (100.0 / 30.0)) as u64)
            .rev()
            .take(width)
            .rev()
            .collect();

        f.render_widget(
            Sparkline::default()
                .data(&bat_temp_data)
                .max(100)
                .style(Style::default().fg(Color::Yellow))
                .block(
                    Block::default()
                        .title(format!("🌡️ Bat Temp: {:.1}°C ", temp))
                        .borders(Borders::ALL),
                ),
            chunks[1],
        );
    }



    fn render_disk_list(&self, f: &mut Frame, area: Rect) {
        // --- 1. 使用缓存数据，不再调用 Disks::new() ---
        let mut sorted_disks = self.mount_points.clone();

        // --- 2. 保持原有的跨平台排序逻辑 ---
        #[cfg(not(target_os = "windows"))]
        sorted_disks.sort_by(|a, b| b.1.cmp(&a.1)); // 按总空间排序 (DiskInf.1 是 total_space)

        #[cfg(target_os = "windows")]
        sorted_disks.sort_by(|a, b| a.3.cmp(&b.3)); // 按挂载点路径排序 (DiskInf.3 是 mount_point)

        // --- 3. 计算分页与显示范围 ---
        let offset = self.scroll_offsets[0] as usize;
        let visible_height = area.height.saturating_sub(2) as usize;

        let displayed_disks = sorted_disks.iter().skip(offset).take(visible_height);

        // --- 4. 构造列表项 (逻辑保持一致，仅数据源切换为 DiskInf 元组) ---
        let items: Vec<ListItem> = displayed_disks
            .map(|(name, total, available, mount_point)| {
                let used = total.saturating_sub(*available);
                let pct = if *total > 0 {
                    (used as f64 / *total as f64)
                } else {
                    0.0
                };

                // 进度条渲染
                let bar_width = 12;
                let filled = (pct * bar_width as f64).round() as usize;
                let empty = bar_width - filled;
                let bar_str = format!("[{}{}] ", "█".repeat(filled), "░".repeat(empty));

                // 颜色策略
                let color = if pct > 0.9 {
                    Color::Red
                } else if pct > 0.7 {
                    Color::Yellow
                } else {
                    Color::Green
                };

                // 文本格式化
                let info_text = format!(
                    "{:>5.1}% {:>6.1} GB  {:<15}",
                    pct * 100.0,
                    *total as f64 / 1024.0 / 1024.0 / 1024.0,
                    mount_point
                );

                ListItem::new(Line::from(vec![
                    Span::styled(bar_str, Style::default().fg(color)),
                    Span::raw(info_text),
                    Span::styled(format!(" ({})", name), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();

        // --- 5. 渲染组件 ---
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" 🗄️ Mount Points ")
                .border_style(if self.focus_index == Some(0) {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                }),
        );

        f.render_widget(list, area);
    }
}

impl Component for InfoComponent {
    fn init() -> Self
    where
        Self: Sized,
    {
        // 1. 瞬间初始化空队列
        let mut db_cpu = VecDeque::with_capacity(HISTORY_CAP);
        let mut db_mem = VecDeque::with_capacity(HISTORY_CAP);
        let mut db_bat = VecDeque::with_capacity(HISTORY_CAP);

        // 默认补齐，保证渲染不崩溃
        for _ in 0..HISTORY_CAP {
            db_cpu.push_back(Default::default());
            db_mem.push_back(Default::default());
            db_bat.push_back(Default::default());
        }

        // 2. 瞬间获取系统静态信息
        let mut sys = System::new_all();
        sys.refresh_all();
        let system_info = format!("{}*{}*{}*{}", 
            System::cpu_arch(),
            System::name().unwrap_or_default(),
            System::kernel_long_version().split('-').next().unwrap_or(""),
            System::os_version().unwrap_or_default()
        );

        // 3. 关键：启动两个异步任务，一个抓取历史，一个持续监控
        Self::spawn_history_fetch_task(); // 新增：后台抓历史
        Self::spawn_monitor_task();       // 持续采样

        Self {
            glob_recv: GlobIO::recv(),
            mount_points: Default::default(),
            dir_list: ProjectPath::collect_dirs(),
            ip_list: Default::default(),
            focus_index: Some(0),
            scroll_offsets: [0, 0, 0],
            total_mem_swap_mb: (sys.total_memory() / 1024 / 1024, sys.total_swap() / 1024 / 1024),
            mem_swap_history: db_mem.clone(),
            mem_swap_long_history: db_mem,
            cpu_info_history: db_cpu.clone(),
            cpu_info_long_history: db_cpu,
            bat_history: db_bat,
            system_info,
        }
    }
        

    /// 接受广播定期回传的信息
    fn update(&mut self) -> bool {
        /*
        要使 update 函数返回合理的 bool 值，核心逻辑是：只要任何一个数据源（MPSC 通道或 Broadcast 频道）在本次调用中产生了新数据，就将标志位设为 true。
        如果不返回 true，主循环就不会触发重绘，用户也就看不到最新的 CPU 使用率或进度条变化。
        */
        let mut changed = false;

        // 持续尝试接收来自全局通道的所有事件
        while let Ok(event) = self.glob_recv.try_recv() {
            match event {

                GlobalEvent::Data { key, data } => {
                    match key {
                        // 在 update 的 match key 逻辑中增加：
                        "HISTORY_REFILL" => {
                            if let Some(records) = data.0.downcast_ref::<Vec<TelemetryRecord>>() {
                                self.cpu_info_history.clear();
                                self.mem_swap_history.clear();
                                self.bat_history.clear();
                                
                                for r in records.iter().rev() {
                                    self.cpu_info_history.push_back(r.cpu_data.clone());
                                    self.mem_swap_history.push_back(r.mem_swap);
                                    self.bat_history.push_back(r.battery_data.clone());
                                }
                                // 再次补齐，防止数据量不足 HISTORY_CAP
                                while self.cpu_info_history.len() < HISTORY_CAP { self.cpu_info_history.push_front(Default::default()); }
                                // ... 对其他队列执行相同补齐操作
                                changed = true;
                            }
                        }

                        // --- 1. 内存与 Swap (短周期) ---
                        MEM_SWAP => {
                            if let Some(pkg) = data.0.downcast_ref::<MemSwapMB>() {
                                self.mem_swap_history.push_back(*pkg);
                                if self.mem_swap_history.len() > HISTORY_CAP {
                                    self.mem_swap_history.pop_front();
                                }
                                changed = true;
                            }
                        }
                        // --- 2. 内存与 Swap (长周期) ---
                        MEM_SWAP_LONG => {
                            if let Some(pkg) = data.0.downcast_ref::<MemSwapMB>() {
                                self.mem_swap_long_history.push_back(*pkg);
                                if self.mem_swap_long_history.len() > HISTORY_CAP {
                                    self.mem_swap_long_history.pop_front();
                                }
                                changed = true;
                            }
                        }
                        // --- 3. CPU 核心、温度 (短周期) ---
                        ANDROID_CPU => {
                            if let Some(pkg) = data.0.downcast_ref::<CpuInfo>() {
                                self.cpu_info_history.push_back(pkg.clone());
                                if self.cpu_info_history.len() > HISTORY_CAP {
                                    self.cpu_info_history.pop_front();
                                }
                                changed = true;
                            }
                        }
                        // --- 4. CPU 核心、温度 (长周期) ---
                        ANDROID_CPU_LONG => {
                            if let Some(pkg) = data.0.downcast_ref::<CpuInfo>() {
                                self.cpu_info_long_history.push_back(pkg.clone());
                                if self.cpu_info_long_history.len() > HISTORY_CAP {
                                    self.cpu_info_long_history.pop_front();
                                }
                                changed = true;
                            }
                        }
                        // --- 5. 电池数据 (长周期) ---
                        ANDROID_BAT => {
                            if let Some(pkg) = data.0.downcast_ref::<AndroidBatInfo>() {
                                self.bat_history.push_back(pkg.clone());
                                if self.bat_history.len() > HISTORY_CAP {
                                    self.bat_history.pop_front();
                                }
                                changed = true;
                            }
                        }
                        // --- 6. 磁盘与 IP ---
                        DISK_IP => {
                            if let Some((disks, ips)) = data.0.downcast_ref::<DiskIP>() {
                                self.mount_points = disks.clone();
                                self.ip_list = ips.clone(); // 此时 ips 是 (Vec<String>, Vec<String>)
                                changed = true;
                            }
                        }

                        _ => {}
                    }
                }
                _ => {}
            }
        }
        changed
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        // 1. 总体纵向分割：顶部图表区(6行) + 下部内容区(剩余)
        // 此时 main_chunks 只有两个索引：0 和 1
        let main_chunks = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(1),
        ])
        .split(area); //;

        let mut main_chunks_cnt = main_chunks.iter();

        {
            if let Some(area) = main_chunks_cnt.next() {
                // 再次切分列表区域并转为迭代器
                let list_chunks = Layout::vertical([
                    Constraint::Percentage(40),
                    Constraint::Percentage(40),
                    Constraint::Percentage(20),
                ])
                .split(*area);
                //.into_iter();

                self.render_disk_list(f, list_chunks[0]);

                {
                    // 目录渲染
                    f.render_widget(
                        Paragraph::new(self.dir_list.join("\n"))
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .title(" 📂 Directories ")
                                    .border_style(if self.focus_index == Some(1) {
                                        Style::default()
                                            .fg(Color::Yellow)
                                            .add_modifier(Modifier::BOLD)
                                    } else {
                                        Style::default().fg(Color::Gray)
                                    }),
                            )
                            .scroll((self.scroll_offsets[1], 0)),
                        list_chunks[1],
                    );
                }
                self.render_ip_addresses(f, list_chunks[2]);
            }
        }

        // 磁盘渲染

        // 剩下的 chunks 严格对应 main_chunks 定义的顺序
        if let Some(a) = main_chunks_cnt.next() {
            self.render_mem_swap_status(f, *a);
        }
        if let Some(a) = main_chunks_cnt.next() {
            self.render_cpu_status(f, *a);
        }
        if let Some(a) = main_chunks_cnt.next() {
            self.render_battery_status(f, *a);
        }
        {
            if let Some(area) = main_chunks_cnt.next() {
                f.render_widget(
                    Paragraph::new(self.system_info.clone())
                        .alignment(Alignment::Right)
                        .style(
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ),
                    *area,
                );
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if let Some(ref mut idx) = self.focus_index {
            match key.code {
                KeyCode::Tab => {
                    *idx = (*idx + 1) % 3;
                    true
                }
                KeyCode::Up => {
                    self.scroll_offsets[*idx] = self.scroll_offsets[*idx].saturating_sub(1);
                    true
                }
                KeyCode::Down => {
                    self.scroll_offsets[*idx] = self.scroll_offsets[*idx].saturating_add(1);
                    true
                }
                _ => false,
            }
        } else {
            false
        }
    }
}



impl InfoComponent { // 辅助采集函数


    /// 在info 初始化时建立长期任务，定期发送系统信息
    fn spawn_monitor_task() {
        tokio::spawn(async move {
            let glob_send = GlobIO::send();
            let mut sys = System::new_all();
            let mut tick_count: u64 = 0;
            let mut interval = tokio::time::interval(Duration::from_secs(INFO_UPDATE_INTERVAL_BASE));

            // 启动预热
            Self::perform_full_sync(&mut sys, &glob_send);

            loop {
                interval.tick().await;
                tick_count = tick_count.wrapping_add(1);

                // --- 1. 基础数据采集 (每秒) ---
                sys.refresh_memory();
                let mem_val: MemSwapMB = (
                    sys.used_memory() / 1024 / 1024,
                    sys.used_swap() / 1024 / 1024,
                );
                let cpu_val = Self::task_collect_cpu();

                // 包装为 Arc Payload
                let mem_payload = DynamicPayload(Arc::new(mem_val));
                let cpu_payload = DynamicPayload(Arc::new(cpu_val.clone()));

                // --- 2. 短周期分发 (实时 UI) ---
                let _ = glob_send.send(GlobalEvent::Data { key: MEM_SWAP, data: mem_payload.clone() });
                let _ = glob_send.send(GlobalEvent::Data { key: ANDROID_CPU, data: cpu_payload.clone() });

                // --- 3. 长周期处理 (数据库存储 + 历史分发) ---
                if tick_count % INFO_UPDATE_INTERVAL_SLOWEST == 1 {
                    let bat_val = Self::task_collect_battery();
                    let bat_payload = DynamicPayload(Arc::new(bat_val.clone()));

                    // A. 构造持久化记录 (结构与发送一致)
                    let record = TelemetryRecord {
                        timestamp: Utc::now().to_rfc3339(),
                        cpu_data: cpu_val, 
                        mem_swap: mem_val,
                        battery_data: bat_val,
                    };

                    // B. 异步存入 MongoDB (使用全局 DATABASE_NAME)
                    // let _ = Mongo::save(DATABASE_NAME, COLL_NAME, record).await;
                    let record_to_save = record.clone();
                    tokio::spawn(async move {
                        if let Err(e) = record_to_save.save_to_db().await {
                                    // 可以通过 glob_send 发送一个错误通知给 UI
                                }
                    });

                    // C. 分发长周期 Payload
                    let _ = glob_send.send(GlobalEvent::Data { key: MEM_SWAP_LONG, data: mem_payload });
                    let _ = glob_send.send(GlobalEvent::Data { key: ANDROID_CPU_LONG, data: cpu_payload });
                    let _ = glob_send.send(GlobalEvent::Data { key: ANDROID_BAT, data: bat_payload });
                }

                // --- 4. 中周期分发 (磁盘与网络) ---
                if tick_count % INFO_UPDATE_INTERVAL_SLOW_TIMES == 1 {
                    let pkg: DiskIP = (Self::task_collect_disks(), Self::ip_list());
                    let _ = glob_send.send(GlobalEvent::Data {
                        key: DISK_IP,
                        data: DynamicPayload(Arc::new(pkg)),
                    });
                }
            }
        });
    }

    fn spawn_history_fetch_task() {
        tokio::spawn(async move {
            let glob_send = GlobIO::send();
            
            // 1. 先确保表已存在（SQLite 启动极快，这里调用是安全的）
            if let Err(e) = TelemetryRecord::init_table().await {
                eprintln!("🔴 SQL Table Init Error: {}", e);
                return;
            }

            // 2. 异步拉取历史
            let db_records = TelemetryRecord::fetch_recent(HISTORY_CAP as i64).await;
            
            if !db_records.is_empty() {
                let _ = glob_send.send(GlobalEvent::Data {
                    key: "HISTORY_REFILL", 
                    data: DynamicPayload(Arc::new(db_records)) 
                });
            }
        });
    }

    // 提取出一个全量同步函数，供初始化和特殊时刻调用
    fn perform_full_sync(sys: &mut System, glob_send: &GlobSend) {
        sys.refresh_memory();
        let mem = (
            sys.used_memory() / 1024 / 1024,
            sys.used_swap() / 1024 / 1024,
        );
        let _ = glob_send.send(GlobalEvent::Data {
            key: MEM_SWAP_LONG,
            data: DynamicPayload(Arc::new(mem)),
        });
        // ... 可按需扩展其他预热项
    }

    fn task_collect_battery() -> AndroidBatInfo 
    {
        #[cfg(target_os = "android")]
        {
            // 尝试调用 termux-api 获取电池状态
            if let Ok(bat_info) = termux::battery::status() {
                (
                    bat_info.percentage,
                    format!("{:?}", bat_info.status),
                    bat_info.temperature,
                )
            } else {
                // 如果 termux-api 调用失败（例如未安装 API 包），返回默认值
                (0, "Unknown".to_string(), 0.0)
            }            
        }
        #[cfg(not(target_os = "android"))]
        {
            (100, "AC-Powered".to_string(), 35.0)
        }

}
    
    // --- CPU ---
    fn task_collect_cpu() -> CpuInfo {
        #[cfg(target_os = "android")]
        {
            let mut freqs = Vec::with_capacity(8);
            for i in 0..8 {
                let path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq", i);
                let f = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|s| s.trim().parse::<f32>().ok())
                    .map(|f| f / 1_000_000.0)
                    .unwrap_or(0.0);
                freqs.push(f);
            }
            let read_zone = |z| {
                std::fs::read_to_string(format!("/sys/class/thermal/thermal_zone{}/temp", z))
                    .ok()
                    .and_then(|s| s.trim().parse::<f32>().ok())
                    .map(|t| t / 1000.0)
                    .unwrap_or(0.0)
            };
            (freqs, read_zone(0), read_zone(7))            
        }
        #[cfg(not(target_os = "android"))]
        {
            (vec![0.0; 8], 0.0, 0.0)
        }

    }

    // --- 辅助采集函数：磁盘 ---
    fn task_collect_disks() -> Vec<DiskInf> {
        let mut disks = Disks::new_with_refreshed_list();
        disks.refresh(true);
        disks
            .iter()
            .map(|d| {
                (
                    d.name().to_string_lossy().into_owned(),
                    d.total_space(),
                    d.available_space(),
                    d.mount_point().to_string_lossy().into_owned(),
                )
            })
            .collect()
    }

    fn ip_list() -> (Vec<String>, Vec<String>) {
        let mut v4_list = Vec::new();
        let mut v6_list = Vec::new();

        if let Ok(ips) = local_ip_address::list_afinet_netifas() {
            for (name, ip) in ips {
                let entry = format!("{}: {}", name, ip);
                if ip.is_ipv4() {
                    v4_list.push(entry);
                } else if ip.is_ipv6() {
                    // v6 地址通常较长，可以做简单截断或处理
                    v6_list.push(entry);
                }
            }
        } else {
            v4_list.push("Error getting IPs".to_string());
        }

        (v4_list, v6_list)
    }


}


#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TelemetryRecord {
    pub timestamp: String, // 改为 String 提高序列化兼容性
    pub cpu_data: CpuInfo,
    pub mem_swap: MemSwapMB,
    pub battery_data: AndroidBatInfo,
}

impl TelemetryRecord {
    /// 初始化表结构
    pub async fn init_table() -> Result<(), String> {
        let ddl = r#"
            CREATE TABLE IF NOT EXISTS telemetry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                cpu_data TEXT NOT NULL,
                mem_swap TEXT NOT NULL,
                battery_data TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_telemetry_ts ON telemetry(timestamp);
        "#;
        crate::db::Database::setup_table(ddl).await
    }

    /// 存储记录到 SQLite
    pub async fn save_to_db(&self) -> Result<(), String> {
        let pool = crate::db::Database::pool();
        sqlx::query("INSERT INTO telemetry (timestamp, cpu_data, mem_swap, battery_data) VALUES (?, ?, ?, ?)")
            .bind(&self.timestamp)
            .bind(serde_json::to_string(&self.cpu_data).unwrap_or_default())
            .bind(serde_json::to_string(&self.mem_swap).unwrap_or_default())
            .bind(serde_json::to_string(&self.battery_data).unwrap_or_default())
            .execute(pool)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// 从 SQLite 获取最近记录
    pub async fn fetch_recent(limit: i64) -> Vec<Self> {
        let pool = crate::db::Database::pool();
        let rows = sqlx::query("SELECT timestamp, cpu_data, mem_swap, battery_data FROM telemetry ORDER BY timestamp DESC LIMIT ?")
            .bind(limit)
            .fetch_all(pool)
            .await
            .unwrap_or_default();

        rows.into_iter().filter_map(|row: SqliteRow| {
            Some(Self {
                timestamp: row.get("timestamp"),
                cpu_data: serde_json::from_str(row.get("cpu_data")).ok()?,
                mem_swap: serde_json::from_str(row.get("mem_swap")).ok()?,
                battery_data: serde_json::from_str(row.get("battery_data")).ok()?,
            })
        }).collect()
    }
}





