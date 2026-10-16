//! 参考源领域错误：稳定机器码 + 中文人类消息，可选字段路径与底层原因。

use mai_protocol::{ReferenceErrorCode, ReferenceErrorResponse, ReferenceFieldError};

/// 领域层统一结果类型。
pub type Result<T> = std::result::Result<T, ReferenceError>;

/// 参考源领域错误。
///
/// `code` 是接口契约里的稳定机器码，`message` 是给管理员看的中文说明；两者不能互相
/// 替代。字段错误用于 422 场景，指出请求体里的具体字段。
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ReferenceError {
    code: ReferenceErrorCode,
    message: String,
    field: Option<String>,
    #[source]
    cause: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl ReferenceError {
    /// 构造一个只有机器码与中文消息的错误。
    pub fn new(code: ReferenceErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            field: None,
            cause: None,
        }
    }

    /// 附带字段路径，用于请求体字段级错误。
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// 附带底层原因；底层原因只进日志，不进对外消息。
    pub fn with_cause(mut self, cause: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    /// 包装 `std::io::Error`：上下文保留在中文消息里。
    pub fn from_io(
        code: ReferenceErrorCode,
        message: impl Into<String>,
        cause: std::io::Error,
    ) -> Self {
        Self::new(code, message).with_cause(cause)
    }

    /// 稳定机器码。
    pub fn code(&self) -> ReferenceErrorCode {
        self.code
    }

    /// 中文人类消息。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 字段级错误；没有字段信息时为 `None`。
    pub fn field_error(&self) -> Option<ReferenceFieldError> {
        self.field.as_ref().map(|field| ReferenceFieldError {
            field: field.clone(),
            message: self.message.clone(),
        })
    }

    /// 建议的 HTTP 状态码。
    pub fn http_status(&self) -> u16 {
        self.code.http_status()
    }

    /// 同条件重试是否可能成功。
    pub fn retryable(&self) -> bool {
        self.code.is_retryable()
    }

    /// 是否属于某个机器码。
    pub fn is_code(&self, code: ReferenceErrorCode) -> bool {
        self.code == code
    }

    /// 转成统一错误响应。
    pub fn to_response(&self, request_id: Option<String>) -> ReferenceErrorResponse {
        ReferenceErrorResponse {
            code: self.code,
            message: self.message.clone(),
            field_errors: self.field_error().into_iter().collect(),
            request_id,
            retryable: self.retryable(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn field_error_is_exposed_in_the_response() {
        let error = ReferenceError::new(ReferenceErrorCode::InvalidAlias, "alias 不符合规则")
            .with_field("bindings[0].alias");

        assert_eq!(422, error.http_status());
        assert!(!error.retryable());
        assert_eq!(
            Some(ReferenceFieldError {
                field: "bindings[0].alias".to_string(),
                message: "alias 不符合规则".to_string(),
            }),
            error.field_error()
        );

        let response = error.to_response(Some("req-9".to_string()));
        assert_eq!(ReferenceErrorCode::InvalidAlias, response.code);
        assert_eq!(1, response.field_errors.len());
        assert_eq!(Some("req-9".to_string()), response.request_id);
    }

    #[test]
    fn error_without_field_has_no_field_errors() {
        let error = ReferenceError::new(ReferenceErrorCode::UnsafeSymlink, "符号链接逃出源码根");
        assert!(error.field_error().is_none());
        assert!(error.to_response(None).field_errors.is_empty());
        assert!(error.is_code(ReferenceErrorCode::UnsafeSymlink));
    }

    #[test]
    fn io_errors_are_wrapped_with_context() {
        let error = ReferenceError::from_io(
            ReferenceErrorCode::SourceUnreadable,
            "无法读取源目录",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        assert_eq!(ReferenceErrorCode::SourceUnreadable, error.code());
        assert!(std::error::Error::source(&error).is_some());
    }
}
