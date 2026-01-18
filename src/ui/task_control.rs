use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock as ARwLock};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use crate::{app::{GlobRecv, GlobSend}, config::SharedConfig, ui::component::Component};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{prelude::*, widgets::*};
use std::sync::RwLock;

//1. 数据模型与 JSON 定义


/// 1. JSON 描述符
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskDescriptor {
    pub id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub envs: Option<HashMap<String, String>>,
    pub autostart: bool,
    pub group: String,
    pub log_limit: Option<usize>,
}

#[derive(Debug, PartialEq, Clone)]
pub enum TaskStatus {
    Stopped,
    Running { pid: u32, start_time: std::time::Instant },
    Failed(String),
}

/// 2. 运行时任务对象
pub struct TaskRuntime {
    pub desc: TaskDescriptor,
    pub status: TaskStatus,
    pub logs: Arc<RwLock<VecDeque<String>>>,
    // 用于向后台协程发送控制指令（停止、输入）
    pub control_tx: Option<mpsc::Sender<TaskControlMsg>>,
}

pub enum TaskControlMsg {
    Stdin(String),
    Stop,
}







//2. 核心组件实现
pub struct TaskControlComponent {
    config: SharedConfig,
    tasks: Vec<TaskRuntime>,
    selected_idx: usize,
    
    // UI 状态
    view_mode: ViewMode,
    log_scroll: u16,
    glob_send: GlobSend,
}

#[derive(PartialEq)]
enum ViewMode {
    List, // 列表 + 详情模式
    Log,  // 全屏日志模式
}

impl Component for TaskControlComponent {
    fn init(config: SharedConfig, glob_send: GlobSend, _glob_recv: GlobRecv) -> Self {
        // 模拟从 JSON 加载过程（实际开发中可使用 std::fs::read_to_string）
        let raw_json = r#"[
            {"id": "api", "name": "Backend Server", "command": "ping", "args": ["127.0.0.1"], "autostart": true, "group": "Srv", "log_limit": 500}
        ]"#;
        let descs: Vec<TaskDescriptor> = serde_json::from_str(raw_json).unwrap_or_default();

        let mut tasks = Vec::new();
        for d in descs {
            let runtime = TaskRuntime {
                desc: d,
                status: TaskStatus::Stopped,
                logs: Arc::new(RwLock::new(VecDeque::with_capacity(1000))),
                control_tx: None,
            };
            tasks.push(runtime);
        }

        let mut component = Self {
            config,
            tasks,
            selected_idx: 0,
            view_mode: ViewMode::List,
            log_scroll: 0,
            glob_send,
        };

        // 处理自动启动
        component.auto_start_tasks();
        
