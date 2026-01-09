use crate::{
    config::{AppColor, Config, SharedConfig},
    message::{DynamicPayload, GlobalEvent, ProgressType},
    ui::component::Component,
};
use crossterm::event::{KeyCode, KeyEvent};
use directories::{BaseDirs, UserDirs};
use ratatui::{prelude::*, widgets::*};
use sysinfo::{Disks, System};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc};

pub struct InfoComponent {
    config: SharedConfig,
    event_rx: tokio::sync::broadcast::Receiver<GlobalEvent>,
    
    // 数据存储
    mount_points: String,
    dir_list: Vec<String>,
    ip_list: Vec<String>,
    
    // UI 状态
    focus_index: usize, // 0: Mounts, 1: Dirs, 2: IPs
    scroll_offsets: [u16; 3],

    sys: System,
    // 内存历史数据（用于 Sparkline）
    mem_history: VecDeque<u64>,
    swap_history: VecDeque<u64>,
    last_refresh: std::time::Instant,
}

pub struct _InfoComponent {
    pub config: SharedConfig, // 持有共享引用的拷贝，开销极小
    event_rx: tokio::sync::broadcast::Receiver<GlobalEvent>,

    cpu_usage: String,
    rx: mpsc::Receiver<String>,

    progress: u16,
    
}

impl InfoComponent {
    // src/ui/info.rs

    fn render_disk_list(&self, f: &mut Frame, area: Rect) {
        let mut disks = Disks::new_with_refreshed_list();
        disks.refresh(true);
        let mut sorted_disks: Vec<_> = disks.iter().collect();
        sorted_disks.sort_by(|a, b| b.total_space().cmp(&a.total_space()));

        let offset = self.scroll_offsets[0] as usize;
        let visible_height = area.height.saturating_sub(2) as usize;
        
        let displayed_disks = sorted_disks.iter()
            .skip(offset)
            .take(visible_height);

        let items: Vec<ListItem> = displayed_disks.map(|d| {
            let total = d.total_space();
            let available = d.available_space();
            let used = total - available;
            let pct = if total > 0 { (used as f64 / total as f64) } else { 0.0 };
            
            // 1. 生成进度条 [████░░░░]
            let bar_width = 12; // 缩短一点进度条，给文字留空间
            let filled = (pct * bar_width as f64).round() as usize;
            let empty = bar_width - filled;
            let bar_str = format!("[{}{}] ", "█".repeat(filled), "░".repeat(empty));
            
            // 2. 根据占用率决定颜色
            let color = if pct > 0.9 { Color::Red } else if pct > 0.7 { Color::Yellow } else { Color::Green };

            // 3. 准备后续文本信息
            let info_text = format!(
                "{:>5.1}% {:>6.1} GB  {:<15}", 
                pct * 100.0,
                total as f64 / 1024.0 / 1024.0 / 1024.0, 
                d.mount_point().to_string_lossy()
            );

            // 4. 按顺序组合：进度条在前，文本在后
            ListItem::new(Line::from(vec![
                Span::styled(bar_str, Style::default().fg(color)), // 进度条最先
                Span::raw(info_text),
                Span::styled(
                    format!(" ({})", d.name().to_string_lossy()), 
                    Style::default().fg(Color::DarkGray)
                ),
            ]))
        }).collect();

        let list = List::new(items)
            .block(Block::default()
                .borders(Borders::ALL)
                .title(" Mount Points ")
                .border_style(if self.focus_index == 0 { 
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD) 
                } else { 
                    Style::default().fg(Color::Gray) 
                }));

