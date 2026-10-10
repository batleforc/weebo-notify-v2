{{/*
What the server Deployment and the migration Job (server-migrate-job.yaml) must
agree on: the WNK_ env sourced from Secrets and the secret-file mounts. One
definition, so the Job can never migrate with a DSN or a config the server
would not have used.

The attachments volume is deliberately NOT in here: it is a ReadWriteOnce PVC
on the filesystem backend, and a Job mounting it from another node would block
on the attach the running server holds. Migrations never touch attachments.
*/}}
{{- define "weebo-notify.serverEnv" -}}
{{- $fsBackend := eq .Values.server.attachments.kind "filesystem" -}}
{{- if .Values.server.database.existingSecret }}
- name: WNK_DATABASE__URL
  valueFrom:
    secretKeyRef:
      name: {{ .Values.server.database.existingSecret }}
      key: {{ .Values.server.database.secretKey }}
{{- end }}
{{- if and (eq .Values.server.attachments.kind "s3") .Values.server.attachments.existingSecret }}
- name: WNK_ATTACHMENTS__ACCESS_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.server.attachments.existingSecret }}
      key: {{ .Values.server.attachments.accessKeyKey }}
- name: WNK_ATTACHMENTS__SECRET_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.server.attachments.existingSecret }}
      key: {{ .Values.server.attachments.secretKeyKey }}
{{- end }}
{{- if and $fsBackend .Values.server.attachments.existingSecret }}
- name: WNK_ATTACHMENTS__TOKEN_SECRET
  valueFrom:
    secretKeyRef:
      name: {{ .Values.server.attachments.existingSecret }}
      key: {{ .Values.server.attachments.tokenSecretKey }}
{{- end }}
{{- if .Values.server.sinks.fingerprintKey.existingSecret }}
- name: WNK_SINKS__FINGERPRINT_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.server.sinks.fingerprintKey.existingSecret }}
      key: {{ .Values.server.sinks.fingerprintKey.secretKey }}
{{- end }}
{{- if and .Values.server.sinks.smtp.enabled .Values.server.sinks.smtp.existingSecret }}
- name: {{ .Values.server.sinks.smtp.passwordEnvName }}
  valueFrom:
    secretKeyRef:
      name: {{ .Values.server.sinks.smtp.existingSecret }}
      key: {{ .Values.server.sinks.smtp.passwordKey }}
{{- end }}
{{- with .Values.server.extraEnv }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{- define "weebo-notify.serverSecretMounts" -}}
{{- if .Values.server.push.webpush.enabled }}
- name: webpush-secret
  mountPath: /etc/weebo-notify/secrets/webpush
  readOnly: true
{{- end }}
{{- if .Values.server.push.apns.enabled }}
- name: apns-secret
  mountPath: /etc/weebo-notify/secrets/apns
  readOnly: true
{{- end }}
{{- if .Values.server.federation.enabled }}
- name: federation-secret
  mountPath: /etc/weebo-notify/secrets/federation
  readOnly: true
{{- end }}
{{- end -}}

{{- define "weebo-notify.serverSecretVolumes" -}}
{{- if .Values.server.push.webpush.enabled }}
- name: webpush-secret
  secret:
    secretName: {{ required "server.push.webpush.existingSecret is required when server.push.webpush.enabled" .Values.server.push.webpush.existingSecret }}
{{- end }}
{{- if .Values.server.push.apns.enabled }}
- name: apns-secret
  secret:
    secretName: {{ required "server.push.apns.existingSecret is required when server.push.apns.enabled" .Values.server.push.apns.existingSecret }}
{{- end }}
{{- if .Values.server.federation.enabled }}
- name: federation-secret
  secret:
    secretName: {{ required "server.federation.existingSecret is required when server.federation.enabled" .Values.server.federation.existingSecret }}
{{- end }}
{{- end -}}
