#!/usr/bin/env bash
# End to end: gw_client.mjs → openab (unified: gateway /acp + core) → claude-agent-acp → CLI →
# server.py, with fake_api.mjs standing in for the model. No Discord, no login.
#   OPENAB_BIN=… ACP_AGENT=… CLAUDE_EXE=… UV=… ./run_e2e.sh [none|form] [accept|decline|error|hang]
# OPENAB_BIN: an openab built with --features unified. ACP_AGENT / CLAUDE_EXE: the (patched)
# adapter and the CLI it runs, as in the image (CLAUDE_CODE_EXECUTABLE).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
: "${OPENAB_BIN:?}" "${ACP_AGENT:?}" "${CLAUDE_EXE:?}" "${UV:?}"
work=${E2E_WORK:-/tmp/elicit-e2e}
port=${E2E_PORT:-18200}
mkdir -p "$work/home" /tmp/elicit-probe

# The adapter ignores CLI flags such as --mcp-config, and core sends only the facade in
# mcpServers, so the probe server is a user-scope server in the agent HOME.
cat > "$work/home/.claude.json" <<EOF
{"mcpServers": {"probe": {"type": "stdio", "command": "$UV", "args": ["run", "--with", "mcp==2.3.0", "python", "$here/server.py"], "env": {"UV_CACHE_DIR": "${UV_CACHE_DIR:-/tmp/uvcache}", "UV_PYTHON_INSTALL_DIR": "${UV_PYTHON_INSTALL_DIR:-/tmp/uvpy}", "PROBE_LOG": "/tmp/elicit-probe/responses.jsonl"}}}}
EOF
cat > "$work/config.toml" <<EOF
[agent]
command = "$ACP_AGENT"
working_dir = "$work"
env = { ANTHROPIC_BASE_URL = "http://127.0.0.1:18099", ANTHROPIC_API_KEY = "sk-ant-fake", ENABLE_TOOL_SEARCH = "false", CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = "1", CLAUDE_CODE_EXECUTABLE = "$CLAUDE_EXE", HOME = "$work/home", PATH = "$(dirname "$(command -v node)"):/usr/bin:/bin", UV_CACHE_DIR = "${UV_CACHE_DIR:-/tmp/uvcache}", UV_PYTHON_INSTALL_DIR = "${UV_PYTHON_INSTALL_DIR:-/tmp/uvpy}", PROBE_LOG = "/tmp/elicit-probe/responses.jsonl" }
EOF

cleanup() { kill "${oab_pid:-}" "${api_pid:-}" 2>/dev/null || true; }
trap cleanup EXIT
node "$here/fake_api.mjs" > "$work/fake_api.log" 2>&1 & api_pid=$!
# env -i: never hand this openab the container's real chat tokens.
env -i HOME="$work/home" PATH="/usr/bin:/bin" RUST_LOG="${RUST_LOG:-info}" \
  OPENAB_ACP_ENABLED=true GATEWAY_LISTEN="127.0.0.1:$port" OPENAB_ACP_AUTH_KEY=e2e \
  GATEWAY_ALLOWED_USERS=acp_client \
  "$OPENAB_BIN" run -c "$work/config.toml" > "$work/openab.log" 2>&1 & oab_pid=$!
for _ in $(seq 1 50); do
  (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break
  sleep 0.2
done
GW_URL="ws://127.0.0.1:$port/acp" GW_TOKEN=e2e node "$here/gw_client.mjs" "$@"
