//! CLI/环境变量/JSON 配置的解析与优先级合并（CLI → env → 文件 → 缺省）。

use super::*;

/// JSON 配置文件形状；字段全部可选，缺省值在解析阶段补齐。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FileConfig {
    pub root: Option<String>,
    pub linux_source_path: Option<String>,
    pub fixture_source_path: Option<String>,
    pub linux_alias: Option<String>,
    pub private_alias: Option<String>,
    pub agent_image: Option<String>,
    pub sidecar_image: Option<String>,
    pub docker_binary: Option<String>,
    pub trust: Option<String>,
    pub provision: Option<String>,
    pub scenario: Option<String>,
    pub inject_phase: Option<String>,
    pub seed_bindings: Option<Vec<String>>,
    pub host_digest: Option<String>,
    pub timeout_secs: Option<u64>,
    pub revalidate_sources: Option<bool>,
    pub extra_allowed_roots: Option<Vec<String>>,
    pub check_commands: Option<Vec<String>>,
}

/// CLI 上出现的原始字符串；与文件配置同形，便于按优先级合并。
#[derive(Debug, Clone, Default)]
struct CliOptions {
    config: Option<String>,
    root: Option<String>,
    scenario: Option<String>,
    inject_phase: Option<String>,
    agent_image: Option<String>,
    sidecar_image: Option<String>,
    docker_binary: Option<String>,
    trust: Option<String>,
    provision: Option<String>,
    seed_bindings: Option<String>,
    host_digest: Option<String>,
    timeout_secs: Option<String>,
    revalidate_sources: bool,
    help: bool,
    command: Option<String>,
}

fn pick(
    cli: Option<String>,
    env: Option<String>,
    file: Option<String>,
    fallback: String,
) -> String {
    cli.or(env).or(file).unwrap_or(fallback)
}

fn pick_string(
    cli: Option<String>,
    env: Option<String>,
    file: Option<String>,
    fallback: &str,
) -> String {
    pick(cli, env, file, fallback.to_string())
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn read_file_config(path: &str) -> Result<FileConfig> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        ProbeError::new(
            "config_unreadable",
            format!("无法读取配置文件 {path}：{error}"),
        )
    })?;
    serde_json::from_str(&text).map_err(|error| {
        ProbeError::new(
            "config_invalid",
            format!("配置文件 {path} 不是严格 JSON：{error}"),
        )
    })
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<CliOptions> {
    fn value_at(arguments: &[String], index: &mut usize, flag: &str) -> Result<String> {
        let value = arguments
            .get(*index)
            .ok_or_else(|| ProbeError::new("missing_argument", format!("{flag} 需要一个取值")))?;
        *index += 1;
        Ok(value.clone())
    }

    let mut options = CliOptions::default();
    let arguments = arguments.collect::<Vec<_>>();
    let mut index = 0usize;
    while index < arguments.len() {
        let argument = arguments[index].clone();
        index += 1;
        match argument.as_str() {
            "--help" | "-h" => options.help = true,
            "--config" => options.config = Some(value_at(&arguments, &mut index, "--config")?),
            "--root" => options.root = Some(value_at(&arguments, &mut index, "--root")?),
            "--scenario" => {
                options.scenario = Some(value_at(&arguments, &mut index, "--scenario")?)
            }
            "--inject-phase" => {
                options.inject_phase = Some(value_at(&arguments, &mut index, "--inject-phase")?)
            }
            "--agent-image" => {
                options.agent_image = Some(value_at(&arguments, &mut index, "--agent-image")?)
            }
            "--sidecar-image" => {
                options.sidecar_image = Some(value_at(&arguments, &mut index, "--sidecar-image")?)
            }
            "--docker-binary" => {
                options.docker_binary = Some(value_at(&arguments, &mut index, "--docker-binary")?)
            }
            "--trust" => options.trust = Some(value_at(&arguments, &mut index, "--trust")?),
            "--provision" => {
                options.provision = Some(value_at(&arguments, &mut index, "--provision")?)
            }
            "--seed-bindings" => {
                options.seed_bindings = Some(value_at(&arguments, &mut index, "--seed-bindings")?)
            }
            "--host-digest" => {
                options.host_digest = Some(value_at(&arguments, &mut index, "--host-digest")?)
            }
            "--timeout-secs" => {
                options.timeout_secs = Some(value_at(&arguments, &mut index, "--timeout-secs")?)
            }
            "--revalidate-sources" => options.revalidate_sources = true,
            other if other.starts_with("--") => {
                return err("unknown_argument", format!("未知参数 `{other}`；见 --help"));
            }
            other => {
                if options.command.is_some() {
                    return err(
                        "duplicate_command",
                        format!("一次只能指定一个子命令，重复出现 `{other}`"),
                    );
                }
                options.command = Some(other.to_string());
            }
        }
    }
    Ok(options)
}

impl ProbeConfig {
    /// 解析 CLI、环境变量与 JSON 配置文件。
    pub fn from_args(arguments: impl Iterator<Item = String>) -> Result<Self> {
        let cli = parse_arguments(arguments)?;
        let help = cli.help;
        let file = match cli.config.as_deref() {
            Some(path) => read_file_config(path)?,
            None => FileConfig::default(),
        };
        Self::resolve(cli, file, help)
    }

