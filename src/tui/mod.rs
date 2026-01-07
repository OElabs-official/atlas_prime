
use std::fs;
use std::io::{self};
use std::path::PathBuf;
use std::time::Duration;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode,
    enable_raw_mode,
    EnterAlternateScreen,
    LeaveAlternateScreen,
};
use directories::{BaseDirs, ProjectDirs, UserDirs};
use local_ip_address::local_ip;
use rand::Rng;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs};
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use sysinfo::{Disks, System};
use core_affinity;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
enum AppColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
}

impl AppColor {
    fn to_ratatui_color(self) -> Color {
        match self {
            AppColor::Black => Color::Black,
            AppColor::Red => Color::Red,
            AppColor::Green => Color::Green,
            AppColor::Yellow => Color::Yellow,
            AppColor::Blue => Color::Blue,
            AppColor::Magenta => Color::Magenta,
            AppColor::Cyan => Color::Cyan,
            AppColor::White => Color::White,
        }
    }

    fn next(self) -> Self {
        match self {
            AppColor::Black => AppColor::Red,
            AppColor::Red => AppColor::Green,
            AppColor::Green => AppColor::Yellow,
            AppColor::Yellow => AppColor::Blue,
            AppColor::Blue => AppColor::Magenta,
            AppColor::Magenta => AppColor::Cyan,
            AppColor::Cyan => AppColor::White,
            AppColor::White => AppColor::Black,
        }
    }
}

impl std::fmt::Display for AppColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Config {
    background_color: AppColor,
    text_color: AppColor,
    refresh_rate: u64,
    cpu_affinity: Option<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            background_color: AppColor::Black,
            text_color: AppColor::White,
            refresh_rate: 1,
            cpu_affinity: None,
        }
    }
}

impl Config {
    fn get_config_path() -> Option<PathBuf> {
        ProjectDirs::from("", "", "ex2")
            .map(|proj_dirs| proj_dirs.data_dir().join("config.json"))
    }

    fn load() -> Self {
        if let Some(path) = Self::get_config_path() {
            if let Ok(content) = fs::read_to_string(path) {
                if let Ok(config) = serde_json::from_str(&content) {
                    return config;
                }
            }
        }
        Self::default()
    }

    fn save(&self) -> std::io::Result<()> {
        if let Some(path) = Self::get_config_path() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let content = serde_json::to_string_pretty(self)?;
            fs::write(path, content)?;
        }
        Ok(())
    }
}

struct TabData {
    title: String,
    content: String,
}

struct App {
    pub tabs: Vec<TabData>,
    pub index: usize,
    pub config: Config,
    pub cpu_count: usize,
    // Fields for Tab 1
    pub dir_list: Vec<String>,
    pub ip_list: Vec<String>,
    pub tab1_scroll: [u16; 3], // [mounts, dirs, ips]
    pub tab1_focus: usize, // 0..2
    
    // Async IP fetching
    pub ip_rx: Receiver<String>,

    // Fields for Tab 2
    pub file_list: Vec<String>,
    pub tab2_vertical_index: usize,
    pub tab2_vertical_items: Vec<String>,
}

