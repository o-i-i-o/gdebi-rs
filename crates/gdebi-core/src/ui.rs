//! 前端交互能力 trait 层:核心流程只通过这些 trait 与 UI 通信,
//! GUI 前端的差异全部收敛在各前端的实现里
//! (对应旧版的 progress adapter + show_alert 机制)。
//!
//! 约定:核心流程通过 [`Ui`] 呈现错误后仍返回 Err,前端收到 Err
//! 只决定退出码,不再重复输出。

use crate::error::GdebiError;

/// 告警类别(对应旧版 Gtk.MessageType)
#[derive(Debug, Clone, Copy)]
pub enum AlertKind {
    Error,
    Warning,
    Info,
}

/// 前端交互能力(模态语义)
pub trait Ui {
    /// 告警/错误呈现(旧版 show_alert)
    fn alert(&self, kind: AlertKind, header: &str, body: Option<&str>) -> Result<(), GdebiError>;

    /// 是/否确认(旧版 y/N 提示与对话框);非交互流程直接返回预设值
    fn confirm(&self, header: &str, body: Option<&str>) -> Result<bool, GdebiError>;
}

/// dpkg/apt 安装进度(旧版 InstallProgressAdapter / status-fd):
/// 子进程输出逐行转发,前端写入嵌入终端(对应旧版 expander 的终端输出区)
pub trait InstallProgress {
    fn output_line(&self, line: &str);
}
