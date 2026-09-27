<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# RustFS backend (first integration slice)

Atlas can discover buckets on a [RustFS](https://github.com/rustfs/rustfs) (or other
MinIO-compatible) S3 endpoint. Enable it with `ATLAS_RUSTFS_ENABLE=1`,
`ATLAS_RUSTFS_ENDPOINT=http://<host>:9000`, and optionally `ATLAS_RUSTFS_BUCKETS=<comma-separated
allow-list>` (omit to discover every bucket `ListBuckets` returns). `ATLAS_RUSTFS_DRIVER_MODE`
selects the implementation:

- `fake` (default) — deterministic fixture buckets, no network calls. Safe for `make run` / CI.
- `real` — calls the endpoint's `/health` (or `/minio/health/live`) plus S3 `ListBuckets` /
  `ListObjectsV2`. Never fabricates a bucket the server did not return; an unreachable endpoint
  surfaces as a real discovery error instead.

A backend can also be registered dynamically via `POST /api/atlas/v1/backends` with
`"backend_type": "rustfs"`, `"server"` as the endpoint, and `"targets"` as the bucket allow-list —
the same shape as the existing NFS/ZFS dynamic-registration path.

The backend shows each bucket as an `s3`-kind `StoragePool` and reports one object volume per
bucket (`kind: object`). Credentials are optional — RustFS anonymous read works for public
buckets; signed requests are a follow-up that would reuse `atlas-driver-rgw`'s `rusty-s3` signer.
Secret material is never stored on the driver beyond a process env reference. Snapshot/clone,
volume creation, and other write-path operations are not implemented by this driver — it is
discovery/inventory only, matching the NFS/ZFS/Longhorn read-only backends.
