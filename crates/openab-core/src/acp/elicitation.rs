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

/// A cancellation signal for one pending elicitation (tokio-util is optional in this crate).
#[derive(Clone, Debug)]
pub struct CancelSignal(std::sync::Arc<tokio::sync::watch::Sender<bool>>);

impl Default for CancelSignal {
    fn default() -> Self {
        Self(std::sync::Arc::new(tokio::sync::watch::channel(false).0))
    }
}

impl CancelSignal {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once [`Self::cancel`] has been called (immediately if it already was).
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }
}

/// Default wait for the user's answer (ADR §2.5, Brett Q1: 10 minutes).
pub const ELICITATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// At most this many questions wait at once in one session (ADR §5.1 Q7).
pub const MAX_OPEN_ELICITATIONS: usize = 3;

/// Keep only a well-formed answer; anything else becomes `cancel`. Core checks the shape only
/// and never rewrites the user's content (ADR §2.3).
pub fn sanitize_elicitation_result(v: &Value) -> Value {
    match v.get("action").and_then(Value::as_str) {
        Some("accept") => match v.get("content") {
            None | Some(Value::Null) => json!({ "action": "accept", "content": {} }),
            Some(c @ Value::Object(_)) => json!({ "action": "accept", "content": c }),
            Some(_) => json!({ "action": "cancel" }),
        },
        Some("decline") => json!({ "action": "decline" }),
        _ => json!({ "action": "cancel" }),
    }
}

/// `params` with `_meta["openab/deadline"]` set, so the front-end can show a countdown.
fn with_deadline(mut params: Value, deadline: chrono::DateTime<chrono::Utc>) -> Value {
    if let Value::Object(map) = &mut params {
        let meta = map.entry("_meta").or_insert_with(|| json!({}));
        if !meta.is_object() {
            *meta = json!({});
        }
        meta["openab/deadline"] = json!(deadline.to_rfc3339());
    }
    params
}

/// The elicitations of one turn: forwards each `elicitation/create` to the chat adapter, owns its
/// timeout, and makes sure every one is answered exactly once (ADR §2.3).
pub struct ElicitationTurn {
    adapter: std::sync::Arc<dyn crate::adapter::ChatAdapter>,
    channel: crate::adapter::ChannelRef,
    responder: super::connection::ElicitationResponder,
    /// When the turn's hard timeout fires; no wait outlives it.
    turn_deadline: tokio::time::Instant,
    timeout: std::time::Duration,
    open: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, CancelSignal>>>,
    tasks: tokio::task::JoinSet<()>,
}

impl ElicitationTurn {
    pub fn new(
        adapter: std::sync::Arc<dyn crate::adapter::ChatAdapter>,
        channel: crate::adapter::ChannelRef,
        responder: super::connection::ElicitationResponder,
        turn_deadline: tokio::time::Instant,
    ) -> Self {
        Self {
            adapter,
            channel,
            responder,
            turn_deadline,
            timeout: ELICITATION_TIMEOUT,
            open: Default::default(),
            tasks: tokio::task::JoinSet::new(),
        }
    }

    fn key(id: &impl serde::Serialize) -> String {
        serde_json::to_string(id).unwrap_or_default()
    }

    pub fn handle(&mut self, event: super::connection::ElicitationEvent) {
        use super::connection::ElicitationEvent as E;
        match event {
            E::Create { agent_id, params } => self.start(agent_id, params),
            E::CancelRequest { request_id } => {
                if let Some(sig) = self
                    .open
                    .lock()
                    .expect("elicitation map poisoned")
                    .get(&Self::key(&request_id))
                {
                    sig.cancel();
                }
            }
            E::Complete { params } => {
                let adapter = self.adapter.clone();
                let channel = self.channel.clone();
                self.tasks.spawn(async move {
                    if let Err(e) = adapter.complete_elicitation(&channel, params).await {
                        tracing::debug!(error = %e, "elicitation/complete not delivered");
                    }
                });
            }
        }
    }

