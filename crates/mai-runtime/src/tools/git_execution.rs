//! mai 的 Git sidecar 执行后端。
//!
//! 每个 Thread 在装配时冻结一个环境租约（[`AgentEnvironmentLease`]）。Git 命令在一次性 sidecar
//! 容器里执行，因此本模块只把租约用于**生命周期**：运行前确认 generation 仍有效，并把这整段工作
//! （docker CLI + 环境撤销时的精确清理 + 等待真实进程结束）登记到该 generation 上。因此即使调用方
//! 自己的 future 被 close/quiesce 丢弃，清理与等待仍然归属同一个 generation，应用可以在发布候选
//! 之前等到它真实收敛；撤销时精确删除本次调用自己创建的 sidecar，并等 docker CLI 这个真实宿主进程
//! 结束，而不是丢掉 future 让容器里的 git 继续跑。
//!
//! sidecar 不是 Agent 容器，两者身份互不替代，因此这里**不**读取租约的 container_id；租约提供的
//! 是"这个 generation 还能不能继续工作"与"它登记的资源是否真的停了"这两条事实。

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use mai_docker::{DockerClient, SidecarParams};
use mai_protocol::{AgentId, ReferenceErrorCode};
use mai_references::{AgentEnvironmentLease, ReferenceError};
use pl_tool::execution::{ExecutionBackend, ExecutionOutput, ExecutionRequest};
use pl_tool::git::GIT_TOKEN_ENV;
use pl_tool::shell::shell_quote_word;

use crate::turn::container::ensure_lease_active;
use crate::{Result, RuntimeError};

/// 租约撤销后等待 docker CLI 真实退出的宽限时间。
///
/// 超出宽限说明 CLI 自己卡住了，此时才中止任务：CLI 设有 `kill_on_drop`，中止后随即收到 SIGKILL；
/// 而 sidecar 容器无论走哪条路径都由本模块按精确名字删除，因此不会留下无人清理的 git 进程。
const SIDECAR_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// 在 mai 产品宿主上用一次性 sidecar 容器执行 Git 命令的后端。
#[derive(Clone)]
pub(crate) struct MaiGitExecutionBackend {
    docker: DockerClient,
    sidecar_image: String,
    workspace_volume: String,
    repo_path: String,
    agent_id: AgentId,
    lease: Arc<AgentEnvironmentLease>,
}

impl MaiGitExecutionBackend {
    /// 绑定一次装配冻结的产品事实；`lease` 只用于 generation 生命周期，不参与 sidecar 身份。
    pub(crate) fn new(
        docker: DockerClient,
        sidecar_image: impl Into<String>,
        workspace_volume: impl Into<String>,
        repo_path: impl Into<String>,
        agent_id: AgentId,
        lease: Arc<AgentEnvironmentLease>,
    ) -> Self {
        Self {
            docker,
            sidecar_image: sidecar_image.into(),
            workspace_volume: workspace_volume.into(),
            repo_path: repo_path.into(),
            agent_id,
            lease,
        }
    }
}

impl fmt::Debug for MaiGitExecutionBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaiGitExecutionBackend")
            .field("agent_id", &self.agent_id)
            .field("container_id", &self.lease.container_id())
            .field("generation", &self.lease.generation())
            .finish_non_exhaustive()
    }
}

impl ExecutionBackend for MaiGitExecutionBackend {
    type Error = RuntimeError;

    async fn run(&self, request: ExecutionRequest) -> Result<ExecutionOutput> {
        // 运行前先确认 generation 仍然有效：已撤销的租约不允许再借用它的环境执行命令。
        ensure_lease_active(&self.lease)?;
        run_sidecar_git_output(
            &self.docker,
            Arc::clone(&self.lease),
            &self.sidecar_image,
            &self.workspace_volume,
            &self.repo_path,
            self.agent_id,
            request,
        )
        .await
    }
}

