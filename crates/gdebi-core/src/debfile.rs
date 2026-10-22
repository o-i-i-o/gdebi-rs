//! .deb 文件打开:ar 容器 → control.tar(.gz/无压缩)→ control 内容。
//! control 以原始字节保留(dpkg 语义,允许非 UTF-8 字节,如 gdebi-test6);
//! 字段解析在 lossy 转换后的文本上进行,仅用于展示。
//!
//! data.tar 的成员列表与内容提取经 `dpkg-deb --fsys-tarfile` 子进程:
//! 与 apt-get/dpkg 同为"系统能力经子进程"模式,一次覆盖全部压缩格式
//! (gzip/xz/zstd),避免逐格式引入解压依赖。

use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::control::parse_control;
use crate::error::GdebiError;

/// 已打开的 .deb:control 内容与解析结果
pub struct DebFile {
    path: PathBuf,
    control_tar: Vec<u8>,
    control_bytes: Vec<u8>,
    paragraphs: Vec<crate::control::ControlParagraph>,
}

impl DebFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, GdebiError> {
        let path = path.as_ref();
        let file = File::open(path)
            .map_err(|e| GdebiError::OpenFailed(path.to_path_buf(), e.to_string()))?;
        let mut archive = ar::Archive::new(file);

        let mut control_tar: Option<Vec<u8>> = None;
        while let Some(entry) = archive.next_entry() {
            let mut entry = entry?;
            let name = entry.header().identifier().to_vec();
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            match name.as_slice() {
                b"debian-binary" => {}
                n if n.starts_with(b"control.tar") => control_tar = Some(data),
                _ => {}
            }
        }

        let control_tar =
            control_tar.ok_or_else(|| GdebiError::NotADeb(path.to_path_buf()))?;
        let control_bytes = extract_control_bytes(&control_tar)?;
        let paragraphs = parse_control(&String::from_utf8_lossy(&control_bytes));
        Ok(Self {
            path: path.to_path_buf(),
            control_tar,
            control_bytes,
            paragraphs,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// control 文件原始字节(与 `dpkg-deb -f` 输出对齐)
    pub fn raw_control(&self) -> &[u8] {
        &self.control_bytes
    }

    /// 主段落字段值(lossy,展示用)
    pub fn field(&self, name: &str) -> Option<&str> {
        self.paragraphs.first()?.get(name)
    }

    /// 修剪后的字段值(展示用)
    pub fn field_trimmed(&self, name: &str) -> Option<&str> {
        self.field(name).map(str::trim)
    }

    /// control.tar 成员名列表(排序;对应旧版 control_filelist)
    pub fn control_filelist(&self) -> Result<Vec<String>, GdebiError> {
        let mut out = Vec::new();
        for entry in tar::Archive::new(self.control_reader()?).entries()? {
            let entry = entry?;
            out.push(normalize_member(&entry.path()?.to_string_lossy()));
        }
        out.sort();
        Ok(out)
    }

    /// data.tar 成员名列表(按 tar 流顺序;对应旧版 filelist)
    pub fn data_filelist(&self) -> Result<Vec<String>, GdebiError> {
        let child = Command::new("dpkg-deb")
            .arg("--fsys-tarfile")
            .arg(&self.path)
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let mut out = Vec::new();
        for entry in tar::Archive::new(child.stdout.expect("piped stdout")).entries()? {
            let entry = entry?;
            out.push(normalize_member(&entry.path()?.to_string_lossy()));
        }
        Ok(out)
    }

    /// control.tar 内单文件内容(name 为去前缀后的展示名)
    pub fn control_content(&self, name: &str) -> Result<Vec<u8>, GdebiError> {
        for entry in tar::Archive::new(self.control_reader()?).entries()? {
            let mut entry = entry?;
            if normalize_member(&entry.path()?.to_string_lossy()) == name {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                return Ok(bytes);
            }
        }
        Ok(Vec::new())
    }

    /// data.tar 内单文件内容(name 为去前缀后的展示名)
    pub fn data_content(&self, name: &str) -> Result<Vec<u8>, GdebiError> {
        let child = Command::new("dpkg-deb")
            .arg("--fsys-tarfile")
            .arg(&self.path)
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        for entry in tar::Archive::new(child.stdout.expect("piped stdout")).entries()? {
            let mut entry = entry?;
            if normalize_member(&entry.path()?.to_string_lossy()) == name {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                return Ok(bytes);
            }
        }
        Ok(Vec::new())
    }

    fn control_reader(&self) -> Result<Box<dyn Read + '_>, GdebiError> {
        if self.control_tar.starts_with(&[0x1f, 0x8b]) {
            Ok(Box::new(flate2::read::GzDecoder::new(Cursor::new(
                &self.control_tar,
            ))))
        } else if self.control_tar.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
            Err(GdebiError::UnsupportedCompression("xz"))
        } else if self.control_tar.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Err(GdebiError::UnsupportedCompression("zstd"))
        } else {
            Ok(Box::new(Cursor::new(&self.control_tar)))
        }
    }
}

