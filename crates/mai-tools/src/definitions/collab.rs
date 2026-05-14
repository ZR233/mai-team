use serde_json::{Value, json};

pub(crate) fn collab_items_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string", "enum": ["text"] },
                        "text": { "type": "string" }
                    },
                    "required": ["type", "text"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string", "enum": ["skill"] },
                        "name": { "type": "string" },
                        "path": { "type": "string" }
                    },
                    "required": ["type"],
                    "anyOf": [
                        { "required": ["name"] },
                        { "required": ["path"] }
                    ],
                    "additionalProperties": false
                }
            ]
        }
    })
}
