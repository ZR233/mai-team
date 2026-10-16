//! 源码树内子挂载检测：一次解析挂载表，拒绝源码根**严格子路径**上的任何挂载。
//!
//! 只看 `metadata.dev()` 会漏掉同一文件系统上的 bind 挂载（bind 复制的是挂载点，
//! 设备号不变），因此 `/proc/self/mountinfo` 是本模块的权威依据。每次扫描只读一次
//! 挂载表并把结果留给整棵树复用；逐文件读取既慢，也会把检测窗口拉长。
//!
//! fail closed：解析不出完整挂载表（字段不足、相对路径、空表）或平台没有该文件时
//! 一律拒绝，不退回"只看设备号"的弱判据。

use std::path::{Path, PathBuf};

use mai_protocol::ReferenceErrorCode;

use crate::error::{ReferenceError, Result};

/// Linux 挂载表路径。
#[cfg(target_os = "linux")]
const MOUNT_INFO_PATH: &str = "/proc/self/mountinfo";

/// mountinfo 每行至少需要的字段数；挂载点是第 5 个字段，后面还必须有挂载选项。
const MIN_MOUNT_INFO_FIELDS: usize = 6;

/// 解析后的挂载点集合。
///
/// 只保存挂载点：检测目标是"源码树下有没有别的挂载"，不需要文件系统类型或来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountTable {
    mount_points: Vec<PathBuf>,
}

impl MountTable {
    /// 从 `/proc/self/mountinfo` 的文本内容解析挂载点。
    ///
    /// 纯函数，便于用 fixture 覆盖转义与边界；字段 5 里的空格、制表符、换行、反斜杠
    /// 按八进制转义（`\040`、`\011`、`\012`、`\134`）还原后再参与比较。
    pub fn parse(contents: &str) -> Result<Self> {
        let mut mount_points: Vec<PathBuf> = Vec::new();
        for line in contents.lines() {
            if line.is_empty() {
                continue;
            }
            let mount_point = parse_mount_point(line)?;
            mount_points.push(mount_point);
        }
        if mount_points.is_empty() {
            return Err(malformed("挂载表没有任何行，无法证明源码树内没有子挂载"));
        }
        mount_points.sort();
        mount_points.dedup();
        Ok(Self { mount_points })
    }

    /// 读取本机挂载表。
    ///
    /// 非 Linux 平台没有 `/proc/self/mountinfo`，明确 fail closed 而不是放宽判据。
    pub fn read_current() -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let contents = std::fs::read_to_string(MOUNT_INFO_PATH).map_err(|error| {
                ReferenceError::from_io(
                    ReferenceErrorCode::UnsafeInternalMount,
                    format!("无法读取 {MOUNT_INFO_PATH}，不能证明源码树内没有子挂载"),
                    error,
                )
            })?;
            Self::parse(&contents)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(malformed(
                "当前平台没有 /proc/self/mountinfo，不能证明源码树内没有子挂载",
            ))
        }
    }

    /// 返回严格位于 `root` 之下的挂载点。
    ///
    /// 按路径组件比较：`/srv/ref-evil` 不是 `/srv/ref` 的子路径；`root` 自身作为挂载点
    /// （例如允许根自己是一个文件系统）不算子挂载。
    pub fn first_mount_strictly_below(&self, root: &Path) -> Option<&Path> {
        let root_depth = root.components().count();
        self.mount_points
            .iter()
            .find(|mount_point| {
                mount_point.components().count() > root_depth && mount_point.starts_with(root)
            })
            .map(PathBuf::as_path)
    }
}

/// 解析单行的挂载点字段。
fn parse_mount_point(line: &str) -> Result<PathBuf> {
    let fields: Vec<&str> = line.split(' ').collect();
    if fields.len() < MIN_MOUNT_INFO_FIELDS {
        return Err(malformed(format!(
            "挂载表行字段不足 {MIN_MOUNT_INFO_FIELDS} 个：{line}"
        )));
    }
    let raw = fields[4];
    let decoded = decode_octal_escapes(raw);
    if !decoded.starts_with(b"/") {
        return Err(malformed(format!("挂载点不是绝对路径：{raw}")));
    }
    Ok(path_from_bytes(decoded))
}

/// 还原八进制转义：`\040` 空格、`\011` 制表符、`\012` 换行、`\134` 反斜杠。
///
/// 只处理完整的 `\ooo` 序列，其余字节（含孤立反斜杠）按原样保留，不猜测语义。
fn decode_octal_escapes(raw: &str) -> Vec<u8> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let escape_end = index + 4;
        if bytes[index] == b'\\' && escape_end <= bytes.len() {
            let digits = &bytes[index + 1..escape_end];
            if digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                let value = u32::from(digits[0] - b'0') * 64
                    + u32::from(digits[1] - b'0') * 8
                    + u32::from(digits[2] - b'0');
                decoded.push(value as u8);
                index = escape_end;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    decoded
}

/// 把解码后的字节还原成路径；挂载点允许非 UTF-8 字节。
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;

    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

