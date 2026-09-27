// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Safe command wrappers for provisioning a raw, unformatted local block device into a new zpool.
//! Local-host-only, same as the rest of this driver (see the module doc in `lib.rs`).
//!
//! Security rule (PDF §17.3, same as `atlas-driver-ceph`'s `cmd.rs`): every command is an
//! argument array, never a shell string.

use atlas_driver_core::DriverError;

use crate::sanitize;

fn cmd(bin: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(bin);
    c.kill_on_drop(true);
    c
}

async fn run(bin: &str, args: &[&str]) -> Result<(bool, String, String), DriverError> {
    let output = cmd(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `{bin}`: {e}")))?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// Everything observed about a candidate device before any mutating command runs. Data-gathering
/// is kept separate from the pass/fail decision (`refusal_reason`) so the decision logic itself is
/// unit-testable without shelling out to real commands.
#[derive(Debug, Clone, Default)]
pub struct DeviceCheck {
    pub exists: bool,
    pub is_disk: bool,
    pub read_only: bool,
    pub has_children: bool,
    pub fstype: Option<String>,
    pub pttype: Option<String>,
    pub mounted_at: Option<String>,
    pub wipefs_signatures: Vec<String>,
    pub member_of_zpool: Option<String>,
    pub is_root_or_boot_disk: bool,
}

impl DeviceCheck {
    /// The refusals that `wipe_existing` can never override, checked first regardless of what
    /// else is true about the device: touching the root/boot disk, a read-only device, something
    /// that isn't a whole disk at all, or a device that's currently mounted (it may be actively
    /// serving files right now — unmounting it is a separate, deliberately out-of-scope action).
    pub fn hard_refusal_reason(&self) -> Option<String> {
        if !self.exists {
            return Some("no such block device".into());
        }
        if self.is_root_or_boot_disk {
            return Some("device backs the host's root or boot filesystem — refusing".into());
        }
        if self.read_only {
            return Some("device is read-only".into());
        }
        if !self.is_disk {
            return Some(
                "device is not a whole disk (a partition, loop, or optical device) — refusing"
                    .into(),
            );
        }
        if let Some(target) = &self.mounted_at {
            return Some(format!("device is mounted at {target}"));
        }
        None
    }

    /// `None` means the device looks safe to format; `Some(reason)` explains why it doesn't.
    /// Checked in order so the *first* applicable reason is the one surfaced. Everything past
    /// `hard_refusal_reason` here is stale *data* on the disk (a partition table, a filesystem or
    /// RAID/LVM signature, prior zpool membership) rather than a structural reason the device is
    /// unsafe to touch at all — `wipe_existing` (see `wipe_device`) clears these, so callers taking
    /// that path check `hard_refusal_reason` alone instead of this.
    pub fn refusal_reason(&self) -> Option<String> {
        if let Some(reason) = self.hard_refusal_reason() {
            return Some(reason);
        }
        if self.has_children {
            return Some(
                "device already has a partition table; wipe it out-of-band first if you really \
                 intend to reuse it — this API only formats a genuinely empty disk"
                    .into(),
            );
        }
        if let Some(fstype) = &self.fstype {
            return Some(format!("device already has a filesystem signature: {fstype}"));
        }
        if let Some(pttype) = &self.pttype {
            return Some(format!("device already has a partition table: {pttype}"));
        }
        if !self.wipefs_signatures.is_empty() {
            return Some(format!(
                "residual filesystem/RAID/LVM signature detected: {}",
                self.wipefs_signatures.join(", ")
            ));
        }
        if let Some(zpool) = &self.member_of_zpool {
            return Some(format!("device is already a member of zpool {zpool}"));
        }
        None
    }
}

fn basename(device_path: &str) -> &str {
    device_path.rsplit('/').next().unwrap_or(device_path)
}

/// The conventional first-disk names `atlas_common::device::validate_raw_device_path` always
/// refuses regardless of live host state — duplicated here as a minimal, dependency-free check
/// (rather than pulling in atlas-common) so this crate's own root/boot detection has a fallback
/// that still works when the dynamic, mount-namespace-dependent check above is blind (see its
/// comment). Keep in sync with that function's static rule if it ever changes.
fn is_conventional_first_disk(basename: &str) -> bool {
    matches!(basename, "sda" | "vda" | "nvme0n1")
}

/// Resolve a mounted mountpoint's source device (`findmnt -n -o SOURCE <mountpoint>`) down to its
/// parent whole-disk basename (`lsblk -no pkname <source>`, falling back to the source's own
/// basename when it has no parent, i.e. it's already a whole disk).
async fn resolve_mounted_disk_basename(mountpoint: &str) -> Option<String> {
    let (ok, source, _) = run("findmnt", &["-n", "-o", "SOURCE", mountpoint])
        .await
        .ok()?;
    if !ok || source.is_empty() {
        return None;
    }
    let (ok, pkname, _) = run("lsblk", &["-no", "PKNAME", &source]).await.ok()?;
    if ok && !pkname.trim().is_empty() {
        Some(pkname.trim().to_string())
    } else {
        Some(basename(&source).to_string())
    }
}

/// Runs every read-only safety probe against `device_path`. Never mutates anything.
pub async fn inspect_device(device_path: &str) -> Result<DeviceCheck, DriverError> {
    let mut check = DeviceCheck::default();

    // 1. lsblk: whole-disk shape, existing fs/partition signatures, read-only flag.
    let (ok, stdout, stderr) = run(
        "lsblk",
        &["-J", "-b", "-o", "NAME,TYPE,MOUNTPOINT,FSTYPE,PTTYPE,RO", device_path],
    )
    .await?;
    if !ok {
        // Not found / not a block device — the safe, honest answer is "does not exist", not an
        // unreachable-backend error, so a bad operator-supplied path surfaces as a normal refusal.
        check.exists = false;
        return Ok(check);
    }
    check.exists = true;
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| DriverError::Parse(format!("lsblk {device_path}: {e} (stderr: {stderr})")))?;
    if let Some(dev) = parsed
        .get("blockdevices")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
    {
        check.is_disk = dev.get("type").and_then(|v| v.as_str()) == Some("disk");
        // Different util-linux versions emit `ro` as a JSON boolean or as a "0"/"1" string.
        check.read_only = match dev.get("ro") {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::String(s)) => s == "1",
            _ => false,
        };
        check.has_children = dev
            .get("children")
            .and_then(|c| c.as_array())
            .is_some_and(|a| !a.is_empty());
        check.fstype = dev
            .get("fstype")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        check.pttype = dev
            .get("pttype")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        check.mounted_at = dev
            .get("mountpoint")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    // 2. findmnt: an independent check that the device (not just its lsblk-reported mountpoint
    // field) isn't mounted anywhere — a device can be a mount source without lsblk's own
    // `mountpoint` column reflecting it in every util-linux version.
    if check.mounted_at.is_none() {
        let (ok, target, _) = run("findmnt", &["-n", "-o", "TARGET", "--source", device_path]).await?;
        if ok && !target.is_empty() {
            check.mounted_at = Some(target);
        }
    }

    // 3. wipefs (no -a: detection only, never wipes) — a second, independent libblkid probe
    // catching residual signatures lsblk under-reports (e.g. stale mdraid superblocks).
    let (_, stdout, _) = run("wipefs", &["-n", device_path]).await?;
    check.wipefs_signatures = stdout
        .lines()
        .skip(1) // header row
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();

    // 4. Dynamic root/boot-disk refusal: resolve the host's actual mounted root/boot devices to
    // their parent whole-disk basenames and compare against this device's own basename. In
    // principle stronger than any static allow-list heuristic, since the real boot disk isn't
    // always the first letter/index (cloud images, unusual partitioning) — BUT this is blind when
    // the gateway runs in a container without visibility into the *host's* mount namespace (e.g.
    // the k8s lab deploy: `findmnt -n -o SOURCE /` inside the container resolves the container's
    // own rootfs, not the host's — confirmed live: `/dev/sda`, this host's actual root/boot disk,
    // was not flagged by this check alone). `is_conventional_first_disk` below is the fallback for
    // exactly that blind spot; `validate_raw_device_path` (atlas-common) enforces the same rule
    // independently at the route/dispatch layer regardless of what `DeviceCheck` reports.
    let target_basename = basename(device_path);
    for mountpoint in ["/", "/boot", "/boot/efi"] {
        if let Some(disk) = resolve_mounted_disk_basename(mountpoint).await {
            if disk == target_basename {
                check.is_root_or_boot_disk = true;
                break;
            }
        }
    }
    if is_conventional_first_disk(target_basename) {
        check.is_root_or_boot_disk = true;
    }

    // 5. Already a zpool member? Cross-check both the plain device path and its `/dev/disk/by-id`
    // aliases, since ZFS commonly reports vdevs by their by-id name, not `/dev/sdX`. No shell
    // pipeline here (arg-arrays only, per PDF §17.3) — `ls -la`'s own output is parsed in Rust.
    let (_, status_stdout, _) = run("zpool", &["status", "-P"]).await?;
    if !status_stdout.is_empty() {
        let (_, by_id_listing, _) = run("ls", &["-la", "/dev/disk/by-id/"]).await.unwrap_or_default();
        // Each symlink line looks like "lrwxrwxrwx ... ata-XXXXX -> ../../sdb" — keep only the
        // link *name* (before " -> "), for entries whose target resolves to this device.
        let by_id_aliases: Vec<String> = by_id_listing
            .lines()
            .filter(|l| l.ends_with(&format!("/{target_basename}")))
            .filter_map(|l| l.split(" -> ").next())
            .filter_map(|before_arrow| before_arrow.split_whitespace().last())
            .map(|name| format!("/dev/disk/by-id/{name}"))
            .collect();
        let by_id_aliases: Vec<&str> = by_id_aliases.iter().map(String::as_str).collect();
        check.member_of_zpool =
            find_zpool_member(&status_stdout, device_path, target_basename, &by_id_aliases);
    }

    Ok(check)
}

/// One row of `list_whole_disks`'s result — the read-only "what's out there" listing the Disks UI
/// renders as a picker instead of a blind device-path text field. Deliberately not `DeviceCheck`:
/// this describes N devices found by one bulk scan, not the single-device pass/fail decision
/// `inspect_device`/`refusal_reason` make right before a mutating command runs — that check is
/// re-run, authoritatively, against the chosen device at that point; this listing is advisory.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BlockDevice {
    pub path: String,
    pub size_bytes: u64,
    pub read_only: bool,
    pub has_children: bool,
    pub fstype: Option<String>,
    pub pttype: Option<String>,
    pub mounted_at: Option<String>,
    pub wipefs_signatures: Vec<String>,
    pub member_of_zpool: Option<String>,
    pub is_root_or_boot_disk: bool,
}

