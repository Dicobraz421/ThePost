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
