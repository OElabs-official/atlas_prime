mod app;
mod config;
mod constants;
mod ui;
mod utils;
//mod db;
mod message;

use crossterm::event::KeyModifiers;
use notify::{RecursiveMode, Watcher};
use std::path::Path;
use tokio::sync::broadcast;

use crate::config::SharedConfig;
use crate::message::{GlobalEvent, StatusLevel};

use crate::{app::App, config::Config, ui::component::Component};
use backtrace::Backtrace;
use crossterm::{
    event::{self, Event, EventStream, KeyCode},
    execute,
    terminal::*,
}; // 需要启用 crossterm 的 "event-stream" feature
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    fs::OpenOptions,
    io::{self, Write},
    panic,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{RwLock, mpsc},
    time::{MissedTickBehavior, interval},
};

pub async fn setup_config_watcher(
    shared_config: SharedConfig,
    // render_tx: tokio::sync::mpsc::Sender<()>,
    glob_send: tokio::sync::broadcast::Sender<GlobalEvent>,
) {
    tokio::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        // 创建文件监听器
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(e) = res {
                if e.kind.is_modify() {
                    let _ = tx.blocking_send(());
                }
            }
        })
        .expect("Failed to create watcher");

        watcher
            .watch(Path::new("config.toml"), RecursiveMode::NonRecursive)
            .ok();

        while let Some(_) = rx.recv().await {
            // 1. 读取新配置
            let new_conf = Config::load();

            // 2. 写入共享内存
            {
                let mut guard = shared_config.write().await;
                *guard = new_conf;
                //println!("Config hot-reloaded!");

                let _ = glob_send.send(GlobalEvent::Status(
                    "Config Hot-Reloaded".into(),
                    StatusLevel::Info,
                    None,
                ));
                // let _ = render_tx.send(()).await;
            }

            // 3. 强制触发全局重绘
            // let _ = render_tx.send(()).await;
            // 发送广播消息，通知 App 数据已变
        }
    });
}

fn setup_panic_hook() {
    panic::set_hook(Box::new(|panic_info| {
        // 1. 立即恢复终端，防止界面错乱
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);

        // 2. 获取当前的堆栈信息
        let bt = Backtrace::new();
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");

        // 3. 构造错误日志
        let payload = panic_info
            .payload()
            .downcast_ref::<&str>()
            .unwrap_or(&"Unknown Panic");
        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();

        let log_msg = format!(
            "--- PANIC AT {} ---\nLocation: {}\nError: {}\nStack Trace:\n{:?}\n\n",
            timestamp, location, payload, bt
        );

        // 4. 写入 crash.log
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open("crash.log")
        {
            let _ = file.write_all(log_msg.as_bytes());
        }

        // 5. 在终端打印简短提示
        eprintln!("程序发生致命错误，详细信息已保存至 crash.log");
        eprintln!("错误摘要: {} at {}", payload, location);
    }));
}

fn main() {
    // 初始化崩溃钩子
    setup_panic_hook();

    // 创建异步运行时
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("无法创建 Tokio 运行时");

    // 在运行时中捕获逻辑错误
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            if let Err(e) = run_app().await {
                eprintln!("应用逻辑错误: {}", e);
            }
        });
    }));

    if result.is_err() {
        // 这里可以执行某些恢复后的后续操作
        eprintln!("运行时异常已捕获，终端环境已恢复。");
    }
}

/*
为了方便你后续 Deno 的开发，这是梳理后的通道映射：
通道	物理载体	逻辑角色	刷新频率控制
渲染节流阀	render_interval	指挥官：决定用户眼睛看到的最高帧率。	16ms (60FPS) 或 33ms (30FPS)
状态计时器	status_interval	采集员：决定内存、磁盘等系统数据的更新精度。	500ms
全局广播	app.tx (Broadcast)	传声筒：后台任务（Deno/Network）的异步通知。	随机（由任务完成时间决定）
事件流	reader (Stream)	交互点：用户的键盘或终端缩放事件。	随机（由用户操作决定）
*/

