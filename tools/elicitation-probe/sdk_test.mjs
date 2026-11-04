// Drive the bundled Claude Code CLI through the Agent SDK against fake_api.mjs + server.py.
// node sdk_test.mjs without            → no onElicitation (what openab gets today)
// node sdk_test.mjs with [accept|decline|cancel|hang]   → onElicitation answers that way
//   (hang = never answers; the abort after HANG_MS shows the signal path)
// Needs fake_api.mjs listening on 18099. Logs the server's input_responses to PROBE_LOG.
import { query } from "@anthropic-ai/claude-agent-sdk";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const [mode = "with", action = "accept"] = process.argv.slice(2);
const uv = process.env.UV ?? "uv";

const abort = new AbortController();
const options = {
  cwd: here,
  abortController: abort,
  permissionMode: "bypassPermissions",
  allowDangerouslySkipPermissions: true,
  settingSources: [],
  mcpServers: {
    probe: { type: "stdio", command: uv, args: ["run", "--with", "mcp==2.3.0", "python", path.join(here, "server.py")] },
  },
  env: {
    ...process.env,
    ANTHROPIC_BASE_URL: "http://127.0.0.1:18099",
    ANTHROPIC_API_KEY: "sk-ant-fake",
    ENABLE_TOOL_SEARCH: "false",
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
  },
};

if (mode === "with") {
  options.onElicitation = async (request, { signal }) => {
    console.log("[sdk_test] onElicitation request:", JSON.stringify(request));
    if (action === "hang") {
      setTimeout(() => abort.abort(), Number(process.env.HANG_MS ?? 3000));
      return new Promise((resolve) => signal.addEventListener("abort", () => {
        console.log("[sdk_test] signal aborted");
        resolve({ action: "cancel" });
      }));
    }
    if (action === "accept") return { action: "accept", content: { choice: "approve", note: "from sdk_test" } };
    return { action };
  };
}

try {
  for await (const msg of query({ prompt: "Call the probe tool.", options })) {
    if (msg.type === "assistant") {
      for (const b of msg.message.content) {
        if (b.type === "text") console.log("[sdk_test] assistant:", b.text);
        if (b.type === "tool_use") console.log("[sdk_test] tool_use:", b.name);
      }
    } else if (msg.type === "user") {
      for (const b of msg.message.content ?? []) {
        if (b.type === "tool_result") console.log("[sdk_test] tool_result:", JSON.stringify(b.content).slice(0, 300));
      }
    } else if (msg.type === "result") {
      console.log("[sdk_test] result:", msg.subtype);
    }
  }
} catch (e) {
  console.log("[sdk_test] ended:", e.message);
}
