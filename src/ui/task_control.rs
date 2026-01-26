use crate::config::Config;
use crate::prelude::*;
use crate::constans::{TASK_RAW_JSON};
use crate::message::{DynamicPayload, GlobalEvent, StatusLevel};
use crate::{
    config::SharedConfig,
    ui::component::Component,
};
use ansi_to_tui::IntoText;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{prelude::*, widgets::*};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt as _, BufReader};
use tokio::sync::broadcast::Sender;
use tokio::sync::{RwLock as ARwLock, mpsc}; // 引入转换 trait

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum RestartPolicy {
    Always, // 自动重启
    Warn,   // 弹出警告（通过全局事件发送）
    Never,  // 仅停止，不做处理
}

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
    pub restart_policy: Option<RestartPolicy>,
}

#[derive(Debug, PartialEq, Clone)]
pub enum TaskStatus {
    Stopped,
    Running {
        pid: u32,
        start_time: std::time::Instant,
    },
    Failed(String),
}

/// 2. 运行时任务对象
pub struct TaskRuntime {
    pub desc: TaskDescriptor,
    // 状态必须是可跨线程修改的，否则 render 永远看不到后台的更新
    pub status: Arc<RwLock<TaskStatus>>,
    pub logs: Arc<RwLock<VecDeque<String>>>,
    pub control_tx: Option<mpsc::Sender<TaskControlMsg>>,
}
pub struct _TaskRuntime {
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
    glob_recv: GlobRecv,

    input: String,
}

#[derive(PartialEq)]
enum ViewMode {
    List, // 列表 + 详情模式
    Log,  // 全屏日志模式
}

impl Component for TaskControlComponent {
    fn init() -> Self {
        // 模拟从 JSON 加载过程（实际开发中可使用 std::fs::read_to_string）

        let mut descs: Vec<TaskDescriptor> =
            serde_json::from_str(&AtlasPath::read_task_json().unwrap_or_default()).unwrap_or_default();

        // --- 新增：扫描 scripts 目录 ---
        let script_dir = AtlasPath::get_script_dir();
        if let Ok(entries) = std::fs::read_dir(&script_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                // 逻辑：必须是文件，且后缀是 .ts
                if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("ts") {
                    let file_stem = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown");

                    // 为脚本创建 Deno 任务描述符
                    let deno_task = TaskDescriptor {
                        id: format!("deno_{}", file_stem),
                        name: format!("🦕 {}", file_stem), // 增加图标区分
                        command: "deno".to_string(),
                        // 常用参数：-A (全权限), run, 脚本路径
                        args: vec![
                            "run".into(),
                            "-A".into(),
                            "--unstable-kv".into(),
                            "--unstable-cron".into(),
                            path.to_string_lossy().into_owned(),
                        ],
                        cwd: Some(script_dir.to_string_lossy().to_string()),
                        envs: None,
                        autostart: false, // 脚本任务建议手动触发
                        group: "Scripts".to_string(),
                        log_limit: Some(1000),
                        restart_policy: Some(RestartPolicy::Never),
                    };
                    descs.push(deno_task);
                }
            }
        }

        let mut tasks = Vec::new();
        for d in descs {
            let runtime = TaskRuntime {
                desc: d,
                status: Arc::new(RwLock::new(TaskStatus::Stopped)),
                //TaskStatus::Stopped,
                logs: Arc::new(RwLock::new(VecDeque::with_capacity(1000))),
                control_tx: None,
            };
            tasks.push(runtime);
        }

        let mut component = Self {
            config:Config::get(),
            tasks,
            selected_idx: 0,
            view_mode: ViewMode::List,
            log_scroll: 0,
            glob_send:GlobIO::send(),
            glob_recv:GlobIO::recv(),
            input: Default::default(),
        };

        // 处理自动启动
        component.auto_start_tasks();

