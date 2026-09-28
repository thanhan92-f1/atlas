#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Deploy a RustFS server (RustFS's official Helm chart, standalone) on a drive prepared from the
# console's Disks page, next to the lab's existing RustFS. RustFS cannot grow a single-drive
# deployment in place, so this is a NEW instance on its own ports; move the data with the console's
# DataBridge -> Object Migrations page, then repoint Atlas (ATLAS_RUSTFS_ENDPOINT /
# ATLAS_RUSTFS_CREDENTIALS_SECRET). Never prints Secret contents.
#
# Usage: scripts/rustfs-drive-remote.sh <host> <user> <pvc> [release] [s3-port] [console-port]
#   e.g. scripts/rustfs-drive-remote.sh 80.79.5.173 sus rustfs-sdb-data
set -euo pipefail
HOST="${1:?usage: rustfs-drive-remote.sh <host> <user> <pvc> [release] [s3-port] [console-port]}"
USER_="${2:?}"
PVC="${3:?}"
REL="${4:-rustfs-sdb}"
S3_PORT="${5:-30930}"
CONSOLE_PORT="${6:-30931}"
NS=zyvor-system
SECRET="${REL}-credentials"
GATEWAY_PORT=30510
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SSH="ssh ${USER_}@${HOST}"
RD=/tmp/atlas-rustfs-drive

$SSH "rm -rf ${RD} && mkdir -p ${RD}"
scp -q "${ROOT}/deploy/helm/atlas/charts/rustfs-1.0.0.tgz" "${ROOT}/deploy/rustfs-lab/values-drive.yaml" "${USER_}@${HOST}:${RD}/"

# One random key pair, stored under both key names (Atlas reads AWS_*, the chart reads RUSTFS_*).
$SSH "set -e; export KUBECONFIG=/etc/rancher/k3s/k3s.yaml; \
  if sudo -E k3s kubectl -n ${NS} get secret ${SECRET} >/dev/null 2>&1; then echo '${SECRET} already exists, leaving it as-is'; else \
    A=\$(openssl rand -base64 24 | tr -d '\n=/+' | head -c 20); S=\$(openssl rand -base64 48 | tr -d '\n=/+' | head -c 40); \
    sudo -E k3s kubectl -n ${NS} create secret generic ${SECRET} \
      --from-literal=AWS_ACCESS_KEY_ID=\$A --from-literal=AWS_SECRET_ACCESS_KEY=\$S \
      --from-literal=RUSTFS_ACCESS_KEY=\$A --from-literal=RUSTFS_SECRET_KEY=\$S >/dev/null; fi"

$SSH "set -e; sudo helm --kubeconfig /etc/rancher/k3s/k3s.yaml upgrade --install ${REL} ${RD}/rustfs-1.0.0.tgz -n ${NS} \
  -f ${RD}/values-drive.yaml \
  --set mode.standalone.existingClaim.dataClaim=${PVC} \
  --set secret.existingSecret=${SECRET} \
  --set service.endpoint.nodePort=${S3_PORT} --set service.console.nodePort=${CONSOLE_PORT} \
  --set 'extraEnv[0].name=NODE_IP' --set 'extraEnv[0].valueFrom.fieldRef.fieldPath=status.hostIP' \
  --set 'extraEnv[1].name=RUSTFS_CORS_ALLOWED_ORIGINS' --set 'extraEnv[1].value=http://\$(NODE_IP):${GATEWAY_PORT}' \
  --wait --timeout 300s"
$SSH "sudo k3s kubectl -n ${NS} get pods,svc -l app.kubernetes.io/instance=${REL}"
echo "RustFS ${REL}: S3 http://${HOST}:${S3_PORT}  console http://${HOST}:${CONSOLE_PORT}  credentials Secret ${SECRET}"
