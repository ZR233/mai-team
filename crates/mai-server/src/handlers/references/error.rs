//! 参考源管理接口的统一错误响应。
//!
//! 新参考源 API 不沿用既有接口的 `{error}` 文本形状，也不把失败降级成 Axum 默认的
//! 纯文本 400：所有失败都返回 `{code,message,field_errors,request_id,retryable}`，
//! 客户端按稳定机器码分支，中文消息只用于展示。
//!
//! 边界：
//! - 状态码只由 `ReferenceErrorCode::http_status()` 决定，本层不再另立一套映射。
//! - 领域错误的机器码、中文消息与字段错误原样透出给管理面；未知的内部失败只写服务端
//!   日志，响应给通用消息，不泄漏宿主路径、命令环境或凭据。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mai_protocol::{ReferenceErrorCode, ReferenceErrorResponse, ReferenceFieldError};

use super::extract::RequestId;

/// 参考源管理接口的错误：状态码由机器码决定，响应体是本接口的稳定契约。
#[derive(Debug)]
pub(crate) struct ReferenceApiError {
    pub(super) status: StatusCode,
    pub(super) response: ReferenceErrorResponse,
}

impl ReferenceApiError {
    /// 由领域错误响应构造：状态码取机器码，`request_id` 由本层统一填充。
    pub(crate) fn domain(request_id: &RequestId, mut response: ReferenceErrorResponse) -> Self {
        response.request_id = Some(request_id.value().to_owned());
        Self {
            status: status_for_code(response.code),
            response,
        }
    }

    /// 请求体、查询参数或路径参数不合法：统一 422，没有字段级信息。
    pub(crate) fn invalid_request(request_id: &RequestId, message: impl Into<String>) -> Self {
        Self::from_parts(
            ReferenceErrorCode::InvalidRequest,
            request_id,
            message,
            Vec::new(),
        )
    }

    /// 请求字段不合法：统一 422，并保留字段级错误。
    pub(crate) fn invalid_request_with_fields(
        request_id: &RequestId,
        message: impl Into<String>,
        field_errors: Vec<ReferenceFieldError>,
    ) -> Self {
        Self::from_parts(
            ReferenceErrorCode::InvalidRequest,
            request_id,
            message,
            field_errors,
        )
    }

    /// 环境仍被占用（例如当前 turn 未结束）：409，条件变化后可以重试。
    pub(crate) fn busy(request_id: &RequestId, message: impl Into<String>) -> Self {
        Self::from_parts(
            ReferenceErrorCode::EnvironmentBusy,
            request_id,
            message,
            Vec::new(),
        )
    }

    /// 存储等后端能力暂不可用：503 且可重试，底层错误文本只进日志。
    pub(crate) fn unavailable(request_id: &RequestId, cause: impl std::fmt::Display) -> Self {
        tracing::warn!(
            request_id = request_id.value(),
            error = %cause,
            "参考源存储不可用"
        );
        Self::from_parts(
            ReferenceErrorCode::ReferenceStorageUnavailable,
            request_id,
            "参考源存储暂不可用，请稍后重试",
            Vec::new(),
        )
    }

    /// 内部失败：真实原因只进服务端日志，响应给通用中文消息。
    pub(crate) fn internal(request_id: &RequestId, cause: impl std::fmt::Display) -> Self {
        tracing::error!(
            request_id = request_id.value(),
            error = %cause,
            "参考源接口内部失败"
        );
        Self::from_parts(
            ReferenceErrorCode::InternalError,
            request_id,
            "参考源操作失败，请稍后重试或查看服务端日志",
            Vec::new(),
        )
    }