/// 在一次性 sidecar 容器里执行一次 git，并保证环境被撤销时真的清理掉本次容器。
///
/// 整段工作登记在本 Thread 的 generation 租约上：撤销之后不再接受登记，因此这里要么在撤销之前
/// 成功登记（并由 generation 的收敛等待覆盖），要么直接失败，不会留下无人跟踪的 sidecar。
async fn run_sidecar_git_output(
    docker: &DockerClient,
    lease: Arc<AgentEnvironmentLease>,
    sidecar_image: &str,
    workspace_volume: &str,
    repo_path: &str,
    agent_id: AgentId,
    request: ExecutionRequest,
) -> Result<ExecutionOutput> {
    let token = request.env.get(GIT_TOKEN_ENV).cloned();
    // mai 负责 sidecar 物理执行环境，因此不复用宿主侧的 `GIT_ASKPASS` 路径：
    // 凭据存在时在容器内生成临时 askpass 脚本，并只透传 token 本身。
    let credential = match token {
        Some(_) => SidecarGitCredential::EnvToken,
        None => SidecarGitCredential::Disabled,
    };
    let command = sidecar_git_command(repo_path, &request.program, &request.args, credential);
    let env = token
        .map(|token| vec![(GIT_TOKEN_ENV.to_string(), token)])
        .unwrap_or_default();
    let timeout_secs = request.timeout.map(|timeout| timeout.as_secs());
    let sidecar_name = format!("mai-tool-git-{agent_id}-{}", uuid::Uuid::new_v4());
    let run = SidecarRun {
        name: sidecar_name.clone(),
        image: sidecar_image.to_string(),
        command,
        workspace_volume: workspace_volume.to_string(),
        repo_path: repo_path.to_string(),
        env,
        timeout_secs,
    };
    // 登记到 generation：调用方之后即使被 close 丢掉等待，这段工作仍会被撤销等待等到。
    let mut tracked = lease
        .spawn_tracked({
            let docker = docker.clone();
            let lease = Arc::clone(&lease);
            async move { run_tracked_sidecar(docker, lease, run).await }
        })
        .map_err(|error| {
            // 撤销 fence 已经落下：与 command/container 后端一致，按取消语义报告，
            // 并把 typed code 留在日志里。
            tracing::warn!(
                code = %error.code(),
                sidecar = %sidecar_name,
                "refusing to start a git sidecar because its environment lease is being revoked"
            );
            RuntimeError::Thread(pl_core::thread::ThreadError::Cancelled)
        })?;
    match tracked.wait().await {
        Ok(result) => result,
        Err(join_error) => {
            // 任务没有正常收束（panic/被中止）：真实清理是否完成未知，按清理失败记载，
            // 让撤销等待返回失败，而不是放行新 generation。
            tracing::warn!(
                %join_error,
                sidecar = %sidecar_name,
                "git sidecar task did not settle; recording an unconfirmed cleanup"
            );
            lease
                .lifecycle()
                .record_cleanup_failure(ReferenceError::new(
                    ReferenceErrorCode::EnvironmentApplyFailed,
                    format!("git sidecar `{sidecar_name}` 的任务没有正常收束，真实清理未确认"),
                ));
            Err(RuntimeError::Io(std::io::Error::other(format!(
                "git sidecar `{sidecar_name}` task failed: {join_error}"
            ))))
        }
    }
}

/// 一次 git sidecar 调用的全部物理事实。
struct SidecarRun {
    name: String,
    image: String,
    command: String,
    workspace_volume: String,
    repo_path: String,
    env: Vec<(String, String)>,
    timeout_secs: Option<u64>,
}

