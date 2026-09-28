<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Status

Short maturity matrix for Atlas 0.4.0. History and narrative live in [ROADMAP.md](ROADMAP.md).
When the two disagree, this file wins.

A cell is **yes** only for that column. Lab verification is not production support. Nothing
below is bank or enterprise GA.

| Capability | Implemented and unit-tested | Verified on real infrastructure | Production-supported | Experimental | Planned |
|---|---|---|---|---|---|
| Ceph / Rook control plane (RBD, CephFS, RGW, jobs) | yes | yes, single-node Rook lab | no | | |
| NFS and ZFS drivers | yes, fake and real modes | no remote production target | no | real mode | SAN, cloud block, external Ceph import |
| RustFS backend — bucket/object discovery | yes, fake and real modes; SigV4-signed `ListBuckets` (unit-tested against the AWS `get-vanilla` vector) | **yes** — real RustFS 1.0.0 server deployed by `deploy/rustfs-lab/`, signed discovery accepted by it (lab host, 2026-09-28) | no | real mode | bucket capacity (RustFS admin API, not S3) |
| RustFS backend — bucket/object write path (primary object backend) | yes | **yes** — through the console on a real RustFS: bucket create (verified on the server side too), browser presigned PUT upload (needs `RUSTFS_CORS_ALLOWED_ORIGINS`), presigned download, and the console's **self-test** (11 MiB multipart round-trip with SHA check, prefix listing, versioned-key prune, non-empty-bucket delete refusal, object + bucket delete) passing in the default region and `eu-west-1`; self-state backup now uploads to RustFS | no | volume backups to RustFS not run (needs Ceph RBD); DataBridge object migration verified RustFS → RustFS only | bucket stats/quota (no `radosgw-admin` equivalent); **Helm** chart deploys RustFS (lab single-volume server) and was installed and walked live |
| RustFS console — admin API, drives, instances (Storage → RustFS) | yes; allow-listed signed proxy, SigV4 client, drive/instance jobs, unit tests | **yes** — Overview, Drives & pools and Access tabs against the real server; users (generated secret), policy attach, service accounts, bucket settings (versioning, lifecycle, quota); `sdb` formatted XFS from the console and mounted with an fstab entry; RustFS installed from its official chart on that drive from the console, data migrated with Object Migrations, Atlas switched to it (endpoint, credentials, state backup) and buckets re-adopted with Import | no | opt-in `ATLAS_RUSTFS_AUTO_DEVICE` (install/switch steps run live; the format-empty-disk step needs a spare disk and has not been run) | pool decommission/rebalance (need a 2+-drive RustFS), least-privilege IAM user for Atlas, TLS, KMS/replication/tiering/object lock (not exposed) |
| Longhorn backend | yes, read-only (nodes/volumes via CRDs) | no | no | | native write path (PVC provisioning goes through the Kubernetes path today, not this driver) |
| Raw disk provisioning (ZFS pool via `zpool create`; Ceph OSD via Rook `CephCluster` patch) | yes, fake mode + safety-check unit tests + `GET /zfs/devices`/`GET /ceph/nodes/{node}/devices` picker endpoints | ZFS: **verified live end-to-end** on the lab host through the console — wiped a disk carrying a real, stale Ceph OSD partition table, then `zpool create` succeeded (`zpool status`: `tank0`/`ONLINE`, backed by the real device, zero errors). Root-caused and fixed two real bugs along the way: `zpool create`'s partition-reopen polls udev's device database (needs `/run/udev` hostPath-mounted, not just `/dev`), and the root/boot-disk check was blind to the host's real mount table in a container (needs a hostPath-mounted `/proc/1/mountinfo`, `ATLAS_HOST_MOUNTINFO_PATH`) — see `docs/DISKS.md`. Ceph: never run against a Rook cluster with the device-discovery daemon enabled (no Rook cluster on this lab host to test against) | no | yes — depends on the target Rook install running `ROOK_ENABLE_DISCOVERY_DAEMON` | stand up a Rook cluster to verify the Ceph OSD path; (pool destroy → wipe → re-provision was run live on the lab: `tank0` destroyed, `sdb` re-provisioned as `tank1`) |
| SQLite default, Postgres query layer, Helm `database.kind`, cross-replica rate limiting | yes | yes, CI `postgres-test` and `deploy/postgres-lab/` | no — needs a real HA Postgres | | enterprise IdP for OIDC (Dex lab only) |
| Auth, tenants, quotas, audit export, Vault resolution | yes | yes, lab (Dex, Vault, SIEM receiver) | no | | |
| DataBridge Postgres | yes | yes, through cutover | no | TLS still `sslmode=disable` | verified TLS migration |
| DataBridge MariaDB | yes | yes, through cutover | no | | repeatable CI/lab automation |
| DataBridge MongoDB | yes | yes, through cutover | no | | change-stream edge cases, repeatable verification |
| DataBridge MySQL | yes | CDC live for `DATETIME` only | no | `TIMESTAMP` CDC | cutover; `TIMESTAMP` SMT |
| DataBridge SQL Server and Oracle | discovery yes | discovery live | no | | full-load, CDC, validate, cutover, rollback; Oracle TCPS |
| Cross-cluster DR (RBD mirror) | control plane yes; `dataplane_verified` is false | no | no | yes | two-site promote/demote drill |
| Ops Advisor, incidents, what-if, anomalies, MCP | yes, read-only | console exercised on the lab gateway | no | | persisted findings; no execution |
| Product integrations beyond gRPC `Owner` | gRPC owner surface yes | | no | | Transiva import, Veyron, GuestKit, PacketWolf, then a small SDK |

Transiva's owner id on the wire remains `hyper2kvm`. v0.4.0 does not rename it.

**Unresolved:** Zeus OS already has an `atlas` module ("Machine Finder") and a Storage Center
UI. Atlas does not yet absorb, replace, or sit beside that UI. Do not treat the names as settled.
