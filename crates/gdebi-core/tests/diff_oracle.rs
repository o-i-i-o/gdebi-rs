//! 差分测试:以 dpkg-deb 与仓库内旧版 python gdebi 为 oracle 校准 core 语义。
//!
//! Tier A:control 字段解析 vs `dpkg-deb -f`(逐字节)
//! Tier B:依赖检查/变更集 vs `python3 gdebi --apt-line -q`
//!   (oracle 命令的环境归一化理由见原 diff-tests 说明:
//!    `-q` 压制 OpProgress 噪声、PYTHONWARNINGS=ignore 压制 SyntaxWarning、
//!    C locale 得到英文基线——这些是 oracle 实现工件,非被测语义)
//!
//! oracle 不可用(python3 无 apt 模块 / 文件缺失)时打印提示并跳过。

use std::path::{Path, PathBuf};
use std::process::Command;

use gdebi_core::depcheck::{Changes, DepChecker, Verdict};
use gdebi_core::{DebFile, GdebiError};

/// 仓库根(crates/gdebi-core → gdebi-rs → gdebi)
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repo root exists")
}

fn test_debs() -> Vec<PathBuf> {
    let dir = repo_root().join("tests");
    let mut debs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("tests dir with oracle debs")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("deb"))
        .collect();
    debs.sort();
    assert!(!debs.is_empty(), "no test debs in {}", dir.display());
    debs
}

fn oracle_available() -> bool {
    Command::new("python3")
        .arg("-c")
        .arg("import apt, apt_inst")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tier A:control 段原文 vs dpkg-deb -f 全文模式(逐字节)
// (注意:dpkg-deb -f <field> 单字段模式会对关系字段做空格归一化,
//  不是原文,不能作为字段级 oracle)
// ---------------------------------------------------------------------------

#[test]
fn tier_a_control_bytes_match_dpkg_deb() {
    for deb in test_debs() {
        let ours = match DebFile::open(&deb) {
            Ok(d) => d,
            Err(e) => panic!("open {}: {e}", deb.display()),
        };
        // control 可能含非法 UTF-8(非 UTF-8 字段正是覆盖项),必须按原始字节比较
        let out = Command::new("dpkg-deb")
            .arg("-f")
            .arg(&deb)
            .output()
            .expect("spawn dpkg-deb");
        assert!(out.status.success(), "dpkg-deb -f {}", deb.display());
        assert_eq!(
            ours.raw_control(),
            out.stdout.as_slice(),
            "control of {}",
            deb.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Tier B:依赖检查 vs 旧版 gdebi --apt-line
// ---------------------------------------------------------------------------

/// 复刻旧 CLI 的 --apt-line 输出契约(stdout/stderr/退出码)。
/// 错误路径顺序与文案同旧版入口脚本 + GDebiCli.open。
fn our_apt_line(deb: &Path) -> (String, String, i32) {
    let err = |s: String| (String::new(), format!("{s}\n"), 1);

    if !deb.exists() {
        return err(format!("gdebi error, file not found: {}", deb.display()));
    }
    let checker = match DepChecker::new(&[]) {
        Ok(c) => c,
        Err(e) => return err(format!("Error opening the cache:\n{e}")),
    };
    let parsed = match DebFile::open(deb) {
        Ok(d) => d,
        // 旧版对非 .deb 扩展名经 file(1) 魔数识别;此处以能否按 deb 解析替代
        Err(_) if deb.extension().and_then(|e| e.to_str()) != Some("deb") => {
            return err(format!("Unknown package type '{}', exiting", deb.display()));
        }
        Err(_) => return err(
            "Failed to open the software package\nThe package might be corrupted or you are not allowed to open the file. Check the permissions of the file.".to_string(),
        ),
    };
    match checker.check(&parsed, deb) {
        Ok(Verdict::Installable(changes)) => {
            let stdout = format!(
                "{}\n{}\n",
                changes.install.join(" "),
                changes.remove.iter().map(|p| format!("{p}-")).collect::<Vec<_>>().join(" ")
            );
            (stdout, String::new(), 0)
        }
        Ok(Verdict::Uninstallable(failure)) => {
            let stderr = format!("This package is uninstallable\n{failure}\n");
            (String::new(), stderr, 1)
        }
        Err(e) => err(format!("gdebi error, {e}")),
    }
}

/// 旧版 oracle:`./gdebi --apt-line -q`(归一化环境,C locale 英文基线)
fn oracle_apt_line(deb: &Path) -> Option<(String, String, i32)> {
    let root = repo_root();
    Command::new("python3")
        .arg(root.join("gdebi"))
        .arg("--apt-line")
        .arg("-q")
        .arg(deb)
        .env("LC_ALL", "C")
        .env("LANGUAGE", "C")
        .env("PYTHONWARNINGS", "ignore")
        .output()
        .ok()
        .map(|o| {
            (
                String::from_utf8_lossy(&o.stdout).into_owned(),
                String::from_utf8_lossy(&o.stderr).into_owned(),
                o.status.code().unwrap_or(-1),
            )
        })
}

#[test]
fn tier_b_apt_line_semantics_match_old_gdebi() {
    if !oracle_available() {
        eprintln!("skip: python3-apt unavailable, oracle disabled");
        return;
    }
    for deb in test_debs() {
        let Some((o_out, o_err, o_code)) = oracle_apt_line(&deb) else {
            eprintln!("skip: oracle failed on {}", deb.display());
            continue;
        };
        let (out, err, code) = our_apt_line(&deb);
        assert_eq!(code, o_code, "exit code of {}", deb.display());
        assert_eq!(out, o_out, "stdout of {}", deb.display());
        assert_eq!(err, o_err, "stderr of {}", deb.display());
    }
}

/// 变更集结构自洽:Changes 非空时 GUI 详情对话框内容与摘要一致(无 oracle,纯逻辑)
#[test]
fn changes_lines_consistent() {
    fn lines(c: &Changes) -> Vec<String> {
        c.remove
            .iter()
            .map(|p| format!("To be removed: {p}"))
            .chain(c.install.iter().map(|p| format!("To be installed: {p}")))
            .collect()
    }
    let c = Changes {
        install: vec!["a".into(), "b".into()],
        remove: vec!["x".into()],
        unauthenticated: vec![],
    };
    assert_eq!(lines(&c), vec!["To be removed: x", "To be installed: a", "To be installed: b"]);
}

/// GdebiError 展示冒烟(错误串英文,保持 gettext msgid 就绪)
#[test]
fn error_messages_in_english() {
    let e = GdebiError::MissingControl;
    assert_eq!(e.to_string(), "control file not found in package");
}
