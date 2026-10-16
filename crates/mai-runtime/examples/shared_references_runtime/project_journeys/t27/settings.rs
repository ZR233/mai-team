//! T27 的固定路径、包名与验收镜像身份。

/// root 已构建的验收镜像；它声明了下面的匿名卷路径。
pub(super) const EXPECTED_IMAGE_ID: &str =
    "sha256:0fe61249a77f6ded65aa811f22e478b02d956dd142578bf17212430a618657ca";

/// 镜像 `Config.Volumes` 声明的数据路径。
pub(super) const ANON_VOLUME_TARGET: &str = "/opt/mai-acceptance-anon";

/// 可写层标记；同时用真实 deb 安装覆盖 `/var/lib/dpkg` 与 `/usr/share`。
pub(super) const USR_LOCAL_MARKER: &str = "/usr/local/mai-t27-marker.txt";

/// 项目工作区内的未提交标记。
pub(super) const WORKSPACE_MARKER: &str = "/workspace/repo/mai-t27-uncommitted.txt";

/// 镜像声明匿名卷内的标记。
pub(super) const ANON_MARKER: &str = "/opt/mai-acceptance-anon/mai-t27-marker.txt";

/// 无网络构建并安装的本地包；读取它证明 dpkg 数据库也被保留。
pub(super) const PACKAGE_NAME: &str = "mai-t27-retention";