    /// 运行时错误：参考源领域错误保留原语义，其余收敛进本接口的错误契约。
    ///
    /// 参考源契约没有 "not found" 机器码（`SourceNotFound` 本身也是 422），因此按 ID
    /// 查不到的客户端错误同样归入 `InvalidRequest`；环境忙与存储失败单独保留可重试语义；
    /// 其余变体不属于参考源接口的正常路径，只记日志。
    pub(crate) fn from_runtime(request_id: &RequestId, error: mai_runtime::RuntimeError) -> Self {
        use mai_runtime::RuntimeError::*;
        match &error {
            Reference(error) => Self::domain(request_id, error.to_response(None)),
            AgentNotFound(agent_id) => {
                Self::invalid_request(request_id, format!("Agent 不存在：{agent_id}"))
            }
            TaskNotFound(task_id) => {
                Self::invalid_request(request_id, format!("任务不存在：{task_id}"))
            }
            ProjectNotFound(project_id) => {
                Self::invalid_request(request_id, format!("项目不存在：{project_id}"))
            }
            ProjectReviewRunNotFound(run_id) => {
                Self::invalid_request(request_id, format!("审查运行不存在：{run_id}"))
            }
            ProjectReviewJobNotFound(job_id) => {
                Self::invalid_request(request_id, format!("审查任务不存在：{job_id}"))
            }
            ThreadNotFound(thread_id) => {
                Self::invalid_request(request_id, format!("Thread 不存在：{thread_id}"))
            }
            TurnNotFound { agent_id, turn_id } => {
                Self::invalid_request(request_id, format!("Turn 不存在：{agent_id}/{turn_id}"))
            }
            ToolTraceNotFound { agent_id, call_id } => Self::invalid_request(
                request_id,
                format!("工具调用记录不存在：{agent_id}/{call_id}"),
            ),
            AgentBusy(agent_id) => Self::busy(
                request_id,
                format!("Agent 正在执行任务，稍后重试：{agent_id}"),
            ),
            TaskBusy(task_id) => {
                Self::busy(request_id, format!("任务正在执行，稍后重试：{task_id}"))
            }
            ProjectReviewerBusy(agent_id) => Self::busy(
                request_id,
                format!("审查槽位仍被占用，稍后重试：{agent_id}"),
            ),
            MissingContainer(agent_id) => Self::from_parts(
                ReferenceErrorCode::EnvironmentNotReady,
                request_id,
                format!("Agent 缺少可用容器，需要恢复执行环境：{agent_id}"),
                Vec::new(),
            ),
            TurnCancelled => Self::busy(request_id, "当前 turn 已被取消，请重新发起应用"),
            InvalidInput(message) => {
                Self::invalid_request(request_id, format!("请求不合法：{message}"))
            }
            Store(_) => Self::unavailable(request_id, &error),
            Docker(_)
            | Model(_)
            | Thread(_)
            | SessionStore(_)
            | Io(_)
            | Http(_)
            | Jwt(_)
            | ProjectReviewRunFinalization { .. }
            | GithubUnavailable { .. }
            | PullRequestNotOpen { .. }
            | PullRequestStateRefresh(_) => Self::internal(request_id, &error),
        }
    }

    fn from_parts(
        code: ReferenceErrorCode,
        request_id: &RequestId,
        message: impl Into<String>,
        field_errors: Vec<ReferenceFieldError>,
    ) -> Self {
        Self {
            status: status_for_code(code),
            response: ReferenceErrorResponse {
                code,
                message: message.into(),
                field_errors,
                request_id: Some(request_id.value().to_owned()),
                retryable: code.is_retryable(),
            },
        }
    }
}

impl IntoResponse for ReferenceApiError {
    fn into_response(self) -> Response {
        let request_id = self.response.request_id.as_deref().unwrap_or("-");
        if self.status.is_server_error() {
            tracing::error!(
                status = %self.status,
                code = %self.response.code,
                request_id,
                "参考源接口请求失败"
            );
        } else {
            tracing::warn!(
                status = %self.status,
                code = %self.response.code,
                request_id,
                "参考源接口拒绝请求"
            );
        }
        (self.status, Json(self.response)).into_response()
    }
}

