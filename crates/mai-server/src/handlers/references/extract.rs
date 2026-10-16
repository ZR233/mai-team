//! 参考源管理接口的提取器与请求关联 ID。
//!
//! 目的：新参考源 API 的请求体、查询参数与路径参数失败都必须返回统一的
//! `{code,message,field_errors,request_id,retryable}`，而不是 Axum 默认的纯文本 400。
//! 这些提取器包住 Axum 自带的 `Json`/`Query`/`Path` 提取器，把它们的拒绝映射成
//! `ReferenceApiError`，因此内容类型处理、反序列化细节与字段错误文本仍由 Axum 提供。

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use mai_protocol::ReferenceFieldError;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use super::error::ReferenceApiError;

/// 请求关联 ID 的请求头；与 `http::router` 的 `SetRequestIdLayer` 使用同一名称。
pub(crate) const REQUEST_ID_HEADER: &str = "x-request-id";

/// 可沿用的请求关联 ID 最长字符数。
const MAX_REQUEST_ID_CHARS: usize = 128;

/// 请求关联 ID：优先沿用请求头 `x-request-id`，缺失或不可用时生成新的 UUID。
///
/// 该值只用于日志与错误响应关联，不参与授权判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestId(String);

impl RequestId {
    /// 从请求头解析；不可用的值（空、过长、非可见 ASCII）一律替换成新生成的 UUID。
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        let existing = headers
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(sanitize_request_id);
        Self(existing.unwrap_or_else(|| Uuid::new_v4().to_string()))
    }

    /// 响应与日志里使用的请求 ID。
    pub(crate) fn value(&self) -> &str {
        &self.0
    }
}

/// 只接受可见 ASCII：避免把任意客户端字节回显进响应体与服务端日志。
fn sanitize_request_id(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_REQUEST_ID_CHARS {
        return None;
    }
    trimmed
        .chars()
        .all(|character| character.is_ascii_graphic())
        .then(|| trimmed.to_string())
}

/// JSON 请求体提取器：拒绝时返回统一结构化错误。
///
/// 必须在 handler 参数列表的最后使用，因为它会消费请求体。
pub(crate) struct ReferenceJson<T>(pub(crate) T);

impl<T, S> FromRequest<S> for ReferenceJson<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ReferenceApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = RequestId::from_headers(request.headers());
        let Json(value) = Json::<T>::from_request(request, state)
            .await
            .map_err(|rejection| json_rejection(&request_id, rejection))?;
        Ok(Self(value))
    }
}

/// 查询参数提取器：参数缺失时用类型自身的默认值，参数不合法时返回结构化 422。
pub(crate) struct ReferenceQuery<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for ReferenceQuery<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ReferenceApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = RequestId::from_headers(&parts.headers);
        let Query(value) = Query::<T>::from_request_parts(parts, state)
            .await
            .map_err(|rejection| query_rejection(&request_id, rejection))?;
        Ok(Self(value))
    }
}

/// 路径参数提取器：ID 解析失败时返回结构化 422，而不是纯文本 400。
pub(crate) struct ReferencePath<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for ReferencePath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ReferenceApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = RequestId::from_headers(&parts.headers);
        let Path(value) = Path::<T>::from_request_parts(parts, state)
            .await
            .map_err(|rejection| path_rejection(&request_id, rejection))?;
        Ok(Self(value))
    }
}

/// 请求体拒绝：缺少/错误的 `Content-Type`、非法 JSON、未知或缺失字段、`null` 都返回 422。
fn json_rejection(request_id: &RequestId, rejection: JsonRejection) -> ReferenceApiError {
    let detail = rejection.body_text();
    let field_errors = field_errors_from(&detail);
    // `JsonRejection` 来自 Axum 且是 non_exhaustive 枚举，因此保留兜底分支。
    match rejection {
        JsonRejection::MissingJsonContentType(_) => {
            ReferenceApiError::invalid_request(request_id, "请求必须使用 application/json 内容类型")
        }
        JsonRejection::JsonSyntaxError(_) => {
            ReferenceApiError::invalid_request(request_id, format!("请求体不是合法 JSON：{detail}"))
        }
        JsonRejection::JsonDataError(_) => ReferenceApiError::invalid_request_with_fields(
            request_id,
            format!("请求体字段不合法：{detail}"),
            field_errors,
        ),
        JsonRejection::BytesRejection(_) => ReferenceApiError::invalid_request_with_fields(
            request_id,
            format!("无法读取请求体：{detail}"),
            field_errors,
        ),
        _ => ReferenceApiError::invalid_request_with_fields(
            request_id,
            format!("请求体不合法：{detail}"),
            field_errors,
        ),
    }
}