        f.render_widget(list, area);
    }

    // pub fn new(config: SharedConfig, event_rx: broadcast::Receiver<GlobalEvent>) -> Self {
    //     let (tx, cpu_rx) = mpsc::channel(1);

    //     // 异步后台任务：采集 CPU
    //     tokio::spawn(async move {
    //         let mut sys = sysinfo::System::new_all();
    //         loop {
    //             sys.refresh_cpu_all();
    //             let usage = format!("{:.1}%", sys.global_cpu_usage());
    //             if tx.send(usage).await.is_err() {
    //                 break;
    //             }
    //             tokio::time::sleep(Duration::from_millis(800)).await;
    //         }
    //     });

    //     Self {
    //         cpu_usage: "0%".into(),
    //         rx: cpu_rx,
    //         progress: 0,
    //         event_rx,
    //         config,
    //         mount_points: todo!(),
    //         dir_list: todo!(),
    //         ip_list: todo!(),
    //         focus_index: todo!(),
    //         scroll_offsets: todo!(),
    //     }
    // }

    fn _render_charts(&self, f: &mut Frame, area: Rect) {
        // 将顶部区域平分为左右两个图表，或者上下两个大图表
        // 这里建议上下排布，每行高度给 3-4，视觉效果最震撼
        let chunks = Layout::vertical([
            Constraint::Length(4), // 大一点的 RAM 区域
            Constraint::Length(4), // 大一点的 Swap 区域
        ]).split(area);

        // 渲染 RAM Sparkline
        let mem_data: Vec<u64> = self.mem_history.iter().cloned().collect();
        let ram_spark = Sparkline::default()
            .block(Block::default().borders(Borders::LEFT | Borders::RIGHT).title(" [ RAM Usage History ] "))
            .data(&mem_data)
            .style(Style::default().fg(Color::Cyan));
        f.render_widget(ram_spark, chunks[0]);

        // 渲染 Swap Sparkline
        let swap_data: Vec<u64> = self.swap_history.iter().cloned().collect();
        let swap_spark = Sparkline::default()
            .block(Block::default().borders(Borders::LEFT | Borders::RIGHT).title(" [ Swap Usage History ] "))
            .data(&swap_data)
            .style(Style::default().fg(Color::Magenta)); // 换个颜色区分
        f.render_widget(swap_spark, chunks[01]);
    }


    fn _refresh_tick(&mut self) -> bool{
        // 只有间隔超过 500ms 才会真正执行系统刷新
        if self.last_refresh.elapsed() >= std::time::Duration::from_millis(500) {
            self.sys.refresh_memory();
            
            // 更新内存历史
            let mem_used = self.sys.used_memory();
            self.mem_history.push_back(mem_used);
            if self.mem_history.len() > 100 { self.mem_history.pop_front(); }

            // 更新 Swap 历史
            let swap_used = self.sys.used_swap();
            self.swap_history.push_back(swap_used);
            if self.swap_history.len() > 100 { self.swap_history.pop_front(); }

            self.last_refresh = std::time::Instant::now();
            return true;
        }
        false
    }

    fn refresh_tick(&mut self) {
        if self.last_refresh.elapsed() >= std::time::Duration::from_millis(500) {
            self.sys.refresh_memory();
            
            // --- RAM 比例计算 ---
            let mem_total = self.sys.total_memory();
            let mem_used = self.sys.used_memory();
            // 计算百分比并存入历史 (0-100)
            let mem_pct = if mem_total > 0 {
                (mem_used as f64 / mem_total as f64 * 100.0) as u64
            } else { 0 };
            
            self.mem_history.push_back(mem_pct);
            if self.mem_history.len() > 100 { self.mem_history.pop_front(); }

            // --- Swap 比例计算 ---
            let swap_total = self.sys.total_swap();
            let swap_used = self.sys.used_swap();
            let swap_pct = if swap_total > 0 {
                (swap_used as f64 / swap_total as f64 * 100.0) as u64
            } else { 0 };

            self.swap_history.push_back(swap_pct);
            if self.swap_history.len() > 100 { self.swap_history.pop_front(); }

            self.last_refresh = std::time::Instant::now();
        }
    }

    pub fn new(
        config: SharedConfig, 
        tx: tokio::sync::broadcast::Sender<GlobalEvent> // 传入 Sender 用于触发异步任务
    ) -> Self {
        // 1. 创建组件实例时先订阅，用于后续 update 中接收数据
        let event_rx = tx.subscribe();

        let mut sys = System::new_all();
        sys.refresh_all();

        let mut inst = Self {
            config,
            event_rx,
            mount_points: String::new(),
            dir_list: Vec::new(),
            ip_list: vec![],
            focus_index: 0,
            scroll_offsets: [0; 3],
            sys,
            mem_history: VecDeque::from(vec![0; 100]), // 预留100个数据点
            swap_history: VecDeque::from(vec![0; 100]),
            last_refresh: std::time::Instant::now(),
        };

        // 2. 填充静态数据（磁盘、目录等）
        inst.dir_list = Self::collect_dirs();
        inst.mount_points= Self::disk_list();
        inst.ip_list = Self::ip_list();
        // 3. 启动异步任务：获取公网 IP
        // 我们直接把克隆的 tx 传给静态方法
        //Self::fetch_public_ip(tx.clone());

        inst
    }

    fn ip_list()->Vec<String>
    {
        let mut ip_list: Vec<String> = Vec::new();
        ip_list.push("--- Local IPs ---".to_string());
        if let Ok(ips) = local_ip_address::list_afinet_netifas() {
            for (name, ip) in ips {
                ip_list.push(format!("{}: {}", name, ip));
            }
        } else {
            ip_list.push("Failed to get local IPs".to_string());
        }

        // ip_list.push("\n--- Public IP ---".to_string());
        // ip_list.push("Loading...".to_string());
        ip_list
    }

    fn disk_list()->String
    {
        let mut disks = Disks::new_with_refreshed_list();
        disks.refresh(true);
        // 1. Mount Points
        let mut sorted_disks: Vec<_> = disks.iter().collect();

        #[cfg(not(target_os = "windows"))]
        sorted_disks.sort_by(|a, b| b.total_space().cmp(&a.total_space()));

        #[cfg(target_os = "windows")]
        sorted_disks.sort_by(|a, b| a.mount_point().cmp(b.mount_point()));

        let mount_str = sorted_disks.iter().map(|d| {
            let total_space = d.total_space();
            let available_space = d.available_space();
            let used_space = total_space - available_space;
            let usage_percent = if total_space > 0 {
                (used_space as f64 / total_space as f64) * 100.0
            } else {
                0.0
            };
            let total_gb = total_space as f64 / 1024.0 / 1024.0 / 1024.0;
            
            format!("[{:.1} GB] {:.1}% {} ({})", total_gb, usage_percent, d.mount_point().display(), d.name().to_string_lossy())
        }).collect::<Vec<String>>().join("\n");
        mount_str
    }
    fn collect_dirs() -> Vec<String> {
        let mut dir_list = Vec::new();
        // ... (此处填入你旧代码中 BaseDirs 和 UserDirs 的逻辑)
        if let Some(base_dirs) = BaseDirs::new() {
            dir_list.push("--- Base Dirs ---".to_string());
            dir_list.push(format!("Home: {:?}", base_dirs.home_dir()));
            dir_list.push(format!("Config: {:?}", base_dirs.config_dir()));
            dir_list.push(format!("Data: {:?}", base_dirs.data_dir()));
            dir_list.push(format!("Data Local: {:?}", base_dirs.data_local_dir()));
            dir_list.push(format!("Cache: {:?}", base_dirs.cache_dir()));
            dir_list.push(format!("Preference: {:?}", base_dirs.preference_dir()));
            
            if let Some(state) = base_dirs.state_dir() {
                dir_list.push(format!("State: {:?}", state));
            }
            if let Some(exe) = base_dirs.executable_dir() {
                dir_list.push(format!("Executable: {:?}", exe));
            }
            if let Some(run) = base_dirs.runtime_dir() {
                dir_list.push(format!("Runtime: {:?}", run));
            }
        }
        
        if let Some(user_dirs) = UserDirs::new() {
            dir_list.push("\n--- User Dirs ---".to_string());
            dir_list.push(format!("Home: {:?}", user_dirs.home_dir()));
            if let Some(audio) = user_dirs.audio_dir() {
                 dir_list.push(format!("Audio: {:?}", audio));
            }
            if let Some(desktop) = user_dirs.desktop_dir() {
                 dir_list.push(format!("Desktop: {:?}", desktop));
            }
            if let Some(doc) = user_dirs.document_dir() {
                 dir_list.push(format!("Document: {:?}", doc));
            }
            if let Some(dl) = user_dirs.download_dir() {
                 dir_list.push(format!("Download: {:?}", dl));
            }
            if let Some(font) = user_dirs.font_dir() {
                 dir_list.push(format!("Font: {:?}", font));
            }
            if let Some(pic) = user_dirs.picture_dir() {
                 dir_list.push(format!("Picture: {:?}", pic));
            }
            if let Some(pub_dir) = user_dirs.public_dir() {
                 dir_list.push(format!("Public: {:?}", pub_dir));
            }
            if let Some(temp) = user_dirs.template_dir() {
                 dir_list.push(format!("Template: {:?}", temp));
            }
            if let Some(vid) = user_dirs.video_dir() {
                 dir_list.push(format!("Video: {:?}", vid));
            }
        }
        dir_list
    }