/// tar 成员名规范化:去 "./" 前缀;根目录项 "./" 保留原样
/// (与 python-apt 的 TarMember.name 语义一致,旧版 GUI 展示基准)
fn normalize_member(name: &str) -> String {
    match name.strip_prefix("./") {
        Some("") => "./".to_string(),
        Some(n) => n.to_string(),
        None => name.to_string(),
    }
}

/// control.tar 原始字节 → 解压后的 control 文件字节
fn extract_control_bytes(data: &[u8]) -> Result<Vec<u8>, GdebiError> {
    let reader: Box<dyn Read> = if data.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(Cursor::new(data)))
    } else if data.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        return Err(GdebiError::UnsupportedCompression("xz"));
    } else if data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Err(GdebiError::UnsupportedCompression("zstd"));
    } else {
        Box::new(Cursor::new(data))
    };

    let mut tar = tar::Archive::new(reader);
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name().and_then(|s| s.to_str()) == Some("control") {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    Err(GdebiError::MissingControl)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 仓库 tests/ 目录下的 gdebi-test4.deb(data.tar 含 "./..deb" 与
    /// "./.deb" 特殊文件名,是成员名规范化的关键样本)
    fn test4() -> DebFile {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/gdebi-test4.deb");
        DebFile::open(&path).expect("open gdebi-test4.deb")
    }

    #[test]
    fn data_filelist_matches_python_apt_semantics() {
        // 真值取自 python-apt: data 成员名去 "./" 前缀,根目录项保留 "./",
        // 顺序为 tar 流序
        let deb = test4();
        assert_eq!(deb.data_filelist().unwrap(), vec!["./", "..deb", ".deb"]);
    }

    #[test]
    fn control_filelist_is_sorted() {
        // 旧版 control_filelist 经 sorted();根目录项为 "./"
        let deb = test4();
        assert_eq!(deb.control_filelist().unwrap(), vec!["./", "control"]);
    }

    #[test]
    fn data_content_extracts_bytes() {
        // 长度基准来自 dpkg-deb -c(test4 固定内容);内嵌 NUL 是
        // flow::content_preview 二进制判定与 MIME 展示的关键样本
        let deb = test4();
        let bin = deb.data_content("..deb").unwrap();
        assert_eq!(bin.len(), 904);
        assert!(bin.contains(&0));
        assert_eq!(deb.data_content(".deb").unwrap().len(), 1396);
    }

    #[test]
    fn content_of_missing_member_is_empty() {
        let deb = test4();
        assert!(deb.data_content("no-such-file").unwrap().is_empty());
        assert!(deb.control_content("no-such-file").unwrap().is_empty());
    }

    #[test]
    fn control_content_returns_control_file() {
        let deb = test4();
        let bytes = deb.control_content("control").unwrap();
        assert!(!bytes.is_empty());
        assert!(String::from_utf8_lossy(&bytes).contains("Package: gdebi-test4"));
    }
}
