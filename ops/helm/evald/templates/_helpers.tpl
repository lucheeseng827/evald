{{- define "evald.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "evald.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "evald.labels" -}}
app.kubernetes.io/name: {{ include "evald.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "evald.selector" -}}
app.kubernetes.io/name: {{ include "evald.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* The Secret holding the tokens, when auth is on; empty when it is off. */}}
{{- define "evald.authSecret" -}}
{{- if .Values.auth.existingSecret -}}
{{- .Values.auth.existingSecret -}}
{{- else if .Values.auth.create -}}
{{- printf "%s-auth" (include "evald.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/*
Refusals: configurations that would deploy something that comes up healthy and is wrong.
*/}}
{{- define "evald.guards" -}}
{{- if and (hasKey .Values "replicas") (ne (int .Values.replicas) 1) -}}
{{- fail "evald runs one replica per store: the store's lock is exclusive, so a second pod could never open it. Run one release per store." -}}
{{- end -}}
{{- if and .Values.auth.create .Values.auth.existingSecret -}}
{{- fail "auth: set auth.existingSecret or auth.create, not both" -}}
{{- end -}}
{{- if .Values.auth.create -}}
{{- if lt (len .Values.auth.token) 16 -}}
{{- fail "auth.create needs auth.token of 16 or more printable characters" -}}
{{- end -}}
{{- end -}}
{{- if and (has .Values.service.type (list "LoadBalancer" "NodePort")) (not (include "evald.authSecret" .)) -}}
{{- fail (printf "service.type %s exposes evald beyond the cluster with no authentication: set auth.existingSecret (or auth.create) first" .Values.service.type) -}}
{{- end -}}
{{- if not (has .Values.service.type (list "ClusterIP" "LoadBalancer" "NodePort")) -}}
{{- fail (printf "service.type must be ClusterIP, LoadBalancer or NodePort (got %s)" .Values.service.type) -}}
{{- end -}}
{{- end -}}