    fn start(&mut self, agent_id: super::protocol::JsonRpcId, params: Value) {
        let key = Self::key(&agent_id);
        let sig = CancelSignal::default();
        let admitted = {
            let mut open = self.open.lock().expect("elicitation map poisoned");
            if open.len() >= MAX_OPEN_ELICITATIONS || open.contains_key(&key) {
                false
            } else {
                open.insert(key.clone(), sig.clone());
                true
            }
        };
        let responder = self.responder.clone();
        if !admitted {
            tracing::info!("elicitation refused: too many open in this session; answering cancel");
            self.tasks.spawn(async move {
                responder
                    .reply(agent_id, super::connection::elicitation_cancel_result())
                    .await;
            });
            return;
        }
        let wait = self
            .timeout
            .min(self.turn_deadline.saturating_duration_since(tokio::time::Instant::now()));
        let deadline = chrono::Utc::now()
            + chrono::Duration::from_std(wait).unwrap_or_else(|_| chrono::Duration::zero());
        let params = with_deadline(params, deadline);
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("form")
            .to_string();
        let adapter = self.adapter.clone();
        let channel = self.channel.clone();
        let open = self.open.clone();
        self.tasks.spawn(async move {
            let started = tokio::time::Instant::now();
            let ask = adapter.request_elicitation(&channel, params, sig.clone());
            tokio::pin!(ask);
            let outcome = tokio::select! {
                r = &mut ask => r,
                _ = tokio::time::sleep(wait) => {
                    // Timeout: withdraw the question (the adapter sees the signal), then
                    // let it finish so the front-end closes the card.
                    sig.cancel();
                    ask.await
                }
            };
            let result = match outcome {
                Ok(v) if !sig.is_cancelled() => sanitize_elicitation_result(&v),
                _ => super::connection::elicitation_cancel_result(),
            };
            // Release the slot before answering, so a follow-up question the agent sends right
            // after this answer is not refused as "too many open" (openab#1533 finding).
            open.lock().expect("elicitation map poisoned").remove(&key);
            // Audit: no content, it may hold secrets (ADR §2.3).
            tracing::info!(
                mode = %mode,
                action = %result["action"].as_str().unwrap_or("cancel"),
                waited_ms = started.elapsed().as_millis() as u64,
                "elicitation settled"
            );
            responder.reply(agent_id, result).await;
        });
    }

    /// Turn end: withdraw every open question and wait (briefly) until each is answered
    /// `cancel`, before the caller clears the sink and sends the final message (ADR §2.3).
    pub async fn finish(mut self) {
        for sig in self.open.lock().expect("elicitation map poisoned").values() {
            sig.cancel();
        }
        let drain = async { while self.tasks.join_next().await.is_some() {} };
        if tokio::time::timeout(std::time::Duration::from_secs(5), drain)
            .await
            .is_err()
        {
            tracing::warn!("elicitation tasks still running at turn end; aborting them");
            self.tasks.abort_all();
        }
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

    #[test]
    fn results_are_shape_checked_not_rewritten() {
        let ok = json!({"action": "accept", "content": {"choice": "approve", "note": "x"}});
        assert_eq!(sanitize_elicitation_result(&ok), ok);
        assert_eq!(
            sanitize_elicitation_result(&json!({"action": "accept"})),
            json!({"action": "accept", "content": {}})
        );
        assert_eq!(
            sanitize_elicitation_result(&json!({"action": "decline", "content": {"a": 1}})),
            json!({"action": "decline"})
        );
        for bad in [
            json!({"action": "accept", "content": "nope"}),
            json!({"action": "maybe"}),
            json!({}),
            json!(null),
        ] {
            assert_eq!(sanitize_elicitation_result(&bad), json!({"action": "cancel"}));
        }
    }

    #[test]
    fn deadline_goes_into_meta_without_dropping_it() {
        let t = chrono::DateTime::parse_from_rfc3339("2026-11-03T16:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let p = with_deadline(json!({"mode": "form", "_meta": {"claudecode/serverName": "probe"}}), t);
        assert_eq!(p["_meta"]["claudecode/serverName"], json!("probe"));
        assert_eq!(p["_meta"]["openab/deadline"], json!("2026-11-03T16:00:00+00:00"));
        let p = with_deadline(json!({"mode": "form", "_meta": null}), t);
        assert_eq!(p["_meta"]["openab/deadline"], json!("2026-11-03T16:00:00+00:00"));
    }

    #[tokio::test]
    async fn cancel_signal_wakes_waiters() {
        let s = CancelSignal::default();
        assert!(!s.is_cancelled());
        let w = s.clone();
        let h = tokio::spawn(async move { w.cancelled().await });
        s.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), h)
            .await
            .expect("woken")
            .unwrap();
        assert!(s.is_cancelled());
        s.cancelled().await; // already cancelled: returns at once
    }

}
