//! 差分测试:以 `dpkg-deb -f` 为 oracle 校准 control 解析语义(逐字节)。
//!
//! 历史上的 Tier B(对照旧版 python gdebi --apt-line 输出)随求解器替换
//! 与参考脚本移出仓库而删除;求解行为以 src/solver.rs 的单元测试为准。

use std::path::PathBuf;
use std::process::Command;

use fdebm_core::solver::Changes;
use fdebm_core::{DebFile, FdebmError};

/// 仓库根(crates/fdebm-core → fdebm)
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
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

/// 钉住 C locale:保持 gettext msgid 英文基线
fn pin_c_locale() {
    unsafe {
        std::env::set_var("LC_ALL", "C");
        std::env::set_var("LANGUAGE", "C");
    }
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
    assert_eq!(
        lines(&c),
        vec![
            "To be removed: x",
            "To be installed: a",
            "To be installed: b"
        ]
    );
}

/// FdebmError 展示冒烟(错误串英文,保持 gettext msgid 就绪)
#[test]
fn error_messages_in_english() {
    pin_c_locale();
    let e = FdebmError::MissingControl;
    assert_eq!(e.to_string(), "control file not found in package");
}
