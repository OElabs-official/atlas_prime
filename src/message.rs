use std::{any::Any, collections::HashMap, sync::Arc};

// src/message.rs
#[derive(Clone, Debug)]
pub enum NotificationLevel {
    Info,    // 绿色/蓝色
    Warning, // 黄色
    Error,   // 红色 (需手动清除)
}

#[derive(Clone, Debug)]
pub enum ProgressType {
    Percentage(u16),     // 0-100%
    TaskCount(u32, u32), // (当前, 总数)
    Indeterminate,       // 未知进度 (显示为 [...])
}

#[derive(Clone, Debug)]
pub enum GlobalEvent {
    SyncProgress(ProgressType),
    Notify(String, NotificationLevel),
    ClearError, // 用于手动清除错误信号
    PushData {
        key: &'static str, // 例如 "public_ip"
        data: DynamicPayload,
    },
}

#[derive(Clone)] // 注意：Arc<dyn Any> 不能直接派生 Debug，需要特殊处理
pub struct DynamicPayload(pub Arc<dyn Any + Send + Sync>);

impl std::fmt::Debug for DynamicPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DynamicPayload(Arc<dyn Any>)")
    }
}