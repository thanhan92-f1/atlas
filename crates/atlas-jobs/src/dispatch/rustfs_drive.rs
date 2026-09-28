// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Provision a raw local disk as a RustFS drive. RustFS never formats or mounts disks — it uses a
//! directory Kubernetes gives it (the official chart takes a PVC). So the steps are the Kubernetes
//! side of that: check the disk is safe to take, format it XFS and mount it on the host, then expose
//! it as a `local` PV plus a PVC bound to it. Formatting/mounting happens in a throwaway root Job on
//! the node (`nsenter` into the host mount namespace) so the gateway itself stays non-root.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use atlas_driver_k8s::K8sDriver;

use crate::spec::JobSpec;

pub(crate) const STORAGE_CLASS: &str = "atlas-rustfs-local";
pub(crate) const DRIVE_LABEL: &str = "atlas.zyvor.dev/rustfs-drive";
const MOUNT_ROOT: &str = "/mnt/atlas-disks";
/// The only device the fake mode "formats" — never a real disk (same rule as the ZFS path).
const FAKE_FIXTURE_DEVICE: &str = "/dev/vdz";

/// Runs inside the node-prep Job as root. `DEV`, `MNT`, `WIPE`, `LABEL` arrive as env vars — never
/// interpolated into the script text. `mkfs.xfs` is deliberately not given `-f`: it refuses a device
/// that still carries a filesystem or partition-table signature, an independent safety net.
const PREP_SCRIPT: &str = r#"set -eu
echo "[prep] device=$DEV mount=$MNT wipe=$WIPE"
case "$DEV" in /dev/sd[a-z]|/dev/vd[a-z]|/dev/nvme[0-9]n[0-9]) ;; *) echo "[prep] refusing odd device path"; exit 2;; esac
if [ -n "$(lsblk -no MOUNTPOINTS "$DEV" | tr -d '[:space:]')" ]; then echo "[prep] device is mounted, refusing"; exit 3; fi
if [ "$WIPE" = "1" ]; then wipefs -a "$DEV"; fi
mkfs.xfs -L "$LABEL" "$DEV"
UUID="$(blkid -s UUID -o value "$DEV")"
[ -n "$UUID" ] || { echo "[prep] no UUID after mkfs"; exit 4; }
nsenter -t 1 -m -- sh -c "mkdir -p '$MNT' && mount -t xfs '$DEV' '$MNT' && chown 10001:10001 '$MNT' && (grep -q 'UUID=$UUID' /etc/fstab || echo 'UUID=$UUID $MNT xfs defaults,noatime,nofail 0 2' >> /etc/fstab)"
echo "[prep] mounted $MNT uuid=$UUID"
"#;

fn drive_name(device_path: &str) -> String {
    device_path.trim_start_matches("/dev/").replace('/', "-")
}

pub(crate) async fn dispatch_rustfs_drive(
    k8s: &Option<Arc<K8sDriver>>,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    let JobSpec::RustfsDriveProvision {
        device_path,
        confirmed_device_path,
        wipe_existing,
    } = spec
    else {
        anyhow::bail!("not a rustfs drive spec");
    };
    anyhow::ensure!(
        device_path == confirmed_device_path,
        "device path confirmation mismatch"
    );
    atlas_common::device::validate_raw_device_path(&device_path).map_err(anyhow::Error::msg)?;
    let name = drive_name(&device_path);
    let mount_path = format!("{MOUNT_ROOT}/{name}");

    let fake = !matches!(
        std::env::var("ATLAS_ZFS_DRIVER_MODE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "real"
    );
    if fake {
        anyhow::ensure!(
            device_path == FAKE_FIXTURE_DEVICE,
            "fake mode only accepts the fixture device {FAKE_FIXTURE_DEVICE}"
        );
        return Ok(serde_json::json!({ "drive": name, "mount_path": mount_path, "fake": true }));
    }

    let check = atlas_driver_zfs::inspect_device(&device_path).await?;
    if let Some(reason) = check.hard_refusal_reason() {
        anyhow::bail!("refusing to format {device_path}: {reason}");
    }
    if !wipe_existing {
        if let Some(reason) = check.refusal_reason() {
            anyhow::bail!("refusing to format {device_path}: {reason}");
        }
    }
    let size_bytes = atlas_driver_zfs::list_whole_disks()
        .await?
        .into_iter()
        .find(|d| d.path == device_path)
        .map(|d| d.size_bytes as i64)
        .ok_or_else(|| anyhow!("{device_path} is not a whole disk on this host"))?;

    let k8s = k8s
        .as_ref()
        .ok_or_else(|| anyhow!("no Kubernetes driver — cannot run the node-prep Job"))?;
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let node = env("ATLAS_NODE_NAME").ok_or_else(|| anyhow!("ATLAS_NODE_NAME is not set"))?;
    let ns = env("ATLAS_POD_NAMESPACE").unwrap_or_else(|| "zyvor-system".into());
    let image = env("ATLAS_SELF_IMAGE").ok_or_else(|| anyhow!("ATLAS_SELF_IMAGE is not set"))?;
    let pull_policy = env("ATLAS_SELF_IMAGE_PULL_POLICY").unwrap_or_else(|| "IfNotPresent".into());

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let job = format!("atlas-drive-prep-{name}-{ts}");
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
                "hostPID": true,
                "nodeName": node,
                "containers": [{
                    "name": "prep",
                    "image": image,
                    "imagePullPolicy": pull_policy,
                    "command": ["/bin/bash", "-c", PREP_SCRIPT],
                    "env": [
                        { "name": "DEV", "value": device_path },
                        { "name": "MNT", "value": mount_path },
                        { "name": "WIPE", "value": if wipe_existing { "1" } else { "0" } },
                        { "name": "LABEL", "value": format!("atlas-{name}") },
                    ],
                    "securityContext": { "privileged": true, "runAsUser": 0 },
                    "volumeMounts": [{ "name": "dev", "mountPath": "/dev" }],
                }],
                "volumes": [{ "name": "dev", "hostPath": { "path": "/dev" } }],
            }},
        }),
    )
    .await
    .context("create node-prep Job")?;

    let mut outcome = None;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        outcome = k8s.job_outcome(&ns, &job).await.context("read Job status")?;
        if outcome.is_some() {
            break;
        }
    }
    let logs = k8s.job_logs(&ns, &job).await.unwrap_or_default();
    let _ = k8s.delete_job(&ns, &job).await;
    match outcome {
        Some(true) => {}
        Some(false) => anyhow::bail!("node-prep Job failed: {}", logs.trim()),
        None => anyhow::bail!("node-prep Job did not finish in time: {}", logs.trim()),
    }

    let labels: BTreeMap<String, String> =
        BTreeMap::from([(DRIVE_LABEL.to_string(), "true".to_string())]);
    let pv = format!("atlas-rustfs-{name}");
    let pvc = format!("rustfs-{name}-data");
    k8s.ensure_local_storage_class(STORAGE_CLASS).await?;
    k8s.apply_local_pv(&pv, &mount_path, size_bytes, &node, STORAGE_CLASS, &labels)
        .await
        .context("create local PV")?;
    k8s.create_bound_pvc(&ns, &pvc, &pv, STORAGE_CLASS, size_bytes, &labels)
        .await
        .context("create PVC")?;

    Ok(serde_json::json!({
        "drive": name,
        "device": device_path,
        "node": node,
        "mount_path": mount_path,
        "pv": pv,
        "pvc": pvc,
        "namespace": ns,
        "storage_class": STORAGE_CLASS,
        "size_bytes": size_bytes,
    }))
}
