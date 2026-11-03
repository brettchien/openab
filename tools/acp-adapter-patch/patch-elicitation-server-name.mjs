#!/usr/bin/env node
// Downstream patch for @agentclientprotocol/claude-agent-acp (Katashiro ADR mcp-elicitation §2.10):
// mcpElicitationToCreateRequest drops the SDK request's serverName. Put it (plus displayName /
// title when present) under _meta["claudecode/serverName"] etc. on both the form and the URL
// request, so the front-end can say which MCP server is asking.
//
// usage: node patch-elicitation-server-name.mjs <adapter package dir>
// Exits non-zero when an anchor is missing or matches more than once, or when the patched
// function does not produce the _meta on a form and a URL sample (build-time enforcement).
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const dir = process.argv[2];
if (!dir) {
  console.error("usage: patch-elicitation-server-name.mjs <claude-agent-acp package dir>");
  process.exit(2);
}
const file = path.join(dir, "dist", "elicitation.js");
let src = fs.readFileSync(file, "utf8");

const fnStart = src.indexOf("export function mcpElicitationToCreateRequest(request, sessionId) {");
if (fnStart < 0 || src.indexOf("export function mcpElicitationToCreateRequest(", fnStart + 1) >= 0) {
  console.error("patch: mcpElicitationToCreateRequest anchor missing or ambiguous");
  process.exit(1);
}
const fnEnd = src.indexOf("\n}\n", fnStart);
let fn = src.slice(fnStart, fnEnd);
const anchors = [
  ["            elicitationId: request.elicitationId ?? randomUUID(),\n        };",
   "            elicitationId: request.elicitationId ?? randomUUID(),\n            ...openabServerMeta(request),\n        };"],
  ["        requestedSchema: normalizeElicitationSchema(request.requestedSchema),\n    };",
   "        requestedSchema: normalizeElicitationSchema(request.requestedSchema),\n        ...openabServerMeta(request),\n    };"],
];
for (const [a, b] of anchors) {
  const n = fn.split(a).length - 1;
  if (n !== 1) {
    console.error(`patch: anchor matched ${n} times: ${a.split("\n")[0].trim()}`);
    process.exit(1);
  }
  fn = fn.replace(a, b);
}
const helper = `
// openab downstream patch (mcp-elicitation ADR §2.10): which MCP server is asking.
function openabServerMeta(request) {
    if (typeof request.serverName !== "string" || request.serverName === "") {
        return {};
    }
    const meta = { "claudecode/serverName": request.serverName };
    if (typeof request.displayName === "string" && request.displayName) {
        meta["claudecode/displayName"] = request.displayName;
    }
    if (typeof request.title === "string" && request.title) {
        meta["claudecode/title"] = request.title;
    }
    return { _meta: meta };
}
`;
src = src.slice(0, fnStart) + fn + src.slice(fnEnd) + helper;
fs.writeFileSync(file, src);

// Enforce: the installed module must now carry the name on both modes.
const mod = await import(pathToFileURL(file).href + `?v=${Date.now()}`);
const form = mod.mcpElicitationToCreateRequest(
  { serverName: "probe", message: "m", mode: "form", requestedSchema: { type: "object", properties: {} } }, "s");
const url = mod.mcpElicitationToCreateRequest(
  { serverName: "probe", message: "m", mode: "url", url: "https://example.com", elicitationId: "e" }, "s");
for (const [name, r] of [["form", form], ["url", url]]) {
  if (r?._meta?.["claudecode/serverName"] !== "probe") {
    console.error(`patch: verification failed for ${name}: ${JSON.stringify(r)}`);
    process.exit(1);
  }
}
console.log(`patched ${file}: _meta["claudecode/serverName"] on form + url`);
