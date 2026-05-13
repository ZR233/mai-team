use serde_json::{Value, json};

pub(crate) fn object_schema(fields: Vec<(&str, Value, bool)>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, schema, is_required) in fields {
        properties.insert(name.to_string(), schema);
        if is_required {
            required.push(Value::String(name.to_string()));
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}
