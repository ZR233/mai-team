//! 项目参考源码的管理接口：登记源、项目绑定与状态查询。
//!
//! 契约要点：
//! - 保存成功与容器应用成功是两件事：`PUT /projects/{id}/references` 返回 200 只代表配置
//!   已 durable 保存，实际挂载由 runtime 的应用流程收敛，本层不直接写库、不绕过 runtime 门禁。
//! - 所有失败都返回 `{code,message,field_errors,request_id,retryable}`（含请求体、查询参数与
//!   路径参数的拒绝），不沿用 Axum 默认的纯文本 400，也不泄漏未知异常与宿主敏感值。
//! - 登记状态与验证在后台进行：`POST /reference-sources`、`POST /reference-sources/{id}/verify`
//!   与 `POST /projects/{id}/references/apply` 都用 202 表示"已接受、仍在收敛"。
//! - 鉴权由部署侧反向代理负责（T37 明确排除在本轮之外），本层不新增登录与鉴权。
//! - 宿主路径只出现在管理面响应里；本层不把源路径写入事件负载或模型可见文本。

mod error;
mod extract;

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use mai_protocol::{
    ApplyProjectReferencesRequest, NewReferenceSourceRequest, PatchReferenceSourceRequest,
    ProjectId, ProjectReferenceConfiguration, ProjectReferenceStatus, ReferenceFieldError,
    ReferenceSource, ReferenceSourcePolicy, ReferenceStatusQuery, ReplaceProjectReferencesRequest,
};

use super::state::AppState;
use error::ReferenceApiError;
use extract::{ReferenceJson, ReferencePath, ReferenceQuery, RequestId};

/// `GET /reference-sources`：列出已登记的参考源及其验证状态。
pub(crate) async fn list_reference_sources(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ReferenceSource>>, ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    let catalog = state.runtime.reference_catalog();
    let sources = catalog
        .sources()
        .await
        .map_err(|error| ReferenceApiError::domain(&request_id, error.to_response(None)))?;
    Ok(Json(sources))
}

/// `POST /reference-sources`：登记宿主目录并排队只读验证，202 返回 `validating` 记录。
pub(crate) async fn register_reference_source(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ReferenceJson(request): ReferenceJson<NewReferenceSourceRequest>,
) -> Result<(StatusCode, Json<ReferenceSource>), ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    let catalog = state.runtime.reference_catalog();
    let source = catalog
        .register(request)
        .await
        .map_err(|error| ReferenceApiError::domain(&request_id, error.to_response(None)))?;
    Ok((StatusCode::ACCEPTED, Json(source)))
}

/// `GET /reference-sources/{id}`：读取单个参考源的验证进度与健康状态。
pub(crate) async fn get_reference_source(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ReferencePath(source_id): ReferencePath<uuid::Uuid>,
) -> Result<Json<ReferenceSource>, ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    let catalog = state.runtime.reference_catalog();
    let source = catalog
        .source(source_id)
        .await
        .map_err(|error| ReferenceApiError::domain(&request_id, error.to_response(None)))?;
    Ok(Json(source))
}

/// `PATCH /reference-sources/{id}`：只允许修改名称、版本说明、供 Agent 使用确认与启用状态。
///
/// 路径、来源与摘要属于登记身份，不在可改字段内；服务端仍要求"全局禁用"没有任何项目绑定。
pub(crate) async fn patch_reference_source(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ReferencePath(source_id): ReferencePath<uuid::Uuid>,
    ReferenceJson(request): ReferenceJson<PatchReferenceSourceRequest>,
) -> Result<Json<ReferenceSource>, ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    if request.is_empty() {
        return Err(ReferenceApiError::invalid_request_with_fields(
            &request_id,
            "PATCH 请求未包含任何可修改字段",
            vec![ReferenceFieldError {
                field: "patch".to_string(),
                message:
                    "至少需要提供 name、revision_label、approved_for_agent_access 或 disabled 之一"
                        .to_string(),
            }],
        ));
    }
    let catalog = state.runtime.reference_catalog();
    let source = catalog
        .patch(source_id, request)
        .await
        .map_err(|error| ReferenceApiError::domain(&request_id, error.to_response(None)))?;
    Ok(Json(source))
}

