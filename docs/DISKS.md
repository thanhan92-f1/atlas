<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Provisioning a raw disk (first integration slice)

Atlas can turn a raw, unformatted block device (e.g. `/dev/sdb`) into usable capacity two ways:
a new **ZFS pool** (direct `zpool create`) or a new **Ceph OSD** via Rook (patches the
`CephCluster` CR's device list and waits for Rook's own reconciler to claim the disk). Both are
async jobs (`202 Accepted` + `job_id`, same shape as every other write path) and both require an
explicit `"confirm": true` — formatting a disk is irreversible.

Device selection is **manual path entry only** in this first slice: there is no device
auto-discovery/listing endpoint. The operator must already know the exact device path.

## Safety model

Every real command is preceded by defense-in-depth checks that refuse anything that looks like it
could be in use. Nothing is ever marked `succeeded` until independently verified — never fabricated
(matching every other driver's "never fabricate" convention in this codebase).

`atlas_common::device::validate_raw_device_path` is a cheap first gate, applied at both the route
and the job-dispatch layer: the path must be a **whole disk** (`/dev/sdb`, `/dev/vdc`,
`/dev/nvme1n1` — never a partition like `/dev/sdb1`), and the conventional first-disk name for each
bus (`sda`, `vda`, `nvme0n1`) is refused outright as a static heuristic.

## ZFS: `POST /api/atlas/v1/zfs/pools/from-device`

```json
{ "pool_name": "tank2", "device_path": "/dev/sdb", "confirm": true }
```

Local-host-only: the gateway process must be running on the host that owns the device (the same
limitation `RealZfsDriver` itself already has — ZFS has no remote query protocol, unlike Ceph).
Requires `ATLAS_ZFS_ENABLE=1` at startup. Before running `zpool create`, the job independently
checks (via `lsblk`, `findmnt`, `wipefs`, `zpool status`) that the device: exists, is a whole disk,
isn't read-only, has no partitions/filesystem/partition-table signature, isn't mounted, isn't
already a zpool member, and isn't the host's actual mounted root/boot disk (resolved dynamically,
not just by name). `zpool create` is **never** passed `-f` — that flag exists specifically to
override zpool's own built-in in-use refusal, which stays as an independent safety net underneath
Atlas's own checks.

`zpool create` is synchronous, so the job succeeds as soon as it returns 0. The new pool/root
dataset are written directly into inventory (not left to a follow-up discovery pass) — because
`RealZfsDriver`'s configured zpool list is fixed at gateway startup (`ATLAS_ZFS_POOLS`), a
subsequent discovery pass alone would never notice a pool that wasn't in that list when the
process started. **Known limitation**: this pool's capacity/health numbers go stale until the
gateway is restarted with the pool name added to `ATLAS_ZFS_POOLS`.

## Ceph: `POST /api/atlas/v1/ceph/devices`

```json
{ "node_name": "node-1", "device_path": "/dev/sdb", "confirm": true }
```

Requires a live Kubernetes cluster. Refuses if the target `CephCluster`'s
`storage.useAllDevices` is `true` — that mode already auto-claims every empty device on every
node, so this API only applies when devices are pinned explicitly per node. Patches
`spec.storage.nodes[].devices` via a scoped JSON merge patch (never a full-spec replace, which
would clobber unrelated cluster settings) and is idempotent — safe to retry.

Rook's reconciliation (`ceph-volume prepare`/`activate`) can take **several minutes**. Rather than
blocking the job engine's single worker (which would stall every other tenant's jobs), each
dispatch invocation does one non-blocking check of `ceph device ls` for a matching OSD and returns;
an unfinished check is an `Err` that the job engine's own retry/backoff machinery reschedules
automatically (exponential backoff capped at 64s/attempt, ~25–30 minutes of polling before a final
`failed`). The job only reaches `succeeded` once `ceph device ls` actually shows the OSD attached
— never on CR-patch-accepted alone.

**Dependency**: the safety check before patching the CR reads Rook's own device-discovery
ConfigMap (`local-device-<node>`, populated by Rook's discovery DaemonSet). If that ConfigMap is
absent, the request fails closed (a clear `503`) rather than skipping the check — enable
`ROOK_ENABLE_DISCOVERY_DAEMON` on the Rook operator if this happens.