        component
    }

    fn update(&mut self) -> bool {
        // 在这里可以检查 glob_recv 里的任务状态变更消息
        // 目前简单返回 false，重绘由 handle_key 触发
        false
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        if self.view_mode == ViewMode::Log {
            self.render_full_log(f, area);
        } else {
            self.render_main_view(f, area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match self.view_mode {
            ViewMode::List => self.handle_list_keys(key),
            ViewMode::Log => self.handle_log_keys(key),
        }
    }
}




//3. 任务启动逻辑 (Tokio Backend)
// 实现 TaskStatus 同步和 stdout 管道监听的核心逻辑。
impl TaskControlComponent {
    fn auto_start_tasks(&mut self) {
        for i in 0..self.tasks.len() {
            if self.tasks[i].desc.autostart {
                self.start_task(i);
            }
        }
    }

    fn start_task(&mut self, idx: usize) {
        let task = &mut self.tasks[idx];
        if let TaskStatus::Running { .. } = task.status { return; }

        let desc = task.desc.clone();
        let logs = task.logs.clone();
        let (tx, mut rx) = mpsc::channel::<TaskControlMsg>(32);
        task.control_tx = Some(tx);

        tokio::spawn(async move {
            let mut cmd = tokio::process::Command::new(&desc.command);
            cmd.args(&desc.args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .stdin(Stdio::piped());

            if let Some(cwd) = &desc.cwd { cmd.current_dir(cwd); }
            
            match cmd.spawn() {
                Ok(mut child) => {
                    let pid = child.id().unwrap_or(0);
                    let stdout = child.stdout.take().unwrap();
                    let mut reader = BufReader::new(stdout).lines();

                    // 这里的日志更新逻辑
                    tokio::spawn(async move {
                        // let mut reader = BufReader::new(stdout).lines();
                        while let Ok(Some(line)) = reader.next_line().await {
                            // 使用同步锁写入
                            if let Ok(mut l) = logs.write() {
                                l.push_back(line);
                                if l.len() > desc.log_limit.unwrap_or(1000) { l.pop_front(); }
                            }
                        }
                    });

                    // 监听控制信号或等待进程结束
                    tokio::select! {
                        status = child.wait() => {
                            // 进程结束逻辑
                        }
                        Some(msg) = rx.recv() => {
                            if let TaskControlMsg::Stop = msg {
                                let _ = child.kill().await;
                            }
                        }
                    }
                }
                Err(e) => {
                    // 处理失败状态
                }
            }
        });

        task.status = TaskStatus::Running { pid: 0, start_time: std::time::Instant::now() };
    }
}


//4. 渲染与交互细节
// 使用你提到的迭代器模式重构渲染函数。
impl TaskControlComponent {
    fn render_main_view(&mut self, f: &mut Frame, area: Rect) {
        let mut chunks = Layout::horizontal([
            Constraint::Percentage(40), // 左侧列表
            Constraint::Percentage(60), // 右侧详情
        ]).split(area);
        let mut chunks = chunks.into_iter();

        // 1. 渲染任务列表
        let items: Vec<ListItem> = self.tasks.iter().enumerate().map(|(i, t)| {
            let style = if i == self.selected_idx {
                Style::default().bg(Color::DarkGray).fg(Color::Yellow)
            } else {
                Style::default()
            };
            let status_sym = match t.status {
                TaskStatus::Running { .. } => Span::styled(" ● ", Style::default().fg(Color::Green)),
                TaskStatus::Stopped => Span::styled(" ○ ", Style::default().fg(Color::Gray)),
                TaskStatus::Failed(_) => Span::styled(" ✘ ", Style::default().fg(Color::Red)),
            };
            ListItem::new(Line::from(vec![status_sym, Span::raw(&t.desc.name)])).style(style)
        }).collect();

        if let Some(a) = chunks.next() {
            f.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title(" Tasks ")), *a);
        }

        // 2. 渲染右侧详情区
        if let Some(a) = chunks.next() {
            if let Some(task) = self.tasks.get(self.selected_idx) {
                let details = vec![
                    Line::from(vec![Span::raw("ID: "), Span::raw(&task.desc.id)]),
                    Line::from(vec![Span::raw("Command: "), Span::raw(&task.desc.command)]),
                    Line::from(vec![Span::raw("Args: "), Span::raw(format!("{:?}", task.desc.args))]),
                ];
                f.render_widget(Paragraph::new(details).block(Block::default().borders(Borders::ALL).title(" Detail ")), *a);
            }
        }
    }

    fn handle_list_keys(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected_idx = (self.selected_idx + 1) % self.tasks.len();
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_idx = self.selected_idx.checked_sub(1).unwrap_or(self.tasks.len() - 1);
                true
            }
            KeyCode::Enter => {
                // Toggle Start/Stop 逻辑
                self.start_task(self.selected_idx);
                true
            }
            KeyCode::Char('l') => {
                self.view_mode = ViewMode::Log;
                true
            }
            _ => false,
        }
    }

    fn render_full_log(&mut self, f: &mut Frame, area: Rect) {
    //     if let Some(task) = self.tasks.get(self.selected_idx) {
    //         // 注意：这里需要同步锁定 logs 进行渲染
    //         let logs = task.logs.blocking_read();
    //         let log_lines: Vec<Line> = logs.iter().map(|s| Line::from(s.as_str())).collect();
            
    //         f.render_widget(
    //             Paragraph::new(log_lines)
    //                 .block(Block::default().borders(Borders::ALL).title(format!(" Logs: {} (Esc to Back) ", task.desc.name)))
    //                 .scroll((self.log_scroll, 0)),
    //             area
    //         );
    //     }
    // }
        // 第 268 行修改如下：
        if let Some(task) = self.tasks.get(self.selected_idx) {
            // 使用 std 的 read()，它不会引起 Tokio Panic
            if let Ok(logs) = task.logs.read() {
                let log_lines: Vec<Line> = logs.iter()
                    .map(|s| Line::from(s.as_str()))
                    .collect();
                
                f.render_widget(
                    Paragraph::new(log_lines)
                        .block(Block::default().borders(Borders::ALL).title(" Logs "))
                        .scroll((self.log_scroll, 0)),
                    area
                );
            }
        }
    }
    fn handle_log_keys(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => { self.view_mode = ViewMode::List; true }
            KeyCode::Up => { self.log_scroll = self.log_scroll.saturating_sub(1); true }
            KeyCode::Down => { self.log_scroll = self.log_scroll.saturating_add(1); true }
            _ => false,
        }
    }
}