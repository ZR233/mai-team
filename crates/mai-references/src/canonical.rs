//! 规范 JSON 片段写入：摘要与指纹必须逐字节可复现，因此不依赖 map 迭代顺序。
//!
//! 转义规则与 `json.dumps(..., ensure_ascii=false)` / `serde_json` 一致：只转义
//! 引号、反斜杠与 C0 控制字符，非 ASCII 字符按 UTF-8 原样输出。发布器的
//! `manifest_digest` 由 Python 计算，服务端必须逐字节复现同一规范形。

/// 追加一个 JSON 字符串字面量。
pub(crate) fn quote_into(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // 与 serde_json 一致：只把 C0 控制字符写成 \u00xx。
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

/// 返回一个 JSON 字符串字面量。
pub(crate) fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    quote_into(&mut out, value);
    out
}

/// 把对象字段拼成 `"key":value` 片段；调用方按字典序提供字段以保证规范形。
pub(crate) fn field(key: &str, value: &str) -> String {
    let mut out = String::with_capacity(key.len() + value.len() + 4);
    out.push_str(&quote(key));
    out.push(':');
    out.push_str(value);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn quoting_matches_serde_json_for_regular_text() {
        for value in [
            "plain",
            "字节.bin",
            "tab\there",
            "line\nbreak",
            "quote\"slash\\",
            "控制\u{0001}字符",
        ] {
            let expected = serde_json::to_string(value).expect("serde 编码");
            assert_eq!(expected, quote(value));
        }
    }

    #[test]
    fn field_fragment_is_canonical() {
        assert_eq!(
            json!({ "path": "a/b", "size": 3 })
                .to_string()
                .replace(' ', ""),
            format!(
                "{{{},{}}}",
                field("path", &quote("a/b")),
                field("size", "3")
            )
        );
    }
}
