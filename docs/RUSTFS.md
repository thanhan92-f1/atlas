<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# RustFS backend (primary object storage)

[RustFS](https://github.com/rustfs/rustfs) is Atlas's **default backend for new bucket
provisioning** — `POST /api/atlas/v1/buckets` targets it unless the request explicitly names
another backend (`"backend_id": "bkd_ceph_lab"` for the original Ceph RGW/Rook path, which keeps
working unchanged). Bucket creation is a direct, synchronous, signed S3 `CreateBucket` call — no
Kubernetes operator involved, unlike RGW's `ObjectBucketClaim`.

## Enable

```
ATLAS_RUSTFS_ENABLE=1
ATLAS_RUSTFS_ENDPOINT=http://<host>:9000
ATLAS_RUSTFS_BUCKETS=<comma-separated allow-list>   # optional; omit to discover every bucket
ATLAS_RUSTFS_DRIVER_MODE=real                       # fake (default) = fixtures, no network
ATLAS_RUSTFS_CREDENTIALS_NAMESPACE=zyvor-system      # namespace of the credentials Secret, below
```

## Discovery (read path — unchanged from the first integration slice)

`ATLAS_RUSTFS_DRIVER_MODE=real` calls the endpoint's `/health` (or `/minio/health/live`) plus an
**anonymous** S3 `ListBuckets`/`ListObjectsV2` — never fabricates a bucket the server didn't
return; an unreachable endpoint surfaces as a real discovery error. Each bucket shows as an
`s3`-kind `StoragePool` with one object volume (`kind: object`). Discovery stays anonymous/unsigned
for this slice — `rusty_s3::Bucket` requires a bucket name and has no signed service-root
`ListBuckets` convenience the way `S3Target`'s other methods have, so credentialed discovery is a
real follow-up, not a trivial one. Bucket `stats` (`GET /buckets/{id}/stats`) is unavailable for
RustFS buckets (no `radosgw-admin` equivalent wired up yet) — the response is an honest
`{"available": false, "reason": "..."}`, not a fabricated number.

## Write path — bucket + object CRUD (this integration slice)

`POST /buckets` (no `backend_id`, or `"backend_id": "bkd_rustfs_lab"`) enqueues a job that:
validates the RustFS backend is enabled, resolves its credentials (below), checks whether the
bucket already exists (`HeadBucket`), and if not, creates it (`CreateBucket`) — synchronously, no
bind-poll the way Rook's OBC path needs. `DELETE /buckets/{id}` mirrors this with `DeleteBucket`
(S3 itself refuses a non-empty bucket, same as RGW). Once bound, every object-level route
(`bucket_object_upload_url`/`download-url`/`delete`/`prune`, `GET /buckets/{id}/objects`) works
exactly as it already does for RGW buckets — they all resolve through the same shared
`bucket_s3_target` helper, keyed off the bucket's own stored endpoint/region/bucket name/secret
reference.

## Credentials

RustFS has no per-bucket credential-provisioning operator the way Rook's OBC does — one
backend-level service credential is shared across every bucket Atlas creates on it. Provision it
as a Kubernetes Secret in the namespace named by `ATLAS_RUSTFS_CREDENTIALS_NAMESPACE`
(default `zyvor-system`, the namespace the gateway itself runs in — no extra RBAC needed, the
ClusterRole already grants cluster-wide `get/list/watch` on `secrets`), with keys
`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` (same key names the existing RGW bucket-secret
convention already uses):

```
kubectl -n zyvor-system create secret generic <secret-name> \
  --from-literal=AWS_ACCESS_KEY_ID=<...> \
  --from-literal=AWS_SECRET_ACCESS_KEY=<...>
```

Then set the RustFS `StorageBackend` row's `connection_ref` to `<secret-name>` (currently set at
backend-registration time in `startup.rs`/`routes/backends.rs` — there is no dedicated API to
update it after the fact yet). Atlas never writes secrets anywhere itself; provisioning the Secret
is an operator/deploy-time action, the same as every other credential this gateway consumes
(`atlas-gateway-auth`'s JWT secret, admin password, etc.).

## Known-unverified risks — pending live testing against a real RustFS server

None of the following have been exercised against a real RustFS instance yet (only against
`rusty-s3`'s own unit-tested signing logic and Ceph RGW). Don't treat this backend as
production-ready for these until each is actually verified live:

- **`CreateBucket` sends no request body** (no `LocationConstraint`) — real AWS S3 requires that
  body for any region other than `us-east-1`; MinIO-family servers are generally lenient, but this
  is unconfirmed for RustFS specifically.
- **Presigned GET/PUT URL compatibility** — SigV4 presigned URLs have known cross-implementation
  footguns (header/query-param signing edge cases, clock skew tolerance, virtual-host vs path-style
  enforcement). Only ever verified live against Ceph RGW so far.
- **Multipart upload** (`put_multipart_streaming`) — large-object backups and DataBridge object
  copies depend on this working correctly against RustFS specifically.

## Self-state backup and DataBridge

Atlas's own control-plane-DB backup (`ATLAS_STATE_BACKUP_*`) and DataBridge's object-migration
destination both already use the same generic `S3Target` this backend does — repointing either to
RustFS is a deploy-manifest/request-parameter change, not a code change (see `docs/DATABRIDGE.md`).
Neither has been switched over in the lab deployment yet, since it has no real RustFS cluster
reachable — both still point at Ceph RGW until one exists.
