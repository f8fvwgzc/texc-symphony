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

out=$(helm template t "$chart")
lacks "$out" "kind: NetworkPolicy" "the NetworkPolicy is opt-in"
has "$out" "drop:" "hardened container by default"

values=$(mktemp)
trap 'rm -f "$values"' EXIT
cat >"$values" <<'YAML'
networkPolicy:
  enabled: true
  egress:
    - to:
        - podSelector:
            matchLabels:
              app.kubernetes.io/name: llm-gateway
      ports:
        - protocol: TCP
          port: 8080
extraVolumes:
  - name: github-app
    secret:
      secretName: github-app-key
      defaultMode: 0400
extraVolumeMounts:
  - name: github-app
    mountPath: /var/run/secrets/github-app
    readOnly: true
YAML
out=$(helm template t "$chart" -f "$values")
has "$out" "kind: NetworkPolicy" "networkPolicy.enabled"
has "$out" "name: t-symphony-egress" "networkPolicy.enabled"
has "$out" "- Egress" "egress only"
has "$out" "k8s-app: kube-dns" "DNS allowed"
has "$out" "protocol: UDP" "DNS over UDP"
has "$out" "app.kubernetes.io/name: llm-gateway" "extra egress rule"
has "$out" "port: 8080" "extra egress rule"
lacks "$out" "- Ingress" "ingress untouched"
has "$out" "secretName: github-app-key" "extra volume"
has "$out" "defaultMode: 256" "the App key is readable by its owner only (0400)"
has "$out" "mountPath: /var/run/secrets/github-app" "extra volume mount"
helm lint "$chart" -f "$values" >/dev/null

out=$(helm template t "$chart")
has "$out" "fieldPath: metadata.name" "replica id"
has "$out" "kind: PersistentVolumeClaim" "one replica keeps volumes"
lacks "$out" "SYMPHONY_DATABASE_URL" "sqlite by default"

out=$(helm template t "$chart" --set replicaCount=3 --set database.existingSecret=symphony-db)
has "$out" "replicas: 3" "replicas"
has "$out" "name: SYMPHONY_DATABASE_URL" "postgres"
has "$out" "type: RollingUpdate" "postgres"
lacks "$out" "kind: PersistentVolumeClaim" "replicas use emptyDir"
out=$(helm template t "$chart" --set database.existingSecret=symphony-db)
has "$out" "type: Recreate" "one replica on PostgreSQL keeps its volumes: no overlapping pods"
has "$out" "kind: PersistentVolumeClaim" "one replica on PostgreSQL keeps its volumes"
if helm template t "$chart" --set replicaCount=2 >/dev/null 2>&1; then
  fail "replicas without PostgreSQL must be refused"
fi

echo "helm chart ok"
