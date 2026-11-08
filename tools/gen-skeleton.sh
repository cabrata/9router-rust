#!/usr/bin/env bash
# One-time wave-0 generator for executor crate stubs. Kept for reference; do not re-run
# over ported code (it overwrites module files).
set -euo pipefail
cd "$(dirname "$0")/../crates"

gen_crate() { # name desc deps...
  local name=$1 desc=$2; shift 2; mkdir -p "$name/src"
  {
    printf '[package]\nname = "%s"\ndescription = "%s"\nversion.workspace = true\nedition.workspace = true\nrust-version.workspace = true\nlicense.workspace = true\n\n[dependencies]\n' "$name" "$desc"
    for d in "$@"; do printf '%s.workspace = true\n' "$d"; done
  } > "$name/Cargo.toml"
}

EXEC_DEPS="nr-core nr-data nr-common nr-oauth nr-translator nr-exec-base async-trait base64 bytes chrono futures hex hmac http indexmap rand regex reqwest serde serde_json sha2 tokio tokio-util tracing url uuid"
declare -A GROUP=(
  [nr-exec-simple]="azure:azure vertex:vertex gemini_cli:gemini-cli iflow:iflow kimchi:kimchi ollama_local:ollama-local xiaomi_tokenplan:xiaomi-tokenplan xiaomi_mimo:xiaomi-mimo mimo_free:mimo-free minimax_code:minimax-code codebuddy_cn:codebuddy-cn codebuddy_intl:codebuddy-intl github:github codex:codex commandcode:commandcode opencode:opencode opencode_go:opencode-go opencode_zen:opencode-zen"
  [nr-exec-binary]="kiro:kiro bedrock:bedrock cursor:cursor"
  [nr-exec-ide]="qoder:qoder zed:zed trae:trae windsurf:windsurf antigravity:antigravity"
  [nr-exec-cli]="devin_cli:devin-cli perchai:perchai grok_cli:grok-cli"
  [nr-exec-webchat]="chatgpt_web:chatgpt-web deepseek_web:deepseek-web perplexity_web:perplexity-web uc_web:uc-web"
  [nr-exec-browser]="gemini_web:gemini-web grok_web:grok-web muse_web:muse-web veoaifree_web:veoaifree-web"
)

for crate in "${!GROUP[@]}"; do
  # shellcheck disable=SC2086
  gen_crate "$crate" "Specialized executors (port of open-sse/executors)" $EXEC_DEPS
  {
    printf '//! Specialized executors — port of open-sse/executors. Ownership: see AGENTS.md.\n//!\n//! Each executor module exposes `pub fn create(provider: &str) -> Arc<dyn Executor>`.\n\n'
    for pair in ${GROUP[$crate]}; do printf 'pub mod %s;\n' "${pair%%:*}"; done
  } > "$crate/src/lib.rs"
  for pair in ${GROUP[$crate]}; do
    mod=${pair%%:*}; js=${pair##*:}
    printf '//! Port of open-sse/executors/%s.js.\n//! Wave-0 stub: delegates to DefaultExecutor until ported.\n\nuse std::sync::Arc;\n\nuse nr_core::Executor;\n\n/// Executor for the canonical provider id (aliases are resolved in nr-executors).\npub fn create(provider: &str) -> Arc<dyn Executor> {\n    nr_exec_base::default_executor::create(provider)\n}\n' "$js" > "$crate/src/$mod.rs"
  done
done