impl BlockDevice {
    /// A single coarse status label for the UI to badge/sort by — the fields above carry the
    /// actual detail. Ordered the same way `DeviceCheck::refusal_reason` is (root/boot first).
    pub fn status(&self) -> &'static str {
        if self.is_root_or_boot_disk {
            "root_or_boot"
        } else if self.read_only {
            "read_only"
        } else if self.mounted_at.is_some() {
            "mounted"
        } else if self.has_children
            || self.fstype.is_some()
            || self.pttype.is_some()
            || !self.wipefs_signatures.is_empty()
            || self.member_of_zpool.is_some()
        {
            "has_data"
        } else {
            "empty"
        }
    }
}

/// Bulk-scans every whole disk on the local host (partitions/loop/optical devices excluded) — the
/// same signals `inspect_device` gathers for one device, run once for all of them, so the Disks UI
/// can show a picker instead of a blind device-path text field.
pub async fn list_whole_disks() -> Result<Vec<BlockDevice>, DriverError> {
    let (ok, stdout, stderr) = run(
        "lsblk",
        &["-J", "-b", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE,PTTYPE,RO"],
    )
    .await?;
    if !ok {
        return Err(DriverError::Unreachable(format!("lsblk: {stderr}")));
    }
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| DriverError::Parse(format!("lsblk: {e} (stderr: {stderr})")))?;
    let devices = parsed
        .get("blockdevices")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();

    // Root/boot-disk resolution done once, matched by basename against every listed disk — see
    // `inspect_device`'s step 4 for why this is dynamic rather than a static first-letter guess.
    let mut root_boot_basenames = std::collections::HashSet::new();
    for mountpoint in ["/", "/boot", "/boot/efi"] {
        if let Some(disk) = resolve_mounted_disk_basename(mountpoint).await {
            root_boot_basenames.insert(disk);
        }
    }
    // zpool membership done once too (see `inspect_device`'s step 5); by-id aliases are re-filtered
    // per device below.
    let (_, status_stdout, _) = run("zpool", &["status", "-P"]).await?;
    let (_, by_id_listing, _) = run("ls", &["-la", "/dev/disk/by-id/"]).await.unwrap_or_default();

    let mut out = Vec::new();
    for dev in &devices {
        if dev.get("type").and_then(|v| v.as_str()) != Some("disk") {
            continue;
        }
        let Some(name) = dev.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
            continue;
        };
        let path = format!("/dev/{name}");
        let size_bytes = dev.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let read_only = match dev.get("ro") {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::String(s)) => s == "1",
            _ => false,
        };
        let has_children = dev
            .get("children")
            .and_then(|c| c.as_array())
            .is_some_and(|a| !a.is_empty());
        let fstype = dev
            .get("fstype")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let pttype = dev
            .get("pttype")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mounted_at = dev
            .get("mountpoint")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let (_, wipefs_stdout, _) = run("wipefs", &["-n", &path]).await.unwrap_or_default();
        let wipefs_signatures: Vec<String> = wipefs_stdout
            .lines()
            .skip(1)
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect();
        let member_of_zpool = if status_stdout.is_empty() {
            None
        } else {
            let by_id_aliases: Vec<String> = by_id_listing
                .lines()
                .filter(|l| l.ends_with(&format!("/{name}")))
                .filter_map(|l| l.split(" -> ").next())
                .filter_map(|before_arrow| before_arrow.split_whitespace().last())
                .map(|alias| format!("/dev/disk/by-id/{alias}"))
                .collect();
            let by_id_aliases: Vec<&str> = by_id_aliases.iter().map(String::as_str).collect();
            find_zpool_member(&status_stdout, &path, name, &by_id_aliases)
        };

        out.push(BlockDevice {
            path,
            size_bytes,
            read_only,
            has_children,
            fstype,
            pttype,
            mounted_at,
            wipefs_signatures,
            member_of_zpool,
            is_root_or_boot_disk: root_boot_basenames.contains(name)
                || is_conventional_first_disk(name),
        });
    }
    Ok(out)
}

