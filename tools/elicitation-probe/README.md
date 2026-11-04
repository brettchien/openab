# elicitation-probe

Reproduces MCP elicitation end to end — MCP server → Claude Code CLI → (Agent SDK | `claude-agent-acp`)
→ client — with no model and no login. Used to verify openab's ACP elicitation forwarding (Katashiro ADR
`docs/adr/mcp-elicitation.md`).

| File | Role |
|---|---|
| `server.py` | stdio MCP server (`mcp==2.3.0`, protocol 2026-07-28). Tool `ask` returns `InputRequiredResult` (form: `choice` enum `approve`/`reject`, optional `note`) and logs the retry's `input_responses` to `PROBE_LOG` (JSONL, default `/tmp/elicit-probe/responses.jsonl`). |
| `fake_api.mjs` | Scripted Anthropic Messages API on `127.0.0.1:18099`: first call → `tool_use` of the probe tool, after a `tool_result` → text. Logs whether `AskUserQuestion` is in the request's tool list. |
| `sdk_test.mjs` | Drives the bundled CLI through the Agent SDK, with or without `onElicitation`. |
| `acp_client.mjs` | Minimal ACP client driving `claude-agent-acp` over stdio — the role openab core plays. Declares `clientCapabilities: {}` or `elicitation.form`, answers `elicitation/create`, logs `$/cancel_request`. |

## Run

```bash
npm install                       # claude-agent-acp 0.99.1, claude-agent-sdk 0.3.312 (bundles CLI 2.1.312)
node fake_api.mjs &               # keep running
export UV=$(command -v uv)        # server.py runs via: uv run --with mcp==2.3.0 python server.py
node sdk_test.mjs without         # → server gets decline
node sdk_test.mjs with accept     # accept | decline | cancel | hang
node acp_client.mjs none accept   # → server gets decline (openab today)
node acp_client.mjs form accept   # accept | decline | cancel | error | hang
cat /tmp/elicit-probe/responses.jsonl
```

To test openab's patched adapter instead of the npm one, set `ACP_AGENT=/path/to/claude-agent-acp`.

## Results (2026-11-03, CLI 2.1.312, claude-agent-acp 0.99.1)

| Run | Server's `input_responses.decision` |
|---|---|
| `sdk_test without` | `decline` |
| `sdk_test with accept` | `accept` + `{choice: approve, note}` |
| `sdk_test with decline` / `cancel` | `decline` / `cancel` |
| `acp_client none` | `decline`; tool list has `AskUserQuestion=false` |
| `acp_client form accept` | `accept` + content; tool list has `AskUserQuestion=true` |
| `acp_client form decline` | `decline` |
| `acp_client form error` (client replies `-32602`) | `cancel` |
| `acp_client form hang` → `session/cancel` | agent sends `$/cancel_request {requestId}`, prompt ends `cancelled`, **the server gets no retry at all** |

Observed on the wire:

- The SDK's `onElicitation` request has `serverName: "probe"`; the ACP `elicitation/create` params do
  not (only `mode`, `sessionId`, `message`, `requestedSchema`) — hence the ADR's §2.10 adapter patch.
- An interrupted elicitation is abandoned, not answered: with the 2026-07-28 flow the tool call is never
  retried, so the server sees nothing (no `cancel`). Tests should assert "no retry" for interrupt.

## Results (2026-11-04, image layout: patched global claude-agent-acp 0.99.1 + `CLAUDE_CODE_EXECUTABLE` = global CLI 2.1.312)

Same as the Dockerfiles: `npm install -g` both, run `tools/acp-adapter-patch/patch-elicitation-server-name.mjs`
on the global adapter, `ACP_AGENT` / `CLAUDE_CODE_EXECUTABLE` pointed at the global binaries.

| Run | Server's `input_responses.decision` | `elicitation/create` `_meta` |
|---|---|---|
| `acp_client none` | `decline` | — (no request) |
| `acp_client form accept` | `accept` + content | `{"claudecode/serverName":"probe"}` |
| `acp_client form decline` | `decline` | same |
| `acp_client form error` | `cancel` | same |

The previous pin (0.84.0 + CLI 2.1.284) fails the probe tool with "Handler returned an invalid result"
with or without the capability: that CLI does not understand `InputRequiredResult`.
