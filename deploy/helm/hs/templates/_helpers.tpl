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

{{/*
The bridge operator's labels. A name of its own (`<name>-bridges-operator`), so nothing that
selects the server's pods by hs.selectorLabels selects the operator's too.
*/}}
{{- define "hs.bridgesOperatorSelectorLabels" -}}
app.kubernetes.io/name: {{ printf "%s-bridges-operator" (include "hs.name" .) | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "hs.bridgesOperatorLabels" -}}
helm.sh/chart: {{ include "hs.chart" . }}
{{ include "hs.bridgesOperatorSelectorLabels" . }}
app.kubernetes.io/component: bridges-operator
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
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
{{/*
Where local media lives. On the embedded data volume when there is one and no dedicated claim;
otherwise on the dedicated claim's own mount.
*/}}
{{- define "hs.mediaPath" -}}
{{- if .Values.media.storage.local.existingClaim -}}
/var/lib/hs/media
{{- else -}}
/var/lib/hs/data/media
{{- end -}}
{{- end -}}

{{- define "hs.validate" -}}
{{- if not (has .Values.media.storage.backend (list "local" "s3")) -}}
{{- fail (printf "media.storage.backend must be `local` or `s3`, not %q" .Values.media.storage.backend) -}}
{{- end -}}
{{- if and (eq .Values.media.storage.backend "s3") (not .Values.media.storage.s3.bucket) -}}
{{- fail "media.storage.backend is s3 but media.storage.s3.bucket is not set" -}}
{{- end -}}
{{- if and (eq .Values.media.storage.backend "local") (ne (include "hs.storageBackend" .) "embedded") (not .Values.media.storage.local.existingClaim) -}}
{{- fail "media.storage.backend is local, but this deployment has no data volume to keep media on (storage.backend is not embedded) and its replicas would not share one anyway: set media.storage.backend to s3, or point media.storage.local.existingClaim at a ReadWriteMany claim" -}}
{{- end -}}
{{- if not .Values.serverName -}}
{{- fail "serverName is required (the hs-config server.server_name; see values.yaml)" -}}
{{- end -}}
{{- if and (eq (include "hs.storageBackend" .) "postgres") (not .Values.cloudNativePG.enabled) (not .Values.storage.postgres.host) -}}
{{- fail "storage.backend is postgres but neither cloudNativePG.enabled nor storage.postgres.host is set" -}}
{{- end -}}
{{- if and (eq .Values.mode "cluster") (not .Values.secrets.signingKey.existingSecret) -}}
{{- fail "secrets.signingKey.existingSecret is required in cluster mode: replicas share no volume and must sign with the same key. Generate one with `hs generate-signing-key -o signing.key` and `kubectl create secret generic <name> --from-file=signing.key`. (singleNode mode generates its key on the data volume and needs no Secret.)" -}}
{{- end -}}
{{- if and (eq .Values.mode "cluster") (eq .Values.storage.backend "embedded") -}}
{{- fail "mode is cluster but storage.backend is embedded: replicas must share one database. Set storage.backend to postgres (with cloudNativePG.enabled or storage.postgres.host) or slatedb." -}}
{{- end -}}
{{- if and (eq .Values.mode "cluster") (not .Values.cluster.mesh.tls.existingSecret) (not .Values.cluster.mesh.sharedSecret.existingSecret) -}}
{{- fail "mode is cluster but the mesh has no authentication: set cluster.mesh.tls.existingSecret (a kubernetes.io/tls Secret with tls.crt, tls.key and ca.crt for *.<release>-headless.<namespace>.svc.<clusterDomain>) or, on a trusted pod network only, cluster.mesh.sharedSecret.existingSecret. See values.yaml, `cluster.mesh`." -}}
{{- end -}}
{{- end -}}

{{/*
The DNS domain of the headless Service, under which every pod has its stable name
(<pod>.<this>). The mesh advertises `<pod>.<this>` and requires peers' certificates to carry a
name under it.
*/}}
{{- define "hs.meshDomain" -}}
{{- printf "%s-headless.%s.svc.%s" (include "hs.fullname" .) .Release.Namespace .Values.cluster.clusterDomain -}}
{{- end -}}

{{/*
The data volume's root in singleNode mode: `hs serve --data-dir` puts the database in `db/`,
signing keys in `keys/` and media in `media/` underneath it, exactly as the container image's
`HS_DATA_DIR=/data` does for `docker run`.
*/}}
{{- define "hs.dataDir" -}}
/var/lib/hs/data
{{- end -}}
