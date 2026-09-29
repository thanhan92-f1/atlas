// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Install/remove a RustFS server with RustFS's OWN Helm chart (vendored in the gateway image at
//! `/usr/share/atlas/charts/`). The `helm` run happens in a throwaway Job under the dedicated
//! `atlas-rustfs-installer` ServiceAccount — a namespaced Role that can manage exactly what the chart
//! renders — so the gateway itself gains no broad write access. The root credential is generated
//! inside the Job (never in the Job spec or the job record) and lands only in the chart's Secret.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use atlas_driver_k8s::K8sDriver;

use crate::spec::JobSpec;

pub(crate) const INSTALLER_SERVICE_ACCOUNT: &str = "atlas-rustfs-installer";
const CHART: &str = "/usr/share/atlas/charts/rustfs-1.0.0.tgz";

const SCRIPT: &str = r#"set -eu
export HOME=/tmp
NAME_RE='^[a-z][a-z0-9-]{1,30}$'
echo "$NAME" | grep -Eq "$NAME_RE" || { echo "[rustfs] bad release name"; exit 2; }
case "$ACTION" in
uninstall)
  helm uninstall "$NAME" -n "$NS" --wait --timeout 240s
  echo "[rustfs] uninstalled $NAME (its data claim, if any, is kept)"
  ;;
install)
  if helm status "$NAME" -n "$NS" >/dev/null 2>&1; then
    if helm status "$NAME" -n "$NS" -o json | grep -q '"status":"deployed"'; then echo "[rustfs] $NAME already installed"; exit 0; fi
    echo "[rustfs] $NAME is in a broken state, removing it before installing"
    helm uninstall "$NAME" -n "$NS" --wait --timeout 240s || true
  fi
  AK="$(head -c 48 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 20)"
  SK="$(head -c 96 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 40)"
  set -- --set image.tag=1.0.0 \
    --set mode.standalone.enabled=true --set mode.distributed.enabled=false \
    --set "secret.rustfs.access_key=$AK" --set "secret.rustfs.secret_key=$SK" \
    --set ingress.enabled=false --set service.type=NodePort --set "service.endpoint.nodePort=$S3_PORT" --set "service.console.nodePort=$CONSOLE_PORT" \
    --set "extraEnv[0].name=NODE_IP" --set "extraEnv[0].valueFrom.fieldRef.fieldPath=status.hostIP" \
    --set "extraEnv[1].name=RUSTFS_CORS_ALLOWED_ORIGINS" --set "extraEnv[1].value=http://\$(NODE_IP):$CONSOLE_ORIGIN_PORT"
  if [ -n "$PVC" ]; then set -- "$@" --set "mode.standalone.existingClaim.dataClaim=$PVC"; fi
  if [ -n "$TLS_SECRET" ]; then
    # RUSTFS_TLS_PATH makes the S3 port TLS-only; the chart's own liveness/readiness probes are
    # plain HTTP unless its (much heavier, cert-manager-based) mtls.enabled mode is used, which
    # this simple file-mount TLS does not — so disable them rather than have them fail forever.
    set -- "$@" \
      --set "extraEnv[2].name=RUSTFS_TLS_PATH" --set "extraEnv[2].value=/opt/tls" \
      --set-json "extraVolumes=[{\"name\":\"tls\",\"secret\":{\"secretName\":\"$TLS_SECRET\",\"items\":[{\"key\":\"tls.crt\",\"path\":\"rustfs_cert.pem\"},{\"key\":\"tls.key\",\"path\":\"rustfs_key.pem\"}]}}]" \
      --set-json "extraVolumeMounts=[{\"name\":\"tls\",\"mountPath\":\"/opt/tls\",\"readOnly\":true}]" \
      --set livenessProbe.enabled=false --set readinessProbe.enabled=false
  fi
  helm install "$NAME" "$CHART" -n "$NS" "$@" --wait --timeout 300s
  echo "[rustfs] installed $NAME: service $NAME-svc, credentials Secret $NAME-secret"
  ;;
