// src/message.rs
#[derive(Clone, Debug)]
pub enum NotificationLevel {
    Info,    // 绿色/蓝色
    Warning, // 黄色
    Error,   // 红色 (需手动清除)
}

#[derive(Clone, Debug)]
pub enum ProgressType {
    Percentage(u16),           // 0-100%
    TaskCount(u32, u32),       // (当前, 总数)
    Indeterminate,             // 未知进度 (显示为 [...])
}

#[derive(Clone, Debug)]
pub enum GlobalEvent {
    SyncProgress(ProgressType),
    Notify(String, NotificationLevel),
    ClearError, // 用于手动清除错误信号
}

// #[derive(Clone, Debug)]
// pub enum _GlobalEvent {
//     // 进度值 0-100
//     SyncProgress(u16),
//     // 全局通知
//     Notify(String),
// }