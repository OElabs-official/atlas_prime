use std::{any::Any, collections::HashMap, sync::Arc};


#[derive(Clone, Debug)]
pub enum GlobalEvent {
    /// 数据更新：用于组件内容填充 (Deno -> Component)
    Data(String, serde_json::Value),
    
    /// 状态反馈：用于 Footer 渲染 (Async Task -> App Footer)
    /// 参数：内容, 等级, 可选进度
    Status(String, StatusLevel, Option<Progress>),
    
    /// 全局指令：改变应用行为 (Component/Deno -> App)
    Action(AppAction),
}

#[derive(Clone, Debug)]
pub enum _GlobalEvent {
    SyncProgress(ProgressType),
    Notify(String, NotificationLevel),
    ClearError, // 用于手动清除错误信号
    PushData {
        key: &'static str, // 例如 "public_ip"
        data: DynamicPayload,
    },
}

#[derive(Clone, Debug)]
pub enum ProgressType {
    Percentage(u16),     // 0-100%
    TaskCount(u32, u32), // (当前, 总数)
    Indeterminate,       // 未知进度 (显示为 [...])
}


// src/message.rs
#[derive(Clone, Debug)]
pub enum NotificationLevel {
    Info,    // 绿色/蓝色
    Warning, // 黄色
    Error,   // 红色 (需手动清除)
}



#[derive(Clone)] // 注意：Arc<dyn Any> 不能直接派生 Debug，需要特殊处理
pub struct DynamicPayload(pub Arc<dyn Any + Send + Sync>);

impl std::fmt::Debug for DynamicPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DynamicPayload(Arc<dyn Any>)")
    }
}

pub struct _ActiveNotification {
    pub content: String,
    pub level: NotificationLevel,
    pub progress: Option<ProgressType>,
    pub created_at: std::time::Instant,
}





#[derive(Clone, Debug)]
pub enum _Message {
    /// 数据更新：用于组件内容填充 (Deno -> Component)
    Data(String, serde_json::Value),
    
    /// 状态反馈：用于 Footer 渲染 (Async Task -> App Footer)
    /// 参数：内容, 等级, 可选进度
    Status(String, StatusLevel, Option<Progress>),
    
    /// 全局指令：改变应用行为 (Component/Deno -> App)
    Action(AppAction),
}

#[derive(Clone, Debug)]
pub enum AppAction {
    /// 切换标签页
    SwitchTab(usize),
    /// 强制退出程序
    Quit,
    /// 手动清除当前的 Footer 状态
    ClearStatus,
    /// 重新加载配置文件
    ReloadConfig,
    /// 弹出模态框（如果未来有 UI 叠加层需求）
    SetOverlay(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub enum StatusLevel {
    Info, Success, Warning, Error
}

#[derive(Clone, Debug)]
pub enum Progress {
    Percent(u8),
    Loading,
}