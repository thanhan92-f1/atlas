<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
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

## The console: one RustFS area

Everything RustFS lives under **Storage → RustFS** (plus a per-bucket **Settings** panel on the Buckets
page), and the raw-disk formatting that used to be its own page is folded into it (Drives & pools →
"Format a disk for RustFS"; `/disks` still works).

- **Overview / Drives & pools / Access** call RustFS's *own native admin API*
  (`/rustfs/admin/v3/...`) through an allow-listed, server-signed proxy
  (`ANY /api/atlas/v1/rustfs/proxy/admin/...`, code in `routes/rustfs.rs`; signing in
  `atlas-driver-rustfs/src/client.rs`, the same SigV4 scheme RustFS's own `rustfs-madmin` client uses).
  RustFS's JSON is passed through unchanged. Forwarded: server/storage/data-usage info, pools
  (list, decommission, cancel), rebalance, heal, users, groups, canned policies, policy attachment,
  service accounts, bucket quota. NOT forwarded: server update/restart, config, IAM import/export,
  inspect-data, KMS, lock-breaking. Every write is audited; the credential never leaves the gateway,
  and each server's `rustfs_env_vars` (which can carry the root key) is scrubbed from `/info`.
- **Bucket Settings** (RustFS buckets): versioning, lifecycle rules, access policy (private / public
  read / JSON), quota, and a version browser — via `/rustfs/proxy/s3/{bucket}?<subresource>` (versioning,
  lifecycle, policy, tagging, cors, object-lock, encryption, versions).
- **Users**: leave the secret empty to have one generated (shown once).

### Drives: how RustFS uses disks

RustFS never formats or mounts disks; it uses a directory Kubernetes gives it (the official chart takes
a PVC). So Disks → **RustFS drive (XFS)** does the Kubernetes side: the same hard refusals as the ZFS path
(root/boot disk, mounted, active pool member — never overridable), an optional wipe, then a throwaway
root **node-prep Job** (`nsenter` into the host mount namespace) formats `mkfs.xfs` (never forced), mounts
it at `/mnt/atlas-disks/<disk>`, adds an fstab entry (`nofail`), and Atlas creates a `local` PV (StorageClass
`atlas-rustfs-local`) plus a PVC `rustfs-<disk>-data` bound to it. Verified live on the lab (`sdb`: XFS,
mounted, fstab, PV Bound).

### RustFS servers: the official chart, deployed from the console

RustFS runs from **its own Helm chart** (https://charts.rustfs.com, vendored at
`deploy/helm/atlas/charts/rustfs-1.0.0.tgz`, `helm` pinned and checksum-verified in the gateway image).
The console's **Deploy RustFS…** starts an installer Job (its own least-privilege ServiceAccount
`atlas-rustfs-installer`) that runs `helm install` in standalone mode on a chosen drive claim; the root
credential is generated inside that Job and lands only in the chart's Secret. **Use for Atlas** patches the
gateway's own Deployment (endpoint, credentials Secret, state backup) and restarts it; **Uninstall**
removes a chart-managed instance (its data claim is kept). A single-drive RustFS cannot be expanded in
place or joined as a pool (RustFS's docs) — to move data, use DataBridge → Object Migrations (destination
buckets are created if missing), then **Import from RustFS** on the Buckets page to re-adopt the buckets.

### Install-time automation (opt-in)

Formatting a disk is deliberately never implicit — Atlas cannot know which disk is safe to take. To get
RustFS on a disk straight from an install, name that one disk: `ATLAS_RUSTFS_AUTO_DEVICE=/dev/sdb`
(Helm: `disks.enabled=true`, `disks.autoRustfsDevice=/dev/sdb`, optionally `disks.autoRustfsActivate=true`).
On start the gateway (`spawn_rustfs_auto`) checks the cluster's current state each step, so a restart
resumes: it formats the disk **only if it is completely empty — it never wipes**; a disk that holds
anything (or is the root/boot disk, mounted, a pool member) is reported in the gateway log and left
alone; then it installs RustFS from the official chart on the resulting drive and, with
`ATLAS_RUSTFS_AUTO_ACTIVATE=1`, points Atlas at it. Data already in a previous RustFS is not moved
automatically — use Object Migrations.

## TLS (do this before production)

Every setup in this doc runs RustFS over plain HTTP — fine for a lab on a private network, not for
production, where the root (or scoped) credential and every object would cross the wire unencrypted.
RustFS terminates TLS itself: point `RUSTFS_TLS_PATH` at a directory holding `tls.crt`/`tls.key` and
it serves HTTPS on both the S3 and console ports. Two things to get right, both already handled by
the pieces below and worth knowing if you build this by hand instead:

