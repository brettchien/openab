# stdio MCP server for the elicitation probe (mcp==2.3.0, protocol 2026-07-28).
# Tool `ask` returns InputRequiredResult (form: `choice` enum approve|reject + optional `note`);
# the client's retry carries input_responses, which are logged to stderr and to PROBE_LOG (JSONL).
# Run: uv run --with 'mcp==2.3.0' python server.py
import json
import os
import sys
import time

from mcp.server.mcpserver import Context, MCPServer
from mcp_types import ElicitRequest, ElicitRequestFormParams, InputRequiredResult

LOG = os.environ.get("PROBE_LOG", "/tmp/elicit-probe/responses.jsonl")
os.makedirs(os.path.dirname(LOG), exist_ok=True)

mcp = MCPServer("probe")

SCHEMA = {
    "type": "object",
    "properties": {
        "choice": {"type": "string", "title": "Decision", "enum": ["approve", "reject"]},
        "note": {"type": "string", "title": "Note"},
    },
    "required": ["choice"],
}


def record(entry: dict) -> None:
    entry["ts"] = time.time()
    line = json.dumps(entry, default=str)
    print(f"[probe] {line}", file=sys.stderr, flush=True)
    with open(LOG, "a") as f:
        f.write(line + "\n")


@mcp.tool()
async def ask(ctx: Context) -> str | InputRequiredResult:
    """Ask the human to approve or reject the probe action."""
    responses = ctx.input_responses
    if not responses:
        record({"round": "initial"})
        return InputRequiredResult(
            input_requests={
                "decision": ElicitRequest(
                    params=ElicitRequestFormParams(
                        message="Probe: approve or reject this test action?",
                        requested_schema=SCHEMA,
                    )
                )
            },
            request_state="probe-state-1",
        )
    resp = responses.get("decision")
    dumped = resp.model_dump(mode="json", exclude_none=True) if resp is not None else None
    record({"round": "retry", "request_state": ctx.request_state, "decision": dumped})
    return f"input_responses.decision = {json.dumps(dumped)}"


if __name__ == "__main__":
    mcp.run()
