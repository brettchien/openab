//! Per-thread, turn-boundary message batching (RFC #580 + ADR
//! "Batched Turn Packing for ACP session/prompt").
//!
//! Each active thread owns one bounded `mpsc::channel` and one consumer task.
//! - The first message after idle fires immediately (zero added latency).
//! - Messages arriving while the consumer is mid-turn accumulate in the channel.
//! - When the in-flight turn finishes, the consumer drains the channel and
//!   dispatches *one* ACP turn whose `Vec<ContentBlock>` is the concatenation
//!   of `pack_arrival_event(...)` for each buffered arrival event. Each
//!   arrival event keeps its own `<sender_context>` block followed by its own
//!   attachments, so attribution is recoverable by array adjacency
//!   (ADR §3 "Packing Format").
//! - When the channel is full, `submit` parks the calling task on
//!   `tx.send().await` until the consumer drains. Per-thread backpressure
//!   only — the platform event loop is unaffected because `submit` runs
//!   inside its own per-message `tokio::spawn`. ADR §2.1 rule 4 ("no
//!   suppression of empty-prompt turns") generalizes to "no silent drop";
//!   parking is the conservative choice.
//!
//! Wired only into the Discord adapter today; Slack hookup is a follow-up.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, warn};

use crate::acp::ContentBlock;
use crate::adapter::{pack_arrival_event, AdapterRouter, ChannelRef, ChatAdapter, MessageRef};

/// One queued user message awaiting dispatch.
///
/// `sender_json` already encodes the per-arrival `SenderContext` (including
/// its `timestamp` field), so the consumer can pack each event independently
/// without revisiting platform-specific wiring.
#[derive(Debug, Clone)]
pub struct BufferedMessage {
    /// Raw user prompt text (already pre-processed by the adapter:
    /// mentions stripped, voice transcribed, etc.).
    pub prompt: String,
    /// Non-text attachments (images, transcripts) carried alongside the prompt.
    /// Placed immediately after this event's `<sender_context>` block at
    /// dispatch time (ADR §3 — adjacency-based attribution).
    pub extra_blocks: Vec<ContentBlock>,
    /// Serialized `SenderContext` for this individual message.
    pub sender_json: String,
    /// Anchor for reactions. When a batch is dispatched, only the *trailing*
    /// message's ref is used so the bot reacts to the newest user message —
    /// they expect the reaction on what they just typed, not a 30s-old one
    /// that already looks "read". Older refs are dropped at consume time but
    /// kept on the struct so future per-message reaction streams remain an
    /// option (RFC #580 decision #3).
    pub trigger_msg: MessageRef,
    /// Enqueue instant. mpsc FIFO is already strict so this is redundant for
    /// ordering; kept for tests + future paranoid `sort_by_key` pass.
    #[allow(dead_code)]
    pub arrived_at: Instant,
}

struct ThreadHandle {
    tx: mpsc::Sender<BufferedMessage>,
    _consumer: tokio::task::JoinHandle<()>,
}

/// Routes incoming user messages through per-thread bounded channels and
/// dispatches them as batched ACP turns following the ADR packing format.
///
/// The adapter is supplied per-`submit` rather than stored, because the
/// Discord adapter is constructed inside serenity's `ready` callback (via
/// `OnceLock`) — well after the Dispatcher itself is built in `main.rs`.
/// Passing it per-call avoids that chicken-and-egg without forcing another
/// `OnceLock` here. In practice all submits for a given thread use the same
/// adapter Arc; only the first one (which creates the consumer) is captured
/// for that thread's lifetime.
pub struct Dispatcher {
    per_thread: Mutex<HashMap<String, ThreadHandle>>,
    router: Arc<AdapterRouter>,
    /// Per-thread mpsc channel cap. Reused as the inner `try_recv`-drain
    /// limit so a single ACP turn never receives more than `cap` events
    /// in one batch. Default in `config.rs` is 30 (ADR §7 — Phase 1
    /// `max_buffered_messages`).
    max_buffered_messages: usize,
}

impl Dispatcher {
    pub fn new(router: Arc<AdapterRouter>, max_buffered_messages: usize) -> Self {
        Self {
            per_thread: Mutex::new(HashMap::new()),
            router,
            max_buffered_messages: max_buffered_messages.max(1),
        }
    }

