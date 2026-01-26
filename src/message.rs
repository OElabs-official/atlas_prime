use std::{any::Any, collections::HashMap, sync::Arc};

use crate::prelude::GlobIO;

#[derive(Clone, Debug)]
pub enum GlobalEvent {
    /// 数据更新：用于组件内容填充 (Deno -> Component)
    //Data(String, serde_json::Value),
    Data {
        key: &'static str, // 例如 "public_ip"
        data: DynamicPayload,
    },

    /// 状态反馈：用于 Footer 渲染 (Async Task -> App Footer)
    /// 参数：内容, 等级, 可选进度
    Status(String, StatusLevel, Option<Progress>),
    // 全局指令：改变应用行为 (Component/Deno -> App) 如果需要，在Data 里
    // Action(AppAction),
}
#[derive(Clone)] // 注意：Arc<dyn Any> 不能直接派生 Debug，需要特殊处理
pub struct DynamicPayload(pub Arc<dyn Any + Send + Sync>);

impl std::fmt::Debug for DynamicPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DynamicPayload(Arc<dyn Any>)")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum StatusLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub enum Progress {
    Percent(u8),
    TaskCount(u32, u32), // (当前, 总数)
    Loading,
}


impl GlobIO {
    /// 模拟 println! -> 发送 Info 级别的通知
    pub fn info<S: Into<String>>(msg: S) {
        let _ = Self::send().send(GlobalEvent::Status(
            msg.into(),
            StatusLevel::Info,
            None,
        ));
    }

    /// 模拟成功提示
    pub fn success<S: Into<String>>(msg: S) {
        let _ = Self::send().send(GlobalEvent::Status(
            msg.into(),
            StatusLevel::Success,
            None,
        ));
    }

    /// 模拟警告提示
    pub fn warn<S: Into<String>>(msg: S) {
        let _ = Self::send().send(GlobalEvent::Status(
            msg.into(),
            StatusLevel::Warning,
            None,
        ));
    }

    /// 模拟 eprintln! -> 发送 Error 级别的通知
    pub fn error<S: Into<String>>(msg: S) {
        let _ = Self::send().send(GlobalEvent::Status(
            msg.into(),
            StatusLevel::Error,
            None,
        ));
    }

    /// 快捷发送带进度的状态
    pub fn progress<S: Into<String>>(msg: S, level: StatusLevel, prog: Progress) {
        let _ = Self::send().send(GlobalEvent::Status(
            msg.into(),
            level,
            Some(prog),
        ));
    }
}