/// 查询参数拒绝：统一 422，并把 serde 能识别的字段名放进字段级错误。
fn query_rejection(request_id: &RequestId, rejection: QueryRejection) -> ReferenceApiError {
    let detail = rejection.body_text();
    ReferenceApiError::invalid_request_with_fields(
        request_id,
        format!("查询参数不合法：{detail}"),
        field_errors_from(&detail),
    )
}

/// 路径参数拒绝：统一 422，具体原因保留在消息里。
fn path_rejection(request_id: &RequestId, rejection: PathRejection) -> ReferenceApiError {
    ReferenceApiError::invalid_request(
        request_id,
        format!("路径参数不合法：{}", rejection.body_text()),
    )
}

/// serde 报出的字段问题种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldProblem {
    Missing,
    Unknown,
    Duplicate,
}

impl FieldProblem {
    /// 字段问题的中文说明。
    fn message(&self) -> &'static str {
        match self {
            Self::Missing => "缺少必填字段",
            Self::Unknown => "请求包含未知字段",
            Self::Duplicate => "字段重复出现",
        }
    }
}

/// 从 serde 错误文本中读出字段问题，例如 missing field 与 unknown field 两种前缀。
fn field_problem(detail: &str) -> Option<(FieldProblem, String)> {
    let prefixes = [
        ("missing field `", FieldProblem::Missing),
        ("unknown field `", FieldProblem::Unknown),
        ("duplicate field `", FieldProblem::Duplicate),
    ];
    for (prefix, problem) in prefixes {
        if let Some((_, rest)) = detail.split_once(prefix)
            && let Some((field, _)) = rest.split_once('`')
        {
            return Some((problem, field.to_string()));
        }
    }
    None
}

/// 把 serde 字段问题收敛成字段级错误；无法识别字段时返回空列表（消息里仍有原始说明）。
fn field_errors_from(detail: &str) -> Vec<ReferenceFieldError> {
    field_problem(detail)
        .map(|(problem, field)| ReferenceFieldError {
            field,
            message: problem.message().to_string(),
        })
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::rejection::MissingJsonContentType;
    use axum::http::{HeaderValue, StatusCode};
    use mai_protocol::ReferenceErrorCode;
    use pretty_assertions::assert_eq;

    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            REQUEST_ID_HEADER,
            HeaderValue::from_str(value).expect("请求头值"),
        );
        headers
    }

    #[test]
    fn request_id_reuses_client_header() {
        assert_eq!(
            "req-42",
            RequestId::from_headers(&headers_with("req-42")).value()
        );
    }

    #[test]
    fn request_id_is_generated_when_header_is_missing() {
        let generated = RequestId::from_headers(&HeaderMap::new());

        assert_eq!(36, generated.value().len(), "生成的 ID 是 UUID v4 字符串");
        assert_ne!(
            generated.value(),
            RequestId::from_headers(&HeaderMap::new()).value()
        );
    }

    #[test]
    fn request_id_replaces_unsafe_or_oversized_values() {
        let unsafe_value = RequestId::from_headers(&headers_with("bad id with space"));
        assert_ne!("bad id with space", unsafe_value.value());
        assert_eq!(36, unsafe_value.value().len());

        let oversized = "a".repeat(MAX_REQUEST_ID_CHARS + 1);
        let oversized_value = RequestId::from_headers(&headers_with(&oversized));
        assert_ne!(oversized, oversized_value.value());
        assert_eq!(36, oversized_value.value().len());
    }

    #[test]
    fn missing_content_type_maps_to_structured_invalid_request() {
        let error = json_rejection(
            &RequestId::from_headers(&HeaderMap::new()),
            JsonRejection::from(MissingJsonContentType::default()),
        );

        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, error.status);
        assert_eq!(ReferenceErrorCode::InvalidRequest, error.response.code);
        assert!(error.response.request_id.is_some());
        assert!(!error.response.retryable);
    }

    #[test]
    fn serde_field_messages_are_recognised() {
        assert_eq!(
            Some((FieldProblem::Missing, "bindings".to_string())),
            field_problem(
                "Failed to deserialize the JSON body into the target type: missing field `bindings` at line 1 column 20"
            )
        );
        assert_eq!(
            Some((FieldProblem::Unknown, "access".to_string())),
            field_problem("unknown field `access`, expected `expected_revision`")
        );
        assert_eq!(
            Some((FieldProblem::Duplicate, "bindings".to_string())),
            field_problem("duplicate field `bindings`")
        );
        assert_eq!(
            None,
            field_problem("invalid type: null, expected a sequence")
        );
    }

    #[test]
    fn field_errors_use_chinese_messages() {
        assert_eq!(
            vec![ReferenceFieldError {
                field: "bindings".to_string(),
                message: "缺少必填字段".to_string(),
            }],
            field_errors_from("missing field `bindings` at line 1 column 20")
        );
        assert!(field_errors_from("invalid type: null, expected a sequence").is_empty());
    }
}
