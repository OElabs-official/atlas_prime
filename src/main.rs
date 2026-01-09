mod app;
mod config;
mod constants;
mod ui;
mod utils;
//mod db;
mod message;

use crate::{
    app::App,
    config::{Config, setup_config_watcher},
    ui::component::Component,
};
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

/*
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;

    let tick_rate = Duration::from_millis(100);
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut app = App::new();

    loop {
        // 方案 3：自治組件數據更新
        app.tick();

        // 方案 2：組件化渲染
        terminal.draw(|f| {
            let area = f.size();
            let chunks = ratatui::layout::Layout::default()
                .constraints([
                    ratatui::layout::Constraint::Length(3), // 頂部標籤
                    ratatui::layout::Constraint::Min(0),    // 組件內容
                ])
                .split(area);

            // 渲染全域導航欄
            ui::render_tabs(f, chunks[0], app.active_tab);

            // 渲染當前自治組件
            if let Some(comp) = app.components.get_mut(app.active_tab) {
                comp.render(f, chunks[1], &app.config);
            }
        })?;

        // 事件處理
        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                // 1. 首先將按鍵交給「當前激活的組件」處理
                // 如果組件處理了（如滾動列表），它會返回 true
                let consumed = if let Some(comp) = app.components.get_mut(app.active_tab) {
                    comp.handle_key(key)
                } else {
                    false
                };

                // 2. 如果組件沒處理，則由全域（App）處理
                if !consumed {
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Right => app.next_tab(),
                        KeyCode::Left => app.prev_tab(),
                        // 這裡可以處理數字鍵直接跳轉 1, 2, 3...
                        KeyCode::Char('1') => app.active_tab = 0,
                        KeyCode::Char('2') => app.active_tab = 1,
                        KeyCode::Char('3') => app.active_tab = 2,
                        _ => {}
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}
    */

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
    // 初始化共享配置
    let shared_config = Arc::new(RwLock::new(Config::load()));
    // 4. 初始化 App
    let mut app = App::new(shared_config.clone()).await;
    // 2. 全局后台数据流 (从 App 获取广播订阅)
    let mut background_rx = app.tx.subscribe();

    let (render_tx, mut render_rx) = mpsc::channel::<()>(1);
    // 初始渲染请求
    let _ = render_tx.send(()).await;

    // 3. 启动热加载监听
    let watchertx = app.tx.clone();
    tokio::spawn(async move {
        let _ = setup_config_watcher(shared_config.clone(), render_tx.clone(), watchertx).await;
    });

    // --- 终端初始化 ---
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut reader = EventStream::new(); // 将 crossterm 事件转为异步流
    let mut render_interval = interval(Duration::from_millis(8));
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
    render_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            // 1. 渲染触发器：收到信号且没有挂起的事件时进行重绘
            /*
            响应式 UI 架构的核心设计模式：渲染收敛（Rendering Convergence）。
            简单来说，除了渲染分支外，其他所有分支（键盘、后台数据、定时器）都扮演着**“生产者”的角色，而渲染分支是唯一的“消费者”**
             */
            // _ = render_rx.recv() => {
            //     terminal.draw(|f| {
            //         let chunks = ratatui::layout::Layout::vertical([
            //             ratatui::layout::Constraint::Length(3),
            //             ratatui::layout::Constraint::Min(0),
            //         ]).split(f.area());

            //         crate::ui::render_tabs(f, chunks[0], app.active_tab);
            //         app.components[app.active_tab].render(f, chunks[1], &app.config);
            //     })?;
            // }

            /*
            如果后台数据更新极快（比如一个高频传感器每秒发 1000 次数据），background_rx 会不停地往 render_tx 塞任务，导致 CPU 依然爆表
            我们需要一个 “节流阀”：无论收到多少重绘请求，在一定时间内（比如 16ms，即 60FPS）只允许渲染一次。
             */
            _ = app.wait_for_render() => {
                if app.should_draw() {
                    terminal.draw(|f| {
                        app.render(f, f.area());
                    })?;
                    app.clear_render_request();
                }
            }

            

            // 2. 真正的异步按键流：完全不使用 sleep
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        if key.code == KeyCode::Char('q') { break; }
                        if !app.components[app.active_tab].handle_key(key) {
                            match key.code {
                                KeyCode::Right => app.next_tab(),
                                KeyCode::Left => app.prev_tab(),
                                // 這裡可以處理數字鍵直接跳轉 1, 2, 3...
                                KeyCode::Char('1') => app.active_tab = 0,
                                KeyCode::Char('2') => app.active_tab = 1,
                                KeyCode::Char('3') => app.active_tab = 2,
                                _ => {}
                            }
                        }
                        //let _ = render_tx.try_send(());
                        // 只要有交互就标记需要渲染
                        app.request_render();
                    },
                    Some(Ok(Event::Resize(_, _))) => {
                        // 窗口大小变了，必须强制重绘
                        app.request_render();
                    },
                    _ => {}
                }
            }

        _ = interval.tick() => {
            // 每 100ms 尝试检查一次 update
            if app.update() {
                // 如果 update 返回 true（数据变了），则标记需要重绘
                app.needs_render = true;
            }
        }

            // 分支 C: 关键修改！
        // 我们只需要感知“有消息来了”，不需要在 main 里处理 msg 的内容
        res = background_rx.recv() => {
            match res {
                Ok(_) => {
                    // 只要后台有任何广播，就驱动 App 整体 tick
                    // App::tick 会让每个组件去 try_recv 它们自己的 event_rx
                    app.tick();
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // 如果落后了，强制 tick 一次来清空缓冲区
                    app.tick();
                }
                _ => {}
            }
        }



            /*

            Ok(msg) = background_rx.recv() => {
                // 这个 tick 现在是“被动触发”的
                // 只有当后台真的发来 GlobalEvent 时，我们才让组件更新状态
                 app.tick();
            }
            这种语法在 Rust 中被称为 模式匹配赋值 (Pattern Matching in Select)。
            在 tokio::select! 宏的内部，它使用了类似于 match 语句的语法，而不是标准的 let 或 if let
            // [模式] = [异步 Future/表达式] => { [执行代码块] }
            Ok(msg) = background_rx.recv() => { ... }
            select! 的强大之处在于： 它不仅帮你 await 了，还顺便帮你做了 if let 的解包工作。

            广播通道（Broadcast Channel）有一个特性：如果发送方发送太快，接收方处理太慢，会产生 Lagged 错误。如果你只想处理成功的消息，上面的写法没问题。但如果你想处理错误，可以这样写：
                        res = background_rx.recv() => {
                            match res {
                                Ok(msg) => {
                                    if app.tick() { let _ = render_tx.try_send(()); }
                                },
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                    // 提示：我们落后了 n 条消息，通常在这里重置某些状态
                                },
                                Err(_) => { /* 通道已关闭 */ }
                            }
                        }
             */
        }
    }
    // --- 清理 ---
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show)?;
    Ok(())
}