/// Scan `zpool status -P` output for a pool that lists this device (or a by-id alias resolving to
/// it) as a vdev. Returns the owning pool's name if found.
fn find_zpool_member(
    status: &str,
    device_path: &str,
    basename: &str,
    by_id_aliases: &[&str],
) -> Option<String> {
    let mut current_pool: Option<&str> = None;
    for line in status.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix("pool: ") {
            current_pool = Some(name.trim());
            continue;
        }
        let Some(first_field) = trimmed.split_whitespace().next() else {
            continue;
        };
        let matches_direct = first_field == device_path || first_field.ends_with(basename);
        let matches_by_id = by_id_aliases.contains(&first_field);
        if (matches_direct || matches_by_id) && current_pool.is_some() {
            return current_pool.map(str::to_string);
        }
    }
    None
}

/// Clears a residual partition table/filesystem/RAID/LVM signature off `device_path` so a
/// subsequent `inspect_device` call no longer reports `has_children`/`fstype`/`pttype`/
/// `wipefs_signatures`. Only reached when the operator has explicitly opted into `wipe_existing`
/// (see `JobSpec::ZfsPoolCreateFromDevice`) — `hard_refusal_reason` is still checked first and is
/// never bypassed by this, so this never runs against the root/boot disk, a mounted device, or
/// anything that isn't a whole disk.
pub async fn wipe_device(device_path: &str) -> Result<(), DriverError> {
    let (ok, _stdout, stderr) = run("wipefs", &["-a", device_path]).await?;
    if !ok {
        return Err(DriverError::Backend(format!(
            "wipefs -a {device_path}: {stderr}"
        )));
    }
    // Best-effort: `wipefs -a` erases the on-disk partition-table signature, but the kernel's
    // already-parsed partition entries (lsblk's `children`) can persist until it re-reads the
    // partition table — ignore failure here, `zpool create` will surface any real problem itself.
    let _ = run("blockdev", &["--rereadpt", device_path]).await;
    Ok(())
}