- RustFS does **not** read `tls.crt`/`tls.key` by name — its default `RUSTFS_TLS_PATH` loader looks
  for files literally named `rustfs_cert.pem`/`rustfs_key.pem` (`crates/config/src/constants/app.rs`'s
  `RUSTFS_TLS_CERT`/`RUSTFS_TLS_KEY`). A `kubernetes.io/tls` Secret's keys are `tls.crt`/`tls.key`, so
  the volume mount must remap them with the Secret volume's `items` field (found live, 2026-09-28: the
  pod crash-looped with "no server certificates were found" until this was added).
- The chart's own liveness/readiness probes (`rustfs.probes` in `_helpers.tpl`) are plain HTTP unless
  its much heavier `mtls.enabled` (cert-manager-issued client+server certs, mutual TLS) is turned on.
  A `RUSTFS_TLS_PATH`-only server makes port 9000 TLS-only, so the chart's default HTTP probes fail
  forever and the pod never goes Ready. The install script disables `livenessProbe`/`readinessProbe`
  when a `tls_secret` is given rather than pull in full mTLS for what is meant to be simple server TLS.

The official chart's `extraVolumes`/`extraVolumeMounts`/`extraEnv` (already used above for CORS) mount
an existing Secret and set `RUSTFS_TLS_PATH` — no chart fork needed.

**Server side** — pass a `kubernetes.io/tls` Secret name (keys `tls.crt`/`tls.key`) when deploying:
console's **Deploy RustFS…** → "TLS Secret (optional)", or `JobSpec::RustfsInstance.tls_secret`. For a
lab/self-signed cert: `scripts/rustfs-tls-selfsigned.sh <host> <user> <secret-name>` (generates the key
pair and creates the Secret entirely on the remote host — the private key is never printed or leaves
that host). The generated cert sets `basicConstraints=CA:FALSE`, `extendedKeyUsage=serverAuth` and a
`subjectAltName` covering the host's IP — a plain `openssl req -x509 -subj` defaults to `CA:TRUE` and
no SAN, which rustls's verifier rejects outright (`invalid peer certificate:
Other(OtherError(CaUsedAsEndEntity))`, found live chasing why discovery couldn't reach an otherwise
healthy HTTPS RustFS). For production, use a cert issued by your own CA or a public one instead.

**Client side (Atlas trusting the cert)** — if the cert's CA is already in the container's system
trust store (a public CA, or your org's CA baked into the image), nothing else is needed: `RustfsClient`,
`S3Target`, and the discovery driver's client (`atlas_driver_core::trusted_http_client`, used by all
three) trust the system roots like any `reqwest::Client`. For a private CA or a self-signed cert
(the only option for a throwaway lab cert), also set `ATLAS_RUSTFS_CA_CERT` to a mounted copy of that
same CA/cert — the Helm chart's `rustfs.caSecretName` value does this (mounts the named Secret's
`tls.crt` into the gateway at a fixed path and sets the env var); it only *adds* that one certificate
to the trust store, never removes the system roots or skips verification. `POST /rustfs/instances`'s
`tls_secret` and the gateway's `rustfs.caSecretName` are typically the *same* Secret for a self-signed
lab cert (it is its own root), and typically *unset* for a real-CA cert (nothing extra to trust). The
raw `deploy/k8s/atlas-gateway.yaml` manifest does not wire this (it has no per-field templating); mirror
the Helm chart's `ATLAS_RUSTFS_CA_CERT`/volume block by hand if you need TLS trust without Helm.
`ATLAS_RUSTFS_ENDPOINT`'s scheme (`http://` vs `https://`) is set automatically by **Use for Atlas**,
based on whether the instance's Deployment has `RUSTFS_TLS_PATH` set.

If `ATLAS_RUSTFS_AUTO_DEVICE`/`ATLAS_RUSTFS_AUTO_ACTIVATE=1` are set (install-time disk automation),
they keep polling and will switch Atlas back to that auto-provisioned instance within ~20s of any
manual **Use for Atlas** to a *different* instance, for up to 30 minutes after gateway startup — set
`ATLAS_RUSTFS_AUTO_ACTIVATE=0` first if you want a manual switch (e.g. to a TLS instance) to stick.

Status: **verified live end to end**, 2026-09-28, on the lab (`80.79.5.173`): self-signed cert
generated via `scripts/rustfs-tls-selfsigned.sh`, `rustfs-tls` instance deployed (probes disabled,
cert filenames remapped as above) and reached `Ready`; confirmed serving HTTPS on both ports and
rejecting plain HTTP; gateway's own Deployment patched with `ATLAS_RUSTFS_CA_CERT` trusting the
self-signed cert (mirroring the Helm `rustfs.caSecretName` block by hand, since this lab runs the raw
manifest); **Use for Atlas** switched the endpoint/credentials to `https://…:30932`, `/readyz` stayed
healthy, discovery completed cleanly (`cluster=cls_rustfs_https___80_79_5_173_30932`), and a bucket
create/delete round-tripped over HTTPS. Switched back to the lab's plain-HTTP instance afterward.