    fn resolve(cli: CliOptions, file: FileConfig, help: bool) -> Result<Self> {
        let root_text = pick(
            cli.root,
            std::env::var("MAI_ACCEPTANCE_ROOT").ok(),
            file.root,
            ACCEPTANCE_ROOT.to_string(),
        );
        let root = PathBuf::from(root_text);

        let fixture_override = std::env::var("MAI_ACCEPTANCE_FIXTURE_ROOT").ok();
        let fixture_repo = match fixture_override.or(file.fixture_source_path) {
            Some(path) => PathBuf::from(path),
            None => root.join(FIXTURE_SOURCE_RELATIVE),
        };
        // 允许根是夹具仓库的父目录；Linux 允许根是版本目录本身。
        let fixture_root = fixture_repo
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| ProbeError::new("invalid_fixture_path", "夹具源码路径没有父目录"))?;

        let linux_source = PathBuf::from(pick(
            None,
            std::env::var("MAI_ACCEPTANCE_LINUX_SOURCE").ok(),
            file.linux_source_path,
            PUBLISHED_LINUX_SOURCE.to_string(),
        ));

        let agent_image = pick_string(
            cli.agent_image,
            std::env::var("MAI_ACCEPTANCE_AGENT_IMAGE").ok(),
            file.agent_image,
            DEFAULT_AGENT_IMAGE,
        );
        let sidecar_image = pick_string(
            cli.sidecar_image,
            std::env::var("MAI_ACCEPTANCE_SIDECAR_IMAGE").ok(),
            file.sidecar_image,
            DEFAULT_SIDECAR_IMAGE,
        );
        let docker_binary = pick_string(
            cli.docker_binary,
            std::env::var("MAI_ACCEPTANCE_DOCKER_BIN").ok(),
            file.docker_binary,
            "docker",
        );

        let trust = TrustChoice::parse(&pick_string(
            cli.trust.clone(),
            None,
            file.trust.clone(),
            "trusted_owner_and_permissions",
        ))?;
        let provision = ProvisionChoice::parse(&pick_string(
            cli.provision.clone(),
            None,
            file.provision.clone(),
            "canonical",
        ))?;
        let host_digest = DigestScope::parse(&pick_string(
            cli.host_digest.clone(),
            None,
            file.host_digest.clone(),
            "fixture",
        ))?;

        let scenario_text = cli.scenario.clone().or_else(|| file.scenario.clone());
        let scenario = match scenario_text.as_deref() {
            Some(text) => Some(Scenario::parse(text)?),
            None => None,
        };
        let inject_text = cli
            .inject_phase
            .clone()
            .or_else(|| file.inject_phase.clone());
        let inject_phase = match inject_text.as_deref() {
            Some(text) => Some(ReferenceApplyPhase::from_str(text).map_err(|error| {
                ProbeError::new(
                    "unknown_inject_phase",
                    format!("未知注入 phase `{text}`：{error}"),
                )
            })?),
            None => None,
        };

        let seed_bindings = match cli.seed_bindings.clone() {
            Some(list) => split_list(&list),
            None => file.seed_bindings.clone().unwrap_or_default(),
        };

        let timeout_secs = match cli.timeout_secs.clone() {
            Some(text) => text.parse::<u64>().map_err(|error| {
                ProbeError::new(
                    "invalid_timeout",
                    format!("--timeout-secs 不是数字：{error}"),
                )
            })?,
            None => file.timeout_secs.unwrap_or(DEFAULT_OPERATION_TIMEOUT_SECS),
        };
        if timeout_secs == 0 {
            return err("invalid_timeout", "--timeout-secs 必须大于 0");
        }

        let command_text = cli
            .command
            .clone()
            .ok_or_else(|| ProbeError::new("missing_command", "缺少子命令；见 --help"))?;
        let command = Command::parse(&command_text)?;

        let extra_allowed_roots = file
            .extra_allowed_roots
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();

        Ok(Self {
            command,
            scenario,
            help,
            data_dir: root.join("data"),
            db_path: root.join("data").join("mai-team.sqlite3"),
            evidence_dir: root.join("evidence"),
            evidence_log: root.join("evidence").join("runs.jsonl"),
            policy_path: root.join("config").join("reference-sources.toml"),
            fixture_root,
            fixture_repo,
            root,
            linux_source,
            linux_alias: pick_string(None, None, file.linux_alias, "linux"),
            private_alias: pick_string(None, None, file.private_alias, "private-fixture"),
            agent_image,
            sidecar_image,
            docker_binary,
            trust,
            provision,
            seed_bindings,
            inject_phase,
            host_digest,
            operation_timeout: Duration::from_secs(timeout_secs),
            revalidate_sources: cli.revalidate_sources || file.revalidate_sources.unwrap_or(false),
            extra_allowed_roots,
            check_commands: file.check_commands.clone().unwrap_or_default(),
        })
    }
}