impl App {
    fn new(cpu_count: usize) -> App {
        let mut rng = rand::rng();
        let mut tabs = Vec::new();
        // Generate 3 random tabs
        for i in 1..=3 {
            let random_text = (0..rng.random_range(5..15))
                .map(|_| {
                    let s: String = rand::rng()
                        .sample_iter(&rand::distr::Alphanumeric)
                        .take(rng.random_range(20..100))
                        .map(char::from)
                        .collect();
                    s
                })
                .collect::<Vec<String>>()
                .join("\n");

            tabs.push(TabData {
                title: if i == 1 { "Info".to_string() } else { format!("Tab {}", i) },
                content: random_text,
            });
        }

        // Add Settings tab
        tabs.push(TabData {
            title: "Settings".to_string(),
            content: String::new(), 
        });

        let config = Config::load();
        
        // Apply initial affinity
        Self::apply_affinity(config.cpu_affinity);

        // Collect directory paths
        let mut dir_list = Vec::new();
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

        // Collect IP Addresses
        let mut ip_list = Vec::new();
        ip_list.push("--- Local IPs ---".to_string());
        if let Ok(ips) = local_ip_address::list_afinet_netifas() {
            for (name, ip) in ips {
                ip_list.push(format!("{}: {}", name, ip));
            }
        } else {
            ip_list.push("Failed to get local IPs".to_string());
        }

        ip_list.push("\n--- Public IP ---".to_string());
        ip_list.push("Loading...".to_string());

        // Setup Async Fetch
        let (tx, rx) = mpsc::channel();
        
        thread::spawn(move || {
            let ureq_config = ureq::config::Config::builder()
                .timeout_global(Some(Duration::from_secs(20)))
                .build();
            let agent = ureq::Agent::new_with_config(ureq_config);
                
            let public_ip_result = agent.get("https://api.ipify.org").call();
            
            match public_ip_result {
                Ok(response) => {
                    if let Ok(body) = response.into_body().read_to_string() {
                         let _ = tx.send(body);
                    } else {
                         let _ = tx.send("Failed to read response".to_string());
                    }
                },
                Err(_) => {
                    let _ = tx.send("Failed to fetch public IP".to_string());
                }
            }
        });

        // Tab 2 File List
        let mut file_list = Vec::new();
        if let Ok(entries) = fs::read_dir(".") {
            for entry in entries {
                if let Ok(entry) = entry {
                    file_list.push(entry.path().display().to_string());
                }
            }
        }
        file_list.sort();

        // Tab 2 Vertical Items
        let tab2_vertical_items = vec![
            "Session 1".to_string(),
            "Session 2".to_string(),
            "Session 3".to_string(),
        ];

        App {
            tabs,
            index: 0,
            config,
            cpu_count,
            dir_list,
            ip_list,
            tab1_scroll: [0, 0, 0],
            tab1_focus: 0,
            ip_rx: rx,
            file_list,
            tab2_vertical_index: 0,
            tab2_vertical_items,
        }
    }

    pub fn check_ip_update(&mut self) {
        if let Ok(ip) = self.ip_rx.try_recv() {
            // Update the last line (Loading...)
            if let Some(last) = self.ip_list.last_mut() {
                if last == "Loading..." {
                    *last = ip;
                } else {
                    self.ip_list.push(ip);
                }
            }
        }
    }

    pub fn next(&mut self) {
        self.index = (self.index + 1) % self.tabs.len();
    }

    pub fn previous(&mut self) {
        if self.index > 0 {
            self.index -= 1;
        } else {
            self.index = self.tabs.len() - 1;
        }
    }

    pub fn is_settings_tab(&self) -> bool {
        self.index == self.tabs.len() - 1
    }

    fn apply_affinity(affinity: Option<usize>) {
        if let Some(core_index) = affinity {
             if let Some(core_ids) = core_affinity::get_core_ids() {
                 if core_index < core_ids.len() {
                     core_affinity::set_for_current(core_ids[core_index]);
                 }
             }
        } else {
             if let Some(_core_ids) = core_affinity::get_core_ids() {
                 // No easy cross-platform reset in this crate
             }
        }
    }
}