/*
pub fn fetch_public_ip(tx: tokio::sync::broadcast::Sender<GlobalEvent>) {
    tokio::spawn(async move {
        // 探测点 1: 任务开始
        let _ = tx.send(GlobalEvent::PushData {
            key: "public_ip",
            data: DynamicPayload(Arc::new("Connecting...".to_string())),
        });

        let client = reqwest::Client::builder()
            //.rustls_tls_webpki_roots()
            .timeout(std::time::Duration::from_secs(15)) // 增加超时到 15s
            .build();

        match client {
            Ok(c) => {
                match c.get("https://api.ipify.org").send().await {
                    Ok(resp) => {
                        if let Ok(ip) = resp.text().await {
                            let _ = tx.send(GlobalEvent::PushData {
                                key: "public_ip",
                                data: DynamicPayload(Arc::new(ip)),
                            });
                        }
                    }
                    Err(e) => {
                        // 探测点 2: 网络错误
                        let _ = tx.send(GlobalEvent::PushData {
                            key: "public_ip",
                            data: DynamicPayload(Arc::new(format!("Net Error: {}", e))),
                        });
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(GlobalEvent::PushData {
                    key: "public_ip",
                    data: DynamicPayload(Arc::new(format!("Client Error: {}", e))),
                });
            }
        }
    });
}
 */

}