// async fn _run_app_immediatemode() -> Result<(), Box<dyn std::error::Error>> {
//     enable_raw_mode()?;
//     let mut stdout = std::io::stdout();
//     execute!(stdout, EnterAlternateScreen)?;
//     let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

//     let mut app = App::new();
//     // 锁定 60 FPS 渲染频率
//     let mut interval = tokio::time::interval(Duration::from_millis(25));

//     loop {
//         interval.tick().await;
//         app.tick(); // 驱动所有自治组件的异步收割

/*
在 TUI（终端用户界面）开发中，app.tick() 和 interval.tick() 构成了程序的心脏脉搏。它们共同协作，既保证了界面的流畅度，又实现了高效的资源管理。

1. interval.tick()：渲染节奏的“指挥棒”
    作用： 锁定渲染帧率（FPS）。 工作原理：
    节拍器效应：tokio::time::interval(Duration::from_millis(16)) 创建了一个每 16 毫秒触发一次的异步计时器。
    非阻塞等待：interval.tick().await 会让出 CPU 控制权，直到下一个 16ms 到来。这保证了你的程序不会因为疯狂跑循环而占满 100% 的 CPU。
    对齐渲染：通过在循环开始处调用它，你确保了后续的 terminal.draw 也是以大约 60 FPS 的频率运行。这让动画和列表滚动看起来非常平滑。

2. app.tick()：状态数据的“收割机”
    作用： 将后台异步任务产生的数据“同步”到 UI 状态中。 工作原理：
    自治组件轮询：在我们的架构中，app.tick() 会遍历所有组件并调用它们的 update() 方法。
    非阻塞通信：组件内部使用 rx.try_recv()。这是一个瞬时操作：
    如果后台线程（Tokio Task）已经把 CPU 使用率或文件列表算好了，try_recv 立即拿到数据并更新内存变量。
    如果数据还没准备好，它立即返回错误，app.tick() 继续执行。
    状态准备：它的核心目的是在执行 terminal.draw 之前，确保内存里的数据是最新的“快照”。

3. 两者如何协同工作？
    在一个典型的 16ms 周期内，流程如下：
    等待节拍 (interval.tick().await)： 主线程挂起，不消耗 CPU。16ms 时间到，Tokio 唤醒主线程。
    数据采集 (app.tick())： 主线程迅速访问各个组件的 mpsc::Receiver。此时主线程就像一个快递员，把后台已经送达的所有“数据包裹”拆开，填到组件的 data 字段里。
    瞬间绘图 (terminal.draw(...))： Ratatui 根据组件里的 data 变量计算哪些字符发生了变化，并写入终端缓冲区。
    事件响应 (event::poll(...))： 检查键盘有没有按。如果有，分发给组件处理。
*/

