# AtlasPrime 全局消息与通知机制

本系统采用 **解耦的事件驱动架构**，确保后台繁重的 I/O 任务（如 SQLite 操作、遥测采集）不会阻塞前台 TUI 的渲染。

## 核心组件

### 1. GlobIO (消息总线)
`GlobIO` 是系统的神经中枢。它基于 `tokio::sync::broadcast` 实现，提供单生产者/多消费者的全局通信能力。

- **解耦输出**：通过 `GlobIO::info()` 等函数替代 `println!`，保护 TUI 界面不被破坏。
- **线程安全**：可以在任意异步 Task 或同步线程中调用。

### 2. GlobalEvent (事件载体)
定义了系统中传递的所有数据类型：
- `Status(String, StatusLevel, Option<Progress>)`: 用于底部通知栏和进度显示。
- `Data { key, data }`: 用于业务数据分发（如数据库统计结果、遥测历史回填）。

### 3. 底部通知联动逻辑
由三个专用组件在每帧 `update` 中协同工作：
- **NotifyComponent**: 负责文字显示与自动过期（Error 级 60s，其他 5s）。
- **ProgressComponent**: 负责渲染百分比条、任务计数器。
- **HintComponent**: 提供全局静态按键提示（如 `Alt+←/→`）。

## 工作流程

1. **产生**：后台 Task 执行完毕或捕获错误，调用 `GlobIO::error("msg")`。
2. **路由**：`GlobIO` 将事件广播给主循环。
3. **驱动**：`App Loop` 捕获到广播，调用 `app.update()` 触发各组件的 `try_recv()`。
4. **渲染**：组件更新内部状态并请求重绘，`terminal.draw()` 更新屏幕。

## 开发者指南

### 如何发送通知？
```rust
GlobIO::success("数据已保存至 SQLite");
GlobIO::error(format!("连接失败: {}", err));