//! 项目 checkout / review worktree 在 sidecar 容器内执行 git 时复用的 shell 片段。
//!
//! 命令编排属于 mai 的 sidecar 传输层：本模块只复用 pl-tool 公开的 askpass 脚本文本,
//! 不重建 pl-tool 已经提供的 Git 能力,也不保留旧 pl-core 接口的兼容层。

use pl_tool::git::git_askpass_script;

/// 生成在 sidecar shell 内安装 git askpass 凭据的脚本前置片段。
///
/// 前置片段把 pl-tool 的 askpass 脚本文本写入容器内临时文件,并导出
/// `GIT_ASKPASS` / `GIT_TERMINAL_PROMPT`,供后续需要认证的 `git` 命令复用。
/// token 本身只通过 `GIT_TOKEN_ENV` 环境变量透传,不写入脚本。
pub(crate) fn git_shell_credential_prelude() -> String {
    format!(
        "askpass=/tmp/mai-git-askpass-$$.sh\n\
         trap 'rm -f \"$askpass\"' EXIT\n\
         cat > \"$askpass\" <<'MAI_GIT_ASKPASS'\n\
         {}MAI_GIT_ASKPASS\n\
         chmod 700 \"$askpass\"\n\
         export GIT_ASKPASS=\"$askpass\"\n\
         export GIT_TERMINAL_PROMPT=0\n",
        git_askpass_script()
    )
}

/// 生成 sidecar shell 脚本中可复用的 `git_with_retry` 函数。
///
/// 该函数在固定清空 `credential.helper` 并强制 `http.version=HTTP/1.1` 后执行
/// `git`,失败最多重试三次(退避 2s、4s),用于弱网下的项目 clone / fetch。
pub(crate) fn git_shell_retry_function() -> &'static str {
    "git_with_retry() {\n\
       attempts=0\n\
       while :; do\n\
         attempts=$((attempts + 1))\n\
         git -c credential.helper= -c http.version=HTTP/1.1 \"$@\" && return 0\n\
         status=$?\n\
         if [ \"$attempts\" -ge 3 ]; then\n\
           return \"$status\"\n\
         fi\n\
         sleep $((attempts * 2))\n\
       done\n\
     }\n"
}
