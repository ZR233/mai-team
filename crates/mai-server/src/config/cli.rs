use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(author, version, about)]
pub(crate) struct Cli {
    #[arg(long = "data-path", value_name = "PATH")]
    pub(crate) data_path: Option<PathBuf>,

    /// 部署侧参考源码策略文件（由管理员维护的独立 TOML，不是可写产品配置）。
    ///
    /// 只接线路径：文件内容由运行时域层读取校验。未提供时参考源登记被禁用，其余功能可用。
    #[arg(long = "reference-policy", value_name = "PATH")]
    pub(crate) reference_policy: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_data_path() {
        let cli =
            Cli::try_parse_from(["mai-server", "--data-path", "/tmp/mai-data"]).expect("parse cli");
        assert_eq!(cli.data_path, Some(PathBuf::from("/tmp/mai-data")));

        let cli =
            Cli::try_parse_from(["mai-server", "--data-path=/tmp/mai-data"]).expect("parse cli");
        assert_eq!(cli.data_path, Some(PathBuf::from("/tmp/mai-data")));
    }

    #[test]
    fn cli_rejects_invalid_data_path_usage() {
        assert!(Cli::try_parse_from(["mai-server", "--data-path"]).is_err());
        assert!(Cli::try_parse_from(["mai-server", "--reference-policy"]).is_err());
        assert!(Cli::try_parse_from(["mai-server", "--unknown"]).is_err());
        assert!(Cli::try_parse_from(["mai-server", "--help"]).is_err());
    }

    #[test]
    fn cli_parses_reference_policy() {
        let cli = Cli::try_parse_from([
            "mai-server",
            "--reference-policy",
            "/etc/mai-team/reference-policy.toml",
        ])
        .expect("parse cli");
        assert_eq!(
            cli.reference_policy,
            Some(PathBuf::from("/etc/mai-team/reference-policy.toml"))
        );
        assert_eq!(cli.data_path, None);

        let cli =
            Cli::try_parse_from(["mai-server", "--data-path", "/tmp/mai-data"]).expect("parse cli");
        assert_eq!(cli.reference_policy, None);
    }
}
