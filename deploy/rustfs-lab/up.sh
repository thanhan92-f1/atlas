#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Stand up a RustFS server (Atlas's primary S3-compatible object backend) in the lab k3s cluster.
# See deployment.yaml and docs/RUSTFS.md. scripts/deploy-remote.sh runs this on every deploy
# (idempotent), so the gateway's ATLAS_RUSTFS_DRIVER_MODE=real always has a server to talk to.
#
# Usage (run ON the lab host, or with kubectl already pointed at it):
#   ./up.sh
#
# What you get:
#   - Namespace zyvor-system (created if missing)
#   - Secret rustfs-credentials with a freshly generated access/secret key (never printed, never
#     committed) — an existing Secret is left alone, so re-running never rotates a live credential
#   - PVC + Deployment + Service `rustfs` on NodePorts 30900 (S3 API) and 30901 (web console)
set -euo pipefail
cd "$(dirname "$0")"
NS=zyvor-system

log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

log "namespace"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -

log "credentials secret (generated, idempotent — leaves an existing secret alone)"
if kubectl -n "$NS" get secret rustfs-credentials >/dev/null 2>&1; then
  echo "rustfs-credentials already exists, leaving it as-is"
else
  # Alphanumeric only: the access key doubles as an S3 credential embedded in signed URLs, and
  # some S3 tooling mishandles '+', '/' and '=' in keys.
  ACCESS="$(openssl rand -base64 24 | tr -d '\n=/+' | head -c 20)"
  SECRET="$(openssl rand -base64 48 | tr -d '\n=/+' | head -c 40)"
  kubectl -n "$NS" create secret generic rustfs-credentials \
    --from-literal=AWS_ACCESS_KEY_ID="$ACCESS" \
    --from-literal=AWS_SECRET_ACCESS_KEY="$SECRET" >/dev/null
  unset ACCESS SECRET
fi

log "PVC + Deployment + Service"
kubectl apply -f deployment.yaml
kubectl -n "$NS" rollout status deploy/rustfs --timeout=240s

NODE_IP="$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')"
cat <<EOF

rustfs is up:
  S3 API:      http://${NODE_IP}:30900   (in-cluster: http://rustfs.${NS}.svc:9000)
  Web console: http://${NODE_IP}:30901

The access/secret key live only in the rustfs-credentials Secret (keys AWS_ACCESS_KEY_ID /
AWS_SECRET_ACCESS_KEY) — Atlas reads that same Secret for its RustFS write path and discovery.
To log in to the console yourself:
  kubectl -n ${NS} get secret rustfs-credentials -o jsonpath='{.data.AWS_ACCESS_KEY_ID}' | base64 -d; echo
  kubectl -n ${NS} get secret rustfs-credentials -o jsonpath='{.data.AWS_SECRET_ACCESS_KEY}' | base64 -d; echo

Tear down (deletes the stored objects and the credential):
  kubectl -n ${NS} delete deploy/rustfs svc/rustfs pvc/rustfs-data secret/rustfs-credentials
EOF