impl Component for InfoComponent {
    fn update(&mut self) -> bool {
        /*
        要使 update 函数返回合理的 bool 值，核心逻辑是：只要任何一个数据源（MPSC 通道或 Broadcast 频道）在本次调用中产生了新数据，就将标志位设为 true。
        如果不返回 true，主循环就不会触发重绘，用户也就看不到最新的 CPU 使用率或进度条变化。
        */
        let mut changed = false;


        // 尝试收割全局广播
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                GlobalEvent::PushData { key, data } => {
                    match key {
                        "public_ip" => {
                            // 使用 downcast_ref 尝试还原为 String
                            if let Some(ip_str) = data.0.downcast_ref::<String>() {
                                // 更新 UI 状态
                                if let Some(pos) = self.ip_list.iter().position(|s| s.contains("Loading")) {
                                    self.ip_list[pos] = format!("Public: {}", ip_str);
                                } else {
                                    //self.ip_list.push(format!("Public: {}", ip_str));
                                }
                                changed = true;
                            }
                        }
                        // 以后增加 CPU 负载等数据只需在这里增加分支
                        "cpu_usage" => {
                            if let Some(usage) = data.0.downcast_ref::<f64>() {
                                // 处理浮点数类型的推送...
                            }
                        }
                        _ => {}
                    }
                }
                // 处理其他事件 (Notify 等)
                _ => {}
            }
        }

        // 2. 内存刷新逻辑：只需调用 refresh_tick
        if self.last_refresh.elapsed() >= std::time::Duration::from_millis(500) {
            // refresh_tick 内部已经完成了 sys.refresh、百分比计算、push_back 和 pop_front
            self.refresh_tick(); 
            changed = true; 
        }
        changed

    }
    
    fn render(&mut self, f: &mut Frame, area: Rect) {
// 1. 总体纵向分割：顶部图表区(6行) + 下部内容区(剩余)
    // 此时 main_chunks 只有两个索引：0 和 1
    let main_chunks = Layout::vertical([
        Constraint::Length(6), 
        Constraint::Min(0),    
    ]).split(area);

    // 2. 顶部横向平铺：左 RAM，右 Swap
    let chart_chunks = Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ]).split(main_chunks[0]);

    // 计算子区域宽度用于数据采样
    let chart_width = chart_chunks[0].width as usize;

    // 数据采样逻辑
    let mem_data: Vec<u64> = self.mem_history.iter().cloned().rev().take(chart_width).rev().collect();
    let swap_data: Vec<u64> = self.swap_history.iter().cloned().rev().take(chart_width).rev().collect();

    let mem_used_mb = self.sys.used_memory() / 1024 / 1024;
    let mem_total_mb = self.sys.total_memory() / 1024 / 1024;
    let swap_used_mb = self.sys.used_swap() / 1024 / 1024;
    let swap_total_mb = self.sys.total_swap() / 1024 / 1024;

    // 渲染 RAM (左侧)
    f.render_widget(
        Sparkline::default()
            .block(Block::default()
                .title(format!(" RAM: {}/{}MB ", mem_used_mb, mem_total_mb))
                .borders(Borders::ALL))
            .data(&mem_data)
            .max(100)
            .style(Style::default().fg(Color::Cyan)),
        chart_chunks[0],
    );

    // 渲染 SWAP (右侧)
    f.render_widget(
        Sparkline::default()
            .block(Block::default()
                .title(format!(" SWAP: {}/{}MB ", swap_used_mb, swap_total_mb))
                .borders(Borders::ALL))
            .data(&swap_data)
            .max(100)
            .style(Style::default().fg(Color::Magenta)),
        chart_chunks[1],
    );

    // 3. 渲染下方列表区（使用 main_chunks[1] 而不是 2）
    let list_chunks = Layout::vertical([
        Constraint::Percentage(33),
        Constraint::Percentage(33),
        Constraint::Percentage(34),
    ]).split(main_chunks[1]); // 这里必须是 1，因为主布局只有两个块

    // 磁盘渲染
    self.render_disk_list(f, list_chunks[0]);

    // 目录渲染
    f.render_widget(
        Paragraph::new(self.dir_list.join("\n"))
            .block(Block::default().borders(Borders::ALL).title(" Directories ").border_style(
                if self.focus_index == 1 { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::Gray) }
            ))
            .scroll((self.scroll_offsets[1], 0)),
        list_chunks[1],
    );

    // IP 渲染
    f.render_widget(
        Paragraph::new(self.ip_list.join("\n"))
            .block(Block::default().borders(Borders::ALL).title(" IP Addresses ").border_style(
                if self.focus_index == 2 { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::Gray) }
            ))
            .scroll((self.scroll_offsets[2], 0)),
        list_chunks[2],
    );

    }