## Least-privilege credential for Atlas (do this before production)

Every setup in this doc so far puts RustFS's **root** access/secret key in `rustfs.credentialsSecret` —
fine for a lab, not for production: Atlas's own compromise would mean full control of the RustFS server
(every bucket, every user, server config). RustFS's admin API is policy-driven like everything else in
it (verified by reading the source: every admin handler calls `authorize_admin_request` against an
`Action`, the same mechanism as S3 bucket actions — nothing is root-only), so a scoped credential covers
everything the console's RustFS proxy (`routes/rustfs.rs`) forwards, and nothing else.

**The policy** — [`deploy/rustfs-lab/atlas-service-policy.json`](../deploy/rustfs-lab/atlas-service-policy.json) —
grants exactly the actions the proxy's allow-lists use, cross-checked against RustFS's own handler source
(`crates/policy/src/policy/action.rs` and each `rustfs/src/admin/handlers/*.rs`'s `authorize_admin_request`
call): bucket/object CRUD and configuration (versioning, lifecycle, policy, tagging, CORS, quota), the
admin actions behind Overview/Drives & pools/Access (server/storage/usage info, pools, heal, rebalance,
users, groups, canned policies, service accounts), and **read-only** tiering (`admin:ListTier`) and KMS key
metadata (`kms:ListKeys`, `kms:DescribeKey`) — see the "Tiering and KMS" section below. It does **not**
grant tier add/edit/remove, KMS key lifecycle or service status/config, replication, object lock
configuration changes, server update/restart/config, or IAM import/export — those stay root-only, matching
what the console already refuses to forward (see `admin_allowed`/`s3_allowed` in `routes/rustfs.rs`).
A handful of read-only admin calls (pool/decommission/rebalance *status*, heal status, quota stats/check)
have no explicit action check in the RustFS source as of this writing — they are covered by the same
statements as their sibling write actions; if one ever 403s, add its specific action once identified.

**Provisioning** (through the console you already have; nothing new to deploy):
1. Storage → RustFS → Access → Service accounts. Leave "Owner access key" blank (creates it under the
   currently-configured root user). Paste the policy JSON above into "Restricting policy". Create.
2. Copy the one-time access/secret key the console shows (it cannot be retrieved again).
3. `kubectl -n <namespace> create secret generic rustfs-sdb-atlas-scoped \`
   `  --from-literal=AWS_ACCESS_KEY_ID=<copied> --from-literal=AWS_SECRET_ACCESS_KEY=<copied>` — Atlas
   never creates this Secret itself (same "Atlas never writes secrets" rule as every other credential here).
4. On the instance's row, **Use different credentials…** → the Secret name from step 3 → Switch and
   restart. This patches only the credentials env vars (`POST /rustfs/instances/{name}/credentials`, no
   new RBAC — it's the same own-Deployment `patch` the console's "Use for Atlas" already has) and keeps
   the endpoint unchanged.

Status: the policy is verified against RustFS's source, not yet exercised live with the scoped key end to
end (that needs the key itself, which this flow deliberately never lets Atlas or its operator's tooling
see except once, in the browser) — after switching, watch the Overview/Drives & pools/Access tabs and the
bucket write path for any 403, and widen the specific missing action rather than reverting to root.

## Tiering and KMS (read-only)

RustFS supports ILM storage tiering (transition cold objects to a remote S3/Azure/GCS target) and
Vault-backed KMS encryption, but only a **read-only** slice of each is exposed through the console —
Drives & pools' "Tiers" panel (`GET admin/v3/tier`, `v3/tier-stats`, `v3/tier/{name}`) and Access's "KMS
keys" panel (`GET admin/v3/kms/list-keys`, `v3/kms/describe-key`). Two things kept the rest out of this
pass, both found by reading RustFS's own source rather than assumed:

- **Tier add/edit/remove** (`PUT`/`POST`/`DELETE admin/v3/tier...`) need a remote target's endpoint and
  credentials entered somewhere — a bigger UX/credential-handling decision than a first read-only pass,
  and deferred with it.
- **KMS key lifecycle** (rotate/enable/disable/delete) carries real data-loss risk if a key backing live
  objects is disabled or deleted, and deserves a dedicated typed-name-confirm flow (like pool destroy),
  not a first pass. `GET admin/v3/kms/status` and `/kms/config` are also left out even though they only
  *read* — RustFS gates them behind `kms:ServiceControl`/`kms:Configure`, the **same actions** that gate
  actual KMS service start/stop/reconfigure in `kms_dynamic.rs`, so granting either to Atlas's credential
  just to show a status page would also hand it those write capabilities. `kms:ListKeys`/`kms:DescribeKey`
  are separate, narrow, metadata-only actions with no such overlap (verified 2026-09-28).
- **Replication** isn't exposed at all yet, read-only or otherwise — it needs a second live S3-compatible
  cluster to test against, which this lab doesn't have (the same reason RBD-mirroring DR is deferred in
  the top-level `CLAUDE.md`). Revisit once a second RustFS/S3 endpoint exists to verify against.

## Self-test (conformance check, console button)

`POST /api/atlas/v1/backends/bkd_rustfs_lab/selftest` (body `{"region": "eu-west-1"}`, optional;
admin) — the Buckets page's **RustFS self-test** button. An async job (`s3.backend.selftest`) that
creates a throwaway `atlas-selftest-*` bucket on the live server and checks: create/head bucket,
put/get of a small object, an **11 MiB multipart upload** (3 parts, SHA-256 checked against the
source) and its streaming download, listing by prefix with sizes, the key-suffix "versioned" upload +
prune scheme (`ver.db.<n>`), that deleting a **non-empty bucket is refused** (`409 BucketNotEmpty`),
object delete and bucket delete. It cleans up after itself even on failure. Every step's outcome is
in the job output (a failing job's error carries the per-step report). For any region other than
`us-east-1`, `CreateBucket` sends a `LocationConstraint` body.

## Self-state backup

The lab gateway's `ATLAS_STATE_BACKUP_*` now targets RustFS (`http://<node-ip>:30900`, bucket
`atlas-self-state`, credentials from `rustfs-credentials`). The backup creates the bucket on first
use (`HeadBucket` → `CreateBucket`).

