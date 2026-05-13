use crate::error::RelayResult;
use crate::state::AppState;
use hmac::{Hmac, Mac};
use mai_protocol::{RelayAck, RelayAckStatus, RelayEvent, RelayEventKind};
use serde_json::Value;
use sha2::Sha256;
use tracing::warn;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub(crate) struct QueuedDelivery {
    pub(crate) sequence: u64,
    pub(crate) delivery_id: String,
    pub(crate) event_name: String,
    pub(crate) payload: Value,
}

impl QueuedDelivery {
    pub(crate) fn into_event(self) -> RelayEvent {
        RelayEvent {
            sequence: self.sequence,
            delivery_id: self.delivery_id,
            kind: RelayEventKind::from_github_event(&self.event_name),
            payload: self.payload,
        }
    }
}

pub(crate) async fn handle_ack(state: &AppState, ack: RelayAck) -> RelayResult<()> {
    match ack.status {
        RelayAckStatus::Processed | RelayAckStatus::Ignored => {
            state.store.ack_delivery(&ack.delivery_id)?;
        }
        RelayAckStatus::Failed => {
            warn!(
                delivery_id = %ack.delivery_id,
                message = ack.message.as_deref().unwrap_or(""),
                "relay client failed delivery"
            );
        }
    }
    Ok(())
}

pub(crate) fn verify_signature(secret: &str, body: &[u8], signature: &str) -> bool {
    let Some(hex) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = decode_hex(hex) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

fn decode_hex(value: &str) -> std::result::Result<Vec<u8>, ()> {
    if !value.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks(2) {
        let high = hex_value(chunk[0])?;
        let low = hex_value(chunk[1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_value(byte: u8) -> std::result::Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_validation_accepts_expected_signature() {
        let secret = "secret";
        let body = br#"{"ok":true}"#;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac");
        mac.update(body);
        let signature = format!("sha256={}", hex_encode(&mac.finalize().into_bytes()));

        assert!(verify_signature(secret, body, &signature));
        assert!(!verify_signature(secret, body, "sha256=00"));
        assert!(!verify_signature(secret, body, ""));
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