/// `DELETE /reference-sources/{id}`：注销登记元数据；仍被项目引用时由领域层返回 409。
///
/// 该接口绝不删除宿主目录：物理清理由管理员独立完成。
pub(crate) async fn delete_reference_source(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ReferencePath(source_id): ReferencePath<uuid::Uuid>,
) -> Result<StatusCode, ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    let catalog = state.runtime.reference_catalog();
    catalog
        .unregister(source_id)
        .await
        .map_err(|error| ReferenceApiError::domain(&request_id, error.to_response(None)))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /reference-sources/{id}/verify`：排队重新验证，202 表示已接受。
pub(crate) async fn verify_reference_source(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ReferencePath(source_id): ReferencePath<uuid::Uuid>,
) -> Result<(StatusCode, Json<ReferenceSource>), ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    let catalog = state.runtime.reference_catalog();
    let source = catalog
        .verify(source_id)
        .await
        .map_err(|error| ReferenceApiError::domain(&request_id, error.to_response(None)))?;
    Ok((StatusCode::ACCEPTED, Json(source)))
}

/// `GET /reference-source-policy`：返回部署侧能力与限制；管理 UI 只能读取，不能扩大允许根。
pub(crate) async fn get_reference_source_policy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<ReferenceSourcePolicy>, ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    let policy = state
        .runtime
        .reference_source_policy()
        .await
        .map_err(|error| ReferenceApiError::from_runtime(&request_id, error))?;
    Ok(Json(policy))
}

/// `GET /projects/{id}/references`：读取项目已 durable 的参考源配置。
///
/// 各 Agent 的实际应用情况、operation 收据与分页见 `GET /projects/{id}/references/status`。
pub(crate) async fn get_project_references(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ReferencePath(project_id): ReferencePath<ProjectId>,
) -> Result<Json<ProjectReferenceConfiguration>, ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    let configuration = state
        .runtime
        .project_reference_configuration(project_id)
        .await
        .map_err(|error| ReferenceApiError::from_runtime(&request_id, error))?;
    Ok(Json(configuration))
}

/// `PUT /projects/{id}/references`：CAS 整体替换配置，200 表示配置已 durable。
///
/// `bindings` 必须显式给出（缺失或 `null` 都是 422，`[]` 才表示清空）；应用策略默认
/// `when_idle`，不在这里直接改容器。
pub(crate) async fn replace_project_references(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ReferencePath(project_id): ReferencePath<ProjectId>,
    ReferenceJson(request): ReferenceJson<ReplaceProjectReferencesRequest>,
) -> Result<Json<ProjectReferenceConfiguration>, ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    let configuration = state
        .runtime
        .replace_project_references(project_id, request)
        .await
        .map_err(|error| ReferenceApiError::from_runtime(&request_id, error))?;
    Ok(Json(configuration))
}

/// `POST /projects/{id}/references/apply`：请求立即重试或"停止并应用"，202 表示已接受。
pub(crate) async fn apply_project_references(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ReferencePath(project_id): ReferencePath<ProjectId>,
    ReferenceJson(request): ReferenceJson<ApplyProjectReferencesRequest>,
) -> Result<(StatusCode, Json<ProjectReferenceStatus>), ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    let status = state
        .runtime
        .apply_project_references(project_id, request)
        .await
        .map_err(|error| ReferenceApiError::from_runtime(&request_id, error))?;
    Ok((StatusCode::ACCEPTED, Json(status)))
}

/// `GET /projects/{id}/references/status`：按分页返回各 Agent 的应用状态与 operation 收据。
///
/// 查询不触发 Docker 创建/重建，只读当前事实。`limit` 超过协议上限时按上限截断。
pub(crate) async fn project_reference_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ReferencePath(project_id): ReferencePath<ProjectId>,
    ReferenceQuery(query): ReferenceQuery<ReferenceStatusQuery>,
) -> Result<Json<ProjectReferenceStatus>, ReferenceApiError> {
    let request_id = RequestId::from_headers(&headers);
    let status = state
        .runtime
        .project_reference_status(project_id, query.normalized())
        .await
        .map_err(|error| ReferenceApiError::from_runtime(&request_id, error))?;
    Ok(Json(status))
}
