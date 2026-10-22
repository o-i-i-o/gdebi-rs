//! gdebi-core:本地 .deb 解析与依赖检查核心(gdebi 的 Rust 重写)。
//!
//! 定位:本程序的目的在于提供 GUI 安装本地 .deb 的体验(纯命令行场景
//! `apt install ./x.deb` 已原生覆盖,故不设 CLI/KDE 以外的 console 前端)。
//! 分层:GUI 前端 → [`flow`] 摘要 → [`ui`] trait 回调 → 解析/比较等纯逻辑。
//!
//! 语义校准(差分测试,oracle 为 dpkg-deb 与仓库内旧版 python gdebi,
//! 见 tests/diff_oracle.rs):
//! - [`debfile`]:.deb 打开与 control 提取(原始字节保留,lossy 字段访问)
//! - [`version`]:dpkg 版本号比较
//! - [`depcheck`]:依赖检查(required_changes / uninstallable 判定),
//!   系统状态经 dpkg-query 与 apt-get -s 子进程获取,零 FFI
//! - [`install`]:安装编排(apt-get 依赖提交 + dpkg 包自身),进度经
//!   [`ui::InstallProgress`] 转发

mod control;

pub mod debfile;
pub mod depcheck;
pub mod error;
pub mod flow;
pub mod i18n;
pub mod install;
pub mod ui;
pub mod version;

pub use debfile::DebFile;
pub use error::GdebiError;
pub use version::cmp_version;
