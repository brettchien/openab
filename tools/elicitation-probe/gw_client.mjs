// ACP client for the gateway's /acp WebSocket — the role Katashiro plays. With run_e2e.sh this
// drives the whole path: gw_client → openab gateway → core → claude-agent-acp → CLI → server.py.
// node gw_client.mjs [none|form] [accept|decline|error|hang]
//   none  → initialize without clientCapabilities.elicitation (no card can be shown)
//   form  → declares elicitation.form
//   hang  → never answer; send session/cancel after HANG_MS and log the $/cancel_request
// Env: GW_URL (default ws://127.0.0.1:18200/acp), GW_TOKEN (default "e2e").
const [caps = "form", action = "accept"] = process.argv.slice(2);
const url = process.env.GW_URL ?? "ws://127.0.0.1:18200/acp";
// The gateway reads the bearer from a header only; a browser-style client puts it in the subprotocol.
const bearer = `openab.bearer.${process.env.GW_TOKEN ?? "e2e"}`;
const log = (...a) => console.log("[gw_client]", ...a);

const ws = new WebSocket(url, [bearer, "acp.v1"]); // the server selects acp.v1
let closing = false;
ws.addEventListener("error", (e) => { if (!closing) log("ws error", e.message ?? ""); });
let nextId = 1;
const pending = new Map();
const send = (obj) => ws.send(JSON.stringify({ jsonrpc: "2.0", ...obj }));
const request = (method, params) =>
  new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    send({ id, method, params });
  });
const open = new Map(); // gateway request id → params

ws.addEventListener("message", (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.method && msg.id !== undefined) return onRequest(msg);
  if (msg.method) return onNotification(msg);
  const p = pending.get(msg.id);
  if (p) { pending.delete(msg.id); msg.error ? p.reject(msg.error) : p.resolve(msg.result); }
});

function onNotification(msg) {
  if (msg.method === "$/cancel_request" || msg.method === "elicitation/complete") {
    log(msg.method, JSON.stringify(msg.params), "open:", open.has(msg.params?.requestId));
    open.delete(msg.params?.requestId);
    return;
  }
  if (msg.method === "session/update") {
    const u = msg.params?.update ?? {};
    if (u.sessionUpdate === "agent_message_chunk") log("answer:", JSON.stringify(u.content?.text ?? "").slice(0, 200));
    else if (u.sessionUpdate?.startsWith("tool_call")) log(u.sessionUpdate, u.toolCallId, u.title ?? "", u.status ?? "");
  }
}

function onRequest(msg) {
  if (msg.method !== "elicitation/create") {
    log("unexpected request", msg.method);
    return send({ id: msg.id, error: { code: -32601, message: "method not found" } });
  }
  log("elicitation/create id", msg.id, "params:", JSON.stringify(msg.params));
  open.set(msg.id, msg.params);
  if (action === "hang") {
    setTimeout(() => { log("→ session/cancel"); send({ method: "session/cancel", params: { sessionId } }); }, Number(process.env.HANG_MS ?? 3000));
    return;
  }
  open.delete(msg.id);
  if (action === "error") return send({ id: msg.id, error: { code: -32602, message: "cannot render" } });
  const result = action === "accept"
    ? { action: "accept", content: { choice: "approve", note: "from gw_client" } }
    : { action };
  send({ id: msg.id, result });
}

let sessionId;
ws.addEventListener("open", async () => {
  try {
    const init = await request("initialize", {
      protocolVersion: 1,
      clientCapabilities: caps === "form" ? { elicitation: { form: {} } } : {},
      clientInfo: { name: "gw_client", version: "0" },
    });
    log("initialize ok, protocolVersion", init?.protocolVersion);
    ({ sessionId } = await request("session/new", { cwd: "/tmp", mcpServers: [] }));
    log("session", sessionId);
    const r = await request("session/prompt", { sessionId, prompt: [{ type: "text", text: "run the probe tool" }] });
    log("prompt done, stopReason", r?.stopReason, "open left:", open.size);
  } catch (e) {
    log("error", JSON.stringify(e));
  }
  closing = true;
  ws.close();
});
ws.addEventListener("close", () => process.exit(0));
setTimeout(() => { log("TIMEOUT"); process.exit(2); }, Number(process.env.CLIENT_TIMEOUT_MS ?? 120000));
