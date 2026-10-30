#!/usr/bin/env bash
# Fake official agent CLIs for atr tests. Emits the JSON shapes seen on the Mini.
set -euo pipefail

KIND=$(basename "$0")
case "$KIND" in
  claude) VER="${ATR_FAKE_CLAUDE_VERSION:-2.1.287}" ;;
  grok) VER="${ATR_FAKE_GROK_VERSION:-1.0.52}" ;;
  cursor-agent) VER="${ATR_FAKE_CURSOR_VERSION:-2026.10.01-e373342}" ;;
  codex) VER="${ATR_FAKE_CODEX_VERSION:-0.162.1}" ;;
  opencode) VER="${ATR_FAKE_OPENCODE_VERSION:-1.0.132}" ;;
  *) VER="0.0.0" ;;
esac

if [[ "${1:-}" == "--version" || "${1:-}" == "-V" ]]; then
  echo "$KIND $VER"
  exit 0
fi

if [[ "${1:-}" == "models" ]]; then
  # Live Rs-MBP shapes. Parser must take real ids and drop headers.
  if [[ "$KIND" == "grok" ]]; then
    cat <<'EOF'
You are logged in with grok.com.

Default model: grok-4.7

Available models:
  * grok-4.7 (default)
  - grok-4.7-build-fast
  - grok-4.6
  - grok-4.5
  - cline-pass-glm-5.2
  - cline-pass-deepseek-v4-pro
  - cline-pass-mimo-v2.5-pro
  - cline-pass-kimi-k3
  - cline-pass-qwen3.8-max
  - cline-pass-minimax-m3
  - cursor-composer-2.5
  - deepseek-v4-flash
  - cursor-claude-opus-5-5
  - cursor-gpt-5.6-sol
  - cursor-grok-4.7
EOF
    exit 0
  fi
  if [[ "$KIND" == "cursor-agent" || "$KIND" == "agent" ]]; then
    cat <<'EOF'
Available models

auto - Auto (default)
composer-2.5 - Composer 2.5
composer-2.5-fast - Composer 2.5 Fast
gpt-5.6-sol-high - GPT-5.6 Sol 1M High
EOF
    exit 0
  fi
  if [[ "$KIND" == "opencode" ]]; then
    echo "cursorapi/composer-2.5"
    echo "openai/gpt-5.6"
    echo "test-model"
    echo "test-model-2"
    exit 0
  fi
  echo "test-model"
  echo "test-model-2"
  exit 0
fi

if [[ "$KIND" == "opencode" && "${1:-}" == "auth" ]]; then
  echo "moonshotai"
  exit 0
fi

# Grok billing probe: `grok agent stdio` + initialize + `_x.ai/billing`.
# Use python3 -c so the JSON-RPC stream stays on stdin (a heredoc would consume it).
# Exit on stdin EOF before the delayed billing reply (real grok does this).
# initialize requires params.protocolVersion == 1 (else -32602).
if [[ "$KIND" == "grok" && "${1:-}" == "agent" && "${2:-}" == "stdio" ]]; then
  python3 -c '
import json, select, sys, time
sample = {"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-10-06T00:00:00Z","end":"2026-10-13T00:00:00Z"},"prepaidBalance":{"val":14032},"creditUsagePercent":1.0},"subscription_tier":"pro"}
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        o = json.loads(line)
    except Exception:
        continue
    if o.get("method") == "initialize":
        params = o.get("params") or {}
        if params.get("protocolVersion") != 1:
            print(json.dumps({"jsonrpc":"2.0","id":o.get("id"),"error":{"code":-32602,"message":"missing protocolVersion"}}), flush=True)
        else:
            print(json.dumps({"jsonrpc":"2.0","id":o.get("id"),"result":{}}), flush=True)
    if o.get("method") == "_x.ai/billing":
        time.sleep(0.35)
        r, _, _ = select.select([sys.stdin], [], [], 0)
        if r:
            extra = sys.stdin.readline()
            if extra == "":
                sys.exit(0)
        print(json.dumps({"jsonrpc":"2.0","id":o.get("id"),"result":sample}), flush=True)
        break
'
  exit 0
fi

SELF_DIR=$(cd "$(dirname "$0")" && pwd)
if [[ -d "$SELF_DIR/../fake" ]]; then
  DIR=$(cd "$SELF_DIR/../fake" && pwd)
else
  DIR="${ATR_FAKE_DIR:-/tmp/atr-fake}"
fi
mkdir -p "$DIR"

