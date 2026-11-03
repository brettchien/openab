//! ACP elicitation support (Katashiro ADR `docs/adr/mcp-elicitation.md`).
//!
//! OpenAB is the ACP *client* of the agent. When the front-end attached to a session can render
//! an elicitation (Katashiro over the gateway's `/acp`), core declares the same modes in the
//! agent's `initialize`; the agent then sends `elicitation/create` instead of auto-declining.

use serde_json::{json, Value};

/// The elicitation modes declared to the agent for one session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ElicitationModes {
    pub form: bool,
    pub url: bool,
}

impl ElicitationModes {
    /// Read the modes from a gateway event's `client_capabilities`
    /// (`{"elicitation": {"form": {}, "url": {}}}`). Only object-valued modes count.
    pub fn from_client_capabilities(caps: Option<&Value>) -> Self {
        let Some(e) = caps
            .and_then(|c| c.get("elicitation"))
            .and_then(Value::as_object)
        else {
            return Self::default();
        };
        Self {
            form: e.get("form").is_some_and(Value::is_object),
            url: e.get("url").is_some_and(Value::is_object),
        }
    }

    pub fn any(self) -> bool {
        self.form || self.url
    }

    /// `clientCapabilities` for the agent's `initialize`: `{}` when no mode is declared, so a
    /// session without a capable front-end keeps today's behaviour (the SDK auto-declines).
    pub fn client_capabilities(self) -> Value {
        if !self.any() {
            return json!({});
        }
        let mut e = serde_json::Map::new();
        if self.form {
            e.insert("form".into(), json!({}));
        }
        if self.url {
            e.insert("url".into(), json!({}));
        }
        json!({ "elicitation": e })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_declared_modes() {
        let c = json!({"elicitation": {"form": {}}});
        assert_eq!(
            ElicitationModes::from_client_capabilities(Some(&c)),
            ElicitationModes {
                form: true,
                url: false
            }
        );
        let c = json!({"elicitation": {"form": {}, "url": {}}});
        assert_eq!(
            ElicitationModes::from_client_capabilities(Some(&c)),
            ElicitationModes {
                form: true,
                url: true
            }
        );
    }

    #[test]
    fn missing_or_malformed_is_none() {
        assert!(!ElicitationModes::from_client_capabilities(None).any());
        assert!(!ElicitationModes::from_client_capabilities(Some(&json!({}))).any());
        let c = json!({"elicitation": {"form": true}});
        assert!(!ElicitationModes::from_client_capabilities(Some(&c)).any());
    }

    #[test]
    fn initialize_capabilities_mirror_modes() {
        assert_eq!(ElicitationModes::default().client_capabilities(), json!({}));
        let m = ElicitationModes {
            form: true,
            url: false,
        };
        assert_eq!(
            m.client_capabilities(),
            json!({"elicitation": {"form": {}}})
        );
        let m = ElicitationModes {
            form: true,
            url: true,
        };
        assert_eq!(
            m.client_capabilities(),
            json!({"elicitation": {"form": {}, "url": {}}})
        );
    }
}