        component
    }

    fn update(&mut self) -> bool {
        // 假设 self.glob_recv 是 App 自己的消息订阅端
        while let Ok(event) = self.glob_recv.try_recv() {
            match event {
                // 只有当收到 Data 且 key 为 "rend" 时才标记需要重绘
                GlobalEvent::Data { key, .. } if key == "rend" => {
                    return true;
                }
                _ => {} // ... 处理其他全局事件
            }
        }
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
                self.start_or_stop_task(i);
            }
        }
    }

    fn start_or_stop_task(&mut self, idx: usize) {
        let task = &mut self.tasks[idx];

        // 1. 停止逻辑
        if let TaskStatus::Running { .. } = *task.status.read().unwrap() {
            if let Some(tx) = &task.control_tx {
                let _ = tx.try_send(TaskControlMsg::Stop);
            }
            // 注意：这里不要直接设为 Stopped，让后台协程退出时自动设置更准确
            let _ = self.glob_send.send(GlobalEvent::Data {
                key: "rend",
                data: DynamicPayload(Arc::new(())),
            });
            return;
        }

        // 2. 准备启动
        let desc = task.desc.clone();
        let logs = task.logs.clone();
        let status_lock = task.status.clone(); // 克隆状态锁给后台
        let (tx, mut rx) = mpsc::channel::<TaskControlMsg>(32);
        task.control_tx = Some(tx);
        let glob_send = self.glob_send.clone();

        tokio::spawn(async move {
            let mut cmd = tokio::process::Command::new(&desc.command);
            cmd.args(&desc.args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .stdin(Stdio::piped());

            if let Some(cwd) = &desc.cwd {
                cmd.current_dir(cwd);
            }

            match cmd.spawn() {
                Ok(mut child) => {
                    let pid = child.id().expect("Failed to get PID");
                    {
                        let mut s = status_lock.write().unwrap();
                        *s = TaskStatus::Running {
                            pid,
                            start_time: std::time::Instant::now(),
                        };
                    }

                    let stdout = child.stdout.take().unwrap();
                    let stderr = child.stderr.take().unwrap(); // 也要捕获错误输出，否则看不到报错
                    let mut stdin = child.stdin.take().unwrap(); // 获取 stdin 句柄

                    // --- 1. 日志读取协程 (继续保留，因为它只读管道) ---
                    let logs_for_io = logs.clone();
                    let glob_for_io = glob_send.clone();
                    tokio::spawn(async move {
                        // use tokio::io::AsyncReadExt as _;
                        let mut out_reader = BufReader::new(stdout).lines();
                        let mut err_reader = BufReader::new(stderr).lines();
                        loop {
                            let glob_send_a = glob_send.clone();
                            let glob_send_b = glob_send.clone();
                            tokio::select! {
                                line = out_reader.next_line() => {
                                    if let Ok(Some(l)) = line { append_log(&logs, l ,glob_send_a); } else { break; }
                                }
                                line = err_reader.next_line() => {
                                    if let Ok(Some(l)) = line { append_log(&logs, format!("[ERR] {}", l),glob_send_b); } else { break; }
                                }
                            }
                        }
                    });
                    // 辅助函数
                    fn append_log(
                        logs: &Arc<RwLock<VecDeque<String>>>,
                        line: String,
                        glob_send: Sender<GlobalEvent>,
                    ) {
                        if let Ok(mut l) = logs.write() {
                            l.push_back(line);
                            if l.len() > 1000 {
                                l.pop_front();
                            }
                            let _ = glob_send.send(GlobalEvent::Data {
                                key: "rend",
                                data: DynamicPayload(Arc::new(())),
                            });
                        }
                    }

                    let mut is_manual_stop = false;

                    let exit_result = loop {
                        tokio::select! {
                            // 监听进程自然退出
                            res = child.wait() => {
                                break res;
                            }
                            // 监听 UI 发来的控制消息
                            Some(msg) = rx.recv() => {
                                match msg {
                                    TaskControlMsg::Stdin(text) => {
                                        let _ = stdin.write_all(text.as_bytes()).await;
                                        let _ = stdin.write_all(b"\n").await;
                                        let _ = stdin.flush().await;
                                    }
                                    TaskControlMsg::Stop => {
                                        is_manual_stop = true;
                                        let _ = child.kill().await;
                                        // 继续循环，等待 child.wait() 在下一轮被触发以回收资源
                                    }
                                }
                            }
                        }
                    };



                    let mut s = status_lock.write().unwrap();
                    match exit_result {
                        Ok(status) => {
                            if is_manual_stop || status.success() {
                                // 手动停止或正常退出 (exit code 0)
                                *s = TaskStatus::Stopped;
                            } else {
                                // 非正常退出
                                let code = status
                                    .code()
                                    .map(|c| c.to_string())
                                    .unwrap_or_else(|| "Killed by signal".into());
                                *s = TaskStatus::Failed(format!("Exit Code: {}", code));

                                // 只有在非手动停止且配置了 Always 时才重启
                                if let Some(RestartPolicy::Always) = desc.restart_policy {
                                    // 这里触发重启逻辑...
                                }
                            }
                        }
                        Err(e) => {
                            *s = TaskStatus::Failed(e.to_string());
                        }
                    }
                }
                Err(e) => {
                    let mut s = status_lock.write().unwrap();
                    *s = TaskStatus::Failed(e.to_string());
                }
            }
        });
        let _ = self.glob_send.send(GlobalEvent::Data {
            key: "rend",
            data: DynamicPayload(Arc::new(())),
        });
    }
}