if [[ "${ATR_SHADOW:-}" == "1" ]]; then
  {
    echo "ATR_SHADOW=${ATR_SHADOW}"
    echo "GH_TOKEN=${GH_TOKEN:-}"
    echo "GITHUB_TOKEN=${GITHUB_TOKEN:-}"
    echo "ATR_SHADOW_GH_TOKEN=${ATR_SHADOW_GH_TOKEN:-}"
  } > "$DIR/${KIND}.env"
fi

python3 - "$KIND" "$$" "$@" <<'PY' >>"$DIR/invocations.jsonl"
import json, sys
kind = sys.argv[1]
pid = int(sys.argv[2])
args = sys.argv[3:]
json.dump({"kind": kind, "pid": pid, "args": args}, sys.stdout)
sys.stdout.write("\n")
PY

SESSION=""
args=("$@")
i=0
while [[ $i -lt ${#args[@]} ]]; do
  a="${args[$i]}"
  if [[ "$a" == "--resume" || "$a" == "-r" || "$a" == "--session-id" || "$a" == "--session" || "$a" == "-s" ]]; then
    i=$((i + 1))
    SESSION="${args[$i]:-}"
  elif [[ "$a" == --session-id=* ]]; then
    SESSION="${a#--session-id=}"
  elif [[ "$a" == "resume" && $i -gt 0 && "${args[$((i-1))]}" == "exec" ]]; then
    i=$((i + 1))
    SESSION="${args[$i]:-}"
  fi
  i=$((i + 1))
done

BEH="success"
if [[ -f "$DIR/${KIND}.behavior" ]]; then
  BEH=$(cat "$DIR/${KIND}.behavior")
elif [[ -f "$DIR/behavior" ]]; then
  BEH=$(cat "$DIR/behavior")
fi
set -- $BEH
ACTION=${1:-success}
SLEEP=${2:-0}

# Opt-in: record each prompt read from stdin (tests that assert prompt text).
if [[ -f "$DIR/${KIND}.capture" ]]; then
  { cat; echo; echo "=====ATR-FAKE-END====="; } >> "$DIR/${KIND}.stdin"
fi

if [[ "$ACTION" == "hold_session" ]]; then
  # Like hold, but report a session id first (a resumable live turn).
  echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"${KIND}-hold-sid\"}"
  ACTION=hold
fi

if [[ "$ACTION" == "hold" ]]; then
  # Stay live until the test writes $DIR/${KIND}.release. Heartbeats keep the runner from stalling.
  python3 - "$DIR/${KIND}.release" <<'PY'
import os, sys, time
path = sys.argv[1]
deadline = time.time() + 60
while time.time() < deadline:
    if os.path.exists(path):
        break
    print('{"type":"system","subtype":"progress"}', flush=True)
    time.sleep(0.1)
else:
    sys.exit(2)
PY
elif [[ "$SLEEP" != "0" && -n "$SLEEP" && "$ACTION" != "stall" && "$ACTION" != "edit_stall" ]]; then
  # Keep the stream alive so the runner heartbeat does not fire mid-sleep.
  python3 - "$SLEEP" <<'PY'
import sys, time
total = float(sys.argv[1])
end = time.time() + total
while time.time() < end:
    print('{"type":"system","subtype":"progress"}', flush=True)
    time.sleep(min(0.15, max(0.05, end - time.time())))
PY
fi

sid="${SESSION:-${KIND}-sess-1}"

emit_success() {
  case "$KIND" in
    claude)
      echo '{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}'
      echo '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1791968400,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0,"resetsAt":1791968400},"seven_day":{"utilization":0.03,"resetsAt":1792195200}}}}'
      cat <<EOF
{"type":"result","subtype":"success","is_error":false,"result":"done — noted issue 429 and the quota in AGENTS.md","session_id":"${sid}","total_cost_usd":0.0123,"usage":{"input_tokens":100,"cache_creation_input_tokens":20,"cache_read_input_tokens":800,"output_tokens":50}}
EOF
      ;;
    grok)
      # Real grok repeats running totals; atr must store one row (last wins).
      echo '{"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5,"output_tokens":12,"reasoning_tokens":3,"total_tokens":120}}'
      echo '{"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5,"output_tokens":12,"reasoning_tokens":3,"total_tokens":120}}'
      cat <<EOF
{"text":"done","sessionId":"${sid}","usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5,"output_tokens":12,"reasoning_tokens":3,"total_tokens":120},"total_cost_usd":0.004}
EOF
      ;;
    cursor-agent)
      echo '{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]},"usage":{"inputTokens":80,"outputTokens":40,"cacheReadTokens":400,"cacheWriteTokens":15}}'
      cat <<EOF
{"type":"result","subtype":"success","result":"done","session_id":"${sid}","usage":{"inputTokens":80,"outputTokens":40,"cacheReadTokens":400,"cacheWriteTokens":15}}
EOF
      ;;
    codex)
      echo "{\"type\":\"thread.started\",\"thread_id\":\"${sid}\"}"
      echo '{"type":"item.completed","usage":{"input_tokens":200,"cached_input_tokens":160,"cache_write_input_tokens":10,"output_tokens":30,"reasoning_output_tokens":8}}'
      echo '{"type":"turn.completed","usage":{"input_tokens":200,"cached_input_tokens":160,"cache_write_input_tokens":10,"output_tokens":30,"reasoning_output_tokens":8}}'
      ;;
    opencode)
      echo "{\"type\":\"step_start\",\"sessionID\":\"${sid}\",\"part\":{\"type\":\"step-start\"}}"
      cat <<EOF
{"type":"step_finish","sessionID":"${sid}","part":{"type":"step-finish","reason":"stop","tokens":{"input":50,"output":10,"reasoning":0,"cache":{"read":100,"write":5}}}}
EOF
      ;;
    *)
      echo '{"type":"result","result":"ok"}'
      ;;
  esac
}

