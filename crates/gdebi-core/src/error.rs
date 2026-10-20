//! 错误类型。字符串保持英文,作为未来 gettext 的 msgid。

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum GdebiError {
    #[error("cannot open {0}: {1}")]
    OpenFailed(PathBuf, String),

    #[error("{0} is not a Debian package")]
    NotADeb(PathBuf),

    #[error("control file not found in package")]
    MissingControl,

    #[error("{0} compressed control.tar is not supported yet (only gzip/plain)")]
    UnsupportedCompression(&'static str),

    #[error("missing control field: {0}")]
    MissingField(&'static str),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