/// 已登记任务的真实工作：跑 docker CLI，并在环境撤销时精确清理本次 sidecar。
///
/// 宿主进程句柄由本任务独占，因此调用方丢弃自己的等待不会把它丢下；撤销时先删容器让正在跑的 git
/// 立刻停下，再等 CLI 真实结束（超过宽限才中止它）。
async fn run_tracked_sidecar(
    docker: DockerClient,
    lease: Arc<AgentEnvironmentLease>,
    run: SidecarRun,
) -> Result<ExecutionOutput> {
    let mut receipt = CleanupReceipt {
        lease: &lease,
        settled: false,
    };
    let mut cli = tokio::spawn({
        let docker = docker.clone();
        let name = run.name.clone();
        let image = run.image.clone();
        let command = run.command.clone();
        let repo_path = run.repo_path.clone();
        let workspace_volume = run.workspace_volume.clone();
        let env = run.env.clone();
        let timeout_secs = run.timeout_secs;
        async move {
            docker
                .run_sidecar_shell_env(&SidecarParams {
                    name: &name,
                    image: &image,
                    command: &command,
                    args: &[],
                    cwd: Some(&repo_path),
                    env: &env,
                    workspace_volume: Some(&workspace_volume),
                    mounts: &[],
                    timeout_secs,
                })
                .await
        }
    });
    let revocation = lease.cancellation();
    // 只有 select 表达式借用 `cli`；收尾（删容器、等待/中止 CLI）在它之后进行。
    let joined = tokio::select! {
        joined = &mut cli => Some(joined),
        _ = revocation.cancelled() => None,
    };
    let Some(joined) = joined else {
        // 1) 先按精确名字删掉本次 sidecar：正在跑的 git 立刻停下（写副作用不该在环境撤销后继续），
        //    容器不存在时是 no-op。
        remove_sidecar(&docker, &lease, &run.name).await;
        // 2) 再等 docker CLI 这个真实进程结束；超过宽限才中止它（CLI 设有 kill_on_drop，随即收到
        //    SIGKILL）。先 await 成值再 match：让 `&mut cli` 的借用随 timeout future 一起在这个
        //    语句结束时释放，收尾分支才能继续持有同一个句柄。
        let cli_exit = tokio::time::timeout(SIDECAR_SHUTDOWN_GRACE, &mut cli).await;
        match cli_exit {
            Ok(Ok(_output)) => {}
            Ok(Err(run_error)) => {
                // 容器刚被删掉，CLI 带着错误返回是预期结果，不是需要告警的故障。
                tracing::debug!(
                    %run_error,
                    sidecar = %run.name,
                    "git sidecar docker client stopped after its container was removed"
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                    sidecar = %run.name,
                    "git sidecar docker client did not exit within the shutdown grace; aborting it"
                );
                cli.abort();
                let _ = cli.await;
            }
        }
        // 3) CLI 已经结束，因此现在再删一次不会和"容器还没建起来"竞争：第一次删除之后才被建起来的
        //    容器也必须被清掉，否则会留下没人清理的 git 进程。
        remove_sidecar(&docker, &lease, &run.name).await;
        receipt.settled = true;
        // 撤销后 `ensure_lease_active` 必然失败，并按取消语义带上 typed code；
        // 这里保持与 command/container 后端一致的"环境换代即取消"报告方式。
        ensure_lease_active(&lease)?;
        return Err(RuntimeError::InvalidInput(format!(
            "git sidecar `{}` stopped because its environment lease was revoked",
            run.name
        )));
    };
    let output = joined
        .map_err(|error| {
            RuntimeError::Io(std::io::Error::other(format!(
                "git sidecar `{}` task failed: {error}",
                run.name
            )))
        })?
        .map_err(RuntimeError::from)?;
    receipt.settled = true;
    Ok(ExecutionOutput {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// 按精确名字删除本次 sidecar；删除失败按真实清理失败记载，让撤销等待返回错误。
///
/// 正常的有意撤销不是清理失败：容器按预期删掉就什么都不记；只有"是否真的删掉了"得不到确认时才
/// 记载，并因此让后续的撤销等待返回失败。
async fn remove_sidecar(docker: &DockerClient, lease: &AgentEnvironmentLease, sidecar_name: &str) {
    if let Err(error) = docker.delete_container(sidecar_name).await {
        tracing::warn!(
            %error,
            sidecar = %sidecar_name,
            "failed to remove a git sidecar whose environment lease was revoked"
        );
        lease
            .lifecycle()
            .record_cleanup_failure(ReferenceError::new(
                ReferenceErrorCode::EnvironmentApplyFailed,
                format!("git sidecar `{sidecar_name}` 删除失败：{error}"),
            ));
    }
}

/// 任务内收据：任务没有正常收束（panic/被中止）时按"真实清理未确认"记载。
///
/// 只丢 future 不等于资源停了：这里把"没人确认过清理"这件事记在 generation 上，让撤销等待返回失败，
/// 而不是让新 generation 在未确认的旧资源之上被发布。
struct CleanupReceipt<'a> {
    lease: &'a AgentEnvironmentLease,
    settled: bool,
}

impl Drop for CleanupReceipt<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.lease
                .lifecycle()
                .record_cleanup_failure(ReferenceError::new(
                    ReferenceErrorCode::EnvironmentApplyFailed,
                    "git sidecar 清理任务没有正常收束，真实清理结果未知",
                ));
        }
    }
}

/// sidecar 内 git 命令的凭据注入模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidecarGitCredential {
    Disabled,
    EnvToken,
}

/// 构造在 sidecar 容器内执行的 git 命令。
///
/// 只复用 pl-tool 公开的 askpass 脚本文本，命令编排属于 mai 的 sidecar 传输层：
/// 固定关闭 hooks、声明 safe.directory 并清空 credential.helper，参数由 pl-tool 已校验。
fn sidecar_git_command(
    repo_path: &str,
    program: &std::path::Path,
    args: &[String],
    credential: SidecarGitCredential,
) -> String {
    let mut parts = vec![
        shell_quote_word(&program.to_string_lossy()),
        "-c".to_string(),
        shell_quote_word("core.hooksPath=/dev/null"),
        "-c".to_string(),
        shell_quote_word(&format!("safe.directory={repo_path}")),
        "-c".to_string(),
        shell_quote_word("credential.helper="),
    ];
    parts.extend(args.iter().map(|arg| shell_quote_word(arg)));
    let git_command = parts.join(" ");
    match credential {
        SidecarGitCredential::Disabled => git_command,
        SidecarGitCredential::EnvToken => format!(
            "askpass=/tmp/mai-git-askpass-$$.sh\n\
             trap 'rm -f \"$askpass\"' EXIT\n\
             cat > \"$askpass\" <<'MAI_GIT_ASKPASS'\n\
             {}MAI_GIT_ASKPASS\n\
             chmod 700 \"$askpass\"\n\
             export GIT_ASKPASS=\"$askpass\"\n\
             export GIT_TERMINAL_PROMPT=0\n\
             {git_command}",
            pl_tool::git::git_askpass_script()
        ),
    }
}
