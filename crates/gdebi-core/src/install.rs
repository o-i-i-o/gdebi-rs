//! 安装流程。零 FFI 子进程实现,语义基准为旧版 GDebiCli.install():
//! - required_changes 非空 → 依赖提交(下载+安装,apt-get 与检查同一解析器),
//!   失败经 ui 呈现并返回退出码 1
//! - 包自身:`dpkg --auto-deconfigure -i`,退出码透传
//! - .dsc 暂不支持(旧版同样直接返回 0,build-dep 路径待后续接入)
//!
//! 进度呈现:子进程输出逐行转发 [`InstallProgress::output_line`]
//! (stdout/stderr 双 reader 线程合并,避免管道缓冲死锁)。

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;

use crate::depcheck::Changes;
use crate::error::GdebiError;
use crate::ui::{AlertKind, InstallProgress, Ui};

/// 执行安装,返回进程退出码(dpkg 透传;依赖提交失败为 1)。
/// 调用方需已持有依赖检查结论(changes);unauthenticated 的确认由前端负责。
pub fn install(
    ui: &dyn Ui,
    progress: &dyn InstallProgress,
    changes: &Changes,
    file: &Path,
    apt_opts: &[String],
) -> Result<i32, GdebiError> {
    if !changes.install.is_empty() || !changes.remove.is_empty() {
        // 依赖提交:apt-get install pkg... [pkg-...];-q 减少输出噪声
        let mut cmd = Command::new("apt-get");
        cmd.arg("-q").arg("-y").arg("--no-install-recommends");
        for o in apt_opts {
            cmd.arg("-o").arg(o);
        }
        cmd.arg("install");
        for p in &changes.install {
            cmd.arg(p);
        }
        for r in &changes.remove {
            cmd.arg(format!("{r}-"));
        }
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        forward_output(&mut child, progress);
        if !child.wait()?.success() {
            // 旧版:Error during install: '<e>' → 返回 1
            let _ = ui.alert(
                AlertKind::Error,
                "Error during install",
                None,
            );
            return Ok(1);
        }
    }

    if file.extension().and_then(|e| e.to_str()) == Some("dsc") {
        // 旧版未实现 build-dep 安装,直接返回 0
        return Ok(0);
    }

    // 包自身(路径绝对化:与 depcheck 同理,dpkg 按 cwd 解析相对路径无碍,
    // 但绝对路径保证子进程与调用目录无关)
    let mut child = Command::new("dpkg")
        .arg("--auto-deconfigure")
        .arg("-i")
        .arg(std::path::absolute(file)?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    forward_output(&mut child, progress);
    let status = child.wait()?;
    Ok(status.code().unwrap_or(-1))
}

/// 合并子进程 stdout/stderr 逐行转发到 progress
fn forward_output(child: &mut Child, progress: &dyn InstallProgress) {
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
    }
    for t in readers {
        let _ = t.join();
    }
}
