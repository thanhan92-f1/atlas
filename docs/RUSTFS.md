<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# RustFS backend (primary object storage)

[RustFS](https://github.com/rustfs/rustfs) is Atlas's **default backend for new bucket
provisioning** — `POST /api/atlas/v1/buckets` targets it unless the request explicitly names
another backend (`"backend_id": "bkd_ceph_lab"` for the original Ceph RGW/Rook path, which keeps
working unchanged). Bucket creation is a direct, synchronous, signed S3 `CreateBucket` call — no
Kubernetes operator involved, unlike RGW's `ObjectBucketClaim`.

## Deploying it

The lab deploy stands up a real RustFS server alongside the gateway:
`scripts/deploy-remote.sh <host> <user>` runs [`deploy/rustfs-lab/up.sh`](../deploy/rustfs-lab/up.sh)
before the gateway rollout (skip with `--without-rustfs`, and set `ATLAS_RUSTFS_ENABLE=0` in the
gateway manifest for a cluster that has no RustFS). It creates:

- Deployment/PVC/Service `rustfs` in `zyvor-system` — image `rustfs/rustfs:1.0.0` (pinned), runs as
  uid/gid 10001, single volume `/data` (a lab topology, **not** HA — a production RustFS wants
  multiple drives/nodes for erasure coding).
- NodePorts **30900** (S3 API) and **30901** (web console). The S3 API must be reachable from
  *browsers*: Atlas hands the console presigned PUT/GET URLs, and object bytes go straight
  browser ↔ RustFS, never through the gateway.
- Secret `rustfs-credentials` (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`), generated once and
  never printed or rotated by re-running the script. It is both RustFS's root credential and
  Atlas's service credential (lab shortcut — production should give Atlas its own least-privilege
  IAM user).
- `RUSTFS_CORS_ALLOWED_ORIGINS=http://<node-ip>:30510` — **required for browser uploads.** RustFS's
  default is empty, meaning the S3 endpoint sends no CORS headers, so the console (a different
  origin) gets a preflight with no `Access-Control-Allow-*` and every upload fails with "Failed to
  fetch". Scoped to the console's exact origin, not `*`.

## Gateway configuration

```
ATLAS_RUSTFS_ENABLE=1
ATLAS_RUSTFS_DRIVER_MODE=real                       # fake (default) = fixtures, no network
ATLAS_RUSTFS_ENDPOINT=http://<node-ip>:30900        # browser-reachable: presigned URLs are built from it
ATLAS_RUSTFS_BUCKETS=                               # empty = every bucket; non-empty = allow-list
ATLAS_RUSTFS_CREDENTIALS_SECRET=rustfs-credentials  # Secret the write path resolves (connection_ref)
ATLAS_RUSTFS_CREDENTIALS_NAMESPACE=zyvor-system     # namespace of that Secret
ATLAS_RUSTFS_ACCESS_KEY / ATLAS_RUSTFS_SECRET_KEY   # signed discovery (from the same Secret via secretKeyRef)
ATLAS_RUSTFS_REGION=us-east-1                       # SigV4 region for discovery (default)
```

`deploy/k8s/atlas-gateway.yaml` sets all of these. (Two defects found and fixed while deploying it
for real: the backend used to be registered with no `connection_ref`, so every bucket create/delete
failed with "has no connection_ref configured"; and real mode defaulted an empty bucket list to the
fixture names `vm-images,backups`, silently hiding every real bucket.)

## Discovery (read path)

`ATLAS_RUSTFS_DRIVER_MODE=real` probes `/health` and lists buckets with a **SigV4-signed**
`ListBuckets` (any RustFS with authentication on — every real deployment — rejects anonymous
listing). `rusty-s3` has no service-level `ListBuckets` action, so the driver signs that one
request itself; the signing chain is pinned by a unit test against the AWS SigV4 `get-vanilla`
vector. Without `ATLAS_RUSTFS_ACCESS_KEY`/`SECRET_KEY` it falls back to an anonymous request and
reports "authentication required" if the server refuses. It never fabricates a bucket the server
didn't return; an unreachable endpoint surfaces as a real discovery error. Each bucket shows as an
`s3`-kind `StoragePool` with one object volume (`kind: object`). Capacity fields are left unknown —
RustFS's capacity lives in its own admin API, not S3. Bucket `stats`
(`GET /buckets/{id}/stats`) is unavailable for RustFS buckets (no `radosgw-admin` equivalent) — an
honest `{"available": false, "reason": "..."}`, not a fabricated number.

## Write path — bucket + object CRUD

`POST /buckets` (no `backend_id`, or `"backend_id": "bkd_rustfs_lab"`) enqueues a job that resolves
the credentials (below), checks whether the bucket exists (`HeadBucket`), and if not creates it
(`CreateBucket`) — synchronously, no bind-poll the way Rook's OBC path needs. `DELETE /buckets/{id}`
mirrors this with `DeleteBucket` (S3 itself refuses a non-empty bucket). Once bound, the object-level
routes (`upload-url`/`download-url`/`delete`/`prune`, `GET /buckets/{id}/objects`) work exactly as
for RGW buckets — they all resolve through the same `bucket_s3_target` helper. In the console the
Buckets page shows a **Backend** column (RustFS / Ceph RGW; legacy rows with no `backend_id` are
Ceph) and the create form picks the backend (RustFS default; the namespace/quota fields only apply
to Ceph).

## Credentials

RustFS has no per-bucket credential-provisioning operator the way Rook's OBC does — one
backend-level service credential is shared across every bucket Atlas creates on it. The Secret
named by `ATLAS_RUSTFS_CREDENTIALS_SECRET`, in `ATLAS_RUSTFS_CREDENTIALS_NAMESPACE`, holds the keys
`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` (the same key names the RGW bucket-secret convention
uses). The gateway's ClusterRole already grants cluster-wide `get/list/watch` on `secrets`. Atlas
never writes secrets itself — `deploy/rustfs-lab/up.sh` creates it for the lab; a real deployment
provisions it however it provisions every other credential this gateway consumes.

## Verified live (2026-09-28, lab host, RustFS 1.0.0, through the Atlas console)

- Real, SigV4-signed discovery against the live server (cluster registered, health OK).
- **Bucket create** from the console → job succeeded, bucket `Bound`, bucket directory present in
  RustFS's storage (checked independently on the server, not just Atlas's own inventory).
- **Object upload** from the browser via a presigned PUT straight to RustFS (after fixing CORS —
  see above), listed back with the exact byte size, and **presigned download** returned the exact
  content.

## Still unverified against a real RustFS

- **Multipart upload** (`put_multipart_streaming`) — large-object backups and DataBridge object
  copies depend on it.
- **Bucket delete**, versioned uploads/`prune`, and objects large enough to matter.
- `CreateBucket` sends no body (no `LocationConstraint`) — worked here with the default
  `us-east-1`; not tested with any other region.
- Bucket stats/quota (no equivalent wired up) and per-bucket capacity.
- **Self-state backup and DataBridge object migration destinations** — both use the same generic
  `S3Target` and could be repointed at RustFS as a config change, but neither has been (the lab's
  `ATLAS_STATE_BACKUP_*` still targets a Ceph RGW that isn't deployed on this host).
