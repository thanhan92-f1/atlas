// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Admin proxy to the RustFS server: `/rustfs/proxy/admin/...` forwards to RustFS's own native
//! admin API (`/rustfs/admin/v3/...`, plain JSON) and `/rustfs/proxy/s3/{bucket}?<sub-resource>`
//! to the S3 bucket configuration calls (versioning, lifecycle, policy, ...). RustFS's JSON/XML is
//! passed through unchanged — Atlas signs the request with the backend's service credential and
//! adds authorization (admin role), an allow-list of exactly which operations may be forwarded, and
//! an audit record for every write. The credential never leaves the gateway.

use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_driver_rustfs::RustfsClient;
use atlas_jobs::JobSpec;

use super::util::accepted;

use crate::auth::Actor;
use crate::state::AppState;

/// Admin API operations that may be forwarded (`rel` is the path after `/rustfs/admin/`). A
/// deliberately explicit list: server update/restart, config, file import/export, inspect-data,
/// KMS and lock-breaking endpoints are not reachable through here.
fn admin_allowed(method: &Method, rel: &str) -> bool {
    let exact_get = [
        "v3/info",
        "v3/storageinfo",
        "v3/datausageinfo",
        "v3/pools/list",
        "v3/pools/status",
        "v3/decommission/status",
        "v3/rebalance/status",
        "v3/list-users",
        "v3/user-info",
        "v3/list-canned-policies",
        "v3/info-canned-policy",
        "v3/groups",
        "v3/group",
        "v3/list-service-accounts",
        "v3/info-service-account",
        "v3/get-bucket-quota",
    ];
    let exact_put = [
        "v3/add-user",
        "v3/set-user-status",
        "v3/add-canned-policy",
        "v3/set-user-or-group-policy",
        "v3/set-group-status",
        "v3/update-group-members",
        "v3/set-bucket-quota",
        "v3/add-service-account",
    ];
    let exact_post = [
        "v3/update-service-account",
        "v3/pools/decommission",
        "v3/pools/cancel",
        "v3/rebalance/start",
        "v3/rebalance/stop",
        "v3/background-heal/status",
    ];
    let exact_delete = [
        "v3/remove-user",
        "v3/remove-canned-policy",
        "v3/delete-service-account",
    ];
    match *method {
        Method::GET => {
            exact_get.contains(&rel)
                || rel.starts_with("v3/quota/")
                || rel.starts_with("v3/quota-stats/")
        }
        Method::PUT => exact_put.contains(&rel) || rel.starts_with("v3/quota/"),
        Method::POST => {
            exact_post.contains(&rel)
                || rel.starts_with("v3/quota-check/")
                || rel == "v3/heal/"
                || rel.starts_with("v3/heal/")
        }
        Method::DELETE => {
            exact_delete.contains(&rel)
                || rel.starts_with("v3/group/")
                || rel.starts_with("v3/quota/")
        }
        _ => false,
    }
}

/// Bucket-level S3 sub-resources that may be read/written through the proxy.
const S3_SUBRESOURCES: [&str; 8] = [
    "versioning",
    "lifecycle",
    "policy",
    "tagging",
    "cors",
    "object-lock",
    "encryption",
    "location",
];