*) echo "[rustfs] unknown action"; exit 2;;
esac
"#;

pub(crate) async fn dispatch_rustfs_instance(
    k8s: &Option<Arc<K8sDriver>>,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    let JobSpec::RustfsInstance {
        action,
        name,
        pvc,
        s3_node_port,
        console_node_port,
        tls_secret,
    } = spec
    else {
        anyhow::bail!("not a rustfs instance spec");
    };
    anyhow::ensure!(
        action == "install" || action == "uninstall",
        "action must be install or uninstall"
    );
    anyhow::ensure!(
        name.len() >= 2
            && name.len() <= 31
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "instance name must be lowercase letters, digits and hyphens (2-31 chars)"
    );
    if action == "install" {
        anyhow::ensure!(
            (30000..=32767).contains(&s3_node_port)
                && (30000..=32767).contains(&console_node_port)
                && s3_node_port != console_node_port,
            "node ports must be distinct values in 30000-32767"
        );
        if !tls_secret.is_empty() {
            anyhow::ensure!(
                tls_secret.len() <= 253
                    && tls_secret
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.'),
                "tls_secret must be a valid Kubernetes Secret name"
            );
        }
    }
    let k8s = k8s
        .as_ref()
        .ok_or_else(|| anyhow!("no Kubernetes driver — cannot run the installer Job"))?;
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let ns = env("ATLAS_POD_NAMESPACE").unwrap_or_else(|| "zyvor-system".into());
    let image = env("ATLAS_SELF_IMAGE").ok_or_else(|| anyhow!("ATLAS_SELF_IMAGE is not set"))?;
    let pull_policy = env("ATLAS_SELF_IMAGE_PULL_POLICY").unwrap_or_else(|| "IfNotPresent".into());
    let console_origin_port = env("ATLAS_CONSOLE_NODE_PORT").unwrap_or_else(|| "30510".into());

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let job = format!("atlas-rustfs-{action}-{name}-{ts}");
    k8s.apply_cr(
        "batch",
        "v1",
        "Job",
        &ns,
        &job,
        serde_json::json!({
            "backoffLimit": 0,
            "ttlSecondsAfterFinished": 600,
            "template": { "spec": {
                "restartPolicy": "Never",
                "serviceAccountName": INSTALLER_SERVICE_ACCOUNT,
                "containers": [{
                    "name": "helm",
                    "image": image,
                    "imagePullPolicy": pull_policy,
                    "command": ["/bin/bash", "-c", SCRIPT],
                    "env": [
                        { "name": "ACTION", "value": action },
                        { "name": "NAME", "value": name },
                        { "name": "NS", "value": ns },
                        { "name": "PVC", "value": pvc },
                        { "name": "S3_PORT", "value": s3_node_port.to_string() },
                        { "name": "CONSOLE_PORT", "value": console_node_port.to_string() },
                        { "name": "CONSOLE_ORIGIN_PORT", "value": console_origin_port },
                        { "name": "CHART", "value": CHART },
                        { "name": "TLS_SECRET", "value": tls_secret },
                    ],
                }],
            }},
        }),
    )
    .await
    .context("create installer Job")?;

    let mut outcome = None;
    for _ in 0..180 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        outcome = k8s.job_outcome(&ns, &job).await.context("read Job status")?;
        if outcome.is_some() {
            break;
        }
    }
    let logs = k8s.job_logs(&ns, &job).await.unwrap_or_default();
    let _ = k8s.delete_job(&ns, &job).await;
    match outcome {
        Some(true) => Ok(serde_json::json!({
            "instance": name,
            "action": action,
            "service": format!("{name}-svc"),
            "secret": format!("{name}-secret"),
            "log": logs.trim(),
        })),
        Some(false) => anyhow::bail!("installer Job failed: {}", logs.trim()),
        None => anyhow::bail!("installer Job did not finish in time: {}", logs.trim()),
    }
}
