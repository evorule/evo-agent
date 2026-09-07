#!/usr/bin/env bash
# Evo-Agent 5-minute demo script (Linux / macOS).
# One command: check env -> fetch + start evorule-server -> build evo-agent
# -> run a finance demo session (LLM + tools, fully audited) -> replay the
# fact chain with hash-chain verification.
#
# Success is dual-criteria: the audit chain must verify AND the business
# artifact must exist on disk (workspace/expenses_2026.json with the 45.50
# entry). A verified audit of "nothing happened" is a false success.
#
# Prerequisites:
#   - bash, curl, cargo (https://rustup.rs)
#   - an LLM API key, exported as an environment variable:
#       MiniMax (default):  MINIMAX_API_KEY
#       DeepSeek:           DEEPSEEK_API_KEY   (run with: PROVIDER=deepseek ./demo.sh)
#
# Usage:
#   ./demo.sh
#   PROVIDER=deepseek ./demo.sh
#
# All demo assets live under .demo/ (git-ignored). Delete the folder to reset.
set -euo pipefail
cd "$(dirname "$0")"

PROVIDER="${PROVIDER:-minimax}"
SERVER_VERSION="${SERVER_VERSION:-v0.5.0}"
DEMO_DIR=".demo"
SERVER_DIR="$DEMO_DIR/server"
SERVER_PORT=18080
SERVER_URL="http://127.0.0.1:$SERVER_PORT"

case "$PROVIDER" in
  minimax)
    KEY_ENV="MINIMAX_API_KEY"
    API_BASE="https://api.minimaxi.com/v1/text/chatcompletion_v2"
    MODEL="MiniMax-M2.5"
    ;;
  deepseek)
    KEY_ENV="DEEPSEEK_API_KEY"
    API_BASE="https://api.deepseek.com/chat/completions"
    MODEL="deepseek-chat"
    ;;
  *) echo "unsupported PROVIDER: $PROVIDER" >&2; exit 1 ;;
esac

echo "== [0/5] environment checks =="
command -v cargo >/dev/null || { echo "FAIL: cargo not found. Install Rust: https://rustup.rs"; exit 1; }
if [ -z "${!KEY_ENV:-}" ]; then
  echo "FAIL: environment variable $KEY_ENV is not set."
  echo "  export it first, e.g.:  export $KEY_ENV=your-api-key"
  exit 1
fi
echo "PASS: cargo found, $KEY_ENV set"

echo "== [1/5] evorule-server =="
server_up=0
code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 "$SERVER_URL" || true)
if [ "$code" = "200" ]; then server_up=1; fi

if [ "$server_up" = "1" ]; then
  echo "PASS: server already running at $SERVER_URL (reusing it)"
  echo "  note: if it requires auth, export EVORULE_AUTH_TOKEN first;"
  echo "  a demo server started by this script manages the token for you."
else
  if [ ! -x "$SERVER_DIR/evorule-server" ]; then
    echo "downloading evorule-server $SERVER_VERSION from Gitee..."
    mkdir -p "$DEMO_DIR"
    # Gitee Release asset names carry the version without the "v" prefix
    # (tag v0.5.0 -> asset evorule-server-0.5.0-linux64.tar.gz)
    ASSET_VER="${SERVER_VERSION#v}"
    curl -L -o "$DEMO_DIR/server.tar.gz" \
      "https://gitee.com/evorule/evorule-server/releases/download/$SERVER_VERSION/evorule-server-$ASSET_VER-linux64.tar.gz"
    mkdir -p "$SERVER_DIR"
    tar -xzf "$DEMO_DIR/server.tar.gz" -C "$SERVER_DIR"
    rm -f "$DEMO_DIR/server.tar.gz"
  fi
  SRV_BIN="$SERVER_DIR/evorule-server"
  if [ ! -x "$SRV_BIN" ]; then
    # some archives nest one level deep
    SRV_BIN=$(find "$SERVER_DIR" -name evorule-server -type f | head -1)
  fi
  if [ -z "$SRV_BIN" ] || [ ! -x "$SRV_BIN" ]; then
    echo "FAIL: evorule-server binary not found after download/extract"
    exit 1
  fi
  echo "starting evorule-server (port $SERVER_PORT)..."
  # v0.5.0+ refuses to start with no auth on loopback unless a token is set;
  # demo uses a random per-run token passed to both server and evo-agent.
  DEMO_TOKEN=$(openssl rand -hex 16 2>/dev/null || head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
  SRV_DIR=$(dirname "$SRV_BIN")
  mkdir -p "$SRV_DIR/data"
  # evo-agent's ReAct loop script is consumer-owned (locality principle):
  # the bundled server_eval.json bridge collect template expects a different
  # tool_calls shape; the agent ships its own full constitution instead.
  # (absolute path: resolved here, before the subshell cds into SRV_DIR)
  CORE_EVAL="$(pwd)/assets/agent_constitution.json"
  (cd "$SRV_DIR" && nohup "$(pwd)/$(basename "$SRV_BIN")" \
    --addr "127.0.0.1:$SERVER_PORT" --web-dir web --rules-dir rules \
    --service-registry service_registry.json \
    --core-eval "$CORE_EVAL" \
    --wal-dir ./data/wal --wal-fsync \
    --auth-token "$DEMO_TOKEN" \
    2>>server-stderr.log >/dev/null &)
  export EVORULE_AUTH_TOKEN="$DEMO_TOKEN"
  ready=0
  for _ in $(seq 1 20); do
    sleep 1
    code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 2 "$SERVER_URL" || true)
    if [ "$code" = "200" ]; then ready=1; break; fi
  done
  if [ "$ready" = "1" ]; then
    echo "PASS: server is up at $SERVER_URL"
  else
    echo "FAIL: server did not become ready. Check $SERVER_DIR/server-stderr.log"
    exit 1
  fi