fn s3_allowed(method: &Method, bucket: &str, query: &[(String, String)]) -> bool {
    let valid_bucket = (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && !bucket.starts_with(['.', '-'])
        && !bucket.ends_with(['.', '-']);
    if !valid_bucket || query.len() != 1 {
        return false;
    }
    let key = query[0].0.as_str();
    if key == "versions" {
        return *method == Method::GET;
    }
    if !S3_SUBRESOURCES.contains(&key) {
        return false;
    }
    match *method {
        Method::GET => true,
        Method::PUT | Method::DELETE => key != "location",
        _ => false,
    }
}

fn parse_query(raw: Option<String>) -> Vec<(String, String)> {
    raw.unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `ANY /rustfs/proxy/{*rest}` — see the module docs.
pub(crate) async fn rustfs_proxy(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    method: Method,
    Path(rest): Path<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Response> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !s.config.rustfs_enable
        || s.config.rustfs_driver_mode != atlas_common::config::DriverMode::Real
    {
        return Err(AppError::Unavailable(
            "RustFS admin needs the real RustFS backend (ATLAS_RUSTFS_ENABLE=1, ATLAS_RUSTFS_DRIVER_MODE=real)"
                .into(),
        ));
    }
    let endpoint = s
        .config
        .rustfs_endpoint
        .clone()
        .ok_or_else(|| AppError::Unavailable("ATLAS_RUSTFS_ENDPOINT is not configured".into()))?;
    let query = parse_query(raw_query);

    let (path, target) = if let Some(rel) = rest.strip_prefix("admin/") {
        if !admin_allowed(&method, rel) {
            return Err(AppError::Forbidden(format!(
                "{method} admin/{rel} is not an operation Atlas forwards to RustFS"
            )));
        }
        (format!("/rustfs/admin/{rel}"), format!("admin/{rel}"))
    } else if let Some(bucket) = rest.strip_prefix("s3/") {
        if !s3_allowed(&method, bucket, &query) {
            return Err(AppError::Forbidden(format!(
                "{method} s3/{bucket} with that query is not an operation Atlas forwards to RustFS"
            )));
        }
        (format!("/{bucket}"), format!("s3/{bucket}"))
    } else {
        return Err(AppError::NotFound(
            "expected /rustfs/proxy/admin/... or /rustfs/proxy/s3/{bucket}".into(),
        ));
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| (!body.is_empty()).then(|| "application/json".to_string()));
    let client = RustfsClient::from_env(&endpoint).map_err(|e| AppError::Unavailable(e.to_string()))?;
    let resp = client
        .request(method.clone(), &path, &query, body.to_vec(), content_type.as_deref())
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;

    if method != Method::GET {
        let _ = atlas_inventory::audit::record(
            &s.pool,
            None,
            &actor.id,
            &format!("rustfs.{}", method.as_str().to_ascii_lowercase()),
            "rustfs",
            &target,
            if resp.status < 400 { "success" } else { "failed" },
            Some(json!({ "query_keys": query.iter().map(|(k, _)| k).collect::<Vec<_>>(), "status": resp.status })),
            None,
        )
        .await;
    }

    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY);
    // /info lists each server's environment (`rustfs_env_vars`), which can carry the root
    // credential — it never leaves the gateway.
    let resp_body = if path == "/rustfs/admin/v3/info" {
        scrub_env_vars(&resp.body)
    } else {
        resp.body
    };
    let mut out = (status, resp_body).into_response();
    if let Some(ct) = resp.content_type.and_then(|c| c.parse().ok()) {
        out.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    Ok(out)
}

/// Remove every `rustfs_env_vars` key from a JSON document (returns the input unchanged if it is
/// not JSON).
fn scrub_env_vars(body: &[u8]) -> Vec<u8> {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.remove("rustfs_env_vars");
                m.values_mut().for_each(walk);
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(mut v) => {
            walk(&mut v);
            serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
        }
        Err(_) => body.to_vec(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct DriveFromDeviceBody {
    device_path: String,
    confirm: bool,
    /// Explicit second opt-in to clear a stale partition table/filesystem signature first.
    #[serde(default)]
    wipe_existing: bool,
}

/// `POST /rustfs/drives/from-device` — format a raw local disk XFS, mount it on the host and expose
/// it as a local PV + PVC a RustFS deployment can use (async job). Same hard refusals as the ZFS
/// path (root/boot disk, mounted, active pool member) that `wipe_existing` can never override.
pub(crate) async fn provision_rustfs_drive(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<DriveFromDeviceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !body.confirm {
        return Err(AppError::Validation(
            "confirm=true is required to format a raw device".into(),
        ));
    }
    atlas_common::device::validate_raw_device_path(&body.device_path)
        .map_err(AppError::Validation)?;
    let spec = JobSpec::RustfsDriveProvision {
        device_path: body.device_path.clone(),
        confirmed_device_path: body.device_path.clone(),
        wipe_existing: body.wipe_existing,
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "rustfs.drive.provision.requested",
        "device",
        &body.device_path,
        "accepted",
        Some(json!({ "wipe_existing": body.wipe_existing })),
        None,
    )
    .await;
    Ok(accepted(&job, json!({ "device": body.device_path })))
}

/// `GET /rustfs/drives` — the RustFS drives Atlas prepared: local PVs labelled
/// `atlas.zyvor.dev/rustfs-drive=true` (path, node, size, and the claim that uses each).
pub(crate) async fn list_rustfs_drives(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let Some(k8s) = s.k8s.as_ref() else {
        return Ok(Json(json!([])));
    };
    let pvs = k8s
        .list_pvs_json("atlas.zyvor.dev/rustfs-drive=true")
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    let rows: Vec<Value> = pvs
        .iter()
        .map(|pv| {
            json!({
                "name": pv["metadata"]["name"],
                "path": pv["spec"]["local"]["path"],
                "capacity": pv["spec"]["capacity"]["storage"],
                "node": pv["spec"]["nodeAffinity"]["required"]["nodeSelectorTerms"][0]["matchExpressions"][0]["values"][0],
                "storage_class": pv["spec"]["storageClassName"],
                "phase": pv["status"]["phase"],
                "claim": pv["spec"]["claimRef"]["name"],
                "claim_namespace": pv["spec"]["claimRef"]["namespace"],
            })
        })
        .collect();
    Ok(Json(json!(rows)))
}

// ---- RustFS instances (official Helm chart, installed from the console) ----

fn pod_namespace() -> String {
    std::env::var("ATLAS_POD_NAMESPACE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "zyvor-system".into())
}

fn label<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
    obj["metadata"]["labels"][key].as_str()
}

/// One RustFS server found in the gateway's namespace: installed by RustFS's chart (labelled
/// `app.kubernetes.io/name=rustfs`) or by the lab manifest (`app=rustfs`).
struct Instance {
    name: String,
    helm: bool,
    ready: i64,
    claim: Option<String>,
    s3_port: Option<i64>,
    console_port: Option<i64>,
    secret: String,
}

async fn find_instances(s: &AppState) -> AppResult<Vec<Instance>> {
    let Some(k8s) = s.k8s.as_ref() else {
        return Ok(vec![]);
    };
    let ns = pod_namespace();
    let deployments = k8s
        .list_json("apps", "v1", "Deployment", &ns, None)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    let services = k8s
        .list_json("", "v1", "Service", &ns, None)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    let node_port = |svc: &str, port: i64| -> Option<i64> {
        services
            .iter()
            .find(|v| v["metadata"]["name"] == svc)?["spec"]["ports"]
            .as_array()?
            .iter()
            .find(|p| p["port"].as_i64() == Some(port))?["nodePort"]
            .as_i64()
    };
    let mut out = Vec::new();
    for d in &deployments {
        let name = d["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let helm = label(d, "app.kubernetes.io/name") == Some("rustfs");
        let legacy = label(d, "app") == Some("rustfs");
        if !helm && !legacy {
            continue;
        }
        let svc = if helm { format!("{name}-svc") } else { name.clone() };
        let claim = d["spec"]["template"]["spec"]["volumes"]
            .as_array()
            .and_then(|vols| vols.iter().find(|v| v["name"] == "data"))
            .and_then(|v| v["persistentVolumeClaim"]["claimName"].as_str())
            .map(str::to_string);
        out.push(Instance {
            secret: if helm { format!("{name}-secret") } else { "rustfs-credentials".into() },
            ready: d["status"]["readyReplicas"].as_i64().unwrap_or(0),
            s3_port: node_port(&svc, 9000),
            console_port: node_port(&svc, 9001),
            claim,
            helm,
            name,
        });
    }
    Ok(out)
}

fn is_active(s: &AppState, i: &Instance) -> bool {
    match (&s.config.rustfs_endpoint, i.s3_port) {
        (Some(ep), Some(port)) => ep.ends_with(&format!(":{port}")),
        _ => false,
    }
}

/// `GET /rustfs/instances` — every RustFS server in the gateway's namespace, and which one Atlas is
/// currently pointed at.
pub(crate) async fn list_rustfs_instances(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let rows: Vec<Value> = find_instances(&s)
        .await?
        .iter()
        .map(|i| {
            json!({
                "name": i.name,
                "managed_by": if i.helm { "helm" } else { "manifest" },
                "ready": i.ready > 0,
                "claim": i.claim,
                "s3_node_port": i.s3_port,
                "console_node_port": i.console_port,
                "credentials_secret": i.secret,
                "active": is_active(&s, i),
            })
        })
        .collect();
    Ok(Json(json!(rows)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct InstallInstanceBody {
    name: String,
    /// Data claim, typically a drive prepared from the Disks page; empty = the chart's own PVC.
    #[serde(default)]
    pvc: String,
    s3_node_port: u16,
    console_node_port: u16,
}

/// `POST /rustfs/instances` — install a RustFS server from RustFS's official chart (async job).
pub(crate) async fn install_rustfs_instance(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<InstallInstanceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    super::util::validate_k8s_name(&body.name)?;
    if find_instances(&s).await?.iter().any(|i| i.name == body.name) {
        return Err(AppError::Conflict(format!("RustFS instance {} already exists", body.name)));
    }
    let spec = JobSpec::RustfsInstance {
        action: "install".into(),
        name: body.name.clone(),
        pvc: body.pvc.clone(),
        s3_node_port: body.s3_node_port,
        console_node_port: body.console_node_port,
    };
    enqueue_instance_job(&s, &actor, spec, "rustfs.instance.install.requested", &body.name).await
}

/// `DELETE /rustfs/instances/{name}` — uninstall a chart-managed instance (its data claim is kept).
pub(crate) async fn delete_rustfs_instance(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let instances = find_instances(&s).await?;
    let inst = instances
        .iter()
        .find(|i| i.name == name)
        .ok_or_else(|| AppError::NotFound(format!("RustFS instance {name}")))?;
    if !inst.helm {
        return Err(AppError::Conflict(
            "this instance was not installed from the chart; remove it with the manifest that created it".into(),
        ));
    }
    if is_active(&s, inst) {
        return Err(AppError::Conflict(
            "Atlas is using this instance — switch Atlas to another instance first".into(),
        ));
    }
    let spec = JobSpec::RustfsInstance {
        action: "uninstall".into(),
        name: name.clone(),
        pvc: String::new(),
        s3_node_port: 0,
        console_node_port: 0,
    };
    enqueue_instance_job(&s, &actor, spec, "rustfs.instance.uninstall.requested", &name).await
}

async fn enqueue_instance_job(
    s: &AppState,
    actor: &Actor,
    spec: JobSpec,
    audit_action: &str,
    name: &str,
) -> AppResult<(StatusCode, Json<Value>)> {
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    let _ = atlas_inventory::audit::record(
        &s.pool, None, &actor.id, audit_action, "rustfs-instance", name, "accepted", None, None,
    )
    .await;
    Ok(accepted(&job, json!({ "instance": name })))
}

/// Patch the gateway's own Deployment so Atlas uses `inst` (endpoint, credentials Secret, and the
/// state backup when it is enabled); the resulting rollout restarts the gateway. Returns the S3 port.
async fn switch_atlas_to(s: &AppState, inst: &Instance) -> AppResult<i64> {
    let k8s = s
        .k8s
        .as_ref()
        .ok_or_else(|| AppError::Unavailable("Kubernetes is not available".into()))?;
    if inst.ready == 0 {
        return Err(AppError::Conflict("the instance is not ready yet".into()));
    }
    let port = inst
        .s3_port
        .ok_or_else(|| AppError::Conflict("the instance has no S3 node port".into()))?;
    let (ak_key, sk_key) = if inst.helm {
        ("RUSTFS_ACCESS_KEY", "RUSTFS_SECRET_KEY")
    } else {
        ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY")
    };
    let endpoint = format!("http://$(NODE_IP):{port}");
    let secret_ref = |key: &str| {
        json!({ "secretKeyRef": { "name": inst.secret, "key": key, "optional": true } })
    };
    let mut env = vec![
        json!({ "name": "ATLAS_RUSTFS_ENDPOINT", "value": endpoint }),
        json!({ "name": "ATLAS_RUSTFS_CREDENTIALS_SECRET", "value": inst.secret }),
        json!({ "name": "ATLAS_RUSTFS_ACCESS_KEY", "valueFrom": secret_ref(ak_key) }),
        json!({ "name": "ATLAS_RUSTFS_SECRET_KEY", "valueFrom": secret_ref(sk_key) }),
    ];
    if std::env::var("ATLAS_STATE_BACKUP_SECS").is_ok() {
        env.extend([
            json!({ "name": "ATLAS_STATE_BACKUP_ENDPOINT", "value": endpoint }),
            json!({ "name": "ATLAS_STATE_BACKUP_ACCESS_KEY", "valueFrom": secret_ref(ak_key) }),
            json!({ "name": "ATLAS_STATE_BACKUP_SECRET_KEY", "valueFrom": secret_ref(sk_key) }),
        ]);
    }
    let deployment =
        std::env::var("ATLAS_DEPLOYMENT_NAME").unwrap_or_else(|_| "atlas-gateway".into());
    k8s.patch_deployment(
        &pod_namespace(),
        &deployment,
        json!({ "spec": { "template": { "spec": { "containers": [
            { "name": "atlas-gateway", "env": env }
        ]}}}}),
    )
    .await
    .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(port)
}

/// Opt-in install-time automation (`ATLAS_RUSTFS_AUTO_DEVICE=/dev/sdX`): get RustFS running on one
/// disk without console clicks — but never at the cost of data. It formats the disk only when it is
/// completely empty (no wipe, ever; a disk that holds anything is reported and left alone), installs
/// RustFS from the official chart on the resulting drive, and, with `ATLAS_RUSTFS_AUTO_ACTIVATE=1`,
/// points Atlas at it. Every step checks the cluster's current state first, so a restart resumes
/// where it stopped.
pub(crate) fn spawn_rustfs_auto(s: AppState) {
    let Some(device) = std::env::var("ATLAS_RUSTFS_AUTO_DEVICE").ok().filter(|v| !v.is_empty()) else {
        return;
    };
    if s.config.zfs_driver_mode != atlas_common::config::DriverMode::Real {
        tracing::warn!("ATLAS_RUSTFS_AUTO_DEVICE is set but real disk access is off — ignoring");
        return;
    }
    if let Err(e) = atlas_common::device::validate_raw_device_path(&device) {
        tracing::warn!("ATLAS_RUSTFS_AUTO_DEVICE {device} rejected: {e}");
        return;
    }
    let auto_activate = std::env::var("ATLAS_RUSTFS_AUTO_ACTIVATE").as_deref() == Ok("1");
    tokio::spawn(async move {
        let drive = device.trim_start_matches("/dev/").to_string();
        let instance = format!("rustfs-{drive}");
        let pvc = format!("rustfs-{drive}-data");
        let pv = format!("atlas-rustfs-{drive}");
        let (mut provisioned, mut installed_at): (bool, Option<std::time::Instant>) = (false, None);
        for _ in 0..90 {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            let Some(k8s) = s.k8s.as_ref() else { continue };
            let Ok(pvs) = k8s.list_pvs_json("atlas.zyvor.dev/rustfs-drive=true").await else { continue };
            if !pvs.iter().any(|p| p["metadata"]["name"] == pv.as_str()) {
                if provisioned {
                    continue;
                }
                match atlas_driver_zfs::inspect_device(&device).await {
                    Ok(check) => {
                        if let Some(reason) = check.hard_refusal_reason().or_else(|| check.refusal_reason()) {
                            tracing::warn!("auto RustFS drive: leaving {device} alone: {reason}");
                            return;
                        }
                        let spec = JobSpec::RustfsDriveProvision {
                            device_path: device.clone(),
                            confirmed_device_path: device.clone(),
                            wipe_existing: false,
                        };
                        if s.jobs.enqueue(&ids::job_id(), "global", "atlas-auto", spec, None).await.is_ok() {
                            tracing::info!("auto RustFS drive: formatting empty disk {device}");
                            provisioned = true;
                        }
                    }
                    Err(e) => tracing::warn!("auto RustFS drive: cannot inspect {device}: {e}"),
                }
                continue;
            }
            let Ok(found) = find_instances(&s).await else { continue };
            match found.iter().find(|i| i.name == instance) {
                None if installed_at.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(300)) => {
                    let used: Vec<i64> = found
                        .iter()
                        .flat_map(|i| [i.s3_port, i.console_port])
                        .flatten()
                        .collect();
                    let (s3, con) = if used.contains(&30900) || used.contains(&30901) {
                        (30930u16, 30931u16)
                    } else {
                        (30900u16, 30901u16)
                    };
                    let spec = JobSpec::RustfsInstance {
                        action: "install".into(),
                        name: instance.clone(),
                        pvc: pvc.clone(),
                        s3_node_port: s3,
                        console_node_port: con,
                    };
                    if s.jobs.enqueue(&ids::job_id(), "global", "atlas-auto", spec, None).await.is_ok() {
                        tracing::info!("auto RustFS drive: installing {instance} on {pvc}");
                        installed_at = Some(std::time::Instant::now());
                    }
                }
                Some(i) if i.ready > 0 => {
                    if auto_activate && !is_active(&s, i) {
                        match switch_atlas_to(&s, i).await {
                            Ok(port) => tracing::info!("auto RustFS drive: switching Atlas to {instance} (:{port})"),
                            Err(e) => tracing::warn!("auto RustFS drive: could not switch Atlas: {e}"),
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
    });
}

/// `POST /rustfs/instances/{name}/activate` — point Atlas at this RustFS server: patches the
/// gateway's own Deployment (RustFS endpoint, credentials Secret, and the state backup if it is
/// enabled), which restarts the gateway. Existing bucket rows keep their old endpoint until
/// `POST /rustfs/buckets/import` re-adopts them.
pub(crate) async fn activate_rustfs_instance(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let instances = find_instances(&s).await?;
    let inst = instances
        .iter()
        .find(|i| i.name == name)
        .ok_or_else(|| AppError::NotFound(format!("RustFS instance {name}")))?;
    let port = switch_atlas_to(&s, inst).await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "rustfs.instance.activated",
        "rustfs-instance",
        &name,
        "success",
        Some(json!({ "s3_node_port": port })),
        None,
    )
    .await;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "instance": name, "restarting": true, "note": "the gateway restarts to pick up the new endpoint" })),
    ))
}

/// `POST /rustfs/buckets/import` — adopt every bucket the current RustFS server reports as an Atlas
/// bucket: existing rows are re-pointed at the current endpoint/credentials, missing ones are
/// created bound. Idempotent; the way to see buckets created outside Atlas (or copied by an object
/// migration) on the Buckets page.
pub(crate) async fn import_rustfs_buckets(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !s.config.rustfs_enable {
        return Err(AppError::Unavailable("the RustFS backend is not enabled".into()));
    }
    let endpoint = s
        .config
        .rustfs_endpoint
        .clone()
        .ok_or_else(|| AppError::Unavailable("ATLAS_RUSTFS_ENDPOINT is not configured".into()))?;
    let secret = std::env::var("ATLAS_RUSTFS_CREDENTIALS_SECRET")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| AppError::Unavailable("ATLAS_RUSTFS_CREDENTIALS_SECRET is not configured".into()))?;
    let driver = s
        .driver_for(crate::startup::RUSTFS_BACKEND_ID)
        .ok_or_else(|| AppError::Unavailable("no RustFS driver registered".into()))?;
    let found = driver
        .discover()
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    let existing = atlas_inventory::buckets::list_buckets(&s.pool).await?;
    let ns = s.config.rustfs_credentials_namespace.clone();
    let (mut created, mut updated) = (0u32, 0u32);
    for pool in &found.pools {
        let bucket = &pool.name;
        let row = existing.iter().find(|b| {
            b.bucket_name.as_deref() == Some(bucket.as_str())
                && b.backend_id.as_deref() == Some(crate::startup::RUSTFS_BACKEND_ID)
        });
        let id = match row {
            Some(b) => {
                updated += 1;
                b.id.clone()
            }
            None => {
                let id = ids::bucket_id();
                atlas_inventory::buckets::insert_bucket(
                    &s.pool,
                    &id,
                    "global",
                    bucket,
                    crate::startup::RUSTFS_BACKEND_ID,
                    &ns,
                    "",
                    "",
                )
                .await?;
                created += 1;
                id
            }
        };
        atlas_inventory::buckets::set_bound(&s.pool, &id, bucket, &endpoint, "us-east-1", &secret)
            .await?;
    }
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "rustfs.buckets.imported",
        "rustfs",
        "buckets",
        "success",
        Some(json!({ "created": created, "updated": updated })),
        None,
    )
    .await;
    Ok(Json(json!({ "created": created, "updated": updated, "buckets": found.pools.len() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_allow_list_is_explicit() {
        assert!(admin_allowed(&Method::GET, "v3/info"));
        assert!(admin_allowed(&Method::PUT, "v3/add-user"));
        assert!(admin_allowed(&Method::DELETE, "v3/group/dev"));
        assert!(admin_allowed(&Method::POST, "v3/heal/mybucket/prefix"));
        // Not forwarded: server update/restart, config, exports, inspect, KMS, wrong verbs.
        assert!(!admin_allowed(&Method::POST, "v3/update"));
        assert!(!admin_allowed(&Method::POST, "v3/service"));
        assert!(!admin_allowed(&Method::GET, "v3/export-iam"));
        assert!(!admin_allowed(&Method::GET, "v3/inspect-data"));
        assert!(!admin_allowed(&Method::GET, "v3/kms/keys"));
        assert!(!admin_allowed(&Method::GET, "v3/add-user"));
        assert!(!admin_allowed(&Method::DELETE, "v3/info"));
    }

    #[test]
    fn s3_subresources_are_bucket_level_only() {
        let q = |k: &str| vec![(k.to_string(), String::new())];
        assert!(s3_allowed(&Method::PUT, "my-bucket", &q("versioning")));
        assert!(s3_allowed(&Method::GET, "my-bucket", &q("versions")));
        assert!(!s3_allowed(&Method::PUT, "my-bucket", &q("versions")));
        assert!(!s3_allowed(&Method::PUT, "my-bucket", &q("location")));
        assert!(!s3_allowed(&Method::GET, "my-bucket", &q("acl")));
        assert!(!s3_allowed(&Method::GET, "My_Bucket", &q("policy")));
        assert!(!s3_allowed(&Method::GET, "my-bucket", &[]));
    }

    #[test]
    fn env_vars_are_scrubbed_from_info() {
        let body = br#"{"info":{"servers":[{"endpoint":"a","rustfs_env_vars":{"RUSTFS_SECRET_KEY":"x"}}]}}"#;
        let out = String::from_utf8(scrub_env_vars(body)).unwrap();
        assert!(!out.contains("RUSTFS_SECRET_KEY") && out.contains("endpoint"));
        assert_eq!(scrub_env_vars(b"<xml/>"), b"<xml/>".to_vec());
    }

    #[test]
    fn query_parsing_decodes() {
        let q = parse_query(Some("accessKey=a%20b&versioning".into()));
        assert_eq!(q[0], ("accessKey".into(), "a b".into()));
        assert_eq!(q[1], ("versioning".into(), String::new()));
    }
}
