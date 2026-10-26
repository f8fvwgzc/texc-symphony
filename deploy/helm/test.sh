#!/usr/bin/env bash
# Lints the chart and checks how it renders the LLM connection contract (global.llm.*).
set -euo pipefail
chart="$(cd "$(dirname "$0")" && pwd)/symphony"

fail() { echo "helm test: $*" >&2; exit 1; }
has() { grep -qF -- "$2" <<<"$1" || fail "$3: missing '$2'"; }
lacks() { ! grep -qF -- "$2" <<<"$1" || fail "$3: unexpected '$2'"; }

helm lint "$chart" >/dev/null

out=$(helm template t "$chart")
has "$out" "name: llm-config" "default"
has "$out" "secretName: llm-credentials" "default"
has "$out" 'value: "/var/run/secrets/llm/api-key"' "default"
has "$out" "- key: api-key" "default"
lacks "$out" "i-understand-that" "the acknowledgement is opt-in"
lacks "$out" "LLM_PROVIDER" "no explicit provider by default"

out=$(helm template t "$chart" --set acknowledgeGuardrails=true \
  --set global.llm.provider=openai-compatible --set global.llm.baseUrl=http://vllm:8000/v1 \
  --set global.llm.model=qwen3-coder --set global.llm.maxRetries=5 \
  --set global.llm.apiKeySecretKey=)
has "$out" "--i-understand-that-this-will-be-running-without-the-usual-guardrails" "acknowledged"
has "$out" 'value: "openai-compatible"' "explicit provider"
has "$out" 'value: "5"' "explicit retries"
lacks "$out" "LLM_API_KEY_FILE" "keyless provider"

out=$(helm template t "$chart" --set global.llm.gatewayUrl=http://attache-gateway:8080 \
  --set global.llm.model=claude-sonnet-4-5)
has "$out" 'value: "http://attache-gateway:8080"' "gateway"
has "$out" 'value: "/var/run/secrets/llm/gateway-token"' "gateway"
lacks "$out" "api-key" "gateway mode mounts no provider key"

echo "helm chart ok"