    /// Enqueue a message for the given `thread_key`.
    ///
    /// If the thread has no active consumer, one is spawned (capturing
    /// `adapter` and `other_bot_present` for that thread's lifetime).
    /// Then `tx.send().await` either succeeds immediately or parks the
    /// caller until the consumer drains.
    pub async fn submit(
        &self,
        thread_key: String,
        thread_channel: ChannelRef,
        adapter: Arc<dyn ChatAdapter>,
        msg: BufferedMessage,
        other_bot_present: bool,
    ) {
        // Pull these out of `self` before locking so the or_insert_with closure
        // doesn't try to re-borrow `self` while `self.per_thread` is locked.
        let cap = self.max_buffered_messages;
        let router = Arc::clone(&self.router);

        let tx = {
            let mut map = self.per_thread.lock().await;
            map.entry(thread_key.clone())
                .or_insert_with(|| {
                    let (tx, rx) = mpsc::channel::<BufferedMessage>(cap);
                    let consumer = tokio::spawn(consumer_loop(
                        thread_key.clone(),
                        thread_channel,
                        rx,
                        router,
                        adapter,
                        cap,
                        other_bot_present,
                    ));
                    ThreadHandle { tx, _consumer: consumer }
                })
                .tx
                .clone()
        };
        // dispatcher mutex released — held only to look up/insert the handle

        // send.await parks the calling task if the channel is full. Only this
        // submit future blocks; the platform event loop is unaffected because
        // submit runs inside its own per-message tokio::spawn'd task. Park
        // (not drop) is mandated by ADR §2.1 rule 4 — the broker must not
        // silently drop arrival events.
        if tx.send(msg).await.is_err() {
            warn!(thread_key, "consumer task has exited; message dropped");
        }
    }
}

