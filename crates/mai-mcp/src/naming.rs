pub fn model_tool_name(server: &str, tool: &str) -> String {
    let base = format!("mcp__{}__{}", sanitize_name(server), sanitize_name(tool));
    if base.len() <= 64 {
        return base;
    }
    let hash = fnv1a_hex(&base);
    let keep = 64usize.saturating_sub(hash.len() + 2);
    format!("{}__{}", &base[..keep], hash)
}

pub(crate) fn sanitize_name(value: &str) -> String {
    let mut out = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if out.is_empty() {
        out = "tool".to_string();
    }
    if out.as_bytes()[0].is_ascii_digit() {
        out.insert(0, '_');
    }
    out
}

pub(crate) fn fnv1a_hex(value: &str) -> String {
    use fnv::FnvHasher;
    use std::hash::Hasher;
    let mut hasher = FnvHasher::default();
    hasher.write(value.as_bytes());
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_tool_names_are_sanitized() {
        assert_eq!(
            model_tool_name("fs.server", "read file"),
            "mcp__fs_server__read_file"
        );
        assert!(model_tool_name("1", "2").starts_with("mcp___1___2"));
    }

    #[test]
    fn long_model_tool_names_are_limited() {
        let name = model_tool_name(&"a".repeat(80), &"b".repeat(80));
        assert!(name.len() <= 64);
    }
}