# atr lessons reflect run (its --json-schema names "lessons"): reply with
# $DIR/reflect.json when the test wrote one.
REFLECT=0
for a in "${args[@]}"; do
  if [[ "$a" == *'"lessons"'* ]]; then
    REFLECT=1
  fi
done
if [[ "$REFLECT" == "1" && -f "$DIR/reflect.json" ]]; then
  echo "reflect" >> "$DIR/reflect.calls"
  python3 - "$(cat "$DIR/reflect.json")" "$sid" <<'PY'
import json, sys
text, sid = sys.argv[1], sys.argv[2]
print(json.dumps({"type":"assistant","message":{"content":[{"type":"text","text":text}]}}), flush=True)
print(json.dumps({"type":"result","subtype":"success","is_error":False,"result":text,"session_id":sid,"total_cost_usd":0.001,"usage":{"input_tokens":30,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":9}}), flush=True)
PY
  exit 0
fi

case "$ACTION" in
  stall)
    # Real Grok only prints the session id on the final result. A killed
    # stall therefore has none unless atr passed --session-id itself.
    # Emit a session-less progress line so the heartbeat (not startup)
    # timeout fires and the generated --session-id can be resumed.
    if [[ "$KIND" != "grok" ]]; then
      echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"${sid}\"}"
    else
      echo '{"type":"system","subtype":"progress"}'
    fi
    # Child in its own session, like cursor-agent's shell tool.
    python3 - "$DIR/${KIND}.orphans" <<'PY' &
import os, sys, time
path = sys.argv[1]
pid = os.fork()
if pid == 0:
    os.setsid()
    with open(path, "a") as f:
        f.write(str(os.getpid()) + "\n")
        f.flush()
    time.sleep(600)
    os._exit(0)
# Stay alive so the setsid child is still in the CLI's process tree
# (cursor-agent does the same: the agent stays up while its shell runs).
time.sleep(600)
PY
    for _ in $(seq 1 40); do
      if [[ -s "$DIR/${KIND}.orphans" ]]; then
        break
      fi
      sleep 0.05
    done
    sleep 1000
    ;;
  edit_stall)
    # Stall after a real worktree edit so failover handoff can include a diff.
    if [[ -f README.md ]]; then
      printf '\natr-handoff-note\n' >> README.md
    else
      printf 'atr-handoff-note\n' > NOTES.md
    fi
    echo '{"type":"assistant","message":{"content":[{"type":"text","text":"I added atr-handoff-note to README.md and will keep going."}]}}'
    echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"printf \\\\natr-handoff-note\\\\n >> README.md"}}]}}'
    echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}'
    printf 'brand-new-untracked\n' > ATR_NEW.md
    sleep 1000
    ;;
  permission_denied)
    echo '{"type":"assistant","message":{"content":[{"type":"text","text":"I cannot edit files without permission."}]}}'
    cat <<EOF
{"type":"result","subtype":"success","is_error":false,"result":"I wasn't able to finish — don't have permission to edit files","permission_denials":[{"tool_name":"Edit","tool_use_id":"t1"}],"session_id":"${sid}","total_cost_usd":0.01,"usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":8}}
EOF
    exit 0
    ;;
  rate_limit)
    echo "rate limit exceeded (429)" >&2
    if [[ "$KIND" == "codex" ]]; then
      echo '{"type":"turn.failed","error":{"message":"rate limit exceeded (429)"}}'
    else
      echo '{"type":"result","is_error":true,"result":"rate limit exceeded (429) too many requests"}'
    fi
    exit 1
    ;;
  unavailable)
    echo '{"type":"result","is_error":true,"result":"connection reset by peer"}'
    exit 1
    ;;
  auth)
    echo "authentication failed: unauthorized / not logged in" >&2
    echo '{"type":"result","is_error":true,"result":"authentication failed: unauthorized"}'
    exit 1
    ;;
  server)
    echo "502 bad gateway internal server error" >&2
    echo '{"type":"result","is_error":true,"result":"internal server error 502"}'
    exit 1
    ;;
  leave_orphan)
    echo '{"type":"system","subtype":"progress"}'
    python3 - "$DIR/${KIND}.orphans" >/dev/null 2>&1 <<'PY' &