async fn run_app() -> Result<(), Box<dyn std::error::Error>> {
    // 讨论是否要移除
    // let (render_tx, mut render_rx) = mpsc::channel::<()>(1);
    // // 初始渲染请求
    // let _ = render_tx.send(()).await;

    // 初始化共享配置
    let shared_config = Arc::new(RwLock::new(Config::load()));
    // 4. 初始化 App
    // let mut app = App::new(shared_config.clone()).await;
    let (glob_send, glob_recv) = broadcast::channel(100);
    let mut app = App::init(shared_config.clone(), glob_send, glob_recv);
    // 2. 全局后台数据流 (从 App 获取广播订阅)
    let mut task_glob_recv = app.glob_send.subscribe();

    // 3. 启动热加载监听
    let watchertx = app.glob_send.clone();
    tokio::spawn(async move {
        // let _ = setup_config_watcher(shared_config.clone(), render_tx.clone(), watchertx).await;
        let _ = setup_config_watcher(shared_config.clone(), watchertx).await;
    });

    // --- 终端初始化 ---
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut reader = EventStream::new(); // 将 crossterm 事件转为异步流
    // let mut render_interval = interval(Duration::from_millis(8));
    // let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
    // render_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut render_clock = interval(Duration::from_millis(8)); // 约 60FPS，用于平滑渲染 ,glob

    loop 
    {
        tokio::select! 
        {
            /*
            如果后台数据更新极快（比如一个高频传感器每秒发 1000 次数据），background_rx 会不停地往 render_tx 塞任务，导致 CPU 依然爆表
            我们需要一个 “节流阀”：无论收到多少重绘请求，在一定时间内（比如 16ms，即 60FPS）只允许渲染一次。
                */

            // --- 核心修改：渲染分支 唯一的渲染出口 ---
            // 每一帧(16ms)都检查是否需要重绘
            _ = render_clock.tick() => 
            {
                // should_draw 应该检查:
                // 1. 之前有没有 request_render()
                // 2. 或者有没有后台数据更新标记
                if app.should_draw() {
                    terminal.draw(|f| app.render(f, f.area()))?;
                    app.clear_render_request();
                }
            }

            // 2. 真正的异步按键流：完全不使用 sleep    分支 A：交互事件
            maybe_event = reader.next() => 
            {
                match maybe_event 
                {
                    Some(Ok(Event::Key(key))) => 
                    {
                        // 1. 只有绝对全局的退出键（如 Ctrl+C 或特定 Q）在这里拦截
                        // 如果你想让子组件也能处理 'q'，就把这一行也删掉，全部交给 app.handle_key
                        // 1. 捕获 Ctrl + C 退出
                        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                            break;
                        }

                        // 2. 【核心逻辑】将所有按键事件交给 App 处理
                        // app.handle_key 内部会处理：Alt+方向键、Tab切换、以及子组件的输入
                        if app.handle_key(key) {
                            // 如果 App 处理了该事件（返回 true），标记需要重绘
                            app.request_render();
                        }
                    },
                    Some(Ok(Event::Resize(_, _))) => {
                        // 窗口大小变了，必须强制重绘
                        app.request_render();
                    },
                        _ => {}
                }
            }



            //  分支 B：后台数据推送
            // 我们只需要感知“有消息来了”，不需要在 main 里处理 msg 的内容
            res = task_glob_recv.recv() => {
                match res {
                    Ok(_ge) => {
                        // match ge
                        // {
                        //     GlobalEvent::Data { key, data } => {app.update();},
                        //     GlobalEvent::Status(_, status_level, progress) => {app.update();},
                        // }
                        // 只要后台有任何广播，就驱动 App 整体 tick
                        // App::tick 会让每个组件去 try_recv 它们自己的 event_rx
                        app.update();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // 如果落后了，强制 tick 一次来清空缓冲区
                        app.update();
                    }
                    _ => {}
                }
            }



        }
    }
    // --- 清理 ---
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show)?;
    Ok(())
}

/*
通道类型,变量名,流向,核心功能
Broadcast (多生产者多消费者),app.tx / event_rx,后台任务 -> 多个组件,全局通知中心。用于异步任务（如未来的 Deno 消息、IP 获取）向 UI 组件推送数据。只要消息发出，所有订阅的组件都能收到。
MPSC (多生产者单消费者),render_tx / render_rx,各种事件 -> 主循环,渲染触发器。当配置更新、按键按下或数据变动时，发送一个信号告诉主循环：“该刷一下屏幕了”。
Async Stream,reader (EventStream),终端 -> 主循环,用户交互输入。将底层的字节流转为 Rust 的 KeyEvent。
Internal MPSC,info.rx (如果有),内部任务 -> 组件,组件私有流。用于组件内部的特定任务（如你之前代码中单独采样的 CPU 频率）。

*/