fi

echo "== [2/5] project config =="
if [ -f evo-agent.toml ]; then
  echo "SKIP: evo-agent.toml already exists (keeping yours)"
else
  cat > evo-agent.toml <<EOF
[llm]
provider = "$PROVIDER"
api_key = "\${ENV:$KEY_ENV}"
model = "$MODEL"
api_base = "$API_BASE"

[evorule]
base_url = "$SERVER_URL"
EOF
  echo "PASS: wrote evo-agent.toml (provider=$PROVIDER, evorule=$SERVER_URL)"
fi

echo "== [3/5] cargo build (first run takes a few minutes) =="
cargo build --quiet
echo "PASS: build"

echo "== [4/5] running finance demo session (streaming) =="
# file_write sandbox: only <workdir>/workspace/ is writable -> aim the goal there
mkdir -p workspace
GOAL="Register one expense: date 2026-09-07, item 'Team lunch', amount 45.50 CNY, category 'Catering', submitter 'evo-agent-demo'. Write it into workspace/expenses_2026.json as a JSON array (create the file if missing; append if it exists). Confirm the written path in one sentence."
STDERR_LOG="$DEMO_DIR/run-stderr.log"
set +e
./target/debug/evo-agent run "$GOAL" --stream --agent general 2>"$STDERR_LOG"
rc=$?
set -e
if [ "$rc" -ne 0 ]; then
  echo "FAIL: agent run failed. See $STDERR_LOG"
  exit 1
fi
SESSION_ID=$(grep -o '\[session: [0-9]*\]' "$STDERR_LOG" | head -1 | grep -o '[0-9]*')
if [ -z "$SESSION_ID" ]; then
  echo "FAIL: session id not found in run output"
  exit 1
fi
echo "PASS: session created (id=$SESSION_ID)"

echo
echo "== [5/5] business artifact + audit chain verification =="
# dual-criteria (fail-fast): the run exit code alone can hide a false success
# (e.g. the LLM chats nicely but never executes the tool). Require the file.
BIZ_OK=1
if [ ! -f workspace/expenses_2026.json ]; then
  echo "FAIL: business task not completed - workspace/expenses_2026.json was not created"
  echo "  (the audit report below may still verify; a verified audit of"
  echo "   'nothing happened' is exactly the false success this check guards)"
  BIZ_OK=0
elif ! grep -q '45\.50' workspace/expenses_2026.json; then
  echo "FAIL: business task not completed - workspace/expenses_2026.json lacks the 45.50 entry"
  BIZ_OK=0
else
  echo "PASS: business artifact written (workspace/expenses_2026.json contains 45.50)"
fi
AUTH=()
if [ -n "${EVORULE_AUTH_TOKEN:-}" ]; then AUTH=(-H "Authorization: Bearer $EVORULE_AUTH_TOKEN"); fi
# GET /api/sessions/{id}/audit/verify -> AuditVerify {verified, fact_count, last_hash}
VERIFY_JSON=$(curl -s "${AUTH[@]}" "$SERVER_URL/api/sessions/$SESSION_ID/audit/verify")
echo "audit chain verify: $VERIFY_JSON"
if ! echo "$VERIFY_JSON" | grep -Eq '"verified"[[:space:]]*:[[:space:]]*true'; then
  echo "FAIL: audit chain not verified"
  exit 1
fi
# GET /api/sessions/{id}/audit -> full fact chain
echo
echo "-- audit report (fact chain) --"
curl -s "${AUTH[@]}" "$SERVER_URL/api/sessions/$SESSION_ID/audit" | head -c 4000
echo

if [ "$BIZ_OK" -ne 1 ]; then
  echo "== RESULT: FAIL (business task not completed) =="
  exit 1
fi

echo
echo "============================================================="
echo " DEMO COMPLETE"
echo " - finance session ran with every LLM/tool call turned into"
echo "   auditable facts by the evorule engine"
echo " - audit/verify checked the fact hash chain"
echo " - business artifact on disk: workspace/expenses_2026.json"
echo "   (45.50 entry present - the task really happened)"
echo " - browse the audit trail: $SERVER_URL"
echo " - stop the demo server:  pkill -f evorule-server"
echo " - reset anytime: delete the .demo folder"
echo "============================================================="
