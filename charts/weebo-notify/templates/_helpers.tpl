{{/*
Base name, honouring nameOverride/fullnameOverride like every other chart.
*/}}
{{- define "weebo-notify.name" -}}
{{- .Values.nameOverride | default .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "weebo-notify.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := include "weebo-notify.name" . -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "weebo-notify.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Common labels / selector labels, split the usual way: selector labels are the
subset immutable across the object's lifetime (Deployment.spec.selector cannot
change without a replace, so nothing version-derived belongs in it).
*/}}
{{- define "weebo-notify.labels" -}}
helm.sh/chart: {{ include "weebo-notify.chart" . }}
{{ include "weebo-notify.selectorLabels" . }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
{{- end -}}

{{- define "weebo-notify.selectorLabels" -}}
app.kubernetes.io/name: {{ include "weebo-notify.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Per-component name/labels — "server" and "web" are two Deployments/Services,
never merged, because they are two images serving two origins (ADR-0021).
*/}}
{{- define "weebo-notify.componentFullname" -}}
{{- printf "%s-%s" (include "weebo-notify.fullname" .ctx) .component -}}
{{- end -}}

{{- define "weebo-notify.componentLabels" -}}
{{ include "weebo-notify.labels" .ctx }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{- define "weebo-notify.componentSelectorLabels" -}}
{{ include "weebo-notify.selectorLabels" .ctx }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{/*
Image references. `.tag` falls back to "v" + Chart.AppVersion: release.yaml
pushes the git tag cog creates (`tag_prefix = "v"` in cog.toml), so the image
for appVersion 0.4.2 is `:v0.4.2` — a bare `:0.4.2` does not exist and would
leave a default install in ImagePullBackOff.

`.digest` (sha256:...) wins over the tag when set: a tag is a mutable pointer,
the digest is the bytes release.yaml scanned and cosign signed, and it is what
an admission controller verifying signatures actually checks.
*/}}
{{- define "weebo-notify.image" -}}
{{- if .image.digest -}}
{{- printf "%s@%s" .image.repository .image.digest -}}
{{- else -}}
{{- printf "%s:%s" .image.repository (.image.tag | default (printf "v%s" .ctx.Chart.AppVersion)) -}}
{{- end -}}
{{- end -}}

{{- define "weebo-notify.serverImage" -}}
{{- include "weebo-notify.image" (dict "image" .Values.image.server "ctx" .) -}}
{{- end -}}

{{- define "weebo-notify.webImage" -}}
{{- include "weebo-notify.image" (dict "image" .Values.image.web "ctx" .) -}}
{{- end -}}

{{/*
Anti-affinity by node (design §14). "soft" is a preference so a single-node
cluster can still schedule replicaCount > 1; "hard" turns the same rule into a
requirement.
*/}}
{{- define "weebo-notify.podAntiAffinity" -}}
{{- $mode := .mode -}}
{{- $component := .component -}}
{{- if eq $mode "soft" }}
podAntiAffinity:
  preferredDuringSchedulingIgnoredDuringExecution:
    - weight: 100
      podAffinityTerm:
        topologyKey: kubernetes.io/hostname
        labelSelector:
          matchLabels:
            {{- include "weebo-notify.componentSelectorLabels" (dict "ctx" .ctx "component" $component) | nindent 12 }}
{{- else if eq $mode "hard" }}
podAntiAffinity:
  requiredDuringSchedulingIgnoredDuringExecution:
    - topologyKey: kubernetes.io/hostname
      labelSelector:
        matchLabels:
          {{- include "weebo-notify.componentSelectorLabels" (dict "ctx" .ctx "component" $component) | nindent 10 }}
{{- end -}}
{{- end -}}