import os, sys, time
path = sys.argv[1]
pid = os.fork()
if pid == 0:
    os.setsid()
    with open(path, "a") as f:
        f.write(str(os.getpid()) + "\n")
        f.flush()
    time.sleep(600)
    os._exit(0)
# Stay in the CLI tree so a delayed runner snapshot still sees the grandchild.
time.sleep(600)
os._exit(0)
PY
    for _ in $(seq 1 40); do
      echo '{"type":"system","subtype":"progress"}'
      if [[ -s "$DIR/${KIND}.orphans" ]]; then
        # Keep emitting while bash is alive so the runner snapshots the
        # live bash → python → setsid tree before we exit.
        for _ in $(seq 1 12); do
          echo '{"type":"system","subtype":"progress"}'
          sleep 0.1
        done
        emit_success
        exit 0
      fi
      sleep 0.05
    done
    emit_success
    exit 0
    ;;
  weaken_tests)
    mkdir -p tests .atr
    if [[ -f tests/check.sh ]]; then
      printf '#!/bin/sh\n# AGENT\nexit 0\n' > tests/check.sh
    fi
    printf -- '- [x] weaken the failing test\n- [ ] leftover\n' > .atr/checklist.md
    emit_success
    exit 0
    ;;
  edit_lines)
    # Compare tests: commit $DIR/${KIND}.lines lines (default 1) to
    # CHANGE_${KIND}.md, then try to push (recorded in $DIR/${KIND}.push).
    # Only inside the harness worktree root: side calls such as
    # `grok usage <sid>` run from the test's cwd and must not touch git.
    case "$(pwd -P)/" in
      "$(cd "$SELF_DIR/.." && pwd -P)/work/"*) ;;
      *) emit_success; exit 0 ;;
    esac
    n=1
    if [[ -f "$DIR/${KIND}.lines" ]]; then
      n=$(cat "$DIR/${KIND}.lines")
    fi
    : > "CHANGE_${KIND}.md"
    for i in $(seq 1 "$n"); do
      echo "line $i from $KIND" >> "CHANGE_${KIND}.md"
    done
    git add -A >/dev/null 2>&1
    git -c user.name=fake -c user.email=fake@example.invalid commit -qm "change from $KIND" >/dev/null 2>&1 || true
    if git push -q origin HEAD >/dev/null 2>&1; then
      echo pushed > "$DIR/${KIND}.push"
    else
      echo blocked > "$DIR/${KIND}.push"
    fi
    emit_success
    exit 0
    ;;
  edit_success)
    if [[ -f README.md ]]; then
      printf '\nshadow-edit\n' >> README.md
    else
      printf 'shadow-edit\n' > NOTES.md
    fi
    printf 'untracked-a\n' > ATR_A.md
    printf 'untracked-b\n' > ATR_B.md
    emit_success
    exit 0
    ;;
  assistant)
    TEXT="${ATR_FAKE_ASSISTANT_TEXT:-Add Kimi with the built-in preset (key → shadow.env).
ATR: /providers disable grok}"
    python3 - "$TEXT" "$sid" <<'PY'
import json, sys
text, sid = sys.argv[1], sys.argv[2]
print(json.dumps({"type":"assistant","message":{"content":[{"type":"text","text":text}]}}), flush=True)
print(json.dumps({"type":"result","subtype":"success","is_error":False,"result":text,"session_id":sid,"total_cost_usd":0.01,"usage":{"input_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":10,"output_tokens":8}}), flush=True)
PY
    exit 0
    ;;
  success|*)
    emit_success
    exit 0
    ;;
esac
