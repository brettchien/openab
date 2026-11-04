// Minimal ACP client driving claude-agent-acp over stdio — the role openab core plays.
// node acp_client.mjs [none|form] [accept|decline|cancel|error|hang]
//   none  → clientCapabilities: {}               (openab today)
//   form  → clientCapabilities.elicitation.form   (the ADR's target)
//   hang  → never answer; send session/cancel after HANG_MS and log the $/cancel_request
// Logs every agent→client request/notification method, the elicitation/create params in full,
// and whether AskUserQuestion is in the session's tool list (from the system init, if exposed).
import { spawn } from "node:child_process";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const [caps = "form", action = "accept"] = process.argv.slice(2);
const uv = process.env.UV ?? "uv";
const log = (...a) => console.log("[acp_client]", ...a);

const agent = spawn(process.env.ACP_AGENT ?? path.join(here, "node_modules/.bin/claude-agent-acp"), [], {
  cwd: here,
  stdio: ["pipe", "pipe", "inherit"],
  env: {
    ...process.env,
    ANTHROPIC_BASE_URL: "http://127.0.0.1:18099",
    ANTHROPIC_API_KEY: "sk-ant-fake",
    ENABLE_TOOL_SEARCH: "false",
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
  },
});

let nextId = 1;
const pending = new Map();
const send = (obj) => agent.stdin.write(JSON.stringify({ jsonrpc: "2.0", ...obj }) + "\n");
const request = (method, params) =>
  new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    send({ id, method, params });
  });

let sessionId;
const openElicitations = new Map(); // agent request id → params

readline.createInterface({ input: agent.stdout }).on("line", (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (msg.method && msg.id !== undefined) return onAgentRequest(msg);
  if (msg.method) return onNotification(msg);
  const p = pending.get(msg.id);
  if (p) { pending.delete(msg.id); msg.error ? p.reject(msg.error) : p.resolve(msg.result); }
});

function onNotification(msg) {
  if (msg.method === "$/cancel_request") {
    log("$/cancel_request", JSON.stringify(msg.params), "open:", openElicitations.has(msg.params?.requestId));
    openElicitations.delete(msg.params?.requestId);
    return;
  }
  if (msg.method === "session/update") {
    const u = msg.params?.update ?? {};
    if (u.sessionUpdate === "tool_call") log("tool_call", u.toolCallId, u.title);
    else if (u.sessionUpdate === "agent_message_chunk") log("agent text:", u.content?.text);
    else if (u.sessionUpdate === "available_commands_update") return;
    return;
  }
  log("notification", msg.method, JSON.stringify(msg.params).slice(0, 300));
}

function onAgentRequest(msg) {
  log("agent→client request", msg.method, "id", msg.id);
  if (msg.method === "session/request_permission") {
    const opt = msg.params.options?.find((o) => o.kind?.startsWith("allow")) ?? msg.params.options?.[0];
    return send({ id: msg.id, result: { outcome: { outcome: "selected", optionId: opt?.optionId } } });
  }
  if (msg.method === "elicitation/create") {
    log("elicitation/create params:", JSON.stringify(msg.params));
    openElicitations.set(msg.id, msg.params);
    if (action === "hang") {
      setTimeout(() => {
        log("sending session/cancel");
        send({ method: "session/cancel", params: { sessionId } });
      }, Number(process.env.HANG_MS ?? 3000));
      return;
    }
    openElicitations.delete(msg.id);
    if (action === "error") return send({ id: msg.id, error: { code: -32602, message: "cannot render" } });
    if (action === "accept") return send({ id: msg.id, result: { action: "accept", content: { choice: "approve", note: "from acp_client" } } });
    return send({ id: msg.id, result: { action } });
  }
  send({ id: msg.id, error: { code: -32601, message: `unsupported ${msg.method}` } });
}

const clientCapabilities = caps === "form" ? { elicitation: { form: {} } } : caps === "url" ? { elicitation: { form: {}, url: {} } } : {};
const init = await request("initialize", { protocolVersion: 1, clientCapabilities, clientInfo: { name: "elicit-probe", version: "0" } });
log("initialize ok, agent", init.agentInfo?.name, init.agentInfo?.version);
const s = await request("session/new", {
  cwd: here,
  mcpServers: [{ name: "probe", command: uv, args: ["run", "--with", "mcp==2.3.0", "python", path.join(here, "server.py")], env: [] }],
});
sessionId = s.sessionId;
log("session", sessionId);
try {
  const r = await request("session/prompt", { sessionId, prompt: [{ type: "text", text: "Call the probe tool." }] });
  log("prompt done:", r.stopReason);
} catch (e) {
  log("prompt error:", JSON.stringify(e));
}
agent.kill();
process.exit(0);
