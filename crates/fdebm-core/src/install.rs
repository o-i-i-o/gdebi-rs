//! dpkg 执行层(纯 Rust 管道的最后一段:实际写入仍由 dpkg 完成)。
//!
//! 语义:
//! - 需先卸载的冲突包:`dpkg --auto-deconfigure -r <names>`(源自求解器)
//! - 依赖 deb + 本地 deb 单次批量:`dpkg --auto-deconfigure -i <debs>`,
//!   dpkg 自行处理解包/配置顺序与 Pre-Depends
//! - conffile 提示:GUI 预检阶段由用户决策([`ConfPolicy`]),安装时以
//!   `--force-confold/--force-confnew` 确定性执行;守护进程无 tty,不透传交互
//! - 进度:子进程输出逐行转发 [`InstallProgress::output_line`]
//!   (stdout/stderr 双 reader 线程合并,避免管道缓冲死锁)
//! - 取消:[`CancelHandle`] 标志位 + 对子进程组发 SIGTERM

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use crate::error::FdebmError;
use crate::i18n::gettext;
use crate::ui::{AlertKind, InstallProgress, Ui};

/// conffile 覆盖策略(GUI 预检对话框的用户决策;D-Bus 上以字符串传递)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConfPolicy {
    /// 无冲突:dpkg 默认语义(未修改的 conffile 静默升级)
    #[default]
    Default,
    /// 保留本地已修改版本(新版本落为 <file>.dpkg-dist)
    Keep,
    /// 使用包携带的新版本(本地版本备份为 <file>.dpkg-old)
    Replace,
}

impl ConfPolicy {
    /// D-Bus 字符串(""/"keep"/"replace")→ 策略
    pub fn from_arg(s: &str) -> Self {
        match s {
            "keep" => Self::Keep,
            "replace" => Self::Replace,
            _ => Self::Default,
        }
    }

    fn dpkg_arg(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Keep => Some("--force-confold"),
            Self::Replace => Some("--force-confnew"),
        }
    }
}

/// 取消句柄:跨线程共享;cancel() 置位并向当前子进程组发 SIGTERM
#[derive(Default)]
pub struct CancelHandle {
    cancelled: AtomicBool,
    pgid: Mutex<Option<u32>>,
}

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// 请求取消:置位并终止运行中的子进程(幂等)
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.pgid.lock().unwrap() {
            // process_group(0) 使子进程自成一个进程组,pgid == 子 pid
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }

    fn register(&self, pid: u32) {
        *self.pgid.lock().unwrap() = Some(pid);
    }

    fn unregister(&self) {
        *self.pgid.lock().unwrap() = None;
    }
}

/// 一次安装事务的执行载荷
pub struct InstallBatch {
    /// 本地 deb(命令行目标)
    pub local: PathBuf,
    /// 已下载的依赖 deb(download_all 的产物)
    pub dep_debs: Vec<PathBuf>,
    /// 需先卸载的冲突包(求解器给出的 remove 集)
    pub remove: Vec<String>,
    /// conffile 覆盖策略(GUI 预检决策)
    pub conf_policy: ConfPolicy,
}

/// 执行安装,返回 dpkg 退出码。unauthenticated 的确认由前端负责。
pub fn install(
    ui: &dyn Ui,
    progress: &dyn InstallProgress,
    cancel: &CancelHandle,
    batch: &InstallBatch,
) -> Result<i32, FdebmError> {
    if cancel.is_cancelled() {
        return Err(FdebmError::Cancelled);
    }

    if !batch.remove.is_empty() {
        let mut cmd = Command::new("dpkg");
        cmd.arg("--auto-deconfigure").arg("-r");
        for name in &batch.remove {
            cmd.arg(name);
        }
        if run_dpkg(cmd, progress, cancel)? != 0 {
            let _ = ui.alert(AlertKind::Error, &gettext("Error during install"), None);
            return Ok(1);
        }
        if cancel.is_cancelled() {
            return Err(FdebmError::Cancelled);
        }
    }

    let mut cmd = Command::new("dpkg");
    cmd.arg("--auto-deconfigure")
        .arg("-i")
        .arg(std::path::absolute(&batch.local)?);
    for deb in &batch.dep_debs {
        cmd.arg(deb);
    }
    if let Some(arg) = batch.conf_policy.dpkg_arg() {
        cmd.arg(arg);
    }
    // 守护进程无 tty:其余提示(noninteractive)按默认策略执行
    cmd.env("DEBIAN_FRONTEND", "noninteractive");
    let code = run_dpkg(cmd, progress, cancel)?;
    if code != 0 {
        let _ = ui.alert(AlertKind::Error, &gettext("Error during install"), None);
    }
    Ok(code)
}

/// 卸载单包(dpkg 语义:反向依赖会阻止卸载并报错,由输出呈现)
pub fn remove(
    ui: &dyn Ui,
    progress: &dyn InstallProgress,
    cancel: &CancelHandle,
    name: &str,
) -> Result<i32, FdebmError> {
    if cancel.is_cancelled() {
        return Err(FdebmError::Cancelled);
    }
    let mut cmd = Command::new("dpkg");
    cmd.arg("-r").arg(name);
    let code = run_dpkg(cmd, progress, cancel)?;
    if code != 0 {
        let _ = ui.alert(AlertKind::Error, &gettext("Error during install"), None);
    }
    Ok(code)
}

/// 启动(独立进程组)、注册取消句柄、转发输出、等待退出码
fn run_dpkg(
    mut cmd: Command,
    progress: &dyn InstallProgress,
    cancel: &CancelHandle,
) -> Result<i32, FdebmError> {
    use std::os::unix::process::CommandExt as _;
    // spawn 前设独立进程组:cancel() 可对整组 SIGTERM(含维护脚本子进程)
    cmd.process_group(0);
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()?;
    // process_group(0) 后 pgid == 子 pid
    cancel.register(child.id());
    forward_output(&mut child, progress, cancel);
    let status = child.wait()?;
    cancel.unregister();
    if cancel.is_cancelled() {
        return Err(FdebmError::Cancelled);
    }
    Ok(status.code().unwrap_or(-1))
}

/// 合并子进程 stdout/stderr 逐行转发到 progress;取消时终止子进程组
fn forward_output(child: &mut Child, progress: &dyn InstallProgress, cancel: &CancelHandle) {
    let (tx, rx) = mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        }));
    }
    // 两个 reader 线程都结束后 channel 关闭,循环退出
    for line in rx {
        progress.output_line(&line);
        if cancel.is_cancelled() {
            // SIGTERM 已由 cancel() 发出;继续读完管道直至进程退出
        }
    }
    for t in readers {
        let _ = t.join();
    }
}
