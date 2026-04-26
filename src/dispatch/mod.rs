//! Per-thread, turn-boundary message batching.
//!
//! Implements the mechanism described in
//! `drafts/rfc-turn-boundary-message-batching.md`:
//!
//! - Each active thread owns one bounded `mpsc::channel` and one consumer task.
//! - The first message after idle fires immediately (zero added latency).
//! - Messages arriving while the consumer is mid-turn accumulate in the channel.
//! - When the in-flight turn finishes, the consumer drains the channel and
//!   dispatches *one* ACP turn carrying all accumulated messages, merged into
//!   a single combined prompt.
//! - When the channel is full, `submit` parks the calling task on `tx.send().await`
//!   until the consumer drains. Per-thread backpressure only — the platform event
//!   loop is unaffected because `submit` runs inside its own `tokio::spawn`'d task.
//!
//! Single-message batches (`batch.len() == 1`) are dispatched verbatim, so
//! `Batched` mode is identical to `PerMessage` mode for isolated input.
//!
//! Currently wired only into the Discord adapter; Slack hookup is a follow-up.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, warn};

use crate::acp::ContentBlock;
use crate::adapter::{AdapterRouter, ChannelRef, ChatAdapter, MessageRef};

/// One queued user message awaiting dispatch.
#[derive(Debug, Clone)]
pub struct BufferedMessage {
    /// The raw user prompt text (already pre-processed by the adapter:
    /// mentions stripped, voice transcribed, etc.).
    pub prompt: String,
    /// Non-text attachments (images, transcripts) carried alongside the prompt.
    pub extra_blocks: Vec<ContentBlock>,
    /// Serialized `SenderContext` for this individual message.
    pub sender_json: String,
    /// The message reference used to anchor reactions. When a batch is merged,
    /// only the *last* message's ref is used (so the reaction lands on the
    /// newest message the user sent — they expect to see the bot react to
    /// what they just typed, not to a message they sent 30s ago that already
    /// looks "read"). Older messages in a batch don't get their own reaction
    /// stream (RFC decision #3) — refs on non-trailing messages are dropped
    /// at merge time but kept on the struct so future iterations can opt
    /// in to per-message reactions if desired.
    pub trigger_msg: MessageRef,
    /// When this message was enqueued. Currently only consumed by tests; kept
    /// on the struct so future iterations can do paranoid `sort_by_key` after
    /// `try_recv`-greedy-drain (RFC §"Deterministic ordering"). mpsc FIFO is
    /// already strict, so this is overkill but cheap to carry.
    #[allow(dead_code)]
    pub arrived_at: Instant,
    /// Sender display name, for inline labelling inside merged batched prompts.
    pub sender_name: String,
}

/// Per-thread runtime state held inside the Dispatcher.
///
/// `tx` is the producer side of the per-thread channel; submitters clone it.
/// `_consumer` keeps the consumer task alive for the lifetime of the handle —
/// dropping the handle closes `tx`, which terminates the consumer's `recv`
/// loop and lets the spawned future complete.
struct ThreadHandle {
    tx: mpsc::Sender<BufferedMessage>,
    _consumer: tokio::task::JoinHandle<()>,
}

/// Routes incoming user messages through per-thread bounded channels and
/// merges them at turn boundaries.
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
    /// Capacity of each per-thread mpsc channel. Same value reused for the
    /// inner `try_recv` drain limit (one in-flight ACP turn never receives
    /// more than `cap` messages in a single batch).
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

        // send.await parks the calling task if the channel is full.
        // Only this submit future blocks; the platform event loop is unaffected
        // because submit runs inside its own per-message tokio::spawn'd task.
        if tx.send(msg).await.is_err() {
            warn!(thread_key, "consumer task has exited; message dropped");
        }
    }
}

/// Single long-lived task per active thread. Pulls messages from `rx`,
/// greedy-drains additional pending messages up to `max_batch`, then dispatches
/// the batch as one ACP turn via the router.
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
        debug!(thread_key, batch_size, "dispatching batch");
        if let Err(e) = dispatch_batch(
            &router,
            &adapter,
            &thread_channel,
            batch,
            other_bot_present,
        ).await {
            error!(thread_key, error = %e, "dispatch_batch failed");
        }
    }
    debug!(thread_key, "dispatch consumer exited");
}

/// Convert a batch into a single `router.handle_message` call.
///
/// - `batch.len() == 1`: identical to per-message dispatch (verbatim params).
/// - `batch.len() > 1`: build a combined prompt with per-message tags so the
///   ACP agent sees each sub-message's sender + body. The leading message
///   provides the sender_context wrapper, but the *trailing* message provides
///   the trigger_msg used for reactions — see `BufferedMessage::trigger_msg`
///   for the UX rationale. `extra_blocks` from all sub-messages are
///   concatenated in order.
async fn dispatch_batch(
    router: &Arc<AdapterRouter>,
    adapter: &Arc<dyn ChatAdapter>,
    thread_channel: &ChannelRef,
    batch: Vec<BufferedMessage>,
    other_bot_present: bool,
) -> anyhow::Result<()> {
    let (prompt, sender_json, trigger_msg, extra_blocks) = merge_batch(batch);
    router
        .handle_message(
            adapter,
            thread_channel,
            &sender_json,
            &prompt,
            extra_blocks,
            &trigger_msg,
            other_bot_present,
        )
        .await
}