//4. 渲染与交互细节
// 使用你提到的迭代器模式重构渲染函数。
impl TaskControlComponent {
    // --- 界面修改：上下排列布局 ---
    fn render_main_view(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Percentage(50), // 上方任务列表
            Constraint::Percentage(50), // 下方详情面板
        ])
        .split(area);
        let mut chunks = chunks.into_iter();

        // 1. 任务列表
        let items: Vec<ListItem> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let is_selected = i == self.selected_idx;

                // 状态文字化
                let status_guard = t.status.read().unwrap(); // 获取当前状态快照
                let (status_text, status_style) = match &*status_guard {
                    TaskStatus::Running { .. } => (
                        " RUNNING ",
                        Style::default().bg(Color::Green).fg(Color::Black),
                    ),
                    TaskStatus::Stopped => (
                        " STOPPED ",
                        Style::default().bg(Color::DarkGray).fg(Color::White),
                    ),
                    TaskStatus::Failed(_) => (
                        " FAILED  ",
                        Style::default().bg(Color::Red).fg(Color::White),
                    ),
                };

                let mut line = Line::from(vec![
                    Span::styled(status_text, status_style),
                    Span::raw(format!(" {:<20}", t.desc.name)),
                    Span::styled(
                        format!(" [{}]", t.desc.group),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);

                if is_selected {
                    line = line.patch_style(
                        Style::default()
                            .add_modifier(Modifier::REVERSED)
                            .fg(Color::Yellow),
                    );
                }
                ListItem::new(line)
            })
            .collect();

        if let Some(a) = chunks.next() {
            f.render_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" ⚙️ Task Manager "),
                    )
                    .highlight_symbol(">> "),
                *a,
            );
        }

        // 2. 详情面板
        if let Some(a) = chunks.next() {
            if let Some(task) = self.tasks.get(self.selected_idx) {
                let status_guard = task.status.read().unwrap();

                let status_str = match &*status_guard {
                    TaskStatus::Running { pid, start_time } => {
                        let elapsed = start_time.elapsed().as_secs();
                        format!("Running (PID: {}) - Uptime: {}s", pid, elapsed)
                    }
                    TaskStatus::Failed(err) => format!("Failed: {}", err),
                    TaskStatus::Stopped => "Inactive / Stopped".to_string(),
                };

                let details = vec![
                    Line::from(vec![
                        Span::styled("● NAME:    ", Style::default().fg(Color::Cyan)),
                        Span::raw(&task.desc.name),
                    ]),
                    Line::from(vec![
                        Span::styled("● STATUS:  ", Style::default().fg(Color::Cyan)),
                        Span::raw(status_str),
                    ]),
                    Line::from(vec![
                        Span::styled("● COMMAND: ", Style::default().fg(Color::Cyan)),
                        Span::raw(&task.desc.command),
                    ]),
                    Line::from(vec![
                        Span::styled("● ARGS:    ", Style::default().fg(Color::Cyan)),
                        Span::raw(format!("{:?}", task.desc.args)),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        " [x] Start/Stop   [Enter] View Logs   [↑/↓] Navigate ",
                        Style::default().bg(Color::Blue).fg(Color::White),
                    )),
                ];
                f.render_widget(
                    Paragraph::new(details).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" 📋 Task Detail "),
                    ),
                    *a,
                );
            }
        }
    }

    // --- 操作修改：按键映射 ---
    fn handle_list_keys(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected_idx = (self.selected_idx + 1) % self.tasks.len();
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_idx = self
                    .selected_idx
                    .checked_sub(1)
                    .unwrap_or(self.tasks.len() - 1);
                true
            }
            // 修改：按下 x 启动或终止
            KeyCode::Char('x') => {
                self.start_or_stop_task(self.selected_idx);
                true
            }
            // 修改：按下 Enter 查看日志
            KeyCode::Enter => {
                self.view_mode = ViewMode::Log;
                true
            }
            _ => false,
        }
    }
    fn handle_log_keys(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.view_mode = ViewMode::List;
                self.input.clear();
                true
            }
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    if let Some(task) = self.tasks.get(self.selected_idx) {
                        if let Some(tx) = &task.control_tx {
                            // 发送给进程
                            let _ = tx.try_send(TaskControlMsg::Stdin(self.input.clone()));
                            // 同时把输入的内容也显示在日志里，方便确认
                            if let Ok(mut l) = task.logs.write() {
                                l.push_back(format!(">>> {}", self.input));
                            }
                        }
                    }
                    self.input.clear();
                }
                true
            }
            KeyCode::Backspace => {
                self.input.pop();
                true
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                true
            }
            // 允许通过 PageUp/Down 滚动日志
            KeyCode::Up => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
                true
            }
            KeyCode::Down => {
                self.log_scroll = self.log_scroll.saturating_add(1);
                true
            }
            _ => false,
        }
    }
    fn render_full_log(&mut self, f: &mut Frame, area: Rect) {
        // 划分布局：上方是日志，下方是 3 行高度的输入框
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

        if let Some(task) = self.tasks.get(self.selected_idx) {
            // 1. 渲染日志 (上方)
            if let Ok(logs) = task.logs.read() {
                let all_logs = logs.iter().cloned().collect::<Vec<_>>().join("\n");

                // 使用 ansi_to_tui 将其解析为 Ratatui 的 Text 对象
                // 如果解析失败，回退到普通字符串显示
                let text = all_logs.into_text().unwrap_or_else(|_| Text::raw(all_logs));

                f.render_widget(
                    Paragraph::new(text)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(format!(" Logs: {} ", task.desc.name)),
                        )
                        .scroll((self.log_scroll, 0)),
                    chunks[0],
                );
            }

            // 2. 渲染输入框 (下方)
            let input_block = Paragraph::new(self.input.as_str())
                .style(Style::default().fg(Color::Yellow))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Stdin (Press Enter to Send) "),
                );
            f.render_widget(input_block, chunks[1]);

            // 设置光标位置，使其看起来像个真正的输入框
            f.set_cursor_position((chunks[1].x + self.input.len() as u16 + 1, chunks[1].y + 1));
        }
    }

    fn _handle_log_keys(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.view_mode = ViewMode::List;
                true
            }
            KeyCode::Up => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
                true
            }
            KeyCode::Down => {
                self.log_scroll = self.log_scroll.saturating_add(1);
                true
            }
            _ => false,
        }
    }
}
