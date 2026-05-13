use crate::*;

pub(crate) fn session_context_tokens_key(agent_id: AgentId, session_id: SessionId) -> String {
    format!("session_context_tokens:{agent_id}:{session_id}")
}

pub(crate) fn parse_agent_id(value: &str) -> Result<AgentId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid agent id `{value}`: {err}")))
}

pub(crate) fn parse_task_id(value: &str) -> Result<TaskId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid task id `{value}`: {err}")))
}

pub(crate) fn parse_project_id(value: &str) -> Result<ProjectId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid project id `{value}`: {err}")))
}

pub(crate) fn parse_session_id(value: &str) -> Result<SessionId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid session id `{value}`: {err}")))
}

pub(crate) fn parse_turn_id(value: &str) -> Result<TurnId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid turn id `{value}`: {err}")))
}

pub(crate) fn parse_uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid uuid `{value}`: {err}")))
}

pub(crate) fn parse_utc(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

pub(crate) fn parse_store_enum<T>(value: &str) -> Result<T>
where
    T: FromStr<Err = strum::ParseError>,
{
    if let Ok(parsed) = value.parse() {
        return Ok(parsed);
    }
    let mut normalized = String::with_capacity(value.len() + 4);
    for (index, ch) in value.char_indices() {
        if ch.is_ascii_uppercase() {
            if index > 0 && !normalized.ends_with('_') {
                normalized.push('_');
            }
            normalized.push(ch.to_ascii_lowercase());
        } else if ch == '-' {
            normalized.push('_');
        } else {
            normalized.push(ch);
        }
    }
    Ok(normalized.parse()?)
}

pub(crate) fn u64_to_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

pub(crate) fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}