/// 机器码 → HTTP 状态码；机器码的取值为协议常量，不会越界。
fn status_for_code(code: ReferenceErrorCode) -> StatusCode {
    StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use pretty_assertions::assert_eq;

    fn request_id(value: &str) -> RequestId {
        let mut headers = HeaderMap::new();
        headers.insert(
            super::super::extract::REQUEST_ID_HEADER,
            HeaderValue::from_str(value).expect("请求头"),
        );
        RequestId::from_headers(&headers)
    }

    fn domain_response(
        code: ReferenceErrorCode,
        request_id: Option<String>,
    ) -> ReferenceErrorResponse {
        ReferenceErrorResponse {
            code,
            message: "领域层消息".to_string(),
            field_errors: vec![ReferenceFieldError {
                field: "bindings[0].alias".to_string(),
                message: "alias 不符合规则".to_string(),
            }],
            request_id,
            retryable: false,
        }
    }

    #[test]
    fn domain_response_keeps_code_fields_and_request_id() {
        let request_id = request_id("req-7");
        let error = ReferenceApiError::domain(
            &request_id,
            domain_response(ReferenceErrorCode::ReferenceRevisionConflict, None),
        );

        assert_eq!(StatusCode::CONFLICT, error.status);
        assert_eq!(
            ReferenceErrorCode::ReferenceRevisionConflict,
            error.response.code
        );
        assert_eq!(Some("req-7".to_string()), error.response.request_id);
        assert!(!error.response.retryable);
        assert_eq!(1, error.response.field_errors.len());
    }

    #[test]
    fn domain_response_request_id_is_replaced_by_current_request() {
        let request_id = request_id("req-8");
        let error = ReferenceApiError::domain(
            &request_id,
            domain_response(
                ReferenceErrorCode::InvalidAlias,
                Some("stale-request".to_string()),
            ),
        );

        assert_eq!(Some("req-8".to_string()), error.response.request_id);
    }

    #[test]
    fn invalid_request_is_unprocessable_entity() {
        let error = ReferenceApiError::invalid_request(&request_id("req-9"), "请求体字段不合法：…");

        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, error.status);
        assert_eq!(ReferenceErrorCode::InvalidRequest, error.response.code);
        assert!(!error.response.retryable);
        assert!(error.response.field_errors.is_empty());
    }

    #[test]
    fn internal_failure_hides_details_but_stays_retryable() {
        let error =
            ReferenceApiError::internal(&request_id("req-10"), "宿主路径 /srv/secret 不可读");

        assert_eq!(StatusCode::INTERNAL_SERVER_ERROR, error.status);
        assert_eq!(ReferenceErrorCode::InternalError, error.response.code);
        assert!(error.response.retryable);
        assert!(!error.response.message.contains("/srv/secret"));
    }

    #[test]
    fn unavailable_storage_is_service_unavailable_and_retryable() {
        let error = ReferenceApiError::unavailable(&request_id("req-11"), "sqlite is locked");

        assert_eq!(StatusCode::SERVICE_UNAVAILABLE, error.status);
        assert_eq!(
            ReferenceErrorCode::ReferenceStorageUnavailable,
            error.response.code
        );
        assert!(error.response.retryable);
        assert!(!error.response.message.contains("sqlite"));
    }

    #[test]
    fn busy_environment_is_conflict_and_retryable() {
        let error = ReferenceApiError::busy(&request_id("req-12"), "Agent 正在执行任务");

        assert_eq!(StatusCode::CONFLICT, error.status);
        assert_eq!(ReferenceErrorCode::EnvironmentBusy, error.response.code);
        assert!(error.response.retryable);
    }

    #[test]
    fn not_found_runtime_errors_become_invalid_request() {
        let request_id = request_id("req-13");
        let project = ReferenceApiError::from_runtime(
            &request_id,
            mai_runtime::RuntimeError::ProjectNotFound(uuid::Uuid::new_v4()),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, project.status);
        assert_eq!(ReferenceErrorCode::InvalidRequest, project.response.code);

        let agent = ReferenceApiError::from_runtime(
            &request_id,
            mai_runtime::RuntimeError::AgentNotFound(uuid::Uuid::new_v4()),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, agent.status);
    }

    #[test]
    fn invalid_input_runtime_error_keeps_message() {
        let error = ReferenceApiError::from_runtime(
            &request_id("req-14"),
            mai_runtime::RuntimeError::InvalidInput("alias 重复".to_string()),
        );

        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, error.status);
        assert!(error.response.message.contains("alias 重复"));
    }

    #[test]
    fn busy_runtime_errors_become_conflict() {
        let request_id = request_id("req-15");
        let busy = ReferenceApiError::from_runtime(
            &request_id,
            mai_runtime::RuntimeError::AgentBusy(uuid::Uuid::new_v4()),
        );
        assert_eq!(StatusCode::CONFLICT, busy.status);
        assert_eq!(ReferenceErrorCode::EnvironmentBusy, busy.response.code);
        assert!(busy.response.retryable);

        let cancelled =
            ReferenceApiError::from_runtime(&request_id, mai_runtime::RuntimeError::TurnCancelled);
        assert_eq!(StatusCode::CONFLICT, cancelled.status);
        assert_eq!(ReferenceErrorCode::EnvironmentBusy, cancelled.response.code);
    }

    #[test]
    fn store_runtime_errors_become_unavailable_storage() {
        let error = ReferenceApiError::from_runtime(
            &request_id("req-16"),
            mai_runtime::RuntimeError::Store(mai_store::StoreError::InvalidConfig(
                "boom".to_string(),
            )),
        );

        assert_eq!(StatusCode::SERVICE_UNAVAILABLE, error.status);
        assert_eq!(
            ReferenceErrorCode::ReferenceStorageUnavailable,
            error.response.code
        );
        assert!(!error.response.message.contains("boom"));
    }

    #[test]
    fn unexpected_runtime_errors_are_internal_without_leaking_details() {
        let error = ReferenceApiError::from_runtime(
            &request_id("req-17"),
            mai_runtime::RuntimeError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "/srv/mai-references/secret denied",
            )),
        );

        assert_eq!(StatusCode::INTERNAL_SERVER_ERROR, error.status);
        assert_eq!(ReferenceErrorCode::InternalError, error.response.code);
        assert!(!error.response.message.contains("/srv/mai-references"));
    }
}