// terminal.draw(|f| {
//     let area = f.area();
//     let chunks = ratatui::layout::Layout::vertical([
//         ratatui::layout::Constraint::Length(3),
//         ratatui::layout::Constraint::Min(0),
//     ])
//     .split(area);

//     crate::ui::render_tabs(f, chunks[0], app.active_tab);
//     app.components[app.active_tab].render(f, chunks[1], &app.config);
// })?;
/*
这段代码是 Ratatui 框架的核心：即时模式（Immediate Mode）渲染循环。它的作用是每秒钟几十次地“重绘”整个屏幕，将内存中的数据转换成你在终端看到的像素。

    我们可以从作用和底层原理两个维度来深度解析。
    1. 代码的作用

    这段代码的主要职责是 “空间分配” 与 “分层渲染”：

        terminal.draw(|f| { ... })：开启一个画布。f（Frame）是这块画布的句柄。

        Layout::vertical：将屏幕垂直切割。

            Length(3)：顶部固定留出 3 行给导航栏。

            Min(0)：剩下的所有空间都动态分配给内容区。

        render_tabs：在顶部的 chunks[0] 区域画出标签页。

        app.components[...].render：在底部的 chunks[1] 区域画出当前选中的组件内容。

    2. 工作原理：即时模式渲染 (Immediate Mode GUI)

    Ratatui 采用的是 即时模式，这与网页（DOM）或传统的 Windows 窗口程序（Retained Mode）完全不同。
    原理 A：每一帧都从零开始

    在传统的 GUI 中，如果你想修改一个按钮的文字，你会调用 button.setText()，系统会记住这个状态。 但在 Ratatui 中，系统不记忆任何 UI 状态。

        当你的主循环运行到这一行时，它会清除上一次的所有显示。

        它根据你 app 变量里当前的数据（比如 cpu_usage 是 50%），重新计算每一行该显示什么字符。

        优势：UI 永远与数据同步，不需要繁琐的状态同步逻辑。

    原理 B：双缓冲区 (Double Buffering)

    当你调用 f.render_widget 时，它并没有立刻在屏幕上打字，而是在内存里的 “新缓冲区（Current Frame）” 中绘图。

        对比差异：Ratatui 会将这个“新缓冲区”与屏幕上正在显示的“旧缓冲区（Last Frame）”进行对比。

        增量更新：它只向终端发送发生变化的那部分字符的 ANSI 转义码。

        结果：即使你每秒重绘 60 次，终端也不会闪烁，且网络带宽占用极低。
 */
//         if event::poll(Duration::from_millis(0))? {
//             if let Event::Key(key) = event::read()? {
//                 if !app.components[app.active_tab].handle_key(key) {
//                     match key.code {
//                         KeyCode::Char('q') => break,
//                         KeyCode::Right => app.next_tab(),
//                         KeyCode::Left => app.prev_tab(),
//                         // 這裡可以處理數字鍵直接跳轉 1, 2, 3...
//                         KeyCode::Char('1') => app.active_tab = 0,
//                         KeyCode::Char('2') => app.active_tab = 1,
//                         KeyCode::Char('3') => app.active_tab = 2,
//                         _ => {}
//                     }
//                 }
//             }
//         }
//     }

//     disable_raw_mode()?;
//     execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
//     Ok(())
// }



/*
通道类型,变量名,流向,核心功能
Broadcast (多生产者多消费者),app.tx / event_rx,后台任务 -> 多个组件,全局通知中心。用于异步任务（如未来的 Deno 消息、IP 获取）向 UI 组件推送数据。只要消息发出，所有订阅的组件都能收到。
MPSC (多生产者单消费者),render_tx / render_rx,各种事件 -> 主循环,渲染触发器。当配置更新、按键按下或数据变动时，发送一个信号告诉主循环：“该刷一下屏幕了”。
Async Stream,reader (EventStream),终端 -> 主循环,用户交互输入。将底层的字节流转为 Rust 的 KeyEvent。
Internal MPSC,info.rx (如果有),内部任务 -> 组件,组件私有流。用于组件内部的特定任务（如你之前代码中单独采样的 CPU 频率）。

*/