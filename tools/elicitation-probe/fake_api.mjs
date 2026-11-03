// Scripted Anthropic Messages API for the elicitation probe — no model, no login.
// Turn 1: call the probe MCP tool. After a tool_result: echo it back as text and stop.
// Run: node fake_api.mjs [port]   (default 18099)
// Point the CLI at it: ANTHROPIC_BASE_URL=http://127.0.0.1:18099 ANTHROPIC_API_KEY=sk-ant-fake ENABLE_TOOL_SEARCH=false
import http from "node:http";

const port = Number(process.argv[2] ?? 18099);
const log = (...a) => console.error(`[fake_api ${new Date().toISOString()}]`, ...a);
let seq = 0;

function plan(body) {
  const msgs = body.messages ?? [];
  // Any tool_result in the history means the probe tool already ran: finish.
  const result = msgs.flatMap((m) => (Array.isArray(m.content) ? m.content : [])).find((b) => b.type === "tool_result");
  if (result) {
    const text = Array.isArray(result.content)
      ? result.content.map((c) => c.text ?? "").join("")
      : String(result.content ?? "");
    return { kind: "text", text: `probe tool returned: ${text}` };
  }
  const tool = (body.tools ?? []).find((t) => /probe/.test(t.name ?? ""));
  if (!tool) return { kind: "text", text: "ok" }; // side requests (titles, quota checks)
  return { kind: "tool", name: tool.name };
}

function message(p, model) {
  const id = `msg_fake_${++seq}`;
  const content = p.kind === "tool"
    ? [{ type: "tool_use", id: `toolu_fake_${seq}`, name: p.name, input: {} }]
    : [{ type: "text", text: p.text }];
  return {
    id, type: "message", role: "assistant", model, content,
    stop_reason: p.kind === "tool" ? "tool_use" : "end_turn", stop_sequence: null,
    usage: { input_tokens: 10, output_tokens: 5 },
  };
}

function sse(res, m) {
  const ev = (type, data) => res.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
  res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
  ev("message_start", { message: { ...m, content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 1 } } });
  m.content.forEach((b, index) => {
    if (b.type === "tool_use") {
      ev("content_block_start", { index, content_block: { ...b, input: {} } });
      ev("content_block_delta", { index, delta: { type: "input_json_delta", partial_json: JSON.stringify(b.input) } });
    } else {
      ev("content_block_start", { index, content_block: { type: "text", text: "" } });
      ev("content_block_delta", { index, delta: { type: "text_delta", text: b.text } });
    }
    ev("content_block_stop", { index });
  });
  ev("message_delta", { delta: { stop_reason: m.stop_reason, stop_sequence: null }, usage: { output_tokens: 5 } });
  ev("message_stop", {});
  res.end();
}

http.createServer((req, res) => {
  let raw = "";
  req.on("data", (c) => (raw += c));
  req.on("end", () => {
    const path = req.url.split("?")[0];
    if (req.method === "POST" && path.endsWith("/v1/messages/count_tokens")) {
      res.writeHead(200, { "content-type": "application/json" });
      return res.end(JSON.stringify({ input_tokens: 10 }));
    }
    if (req.method === "POST" && path.endsWith("/v1/messages")) {
      let body = {};
      try { body = JSON.parse(raw); } catch {}
      const p = plan(body);
      const names = (body.tools ?? []).map((t) => t.name);
      log(path, body.stream ? "stream" : "json", "→", p.kind, p.name ?? p.text.slice(0, 60),
        names.length ? `| tools=${names.length} AskUserQuestion=${names.includes("AskUserQuestion")}` : "");
      const m = message(p, body.model ?? "claude-fake");
      if (body.stream) return sse(res, m);
      res.writeHead(200, { "content-type": "application/json" });
      return res.end(JSON.stringify(m));
    }
    log("unhandled", req.method, req.url);
    res.writeHead(404, { "content-type": "application/json" });
    res.end(JSON.stringify({ type: "error", error: { type: "not_found_error", message: "fake_api" } }));
  });
}).listen(port, "127.0.0.1", () => log(`listening on 127.0.0.1:${port}`));
