//! UnifiedGatewayAdapter — routes ChatAdapter calls through in-process gateway
//! platform adapters based on the ChannelRef.platform field.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use openab_core::adapter::{ChannelRef, ChatAdapter, MessageRef, ToolCallProgress};
use openab_gateway::schema::{Content, GatewayReply, ReplyChannel};
use openab_gateway::AppState;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

fn synthetic_unified_id() -> String {
    format!(
        "unified_{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

pub struct UnifiedGatewayAdapter {
    pub gw_state: Arc<AppState>,
    /// Telegram reaction state (message_id -> emoji list) for add/remove_reaction
    pub telegram_reaction_state: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

impl UnifiedGatewayAdapter {
    pub fn new(gw_state: Arc<AppState>) -> Self {
        Self {
            gw_state,
            telegram_reaction_state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Dispatch a GatewayReply to the correct platform adapter. Platforms with
    /// direct delivery receipts return the real message resource name; legacy
    /// fire-and-forget adapters return `None`.
    async fn dispatch_reply(&self, reply: &GatewayReply) -> Result<Option<String>> {
        let client = &self.gw_state.client;
        match reply.platform.as_str() {
            #[cfg(feature = "telegram")]
            "telegram" => {
                if let Some(ref token) = self.gw_state.telegram_bot_token {
                    openab_gateway::adapters::telegram::handle_reply(
                        reply,
                        token,
                        client,
                        &self.gw_state.event_tx,
                        &self.telegram_reaction_state,
                        self.gw_state.telegram_rich_messages,
                    )
                    .await;
                }
            }
            #[cfg(feature = "line")]
            "line" => {
                if let Some(ref access_token) = self.gw_state.line_access_token {
                    openab_gateway::adapters::line::dispatch_line_reply(
                        client,
                        access_token,
                        &self.gw_state.reply_token_cache,
                        reply,
                        openab_gateway::adapters::line::LINE_API_BASE,
                    )
                    .await;
                }
            }
            #[cfg(feature = "feishu")]
            "feishu" => {
                if let Some(ref feishu) = self.gw_state.feishu {
                    openab_gateway::adapters::feishu::handle_reply(
                        reply,
                        feishu,
                        &self.gw_state.event_tx,
                    )
                    .await;
                }
            }
            #[cfg(feature = "googlechat")]
            "googlechat" => {
                if let Some(ref gc) = self.gw_state.google_chat {
                    if reply.command.is_none() {
                        return gc
                            .deliver_message(reply)
                            .await
                            .map(Some)
                            .map_err(anyhow::Error::msg);
                    }
                    gc.handle_reply(reply, &self.gw_state.event_tx).await;
                } else if reply.command.is_none() {
                    return Err(anyhow!("googlechat adapter is not configured"));
                } else {
                    tracing::warn!(
                        command = ?reply.command.as_deref(),
                        "googlechat command dropped: adapter is not configured"
                    );
                }
            }
            #[cfg(feature = "wecom")]
            "wecom" => {
                if let Some(ref wecom) = self.gw_state.wecom {
                    wecom.handle_reply(reply, &self.gw_state.event_tx).await;
                }
            }
            #[cfg(feature = "lineworks")]
            "lineworks" => {
                if let Some(ref lineworks) = self.gw_state.lineworks {
                    let ok = openab_gateway::adapters::lineworks::dispatch_lineworks_reply(
                        client, lineworks, reply,
                    )
                    .await;
                    if !ok {
                        tracing::error!(
                            channel = %reply.channel.id,
                            command = ?reply.command.as_deref(),
                            "lineworks reply delivery failed — reply lost"
                        );
                    }
                }
            }
            #[cfg(feature = "teams")]
            "teams" => {
                if let Some(ref teams) = self.gw_state.teams {
                    openab_gateway::adapters::teams::handle_reply(
                        reply,
                        teams,
                        &self.gw_state.teams_service_urls,
                    )
                    .await;
                }
            }
            #[cfg(feature = "acp")]
            "acp" => {
                if let Some(ref registry) = self.gw_state.acp_reply_registry {
                    openab_gateway::adapters::acp_server::handle_reply(reply, registry).await;
                }
            }
            other => {
                tracing::warn!(
                    platform = other,
                    "unified adapter: unknown platform, cannot route reply"
                );
            }
        }
        Ok(None)
    }

    /// Build a GatewayReply from ChatAdapter parameters.
    fn build_reply(
        &self,
        channel: &ChannelRef,
        content: &str,
        command: Option<&str>,
        quote_message_id: Option<&str>,
    ) -> GatewayReply {
        GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: channel.origin_event_id.clone().unwrap_or_default(),
            platform: channel.platform.clone(),
            channel: ReplyChannel {
                id: channel.channel_id.clone(),
                thread_id: channel.thread_id.clone(),
            },
            content: Content {
                content_type: "text".into(),
                text: content.into(),
                attachments: vec![],
            },
            command: command.map(|s| s.into()),
            request_id: None,
            quote_message_id: quote_message_id.map(|s| s.into()),
        }
    }
}

#[async_trait]
impl ChatAdapter for UnifiedGatewayAdapter {
    fn platform(&self) -> &'static str {
        "unified"
    }

    fn message_limit(&self) -> usize {
        4096 // conservative limit across platforms
    }

    async fn send_message(&self, channel: &ChannelRef, content: &str) -> Result<MessageRef> {
        let reply = self.build_reply(channel, content, None, None);
        let message_id = self
            .dispatch_reply(&reply)
            .await?
            .unwrap_or_else(synthetic_unified_id);
        Ok(MessageRef {
            channel: channel.clone(),
            message_id,
        })
    }

    async fn create_thread(
        &self,
        channel: &ChannelRef,
        trigger_msg: &MessageRef,
        title: &str,
    ) -> Result<ChannelRef> {
        let reply = self.build_reply(channel, title, Some("create_topic"), None);
        let _ = self.dispatch_reply(&reply).await?;
        // Return a thread channel ref with the trigger message as thread_id
        Ok(ChannelRef {
            platform: channel.platform.clone(),
            channel_id: channel.channel_id.clone(),
            thread_id: Some(trigger_msg.message_id.clone()),
            parent_id: Some(channel.channel_id.clone()),
            origin_event_id: channel.origin_event_id.clone(),
        })
    }

    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()> {
        let mut reply = self.build_reply(&msg.channel, emoji, Some("add_reaction"), None);
        // Use the actual platform message_id (not origin_event_id which is a UUID)
        reply.reply_to = msg.message_id.clone();
        let _ = self.dispatch_reply(&reply).await?;
        Ok(())
    }

    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()> {
        let mut reply = self.build_reply(&msg.channel, emoji, Some("remove_reaction"), None);
        // Use the actual platform message_id (not origin_event_id which is a UUID)
        reply.reply_to = msg.message_id.clone();
        let _ = self.dispatch_reply(&reply).await?;
        Ok(())
    }

    async fn edit_message(&self, msg: &MessageRef, content: &str) -> Result<()> {
        let mut reply = self.build_reply(&msg.channel, content, Some("edit_message"), None);
        // Use the actual platform message_id (e.g. "draft" for streaming, or numeric for edits)
        reply.reply_to = msg.message_id.clone();
        let _ = self.dispatch_reply(&reply).await?;
        Ok(())
    }

    async fn send_tool_progress(
        &self,
        channel: &ChannelRef,
        progress: &ToolCallProgress,
    ) -> Result<()> {
        let mut reply = self.build_reply(
            channel,
            &progress.payload().to_string(),
            Some(progress.command()),
            None,
        );
        reply.content.content_type = "json".into();
        let _ = self.dispatch_reply(&reply).await?;
        Ok(())
    }

    async fn send_keepalive(&self, channel: &ChannelRef) -> Result<()> {
        let reply = self.build_reply(channel, "", Some("keepalive"), None);
        let _ = self.dispatch_reply(&reply).await?;
        Ok(())
    }

    async fn send_message_with_reply(
        &self,
        channel: &ChannelRef,
        content: &str,
        reply_to_message_id: &str,
    ) -> Result<MessageRef> {
        let reply = self.build_reply(channel, content, None, Some(reply_to_message_id));
        let message_id = self
            .dispatch_reply(&reply)
            .await?
            .unwrap_or_else(synthetic_unified_id);
        Ok(MessageRef {
            channel: channel.clone(),
            message_id,
        })
    }

    fn use_streaming(&self, _other_bot_present: bool) -> bool {
        // Streaming override is resolved once at startup (config `[telegram].streaming`
        // → `TELEGRAM_STREAMING` env → unset). When unset, default to `true` when
        // Telegram Rich Messages are enabled (implies sendRichMessageDraft support),
        // `false` otherwise. This gives Telegram-only deployments streaming out of the
        // box while multi-platform deployments stay safe by default.
        if let Some(streaming) = self.gw_state.telegram_streaming {
            return streaming;
        }
        self.gw_state.telegram_rich_messages
    }

    fn show_streaming_placeholder(&self) -> bool {
        // No placeholder needed — Telegram uses sendRichMessageDraft for streaming preview.
        // The draft mechanism handles the "typing" indicator natively.
        false
    }

    fn renders_native_tables(&self, platform: &str) -> bool {
        // Telegram Rich Messages render markdown tables natively — skip the
        // table→code-block pre-pass so tables display with proper formatting.
        // Only applies to Telegram; other platforms in unified mode keep wrapping.
        platform == "telegram" && self.gw_state.telegram_rich_messages
    }
}

#[cfg(all(test, feature = "googlechat"))]
mod tests {
    use super::*;
    use openab_gateway::adapters::googlechat::GoogleChatAdapter;
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn googlechat_send_failure_propagates_in_unified_mode() {
        let (event_tx, _event_rx) = broadcast::channel(4);
        let mut state = AppState::test_default(event_tx);
        // No credential source: delivery fails before any network access.
        state.google_chat = Some(GoogleChatAdapter::new(None, None, None));
        let adapter = UnifiedGatewayAdapter::new(Arc::new(state));
        let channel = ChannelRef {
            platform: "googlechat".into(),
            channel_id: "spaces/TEST".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_test".into()),
        };

        let err = adapter
            .send_message(&channel, "hello")
            .await
            .expect_err("unified mode must not synthesize success after delivery failure");
        assert!(
            err.to_string().contains("no credentials configured"),
            "{err}"
        );
    }

    /// Core's tool progress and keepalive, through the real reply path into the ACP server's
    /// reply sink: the wire shape (command, JSON body, `reply_to` fence) is what the gateway
    /// parses, so assert it end to end rather than per side.
    #[cfg(feature = "acp")]
    #[tokio::test]
    async fn tool_progress_and_keepalive_reach_the_acp_reply_sink() {
        use openab_core::acp::ToolIdentity;
        use openab_gateway::adapters::acp_server::{new_reply_registry, ReplyChunk, ReplySink};

        let (event_tx, _event_rx) = broadcast::channel(4);
        let mut state = AppState::test_default(event_tx);
        let registry = new_reply_registry();
        state.acp_reply_registry = Some(registry.clone());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        registry.lock().unwrap().insert(
            "acp_chan".into(),
            ReplySink { turn_id: "evt_turn".into(), tx, owner: "conn".into(), generation: 0 },
        );
        let adapter = UnifiedGatewayAdapter::new(Arc::new(state));
        let channel = ChannelRef {
            platform: "acp".into(),
            channel_id: "acp_chan".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_turn".into()),
        };

        let progress = ToolCallProgress {
            tool_call_id: "t1".into(),
            status: "in_progress",
            is_new: true,
            identity: ToolIdentity {
                kind: Some("execute"),
                name: Some("Bash".into()),
                capability: None,
            },
        };
        adapter.send_tool_progress(&channel, &progress).await.unwrap();
        match rx.try_recv() {
            Ok(ReplyChunk::Update(u)) => {
                assert_eq!(u["sessionUpdate"], "tool_call", "{u}");
                assert_eq!(u["toolCallId"], "t1", "{u}");
                assert_eq!(u["status"], "in_progress", "{u}");
                assert_eq!(u["kind"], "execute", "{u}");
                assert_eq!(u["title"], "Bash", "{u}");
            }
            _ => panic!("tool progress must arrive as a session/update body"),
        }

        adapter.send_keepalive(&channel).await.unwrap();
        assert!(matches!(rx.try_recv(), Ok(ReplyChunk::Keepalive)));
        assert!(
            registry.lock().unwrap().contains_key("acp_chan"),
            "progress and keepalives must leave the turn's sink in place"
        );
    }
}
