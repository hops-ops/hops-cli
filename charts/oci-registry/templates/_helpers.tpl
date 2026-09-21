{{- define "oci.labels" -}}
app.kubernetes.io/name: oci-registry
app.kubernetes.io/instance: {{ .Release.Name }}
hops.ops.com.ai/registry-mode: push
{{- end -}}

{{- define "oci.selector" -}}
app.kubernetes.io/name: oci-registry
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "oci.storage" -}}
{{- if eq .Values.storage.type "pvc" }}
filesystem:
  rootdirectory: /var/lib/registry
{{- else }}
s3:
  bucket: {{ required "storage.s3.bucket is required" .Values.storage.s3.bucket | quote }}
  region: {{ required "storage.s3.region is required" .Values.storage.s3.region | quote }}
  rootdirectory: {{ .Values.storage.s3.rootDirectory | quote }}
  encrypt: true
  secure: true
  {{- with .Values.storage.s3.kmsKeyId }}
  keyid: {{ . | quote }}
  {{- end }}
{{- end }}
# Prevent clients being redirected around the internal read endpoint.
redirect:
  disable: true
delete:
  enabled: false
{{- end -}}