/*
    fn _render(&mut self, f: &mut Frame, area: Rect) {
        // 将垂直空间分为两块：上方内容，下方进度条
        // let chunks = Layout::default()
        //     .direction(Direction::Vertical)
        //     .constraints([
        //         Constraint::Min(0),    // 占用剩余所有空间
        //         Constraint::Length(3), // 固定 3 行给进度条
        //     ])
        //     .split(area);

        // 绘制 CPU 监控内容
        // let info_text = vec![
        //     Line::from(vec![
        //         Span::raw(" 系统状态: "),
        //         Span::styled("运行中", Style::default().fg(Color::Green)),
        //     ]),
        //     Line::from(""),
        //     Line::from(vec![
        //         Span::raw(" 当前 CPU 使用率: "),
        //         Span::styled(
        //             &self.cpu_usage,
        //             Style::default()
        //                 .fg(Color::Yellow)
        //                 .add_modifier(Modifier::BOLD),
        //         ),
        //     ]),
        // ];

        // let theme_color;
        // {
        //     let g = self.config.try_read(); // 异步运行时内不能使用blocking_read() 
        //     if let Ok(x) = g {
        //         theme_color = x.clone().theme_color
        //     } else {
        //         theme_color = AppColor::Cyan
        //     }
        // }
        // let info_block = Paragraph::new(info_text).block(
        //     Block::default()
        //         .borders(Borders::ALL)
        //         .title(" System Monitor ")
        //         .border_style(Style::default().fg(theme_color.to_ratatui_color())),
        // );

        // frame.render_widget(info_block, chunks[0]);

        // 绘制同步进度条 (形式 A: Gauge)
        // let gauge = Gauge::default()
        //     .block(
        //         Block::default()
        //             .title(" 全局同步进度 (Broadcast) ")
        //             .borders(Borders::ALL),
        //     )
        //     .gauge_style(Style::default().fg(Color::Magenta).bg(Color::Black))
        //     .percent(self.progress);

        // frame.render_widget(gauge, chunks[1]);

// 布局：等分为三个垂直面板
        let chunks = Layout::horizontal([
            Constraint::Percentage(33),
            Constraint::Percentage(34),
            Constraint::Percentage(33),
        ]).split(area);

        let titles = [" Mount Points ", " Directories ", " IP Addresses "];
        
        for i in 0..3 {
            let is_focused = self.focus_index == i;
            let border_style = if is_focused {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };

            let content = match i {
                0 => self.mount_points.clone(),
                1 => self.dir_list.join("\n"),
                2 => self.ip_list.join("\n"),
                _ => String::new(),
            };

            let para = Paragraph::new(content)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(titles[i])
                        .border_style(border_style)
                )
                .scroll((self.scroll_offsets[i], 0));

            f.render_widget(para, chunks[i]);
        }



    }
*/
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            // Tab 或左右键切换焦点
            KeyCode::Tab => {
                self.focus_index = (self.focus_index + 1) % 3;
                true
            }
            // 上下键控制当前焦点的面板滚动
            KeyCode::Up => {
                self.scroll_offsets[self.focus_index] = self.scroll_offsets[self.focus_index].saturating_sub(1);
                true
            }
            KeyCode::Down => {
                self.scroll_offsets[self.focus_index] = self.scroll_offsets[self.focus_index].saturating_add(1);
                true
            }
            _ => false,
        }
    }
}
