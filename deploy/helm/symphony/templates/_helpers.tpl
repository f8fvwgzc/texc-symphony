{{- define "symphony.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "symphony.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "symphony.selector" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* LLM_* variables from global.llm values that are set (they win over the ConfigMap). */}}
{{- define "symphony.llmEnv" -}}
{{- $llm := .Values.global.llm -}}
{{- $vars := dict "LLM_PROVIDER" $llm.provider "LLM_BASE_URL" $llm.baseUrl "LLM_MODEL" $llm.model "LLM_MODEL_SMALL" $llm.modelSmall "LLM_TIMEOUT_SECS" (toString $llm.timeoutSecs) "LLM_MAX_RETRIES" (toString $llm.maxRetries) "LLM_GATEWAY_URL" $llm.gatewayUrl -}}
{{- range $name := keys $vars | sortAlpha }}
{{- $value := get $vars $name }}
{{- if $value }}
- name: {{ $name }}
  value: {{ $value | quote }}
{{- end }}
{{- end }}
{{- if and $llm.secret $llm.apiKeySecretKey (not $llm.gatewayUrl) }}
- name: LLM_API_KEY_FILE
  value: {{ printf "/var/run/secrets/llm/%s" $llm.apiKeySecretKey | quote }}
{{- end }}
{{- if and $llm.secret $llm.gatewayTokenSecretKey }}
- name: LLM_GATEWAY_TOKEN_FILE
  value: {{ printf "/var/run/secrets/llm/%s" $llm.gatewayTokenSecretKey | quote }}
{{- end }}
{{- end -}}

{{/* Secret keys to mount: only the gateway token in gateway mode, so no provider key is in the pod. */}}
{{- define "symphony.llmSecretKeys" -}}
{{- $llm := .Values.global.llm -}}
{{- $keys := list -}}
{{- if and $llm.apiKeySecretKey (not $llm.gatewayUrl) }}{{ $keys = append $keys $llm.apiKeySecretKey }}{{ end -}}
{{- if $llm.gatewayTokenSecretKey }}{{ $keys = append $keys $llm.gatewayTokenSecretKey }}{{ end -}}
{{- join "," $keys -}}
{{- end -}}