fn malformed(message: impl Into<String>) -> ReferenceError {
    ReferenceError::new(ReferenceErrorCode::UnsafeInternalMount, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// 源码根是 `/srv/mai-references/linux/repo`，它自己是一个挂载点（允许根是独立文件系统）。
    const REPO_ROOT: &str = "/srv/mai-references/linux/repo";

    /// 同设备（8:1，ext4）的 bind 挂载：`metadata.dev()` 与源码根相同，只有挂载表能发现。
    const SAME_FS_BIND_LINE: &str = "52 41 8:1 /srv/mai-references/linux/repo/vendor /srv/mai-references/linux/repo/vendor ro,relatime shared:1 - ext4 /dev/sda1 ro";

    #[test]
    fn same_filesystem_bind_mount_below_root_is_detected() {
        let table = MountTable::parse(SAME_FS_BIND_LINE).expect("挂载表");
        assert_eq!(
            Some(Path::new("/srv/mai-references/linux/repo/vendor")),
            table.first_mount_strictly_below(Path::new(REPO_ROOT))
        );
    }

    #[test]
    fn mount_on_root_itself_and_ancestors_is_not_a_child_mount() {
        let contents = "\
30 1 0:26 / /proc rw,nosuid,nodev,noexec,relatime shared:5 - proc proc rw
33 1 8:1 / /srv rw,relatime shared:1 - ext4 /dev/sda1 rw
41 33 8:1 /srv/mai-references /srv/mai-references rw,relatime shared:1 - ext4 /dev/sda1 rw
47 41 8:1 /srv/mai-references/linux/repo /srv/mai-references/linux/repo rw,relatime shared:1 - ext4 /dev/sda1 rw";
        let table = MountTable::parse(contents).expect("挂载表");
        assert_eq!(None, table.first_mount_strictly_below(Path::new(REPO_ROOT)));
    }

    #[test]
    fn prefix_lookalike_paths_are_not_children() {
        let contents = "\
60 41 8:1 /srv/mai-references-evil /srv/mai-references-evil rw,relatime - ext4 /dev/sda2 rw
61 41 8:1 /srv/mai-referencesX /srv/mai-referencesX rw,relatime - ext4 /dev/sda2 rw
62 41 8:1 /linux/repo-evil /srv/mai-references/linux/repo-evil rw,relatime - ext4 /dev/sda2 rw
63 41 8:1 /linux/repoX /srv/mai-references/linux/repoX rw,relatime - ext4 /dev/sda2 rw";
        let table = MountTable::parse(contents).expect("挂载表");
        assert_eq!(
            Some(Path::new("/srv/mai-references/linux/repo-evil")),
            table.first_mount_strictly_below(Path::new("/srv/mai-references"))
        );
        assert_eq!(None, table.first_mount_strictly_below(Path::new(REPO_ROOT)));
    }

    #[test]
    fn escaped_mount_points_are_decoded_before_comparison() {
        let contents = "\
70 41 8:1 /weird /srv/mai-references/linux/repo/weird\\040name rw,relatime - ext4 /dev/sda3 rw
71 41 8:1 /tab /srv/mai-references/linux/repo/tab\\011name rw,relatime - ext4 /dev/sda3 rw
72 41 8:1 /slash /srv/mai-references/linux/repo/back\\134slash rw,relatime - ext4 /dev/sda3 rw";
        let table = MountTable::parse(contents).expect("挂载表");
        assert_eq!(
            Some(Path::new("/srv/mai-references/linux/repo/back\\slash")),
            table.first_mount_strictly_below(Path::new(REPO_ROOT))
        );

        let single = MountTable::parse(
            "70 41 8:1 /weird /srv/mai-references/linux/repo/weird\\040name rw,relatime - ext4 /dev/sda3 rw",
        )
        .expect("挂载表");
        assert_eq!(
            Some(Path::new("/srv/mai-references/linux/repo/weird name")),
            single.first_mount_strictly_below(Path::new(REPO_ROOT))
        );
    }

    #[test]
    fn incomplete_or_empty_mount_tables_are_rejected() {
        assert_eq!(
            ReferenceErrorCode::UnsafeInternalMount,
            MountTable::parse("").expect_err("空挂载表").code()
        );
        assert_eq!(
            ReferenceErrorCode::UnsafeInternalMount,
            MountTable::parse("52 41 8:1 /a /b")
                .expect_err("字段不足")
                .code()
        );
        assert_eq!(
            ReferenceErrorCode::UnsafeInternalMount,
            MountTable::parse("52 41 8:1 /a relative/path rw - ext4 /dev/sda1 rw")
                .expect_err("相对挂载点")
                .code()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_mount_table_can_be_read_from_proc() {
        let table = MountTable::read_current().expect("本机挂载表");
        // 根文件系统之下必然存在挂载点（如 /proc、/dev），用它证明真的读到了内容。
        assert!(table.first_mount_strictly_below(Path::new("/")).is_some());
    }
}
