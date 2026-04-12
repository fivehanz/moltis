#!/usr/bin/env bash
# Fail if any Rust source file exceeds MAX_LINES (unless allowlisted).
# Allowlisted files are tracked for decomposition — remove entries as they're split.

set -euo pipefail

MAX_LINES=1500

# Files queued for decomposition — remove as they're split below the limit.
ALLOW=(
  crates/agents/src/prompt.rs
  crates/agents/src/runner.rs
  crates/auth/src/credential_store.rs
  crates/channels/src/plugin.rs
  crates/chat/src/compaction.rs
  crates/chat/src/lib.rs
  crates/cli/src/doctor_commands.rs
  crates/config/src/loader.rs
  crates/config/src/schema.rs
  crates/config/src/validate.rs
  crates/cron/src/service.rs
  crates/discord/src/handler.rs
  crates/gateway/src/channel_events.rs
  crates/gateway/src/local_llm_setup.rs
  crates/gateway/src/methods/services.rs
  crates/gateway/src/server.rs
  crates/gateway/src/services.rs
  crates/gateway/src/session.rs
  crates/httpd/src/auth_routes.rs
  crates/httpd/src/server.rs
  crates/httpd/tests/auth_middleware.rs
  crates/matrix/src/handler.rs
  crates/openclaw-import/src/sessions.rs
  crates/provider-setup/src/lib.rs
  crates/providers/src/anthropic.rs
  crates/providers/src/github_copilot.rs
  crates/providers/src/lib.rs
  crates/providers/src/local_gguf/models.rs
  crates/providers/src/local_llm/backend.rs
  crates/providers/src/openai.rs
  crates/providers/src/openai_compat.rs
  crates/providers/src/openai_codex.rs
  crates/service-traits/src/lib.rs
  crates/sessions/src/metadata.rs
  crates/swift-bridge/src/lib.rs
  crates/telegram/src/handlers.rs
  crates/telegram/src/outbound.rs
  crates/tools/src/cron_tool.rs
  crates/tools/src/exec.rs
  crates/tools/src/sandbox/tests.rs
  crates/tools/src/skill_tools.rs
  crates/web/src/terminal.rs
)

# Build associative array for O(1) lookup.
declare -A ALLOWED
for f in "${ALLOW[@]}"; do
  ALLOWED["$f"]=1
done

violations=0

while IFS=$'\t' read -r lines file; do
  rel="${file#./}"
  if [[ -n "${ALLOWED[$rel]:-}" ]]; then
    continue
  fi
  echo "FAIL: $rel ($lines lines > $MAX_LINES)"
  violations=$((violations + 1))
done < <(
  find . -name '*.rs' \
    -not -path './target/*' \
    -not -path './.claude/*' \
    -print0 \
  | xargs -0 wc -l \
  | awk -v max="$MAX_LINES" '$2 != "total" && $1 > max { printf "%d\t%s\n", $1, $2 }' \
  | sort -rn
)

if [[ $violations -gt 0 ]]; then
  echo ""
  echo "$violations file(s) exceed $MAX_LINES lines."
  echo "Split them into smaller modules or add to the allowlist in $0."
  exit 1
fi

echo "All Rust files within $MAX_LINES-line limit (${#ALLOW[@]} allowlisted)."
