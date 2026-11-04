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

    // --- ElicitationTurn (ADR §2.3 / §4.2): every question is answered exactly once ---

    use crate::acp::connection::{ElicitationEvent, ElicitationResponder};
    use crate::acp::protocol::JsonRpcId;
    use crate::adapter::{ChannelRef, ChatAdapter, MessageRef};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    /// How the fake front-end answers.
    enum Answer {
        Now(Value),
        Fail,
        /// Waits until the question is withdrawn, then reports it gone.
        UntilWithdrawn,
    }

    struct AskAdapter {
        answer: Answer,
        asked: StdMutex<Vec<Value>>,
        withdrawn: AtomicUsize,
        completed: StdMutex<Vec<Value>>,
    }

    impl AskAdapter {
        fn new(answer: Answer) -> Arc<Self> {
            Arc::new(Self {
                answer,
                asked: StdMutex::new(Vec::new()),
                withdrawn: AtomicUsize::new(0),
                completed: StdMutex::new(Vec::new()),
            })
        }
        fn asked(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl ChatAdapter for AskAdapter {
        fn platform(&self) -> &'static str {
            "unified"
        }
        fn message_limit(&self) -> usize {
            4096
        }
        async fn send_message(&self, channel: &ChannelRef, _: &str) -> anyhow::Result<MessageRef> {
            Ok(MessageRef {
                channel: channel.clone(),
                message_id: "m".into(),
            })
        }
        async fn create_thread(
            &self,
            c: &ChannelRef,
            _: &MessageRef,
            _: &str,
        ) -> anyhow::Result<ChannelRef> {
            Ok(c.clone())
        }
        async fn add_reaction(&self, _: &MessageRef, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn remove_reaction(&self, _: &MessageRef, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn use_streaming(&self, _: bool) -> bool {
            false
        }
        async fn request_elicitation(
            &self,
            _: &ChannelRef,
            params: Value,
            cancel: CancelSignal,
        ) -> anyhow::Result<Value> {
            self.asked.lock().unwrap().push(params);
            match &self.answer {
                Answer::Now(v) => Ok(v.clone()),
                Answer::Fail => Err(anyhow::anyhow!("front-end unreachable")),
                Answer::UntilWithdrawn => {
                    cancel.cancelled().await;
                    self.withdrawn.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!("withdrawn"))
                }
            }
        }
        async fn complete_elicitation(&self, _: &ChannelRef, params: Value) -> anyhow::Result<()> {
            self.completed.lock().unwrap().push(params);
            Ok(())
        }
    }

    fn channel() -> ChannelRef {
        ChannelRef {
            platform: "acp".into(),
            channel_id: "acp_test".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_1".into()),
        }
    }

    fn turn(
        adapter: Arc<AskAdapter>,
        turn_left: Duration,
    ) -> (ElicitationTurn, tokio::sync::mpsc::UnboundedReceiver<(JsonRpcId, Value)>) {
        let (responder, answers) = ElicitationResponder::recorded();
        let t = ElicitationTurn::new(
            adapter,
            channel(),
            responder,
            tokio::time::Instant::now() + turn_left,
        );
        (t, answers)
    }

    fn create(id: u64) -> ElicitationEvent {
        ElicitationEvent::Create {
            agent_id: JsonRpcId::Unsigned(id),
            params: json!({"mode": "form", "message": "approve?",
                           "requestedSchema": {"type": "object", "properties": {}},
                           "_meta": {"claudecode/serverName": "probe"}}),
        }
    }

    async fn next_answer(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<(JsonRpcId, Value)>,
    ) -> (JsonRpcId, Value) {
        tokio::time::timeout(Duration::from_secs(3600), rx.recv())
            .await
            .expect("an answer")
            .expect("responder alive")
    }

    #[tokio::test]
    async fn an_accepted_answer_reaches_the_agent_with_the_deadline_shown() {
        let ok = json!({"action": "accept", "content": {"choice": "approve"}});
        let a = AskAdapter::new(Answer::Now(ok.clone()));
        let (mut t, mut answers) = turn(a.clone(), Duration::from_secs(1800));
        t.handle(create(7));
        assert_eq!(next_answer(&mut answers).await, (JsonRpcId::Unsigned(7), ok));
        let asked = a.asked.lock().unwrap()[0].clone();
        assert_eq!(asked["_meta"]["claudecode/serverName"], json!("probe"));
        assert!(asked["_meta"]["openab/deadline"].is_string(), "{asked}");
        t.finish().await;
        assert!(answers.try_recv().is_err(), "answered exactly once");
    }

    #[tokio::test]
    async fn a_malformed_answer_or_an_adapter_error_is_cancel() {
        for answer in [Answer::Now(json!({"action": "maybe"})), Answer::Fail] {
            let (mut t, mut answers) = turn(AskAdapter::new(answer), Duration::from_secs(1800));
            t.handle(create(1));
            assert_eq!(next_answer(&mut answers).await.1, json!({"action": "cancel"}));
            t.finish().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_question_times_out_as_cancel_and_is_withdrawn() {
        let a = AskAdapter::new(Answer::UntilWithdrawn);
        let (mut t, mut answers) = turn(a.clone(), Duration::from_secs(1800));
        let started = tokio::time::Instant::now();
        t.handle(create(1));
        assert_eq!(next_answer(&mut answers).await.1, json!({"action": "cancel"}));
        assert_eq!(started.elapsed(), ELICITATION_TIMEOUT, "the 10-minute wait");
        assert_eq!(a.withdrawn.load(Ordering::SeqCst), 1, "the card is closed");
        t.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_never_outlives_the_turn() {
        let a = AskAdapter::new(Answer::UntilWithdrawn);
        let (mut t, mut answers) = turn(a, Duration::from_secs(90));
        let started = tokio::time::Instant::now();
        t.handle(create(1));
        assert_eq!(next_answer(&mut answers).await.1, json!({"action": "cancel"}));
        assert_eq!(started.elapsed(), Duration::from_secs(90), "clamped to what is left of the turn");
        t.finish().await;
    }

    #[tokio::test]
    async fn the_agent_cancelling_withdraws_the_question() {
        let a = AskAdapter::new(Answer::UntilWithdrawn);
        let (mut t, mut answers) = turn(a.clone(), Duration::from_secs(1800));
        t.handle(create(4));
        while a.asked() == 0 {
            tokio::task::yield_now().await;
        }
        t.handle(ElicitationEvent::CancelRequest { request_id: json!(4) });
        assert_eq!(next_answer(&mut answers).await, (JsonRpcId::Unsigned(4), json!({"action": "cancel"})));
        assert_eq!(a.withdrawn.load(Ordering::SeqCst), 1);
        t.finish().await;
    }

    // ADR §5.1 Q7: at most 3 wait at once; the 4th is answered cancel without asking, and a
    // settled question frees its slot for the next one.
    #[tokio::test]
    async fn at_most_three_open_and_a_settled_one_frees_its_slot() {
        let a = AskAdapter::new(Answer::UntilWithdrawn);
        let (mut t, mut answers) = turn(a.clone(), Duration::from_secs(1800));
        for id in 1..=4 {
            t.handle(create(id));
        }
        assert_eq!(next_answer(&mut answers).await, (JsonRpcId::Unsigned(4), json!({"action": "cancel"})));
        while a.asked() < 3 {
            tokio::task::yield_now().await;
        }
        t.handle(ElicitationEvent::CancelRequest { request_id: json!(1) });
        assert_eq!(next_answer(&mut answers).await.0, JsonRpcId::Unsigned(1));
        t.handle(create(5));
        while a.asked() < 4 {
            tokio::task::yield_now().await;
        }
        assert!(answers.try_recv().is_err(), "the 5th was admitted, not refused");
        t.finish().await;
    }

    #[tokio::test]
    async fn a_duplicate_id_is_refused() {
        let a = AskAdapter::new(Answer::UntilWithdrawn);
        let (mut t, mut answers) = turn(a.clone(), Duration::from_secs(1800));
        t.handle(create(1));
        t.handle(create(1));
        assert_eq!(next_answer(&mut answers).await, (JsonRpcId::Unsigned(1), json!({"action": "cancel"})));
        t.finish().await;
        assert_eq!(a.asked(), 1, "asked once");
        assert_eq!(next_answer(&mut answers).await.0, JsonRpcId::Unsigned(1), "the original, at turn end");
    }

    #[tokio::test]
    async fn turn_end_answers_every_open_question_cancel() {
        let a = AskAdapter::new(Answer::UntilWithdrawn);
        let (mut t, mut answers) = turn(a.clone(), Duration::from_secs(1800));
        t.handle(create(1));
        t.handle(create(2));
        while a.asked() < 2 {
            tokio::task::yield_now().await;
        }
        t.finish().await;
        let mut ids = vec![answers.try_recv().unwrap(), answers.try_recv().unwrap()];
        ids.sort_by_key(|(id, _)| id.as_u64());
        assert_eq!(
            ids,
            vec![
                (JsonRpcId::Unsigned(1), json!({"action": "cancel"})),
                (JsonRpcId::Unsigned(2), json!({"action": "cancel"})),
            ]
        );
        assert_eq!(a.withdrawn.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn complete_is_forwarded_to_the_adapter() {
        let a = AskAdapter::new(Answer::Fail);
        let (mut t, _answers) = turn(a.clone(), Duration::from_secs(1800));
        t.handle(ElicitationEvent::Complete { params: json!({"elicitationId": "e1"}) });
        t.finish().await;
        assert_eq!(*a.completed.lock().unwrap(), vec![json!({"elicitationId": "e1"})]);
    }
}
