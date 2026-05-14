use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(author, version, about)]
pub(crate) struct Cli {
    #[arg(long = "data-path", value_name = "PATH")]
    pub(crate) data_path: Option<PathBuf>,
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
        assert!(Cli::try_parse_from(["mai-server", "--unknown"]).is_err());
        assert!(Cli::try_parse_from(["mai-server", "--help"]).is_err());
    }
}