/// Merge a batch into the four fields needed by `AdapterRouter::handle_message`.
///
/// Pure function, no I/O, so it's directly unit-testable.
pub(crate) fn merge_batch(
    mut batch: Vec<BufferedMessage>,
) -> (String, String, MessageRef, Vec<ContentBlock>) {
    debug_assert!(!batch.is_empty(), "merge_batch called with empty batch");

    if batch.len() == 1 {
        let m = batch.pop().unwrap();
        return (m.prompt, m.sender_json, m.trigger_msg, m.extra_blocks);
    }

    let n = batch.len();
    let lead_sender_json = batch[0].sender_json.clone();
    // Reactions anchor on the *trailing* message — see BufferedMessage::trigger_msg.
    let trailing_trigger = batch.last().unwrap().trigger_msg.clone();

    let mut combined = String::new();
    combined.push_str(&format!(
        "[Batched: {n} messages received during the previous turn — handle as one logical unit]\n\n"
    ));
    let mut extras: Vec<ContentBlock> = Vec::new();
    for (idx, m) in batch.into_iter().enumerate() {
        combined.push_str(&format!(
            "<message index=\"{idx}\" from=\"{from}\">\n{body}\n</message>\n\n",
            idx = idx + 1,
            from = xml_escape(&m.sender_name),
            body = m.prompt,
        ));
        extras.extend(m.extra_blocks);
    }

    (combined, lead_sender_json, trailing_trigger, extras)
}

/// Minimal XML attribute escape — sender names can contain `"`, `<`, `>`, `&`.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

    fn make(prompt: &str, sender: &str, mid: &str) -> BufferedMessage {
        BufferedMessage {
            prompt: prompt.into(),
            extra_blocks: vec![],
            sender_json: format!("{{\"sender\":\"{sender}\"}}"),
            trigger_msg: MessageRef {
                channel: dummy_channel(),
                message_id: mid.into(),
            },
            arrived_at: Instant::now(),
            sender_name: sender.into(),
        }
    }

    #[test]
    fn merge_single_message_is_verbatim() {
        let m = make("hello world", "alice", "1");
        let (prompt, sender, trigger, extras) = merge_batch(vec![m.clone()]);
        assert_eq!(prompt, "hello world");
        assert_eq!(sender, m.sender_json);
        assert_eq!(trigger.message_id, "1");
        assert!(extras.is_empty());
    }

    #[test]
    fn merge_multiple_messages_combines_prompts() {
        let batch = vec![
            make("can you check the build", "alice", "1"),
            make("actually wait", "alice", "2"),
            make("check the build *and* run e2e tests", "alice", "3"),
        ];
        let (prompt, sender, trigger, extras) = merge_batch(batch);
        assert!(prompt.contains("[Batched: 3 messages"));
        assert!(prompt.contains("can you check the build"));
        assert!(prompt.contains("actually wait"));
        assert!(prompt.contains("check the build *and* run e2e tests"));
        assert!(prompt.contains("index=\"1\""));
        assert!(prompt.contains("index=\"3\""));
        // Leading sender preserved; trigger anchors on trailing message
        // (so reactions land on the newest user message in the batch).
        assert_eq!(trigger.message_id, "3");
        assert!(sender.contains("alice"));
        assert!(extras.is_empty());
    }

    #[test]
    fn merge_preserves_extra_blocks_in_order() {
        let mut m1 = make("look at this", "alice", "1");
        m1.extra_blocks.push(ContentBlock::Text {
            text: "transcript-1".into(),
        });
        let mut m2 = make("and this", "alice", "2");
        m2.extra_blocks.push(ContentBlock::Text {
            text: "transcript-2".into(),
        });
        let (_p, _s, _t, extras) = merge_batch(vec![m1, m2]);
        assert_eq!(extras.len(), 2);
        match (&extras[0], &extras[1]) {
            (ContentBlock::Text { text: t1 }, ContentBlock::Text { text: t2 }) => {
                assert_eq!(t1, "transcript-1");
                assert_eq!(t2, "transcript-2");
            }
            _ => panic!("expected two Text blocks"),
        }
    }

    #[test]
    fn xml_escape_replaces_special_chars() {
        assert_eq!(xml_escape("a & b"), "a &amp; b");
        assert_eq!(xml_escape("<x>"), "&lt;x&gt;");
        assert_eq!(xml_escape("say \"hi\""), "say &quot;hi&quot;");
    }
}
