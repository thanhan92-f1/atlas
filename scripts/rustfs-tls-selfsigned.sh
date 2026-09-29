#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Generate a self-signed TLS cert/key and store it as a `kubernetes.io/tls` Secret (keys tls.crt /
# tls.key — exactly what RUSTFS_TLS_PATH expects) for a LAB RustFS instance. A self-signed cert is
# fine for testing that TLS actually works end to end; it is not a substitute for a real CA in
# production (see docs/RUSTFS.md's TLS section for the production path). The private key is
# generated and consumed entirely on the remote host and is never printed or transferred.
#
# Usage: scripts/rustfs-tls-selfsigned.sh <host> <user> <secret-name> [namespace] [common-name]
#   e.g. scripts/rustfs-tls-selfsigned.sh 80.79.5.173 sus rustfs-sdb-tls zyvor-system
set -euo pipefail
HOST="${1:?usage: rustfs-tls-selfsigned.sh <host> <user> <secret-name> [namespace] [common-name]}"
USER_="${2:?}"
SECRET="${3:?}"
NS="${4:-zyvor-system}"
CN="${5:-rustfs.local}"
SSH="ssh ${USER_}@${HOST}"

# basicConstraints=CA:FALSE + a SAN (IP of the host Atlas actually connects to) are required: rustls's
# verifier rejects a CA:TRUE cert presented as a TLS server leaf ("CaUsedAsEndEntity"), and modern
# rustls ignores CN for hostname matching, needing a matching IP/DNS SAN instead (found live, 2026-09-28,
# self-signed cert generated with plain `openssl req -x509 -subj` defaults to CA:TRUE with no SAN).
$SSH "set -e; if sudo k3s kubectl -n ${NS} get secret ${SECRET} >/dev/null 2>&1; then echo '${SECRET} already exists, leaving it as-is'; exit 0; fi; \
  D=\$(mktemp -d); trap 'rm -rf \"\$D\"' EXIT; \
  openssl req -x509 -newkey rsa:2048 -keyout \"\$D/tls.key\" -out \"\$D/tls.crt\" -days 825 -nodes -subj '/CN=${CN}' \
    -addext 'basicConstraints=critical,CA:FALSE' \
    -addext 'keyUsage=critical,digitalSignature,keyEncipherment' \
    -addext 'extendedKeyUsage=serverAuth' \
    -addext 'subjectAltName=DNS:${CN},IP:${HOST}' >/dev/null 2>&1; \
  sudo k3s kubectl -n ${NS} create secret tls ${SECRET} --cert=\"\$D/tls.crt\" --key=\"\$D/tls.key\" >/dev/null; \
  echo 'created Secret ${SECRET} (self-signed, CN=${CN}, SAN=DNS:${CN}+IP:${HOST}, 825 days)'"
