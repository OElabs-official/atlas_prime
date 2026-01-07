use crate::config::Config;
use ratatui::{Frame, layout::Rect};

pub trait Component: Send + Sync {
    // 同步函数：由主循环高频调用，内部使用 try_recv 检查异步状态
    fn update(&mut self) -> bool;
    /*
    “consider moving update to another trait”。 在大型项目中，渲染（Render）和逻辑更新（Update）的生命周期其实是可以分离的。我们可以定义一个不需要动态分发的后台逻辑层。
    但在 TUI 里，我们更常用的变通方法是：保留同步的 update 接口，但在内部驱动异步逻辑。
    例如：
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(10);
        // 在创建组件时，启动一个真正的异步任务
        tokio::spawn(async move {
            loop {
                // 真正的异步逻辑在这里跑
                let data = fetch_complex_data().await;
                let _ = tx.send(data).await;
            }
        });
        Self { rx, data: None }
    }
     */

    // 同步渲染：根据当前内存状态绘图
    fn render(&mut self, f: &mut Frame, area: Rect); //此函数不能异步，因此需要使用同步解锁

    // 事件处理：返回 true 表示消费了事件，阻止冒泡
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool;
}