pub async fn run_sync() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut sys = System::new_all();
    let cpu_count = sys.cpus().len();
    let cpu_count = if cpu_count == 0 { 1 } else { cpu_count };

    let mut app = App::new(cpu_count);
    let mut disks = Disks::new_with_refreshed_list();

    loop {
        // Check for async IP updates
        app.check_ip_update();

        // Refresh system info
        sys.refresh_memory();
        disks.refresh(true); 

        terminal.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1) 
                .constraints(
                    [
                        Constraint::Length(3), // Tabs
                        Constraint::Min(0),    // Content
                        Constraint::Length(3), // Status Bar
                    ]
                    .as_ref(),
                )
                .split(area);

            // Use config colors
            let bg_color = app.config.background_color.to_ratatui_color();
            let fg_color = app.config.text_color.to_ratatui_color();

            let block = Block::default()
                .style(Style::default().bg(bg_color).fg(fg_color));
            f.render_widget(block, area);

            // Tabs
            let titles: Vec<Line> = app
                .tabs
                .iter()
                .map(|t| Line::from(Span::styled(&t.title, Style::default().fg(Color::Green))))
                .collect();

            let tabs = Tabs::new(titles)
                .block(Block::default().borders(Borders::ALL).title("Ex2"))
                .select(app.index)
                .style(Style::default().fg(Color::Cyan))
                .highlight_style(
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .bg(fg_color)
                        .fg(bg_color),
                );
            f.render_widget(tabs, chunks[0]);

            // Content
            let inner_area = chunks[1];
            
            if app.is_settings_tab() {
                let affinity_display = if let Some(core) = app.config.cpu_affinity {
                    format!("Core {}", core)
                } else {
                    "All Cores (Requires Restart/Limitation)".to_string()
                };

                let settings_text = format!(
                    "Settings:\n\n\t[b] Background Color: {}\n\t[t] Text Color:       {}\n\t[+/-] Refresh Rate:   {} sec\n\t[c] CPU Affinity:     {}\n\n\tPress corresponding keys to change settings.",
                    app.config.background_color,
                    app.config.text_color,
                    app.config.refresh_rate,
                    affinity_display
                );

                let paragraph = Paragraph::new(settings_text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Settings")
                            .style(Style::default().fg(fg_color).bg(bg_color))
                    );
                f.render_widget(paragraph, inner_area);
            } else if app.index == 0 {
                // Tab 1: Disk Mounts & Directories & IPs
                let constraints = [
                    Constraint::Percentage(33), 
                    Constraint::Percentage(33),
                    Constraint::Percentage(34)
                ];
                let sub_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(constraints)
                    .split(inner_area);

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
                
                let focus_style = if app.tab1_focus == 0 {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(fg_color).bg(bg_color)
                };

                let mounts_para = Paragraph::new(mount_str)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Mount Points (Scroll with Up/Down)")
                            .border_style(focus_style)
                    )
                    .scroll((app.tab1_scroll[0], 0));
                f.render_widget(mounts_para, sub_chunks[0]);

                // 2. Directories
                let dirs_str = app.dir_list.join("\n");
                let focus_style_2 = if app.tab1_focus == 1 {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(fg_color).bg(bg_color)
                };

                let dirs_para = Paragraph::new(dirs_str)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Directories (Scroll with Up/Down)")
                            .border_style(focus_style_2)
                    )
                    .scroll((app.tab1_scroll[1], 0));
                f.render_widget(dirs_para, sub_chunks[1]);

                // 3. IPs
                let ips_str = app.ip_list.join("\n");
                let focus_style_3 = if app.tab1_focus == 2 {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(fg_color).bg(bg_color)
                };

                let ips_para = Paragraph::new(ips_str)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("IP Addresses (Scroll with Up/Down)")
                            .border_style(focus_style_3)
                    )
                    .scroll((app.tab1_scroll[2], 0));
                f.render_widget(ips_para, sub_chunks[2]);

            } else if app.index == 1 {
                // Tab 2: Sessions (Vertical) + Content
                let chunks_h = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(15), Constraint::Min(0)])
                    .split(inner_area);

                // Left: Vertical List
                let items: Vec<ListItem> = app.tab2_vertical_items
                    .iter()
                    .map(|i| ListItem::new(Span::raw(i)))
                    .collect();
                
                let mut state = ListState::default();
                state.select(Some(app.tab2_vertical_index));

                let list = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title("Sessions"))
                    .highlight_style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Yellow))
                    .highlight_symbol("> ");
                
                f.render_stateful_widget(list, chunks_h[0], &mut state);

                // Right: Content
                let right_content = if app.tab2_vertical_index == 0 {
                    app.file_list.join("\n")
                } else {
                    format!("Content for {}", app.tab2_vertical_items[app.tab2_vertical_index])
                };

                let paragraph = Paragraph::new(right_content)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!("{} Content", app.tab2_vertical_items[app.tab2_vertical_index]))
                            .style(Style::default().fg(fg_color).bg(bg_color))
                    );
                f.render_widget(paragraph, chunks_h[1]);

            } else {
                let paragraph = Paragraph::new(app.tabs[app.index].content.clone())
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!("Content of {}", app.tabs[app.index].title))
                            .style(Style::default().fg(fg_color).bg(bg_color))
                    );
                f.render_widget(paragraph, inner_area);
            }

            // Status Bar
            // Memory
            let total_mem = sys.total_memory();
            let used_mem = sys.used_memory();
            let mem_percent = if total_mem > 0 {
                (used_mem as f64 / total_mem as f64) * 100.0
            } else {
                0.0
            };
            let free_mem_mb = (total_mem - used_mem) / 1024 / 1024;

            // Swap
            let total_swap = sys.total_swap();
            let used_swap = sys.used_swap();
            let swap_percent = if total_swap > 0 {
                (used_swap as f64 / total_swap as f64) * 100.0
            } else {
                0.0
            };
            let free_swap_mb = (total_swap - used_swap) / 1024 / 1024;


            // Disk: Find the largest partition
            let largest_disk = disks.iter().max_by_key(|d| d.total_space());
            let disk_info = if let Some(disk) = largest_disk {
                let total_space = disk.total_space();
                let available_space = disk.available_space();
                let used_space = total_space - available_space;
                let disk_percent = if total_space > 0 {
                     (used_space as f64 / total_space as f64) * 100.0
                } else {
                    0.0
                };
                let free_space_gb = available_space as f64 / 1024.0 / 1024.0 / 1024.0;
                let mount_point = disk.mount_point().to_str().unwrap_or("Unknown");
                format!("Disk ({}):({:.1}% Used, {:.2} GB Free", mount_point, disk_percent, free_space_gb)
            } else {
                "Disk: N/A".to_string()
            };

            // IP Address
            let my_local_ip = local_ip().map(|ip| ip.to_string()).unwrap_or_else(|_| "Unknown".to_string());

            let affinity_status = if let Some(core) = app.config.cpu_affinity {
                format!("CPU: {}", core)
            } else {
                "CPU: All".to_string()
            };

            let status_text = format!(
                "Mem: {:.1}% Used, {} MB Free | Swap: {:.1}% Used, {} MB Free | {} |  ",
                mem_percent, free_mem_mb, swap_percent, free_swap_mb, disk_info
            );

            let status_paragraph = Paragraph::new(status_text)
                .block(Block::default().borders(Borders::ALL).title("System Info"))
                .style(Style::default().fg(Color::Yellow));
            
            f.render_widget(status_paragraph, chunks[2]);

        })?;

        // Poll with configured refresh rate
        let poll_duration = Duration::from_secs(app.config.refresh_rate); 
        
        if event::poll(poll_duration)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Right => app.next(),
                    KeyCode::Left => app.previous(),
                    KeyCode::Tab => {
                        if app.index == 0 {
                             app.tab1_focus = (app.tab1_focus + 1) % 3;
                        }
                    }
                    KeyCode::Up => {
                        if app.index == 0 {
                             if app.tab1_scroll[app.tab1_focus] > 0 {
                                 app.tab1_scroll[app.tab1_focus] -= 1;
                             }
                        } else if app.index == 1 {
                            if app.tab2_vertical_index > 0 {
                                app.tab2_vertical_index -= 1;
                            }
                        }
                    }
                    KeyCode::Down => {
                        if app.index == 0 {
                             app.tab1_scroll[app.tab1_focus] += 1;
                        } else if app.index == 1 {
                            if app.tab2_vertical_index < app.tab2_vertical_items.len() - 1 {
                                app.tab2_vertical_index += 1;
                            }
                        }
                    }
                    // Settings handling
                    _ if app.is_settings_tab() => {
                        let mut changed = false;
                        match key.code {
                            KeyCode::Char('b') => {
                                app.config.background_color = app.config.background_color.next();
                                changed = true;
                            }
                            KeyCode::Char('t') => {
                                app.config.text_color = app.config.text_color.next();
                                changed = true;
                            }
                            KeyCode::Char('+') | KeyCode::Char('=') => {
                                app.config.refresh_rate += 1;
                                changed = true;
                            }
                            KeyCode::Char('-') => {
                                if app.config.refresh_rate > 1 {
                                    app.config.refresh_rate -= 1;
                                    changed = true;
                                }
                            }
                            #[cfg(not(target_os = "android"))]
                            KeyCode::Char('c') => {
                                // Cycle affinity: Some(0) -> Some(1) ... -> Some(N-1) -> None -> Some(0)
                                match app.config.cpu_affinity {
                                    Some(current) => {
                                        if current + 1 < app.cpu_count {
                                            app.config.cpu_affinity = Some(current + 1);
                                        } else {
                                            app.config.cpu_affinity = None; // All Cores
                                        }
                                    }
                                    None => {
                                        app.config.cpu_affinity = Some(0);
                                    }
                                }
                                // Apply immediate
                                App::apply_affinity(app.config.cpu_affinity);
                                changed = true;
                            }
                            _ => {} // Ignore other keys
                        }
                        if changed {
                            // Persist settings
                            let _ = app.config.save();
                        }
                    }
                    _ => {} // Ignore other keys
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}