## Verified live (2026-09-28, lab host, RustFS 1.0.0, through the Atlas console)

- Real, SigV4-signed discovery against the live server (cluster registered, health OK).
- **Bucket create** from the console → job succeeded, bucket `Bound`, bucket directory present in
  RustFS's storage (checked independently on the server, not just Atlas's own inventory).
- **Object upload** from the browser via a presigned PUT straight to RustFS (after fixing CORS —
  see above), listed back with the exact byte size, and **presigned download** returned the exact
  content.
- **Self-test, default region and `eu-west-1`**: both runs succeeded — multipart upload/download
  round-trip, prefix listing, versioned-key prune, non-empty-bucket delete refusal, object delete
  and bucket delete against the real server. The first run caught a real bug: `HeadBucket` was
  signed for HEAD but sent as GET, so `bucket_exists` always answered "missing" (harmless for the
  create path, but it would have made the state backup's ensure-bucket step fail every run after the
  first).
- **Self-state backup**: two uploads (`atlas-state/atlas-state-*.db`, ~545 KB) into RustFS, the
  second into the already-existing bucket.

### Verified live, second pass (2026-09-28)

- **Console walk (raw-manifest gateway):** object browser; **50 MiB browser upload** and presigned
  download **byte-identical** (SHA-256 matched); keep-N versioned uploads (3 uploads, keep 2 → 2
  remain); object delete; bucket delete **refused while non-empty** (`BucketNotEmpty` shown in the
  UI), then succeeded once emptied; "stats unavailable" copy for RustFS buckets.
- **DataBridge object migration into RustFS** from the new **Object Migrations** console page
  (RustFS → RustFS, credentials resolved server-side via `source_backend_id`/`dest_backend_id`):
  `completed`, verified — on both the raw-manifest gateway (54 B) and the Helm gateway (8 MiB).
- **Helm deployment** (`deploy/helm/atlas`, `rustfs.server.enabled`, release `atlas-helm`, side by
  side with the raw-manifest install): the in-chart RustFS server and credentials Secret came up, the
  gateway registered and discovered it, its state backup uploaded into it, the CORS preflight allowed
  only the console origin, and through the Helm console the self-test passed, buckets were created in
  the release namespace, an 8 MiB browser upload worked and a migration completed.

## Helm

See [`deploy/helm/atlas/README.md`](../deploy/helm/atlas/README.md): `rustfs.enabled`,
`rustfs.server.*` (lab single-volume server, generate-once credentials Secret, CORS origin),
`stateBackup.useRustfs`, and `disks.enabled` for raw-disk formatting.

## Still unverified against a real RustFS

- **DataBridge object migration from a non-RustFS source** (AWS/GCS/Azure) into RustFS — only the
  RustFS → RustFS leg was run.
- **Volume backups to RustFS** (`export-diff` → S3) — needs Ceph RBD, which the lab host has none of.
- Real S3 object *versioning* (the `?versions` API) — Atlas's "versioned uploads" are a key-suffix
  convention, which the self-test does cover.
- Bucket stats/quota (no equivalent wired up) and per-bucket capacity.
- Whether RustFS itself enforces the region: with the lab's default server config both
  `us-east-1` and `eu-west-1` were accepted.