/// `zpool create <pool_name> <device_path>` — **never** passes `-f` (force): that flag exists
/// specifically to override zpool's own built-in "this looks like it's in use" refusal, which is
/// the safety net underneath everything `inspect_device` already checked.
pub async fn zpool_create(pool_name: &str, device_path: &str) -> Result<(), DriverError> {
    let (ok, _stdout, stderr) = run("zpool", &["create", pool_name, device_path]).await?;
    if !ok {
        return Err(DriverError::Backend(format!(
            "zpool create {pool_name} {device_path}: {stderr}"
        )));
    }
    Ok(())
}

/// The synthesized inventory pool/cluster ids this device-provision path writes, matching
/// `RealZfsDriver`/`FakeZfsDriver`'s own id-derivation scheme exactly, so a later real discovery
/// pass (once the pool name is added to `ATLAS_ZFS_POOLS`) upserts onto the same row.
pub fn pool_id(pool_name: &str) -> String {
    format!("pool_zfs_{}", sanitize(pool_name))
}

pub fn cluster_id(host: &str) -> String {
    format!("cls_zfs_{}", sanitize(host))
}

/// The id a discovery pass gives the pool's own root dataset — `FakeZfsDriver::list_volumes`
/// derives it from the pool name directly, and `RealZfsDriver::list_volumes` derives it from the
/// dataset name via `zfs list -r <pool>`, whose first/root entry is always named exactly
/// `pool_name` — so both modes agree on this id for the pool's root dataset.
pub fn root_volume_id(pool_name: &str) -> String {
    format!("vol_zfs_{}", sanitize(pool_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_root_disk_before_anything_else() {
        let check = DeviceCheck {
            exists: true,
            is_root_or_boot_disk: true,
            read_only: true, // also true, but root/boot must win regardless of order
            ..Default::default()
        };
        assert!(check.refusal_reason().unwrap().contains("root or boot"));
    }

    #[test]
    fn refuses_missing_device() {
        let check = DeviceCheck::default();
        assert!(check.refusal_reason().unwrap().contains("no such"));
    }

    #[test]
    fn refuses_partition() {
        let check = DeviceCheck {
            exists: true,
            is_disk: false,
            ..Default::default()
        };
        assert!(check.refusal_reason().unwrap().contains("whole disk"));
    }

    #[test]
    fn refuses_existing_filesystem() {
        let check = DeviceCheck {
            exists: true,
            is_disk: true,
            fstype: Some("ext4".into()),
            ..Default::default()
        };
        assert!(check.refusal_reason().unwrap().contains("ext4"));
    }

    #[test]
    fn refuses_mounted_device() {
        let check = DeviceCheck {
            exists: true,
            is_disk: true,
            mounted_at: Some("/mnt/data".into()),
            ..Default::default()
        };
        assert!(check.refusal_reason().unwrap().contains("/mnt/data"));
    }

    #[test]
    fn hard_refusal_reason_ignores_wipeable_data() {
        // A stale partition table + filesystem signature — exactly what `wipe_existing` is for —
        // must NOT show up in `hard_refusal_reason`, only in the full `refusal_reason`.
        let check = DeviceCheck {
            exists: true,
            is_disk: true,
            has_children: true,
            fstype: Some("ceph_bluestore".into()),
            pttype: Some("gpt".into()),
            wipefs_signatures: vec!["gpt".into()],
            member_of_zpool: Some("tank".into()),
            ..Default::default()
        };
        assert!(check.hard_refusal_reason().is_none());
        assert!(check.refusal_reason().is_some());
    }

    #[test]
    fn hard_refusal_reason_still_blocks_mounted_and_root_disk() {
        let mounted = DeviceCheck {
            exists: true,
            is_disk: true,
            mounted_at: Some("/mnt/data".into()),
            ..Default::default()
        };
        assert!(mounted.hard_refusal_reason().unwrap().contains("/mnt/data"));

        let root_disk = DeviceCheck {
            exists: true,
            is_disk: true,
            is_root_or_boot_disk: true,
            ..Default::default()
        };
        assert!(root_disk.hard_refusal_reason().unwrap().contains("root or boot"));
    }

    #[test]
    fn refuses_existing_zpool_member() {
        let check = DeviceCheck {
            exists: true,
            is_disk: true,
            member_of_zpool: Some("tank".into()),
            ..Default::default()
        };
        assert!(check.refusal_reason().unwrap().contains("tank"));
    }

    #[test]
    fn accepts_a_genuinely_empty_disk() {
        let check = DeviceCheck {
            exists: true,
            is_disk: true,
            ..Default::default()
        };
        assert!(check.refusal_reason().is_none());
    }

    #[test]
    fn pool_and_cluster_ids_match_the_driver_scheme() {
        assert_eq!(pool_id("tank"), "pool_zfs_tank");
        assert_eq!(cluster_id("zfs01.zyvor.lab"), "cls_zfs_zfs01_zyvor_lab");
    }

    #[test]
    fn conventional_first_disk_fallback_covers_the_container_blind_spot() {
        // This is the static backstop for exactly the bug found live: inside a container without
        // visibility into the host's mount namespace, the dynamic findmnt-based check in
        // inspect_device/list_whole_disks can't see that e.g. /dev/sda is actually mounted at the
        // host's "/" — /dev/sda showed up in the Disks UI picker labeled "has data — wipeable"
        // before this fallback existed.
        assert!(is_conventional_first_disk("sda"));
        assert!(is_conventional_first_disk("vda"));
        assert!(is_conventional_first_disk("nvme0n1"));
        assert!(!is_conventional_first_disk("sdb"));
        assert!(!is_conventional_first_disk("nvme1n1"));
    }
}
