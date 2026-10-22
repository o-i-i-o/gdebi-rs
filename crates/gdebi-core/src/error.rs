//! 错误类型。用户可见文案为英文 msgid,经 gettext 翻译(见 i18n 模块);
//! C locale / 无翻译时回落英文原文。

use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::i18n::{gettext, tf};

#[derive(Debug)]
pub enum GdebiError {
    OpenFailed(PathBuf, String),
    NotADeb(PathBuf),
    MissingControl,
    UnsupportedCompression(&'static str),
    MissingField(&'static str),
    Io(io::Error),
    /// 子进程(dpkg 系统能力调用)执行失败:命令 + stderr 摘要
    Subprocess(String, String),
    /// 仓库索引解压失败:文件 + 原因
    ListDecompress(String, String),
    /// 下载失败:URL + 原因
    Download(String, String),
    /// 用户请求取消
    Cancelled,
}

impl From<io::Error> for GdebiError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl fmt::Display for GdebiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::OpenFailed(path, reason) => {
                tf("cannot open {0}: {1}", &[&path.display().to_string(), reason])
            }
            Self::NotADeb(path) => tf("{0} is not a Debian package", &[&path.display().to_string()]),
            Self::MissingControl => gettext("control file not found in package"),
            Self::UnsupportedCompression(compression) => tf(
                "{0} compressed control.tar is not supported yet (only gzip/plain)",
                &[compression],
            ),
            Self::MissingField(name) => tf("missing control field: {0}", &[name]),
            // 透明转发:OS strerror 已随系统 locale 本地化,不再包裹
            Self::Io(e) => return write!(f, "{e}"),
            Self::Subprocess(cmd, stderr) => {
                tf("subprocess {0} failed: {1}", &[cmd, stderr.trim()])
            }
            Self::ListDecompress(path, reason) => {
                tf("cannot decompress repository index {0}: {1}", &[path, reason])
            }
            Self::Download(url, reason) => tf("cannot download {0}: {1}", &[url, reason]),
            Self::Cancelled => gettext("operation cancelled"),
        };
        f.write_str(&msg)
    }
}

impl Error for GdebiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
