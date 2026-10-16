//! T27 的已注册 exec 工具请求与真实输出判定。

use serde_json::json;

use crate::probe::{Probe, ProbeCall};

use super::settings::{ANON_MARKER, PACKAGE_NAME, USR_LOCAL_MARKER, WORKSPACE_MARKER};

/// 先显式确认镜像里有构建与安装 deb 的基础工具。
pub(super) fn availability(tag: &str) -> Probe {
    Probe::new(
        format!("t27-debian-tools-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({ "command": "command -v dpkg-deb && command -v dpkg" }),
        )],
    )
}

/// 写入三类数据，并安装一个不依赖网络的小型 deb 包。
pub(super) fn write_data(tag: &str) -> Probe {
    let command = format!(
        r#"set -eu
marker='{tag}'
printf '%s' "$marker" > {USR_LOCAL_MARKER}
mkdir -p "$(dirname '{WORKSPACE_MARKER}')" "$(dirname '{ANON_MARKER}')"
printf '%s' "$marker" > '{WORKSPACE_MARKER}'
printf '%s' "$marker" > '{ANON_MARKER}'
package_root=/tmp/mai-t27-package
rm -rf "$package_root" /tmp/mai-t27-retention.deb
mkdir -p "$package_root/DEBIAN" "$package_root/usr/share/mai-t27-retention"
cat > "$package_root/DEBIAN/control" <<'CONTROL'
Package: {PACKAGE_NAME}
Version: 1
Section: misc
Priority: optional
Architecture: all
Maintainer: Mai acceptance <mai@example.invalid>
Description: T27 writable layer retention marker
CONTROL
printf '%s' "$marker" > "$package_root/usr/share/mai-t27-retention/payload"
dpkg-deb --build "$package_root" /tmp/mai-t27-retention.deb
dpkg -i /tmp/mai-t27-retention.deb
test "$(cat "$package_root/usr/share/mai-t27-retention/payload")" = "$marker"
echo t27_data_written
"#
    );
    Probe::new(
        format!("t27-write-{tag}"),
        vec![ProbeCall::new("exec", json!({ "command": command }))],
    )
}

/// 换代后读取三类标记；输出必须携带精确 tag。
pub(super) fn read_data(tag: &str) -> Probe {
    let command = format!(
        r#"set -eu
printf 'usr_local=%s\n' "$(cat '{USR_LOCAL_MARKER}')"
printf 'workspace=%s\n' "$(cat '{WORKSPACE_MARKER}')"
printf 'anon=%s\n' "$(cat '{ANON_MARKER}')"
"#
    );
    Probe::new(
        format!("t27-retained-{tag}"),
        vec![ProbeCall::new("exec", json!({ "command": command }))],
    )
}

/// 换代后读取 dpkg 数据库和包 payload。
pub(super) fn query_package(tag: &str) -> Probe {
    let command = format!(
        r#"set -eu
dpkg-query -W -f='${{Status}}|${{Package}}|${{Version}}\n' {PACKAGE_NAME}
payload=/usr/share/{PACKAGE_NAME}/payload
test -f "$payload"
printf 'package_payload=%s\n' "$(cat "$payload")"
"#
    );
    Probe::new(
        format!("t27-package-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({
                "command": command,
            }),
        )],
    )
}

/// 判断三类标记的精确输出。
pub(super) fn marker_output_matches(output: &str, tag: &str) -> bool {
    output.contains(&format!("usr_local={tag}"))
        && output.contains(&format!("workspace={tag}"))
        && output.contains(&format!("anon={tag}"))
}

/// 判断 dpkg 安装状态与包 payload。
pub(super) fn package_output_matches(output: &str, tag: &str) -> bool {
    output.contains(&format!("install ok installed|{PACKAGE_NAME}|1"))
        && output.contains(&format!("package_payload={tag}"))
}