async fn consumer_loop(
    thread_key: String,
    thread_channel: ChannelRef,
    mut rx: mpsc::Receiver<BufferedMessage>,
    router: Arc<AdapterRouter>,
    adapter: Arc<dyn ChatAdapter>,
    max_batch: usize,
    other_bot_present: bool,
) {
    debug!(thread_key, "dispatch consumer started");
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        // Greedy drain: pick up everything else that's already queued, up to cap.
        while batch.len() < max_batch {
            match rx.try_recv() {
                Ok(more) => batch.push(more),
                Err(_) => break,
            }
        }
        let batch_size = batch.len();

        // Pack each arrival event independently, then concatenate. The
        // resulting Vec<ContentBlock> carries N <sender_context> repetitions
        // with attachments interleaved in arrival order — ADR §3.3.
        let last_trigger = batch.last().expect("batch non-empty").trigger_msg.clone();
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        for m in batch {
            content_blocks.extend(pack_arrival_event(
                &m.sender_json,
                &m.prompt,
                m.extra_blocks,
            ));
        }

        debug!(
            thread_key,
            batch_size,
            block_count = content_blocks.len(),
            "dispatching batch"
        );

        if let Err(e) = router
            .dispatch_batch(
                &adapter,
                &thread_key,
                &thread_channel,
                content_blocks,
                &last_trigger,
                other_bot_present,
            )
            .await
        {
            error!(thread_key, error = %e, "dispatch_batch failed");
        }
    }
    debug!(thread_key, "dispatch consumer exited");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::ContentBlock;
    use crate::adapter::{ChannelRef, MessageRef};

    fn dummy_channel() -> ChannelRef {
        ChannelRef {
            platform: "discord".into(),
            channel_id: "C1".into(),
            thread_id: None,
            parent_id: None,
        }
    }

    fn make(prompt: &str, sender_id: &str, ts: &str, mid: &str) -> BufferedMessage {
        let sender_json = format!(
            r#"{{"schema":"openab.sender.v1","sender_id":"{sender_id}","sender_name":"{sender_id}","display_name":"{sender_id}","channel":"discord","channel_id":"C1","is_bot":false,"timestamp":"{ts}"}}"#
        );
        BufferedMessage {
            prompt: prompt.into(),
            extra_blocks: vec![],
            sender_json,
            trigger_msg: MessageRef {
                channel: dummy_channel(),
                message_id: mid.into(),
            },
            arrived_at: Instant::now(),
        }
    }

    /// One arrival event packs into one Text block carrying its full
    /// <sender_context> + prompt. No banner, no wrapper tag.
    #[test]
    fn pack_single_event_yields_one_text_block() {
        let m = make("hello", "alice", "2026-04-27T00:00:00Z", "1");
        let blocks = pack_arrival_event(&m.sender_json, &m.prompt, m.extra_blocks);
        assert_eq!(blocks.len(), 1);
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected Text block");
        };
        assert!(text.contains("<sender_context>"));
        assert!(text.contains(r#""sender_id":"alice""#));
        assert!(text.contains(r#""timestamp":"2026-04-27T00:00:00Z""#));
        assert!(text.ends_with("</sender_context>\n\nhello"));
    }

    /// Two events concatenated: each gets its own <sender_context>; no
    /// banner string between them; attachments interleave in arrival order.
    #[test]
    fn concatenated_events_keep_per_event_sender_context() {
        let mut m1 = make("look at this", "alice", "2026-04-27T00:00:01Z", "1");
        m1.extra_blocks.push(ContentBlock::Text {
            text: "[ImageBlock-screenshot]".into(),
        });
        let m2 = make("listen to this", "alice", "2026-04-27T00:00:02Z", "2");

        let mut blocks: Vec<ContentBlock> = Vec::new();
        blocks.extend(pack_arrival_event(&m1.sender_json, &m1.prompt, m1.extra_blocks));
        blocks.extend(pack_arrival_event(&m2.sender_json, &m2.prompt, m2.extra_blocks));

        // [text(M1), text(image), text(M2)]
        assert_eq!(blocks.len(), 3);
        let texts: Vec<&str> = blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.as_str(),
                _ => panic!("expected text-only test fixtures"),
            })
            .collect();

        assert!(texts[0].contains(r#""timestamp":"2026-04-27T00:00:01Z""#));
        assert!(texts[0].ends_with("</sender_context>\n\nlook at this"));
        assert_eq!(texts[1], "[ImageBlock-screenshot]");
        assert!(texts[2].contains(r#""timestamp":"2026-04-27T00:00:02Z""#));
        assert!(texts[2].ends_with("</sender_context>\n\nlisten to this"));
        // No banner anywhere.
        assert!(!texts.iter().any(|t| t.contains("[Batched:")));
        // No <message index=...> wrapper.
        assert!(!texts.iter().any(|t| t.contains("<message index=")));
    }

    /// Voice-only / attachment-only arrival events still emit their own
    /// <sender_context> with empty prompt (ADR Compliance §8 "no suppression
    /// of empty-prompt turns").
    #[test]
    fn empty_prompt_event_still_dispatches_sender_context() {
        let mut m = make("", "alice", "2026-04-27T00:00:03Z", "3");
        m.extra_blocks.push(ContentBlock::Text {
            text: "[Voice message transcript]: hi".into(),
        });
        let blocks = pack_arrival_event(&m.sender_json, &m.prompt, m.extra_blocks);
        assert_eq!(blocks.len(), 2);
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected Text block");
        };
        assert!(text.contains("<sender_context>"));
        assert!(text.ends_with("</sender_context>\n\n"));
    }

    /// Greedy drain coalesces messages already in the channel up to `max_batch`,
    /// leaving the remainder for the next loop iteration (mpsc FIFO order).
    #[tokio::test]
    async fn drain_caps_at_max_buffered() {
        let max = 3;
        let (tx, mut rx) = mpsc::channel::<u32>(16);
        for i in 0..5 {
            tx.send(i).await.unwrap();
        }
        let first = rx.recv().await.unwrap();
        let mut batch = vec![first];
        while batch.len() < max {
            match rx.try_recv() {
                Ok(v) => batch.push(v),
                Err(_) => break,
            }
        }
        assert_eq!(batch, vec![0, 1, 2]);
        assert_eq!(rx.try_recv().unwrap(), 3);
        assert_eq!(rx.try_recv().unwrap(), 4);
    }
}
