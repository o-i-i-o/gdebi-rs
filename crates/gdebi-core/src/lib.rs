//! gdebi-core:本地 .deb 解析、纯 Rust 依赖求解与安装编排核心(gdebi 的 Rust 重写)。
//!
//! 定位:本程序的目的在于提供 GUI 安装本地 .deb 的体验(纯命令行场景
//! `apt install ./x.deb` 已原生覆盖,故不设 CLI/KDE 以外的 console 前端)。
//! 分层:GUI 瘦客户端 → D-Bus 守护进程([`gdebi-daemon`]) → [`flow`] 摘要 →
//! [`ui`] trait 回调 → 解析/求解等纯逻辑。
//!
//! 纯 Rust 管道(无 libapt-pkg FFI;系统能力仅经 dpkg-query/dpkg/indextargets
//! 子进程,索引解析与 HTTP 下载全 Rust 后端,见 crates/readme.md 路线二):
//! - [`debfile`]:.deb 打开与 control 提取(原始字节保留,lossy 字段访问)
//! - [`version`]:dpkg 版本号比较
//! - [`sysstate`]:dpkg 状态 + apt 仓库索引的纯 Rust 读取
//! - [`solver`]:varisat CDCL 编码的依赖求解(替代原 apt-get -s 编排)
//! - [`download`]:ureq(rustls)下载 + sha256 校验
//! - [`install`]:dpkg 执行层(批量 -i + 取消句柄),进度经 [`ui::InstallProgress`]
//!   转发
//!
//! [`gdebi-daemon`]: https://github.com/ 载荷分离说明见 crates/gdebi-daemon

mod control;

pub mod debfile;
pub mod download;
pub mod error;
pub mod flow;
pub mod i18n;
pub mod install;
pub mod solver;
pub mod sysstate;
pub mod ui;
pub mod version;

pub use debfile::DebFile;
pub use error::GdebiError;

/// D-Bus `AnalyzeLocalFile` 返回契约(daemon/gtk 共用,类型别名仅消解
/// clippy::type_complexity,线型不变):
/// (可安装, 错误信息, install[], remove[], unauthenticated[], 已安装版本)
pub type AnalyzeResult = (bool, String, Vec<String>, Vec<String>, Vec<String>, String);
pub use version::cmp_version;
