{{- define "thepost.fullname" -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "thepost.selectorLabels" -}}
app.kubernetes.io/name: thepost
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "thepost.labels" -}}
{{ include "thepost.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{/* Кластер CloudNativePG (cnpg.enabled): Cluster, служба primary <имя>-rw, CA сервера. */}}
{{- define "thepost.dbCluster" -}}
{{- printf "%s-db" (include "thepost.fullname" .) | trunc 50 | trimSuffix "-" -}}
{{- end -}}

{{- define "thepost.dbPooler" -}}
{{- printf "%s-pooler-rw" (include "thepost.dbCluster" .) -}}
{{- end -}}

{{- define "thepost.dbCASecret" -}}
{{- (.Values.cnpg.certificates).serverCASecret | default (printf "%s-ca" (include "thepost.dbCluster" .)) -}}
{{- end -}}

{{/* config.yaml: с cnpg.enabled адреса database задаёт чарт — служба primary (<кластер>-rw) или PgBouncer (Pooler). */}}
{{- define "thepost.config" -}}
{{- $cfg := deepCopy .Values.config -}}
{{- if .Values.cnpg.enabled -}}
{{- $db := deepCopy (default (dict) $cfg.database) -}}
{{- $tls := "sslmode=verify-full&sslrootcert=/etc/thepost/db-ca/ca.crt" -}}
{{- $rw := printf "postgres://thepost@%s-rw:5432/%s?%s" (include "thepost.dbCluster" .) .Values.cnpg.database $tls -}}
{{- $_ := unset $db "urls" -}}
{{- if .Values.cnpg.pooler.enabled -}}
{{- $_ := set $db "url" (printf "postgres://thepost@%s:5432/%s?%s" (include "thepost.dbPooler" .) .Values.cnpg.database $tls) -}}
{{- $_ := set $db "pgbouncer" true -}}
{{- $_ := set $db "direct_url" $rw -}}
{{- else -}}
{{- $_ := set $db "url" $rw -}}
{{- $_ := unset $db "pgbouncer" -}}
{{- $_ := unset $db "direct_url" -}}
{{- end -}}
{{- $_ := set $cfg "database" $db -}}
{{- end -}}
{{- toYaml $cfg -}}
{{- end -}}
