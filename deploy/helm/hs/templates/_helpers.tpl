{{/*
Chart name, truncated and DNS-1123-safe.
*/}}
{{- define "hs.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Fully qualified app name.
*/}}
{{- define "hs.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "hs.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Standard labels, following https://helm.sh/docs/chart_best_practices/labels/.
*/}}
{{- define "hs.labels" -}}
helm.sh/chart: {{ include "hs.chart" . }}
{{ include "hs.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "hs.selectorLabels" -}}
app.kubernetes.io/name: {{ include "hs.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "hs.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{ default (include "hs.fullname" .) .Values.serviceAccount.name }}
{{- else -}}
{{ default "default" .Values.serviceAccount.name }}
{{- end -}}
{{- end -}}

{{/*
Number of replicas: singleNode mode is always exactly 1, regardless of replicaCount.
*/}}
{{- define "hs.replicaCount" -}}
{{- if eq .Values.mode "singleNode" -}}
1
{{- else -}}
{{- .Values.replicaCount -}}
{{- end -}}
{{- end -}}

{{/*
Effective storage backend: singleNode mode forces `embedded` regardless of storage.backend.
*/}}
{{- define "hs.storageBackend" -}}
{{- if eq .Values.mode "singleNode" -}}
embedded
{{- else -}}
{{- .Values.storage.backend -}}
{{- end -}}
{{- end -}}

{{/*
The image reference, from either a tag or a digest.
*/}}
{{- define "hs.image" -}}
{{- if .Values.image.digest -}}
{{- printf "%s/%s@%s" .Values.image.registry .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s/%s:%s" .Values.image.registry .Values.image.repository (.Values.image.tag | default .Chart.AppVersion) -}}
{{- end -}}
{{- end -}}

{{- define "hs.imagePullPolicy" -}}
{{- if .Values.image.pullPolicy -}}
{{- .Values.image.pullPolicy -}}
{{- else if .Values.image.digest -}}
IfNotPresent
{{- else -}}
Always
{{- end -}}
{{- end -}}

{{/*
Fails the render with a clear message if a required value is missing, rather than producing a
Deployment/StatefulSet that will CrashLoopBackOff on `hs serve`'s own config validation.
*/}}
{{- define "hs.validate" -}}
{{- if not .Values.serverName -}}
{{- fail "serverName is required (the hs-config server.server_name; see values.yaml)" -}}
{{- end -}}
{{- if and (eq (include "hs.storageBackend" .) "postgres") (not .Values.cloudNativePG.enabled) (not .Values.storage.postgres.host) -}}
{{- fail "storage.backend is postgres but neither cloudNativePG.enabled nor storage.postgres.host is set" -}}
{{- end -}}
{{- if not .Values.secrets.signingKey.existingSecret -}}
{{- fail "secrets.signingKey.existingSecret is required; generate one with `hs generate-signing-key` and create the Secret out of band (this chart never generates or stores a signing key itself, since it must survive `helm uninstall`/reinstall)" -}}
{{- end -}}
{{- end -}}
