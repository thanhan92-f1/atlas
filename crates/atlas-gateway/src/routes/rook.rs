// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Rook lifecycle routes: create/list/delete `CephBlockPool`/`CephFilesystem`/`CephObjectStore`
//! CRs (+ their StorageClass) from the API instead of hand-edited YAML manifests. Mirrors
//! `routes::object_store`'s bucket create/delete shape (async job, dependent-delete guard).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_jobs::JobSpec;

use super::util::accepted;
use super::volumes::ForceParams;
use crate::auth::Actor;
use crate::state::AppState;

fn require_k8s(s: &AppState) -> AppResult<()> {
    if s.k8s.is_none() {
        return Err(AppError::Unavailable(
            "no kubernetes cluster attached; cannot manage Rook resources".into(),
        ));
    }
    Ok(())
}

// ---- pools ----

#[derive(Debug, Deserialize)]
pub(crate) struct CreatePoolBody {
    name: String,
    namespace: Option<String>,
    storage_class: Option<String>,
    replicated_size: Option<i64>,
    failure_domain: Option<String>,
    device_class: Option<String>,
}

/// `GET /ceph/pools` — every live `CephBlockPool` CR (name + phase).
pub(crate) async fn list_ceph_pools(State(s): State<AppState>) -> AppResult<Json<Value>> {
    require_k8s(&s)?;
    let pools = s
        .k8s
        .as_ref()
        .unwrap()
        .list_ceph_block_pools(&s.config.rook_namespace)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(json!(pools)))
}

/// `POST /ceph/pools` — create a `CephBlockPool` + matching StorageClass (async job).
pub(crate) async fn create_ceph_pool(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreatePoolBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    require_k8s(&s)?;
    super::util::validate_k8s_name(&body.name)?;
    let namespace = body
        .namespace
        .unwrap_or_else(|| s.config.rook_namespace.clone());
    let storage_class = body
        .storage_class
        .unwrap_or_else(|| format!("zyvor-{}", body.name));
    let spec = JobSpec::CephPoolCreate {
        name: body.name.clone(),
        namespace,
        storage_class: storage_class.clone(),
        replicated_size: body.replicated_size.unwrap_or(3).clamp(1, 9),
        failure_domain: body.failure_domain.unwrap_or_else(|| "host".into()),
        device_class: body.device_class,
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
        "ceph.pool.create.requested",
        "pool",
        &body.name,
        "accepted",
        Some(json!({ "storage_class": storage_class })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "name": body.name, "storage_class": storage_class }),
    ))
}

/// `DELETE /ceph/pools/{name}[?force=true]` — delete the CR + StorageClass; blocked (409) while
/// any volume still references the StorageClass, unless `force=true`.
pub(crate) async fn delete_ceph_pool(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Query(q): Query<ForceParams>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    require_k8s(&s)?;
    let storage_class = format!("zyvor-{name}");
    let deps = atlas_inventory::count_volumes_by_storage_class(&s.pool, &storage_class).await?;
    if deps > 0 && !q.force {
        return Err(AppError::Conflict(format!(
            "pool {name}'s StorageClass {storage_class} still has {deps} volume(s); delete them first or pass ?force=true"
        )));
    }
    let spec = JobSpec::CephPoolDelete {
        name: name.clone(),
        namespace: s.config.rook_namespace.clone(),
        storage_class,
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "name": name })))
}

// ---- filesystems ----

#[derive(Debug, Deserialize)]
pub(crate) struct CreateFilesystemBody {
    name: String,
    namespace: Option<String>,
    storage_class: Option<String>,
    data_pool_name: Option<String>,
    replicated_size: Option<i64>,
}

/// `GET /ceph/filesystems` — every live `CephFilesystem` CR (name + phase).
pub(crate) async fn list_ceph_filesystems(State(s): State<AppState>) -> AppResult<Json<Value>> {
    require_k8s(&s)?;
    let fs = s
        .k8s
        .as_ref()
        .unwrap()
        .list_ceph_filesystems(&s.config.rook_namespace)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(json!(fs)))
}

/// `POST /ceph/filesystems` — create a `CephFilesystem` (RWX CephFS) + StorageClass (async job).
pub(crate) async fn create_ceph_filesystem(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateFilesystemBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    require_k8s(&s)?;
    super::util::validate_k8s_name(&body.name)?;
    let namespace = body
        .namespace
        .unwrap_or_else(|| s.config.rook_namespace.clone());
    let storage_class = body
        .storage_class
        .unwrap_or_else(|| format!("zyvor-{}-shared", body.name));
    let spec = JobSpec::CephFilesystemCreate {
        name: body.name.clone(),
        namespace,
        storage_class: storage_class.clone(),
        data_pool_name: body.data_pool_name.unwrap_or_else(|| "data0".into()),
        replicated_size: body.replicated_size.unwrap_or(3).clamp(1, 9),
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(
        &job,
        json!({ "name": body.name, "storage_class": storage_class }),
    ))
}

/// `DELETE /ceph/filesystems/{name}[?force=true]`.
pub(crate) async fn delete_ceph_filesystem(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Query(q): Query<ForceParams>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    require_k8s(&s)?;
    let storage_class = format!("zyvor-{name}-shared");
    let deps = atlas_inventory::count_volumes_by_storage_class(&s.pool, &storage_class).await?;
    if deps > 0 && !q.force {
        return Err(AppError::Conflict(format!(
            "filesystem {name}'s StorageClass {storage_class} still has {deps} volume(s); delete them first or pass ?force=true"
        )));
    }
    let spec = JobSpec::CephFilesystemDelete {
        name: name.clone(),
        namespace: s.config.rook_namespace.clone(),
        storage_class,
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "name": name })))
}

// ---- object stores ----

#[derive(Debug, Deserialize)]
pub(crate) struct CreateObjectStoreBody {
    name: String,
    namespace: Option<String>,
    storage_class: Option<String>,
    replicated_size: Option<i64>,
    gateway_port: Option<i64>,
    gateway_instances: Option<i64>,
}

/// `GET /ceph/object-stores` — every live `CephObjectStore` CR (name + phase).
pub(crate) async fn list_ceph_object_stores(State(s): State<AppState>) -> AppResult<Json<Value>> {
    require_k8s(&s)?;
    let stores = s
        .k8s
        .as_ref()
        .unwrap()
        .list_ceph_object_stores(&s.config.rook_namespace)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(json!(stores)))
}

/// `POST /ceph/object-stores` — create a `CephObjectStore` (RGW) + bucket StorageClass (async job).
pub(crate) async fn create_ceph_object_store(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateObjectStoreBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    require_k8s(&s)?;
    super::util::validate_k8s_name(&body.name)?;
    let namespace = body
        .namespace
        .unwrap_or_else(|| s.config.rook_namespace.clone());
    let storage_class = body
        .storage_class
        .unwrap_or_else(|| format!("zyvor-{}-bucket", body.name));
    let spec = JobSpec::CephObjectStoreCreate {
        name: body.name.clone(),
        namespace,
        storage_class: storage_class.clone(),
        replicated_size: body.replicated_size.unwrap_or(3).clamp(1, 9),
        gateway_port: body.gateway_port.unwrap_or(80),
        gateway_instances: body.gateway_instances.unwrap_or(1).max(1),
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(
        &job,
        json!({ "name": body.name, "storage_class": storage_class }),
    ))
}

// ---- raw disk -> OSD provisioning ----

#[derive(Debug, Deserialize)]
pub(crate) struct AddCephDeviceBody {
    node_name: String,
    device_path: String,
    confirm: bool,
    namespace: Option<String>,
    cluster_name: Option<String>,
}

/// `POST /ceph/devices` — claim a raw, unformatted disk on a specific Kubernetes node as a new
/// Ceph OSD via Rook (async job: patches the `CephCluster` CR's device list, then polls — via the
/// job engine's own retry machinery, not a blocking wait — until Rook actually produces the OSD).
pub(crate) async fn add_ceph_device(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<AddCephDeviceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !body.confirm {
        return Err(AppError::Validation(
            "confirm=true is required to claim a raw device as an OSD".into(),
        ));
    }
    atlas_common::device::validate_raw_device_path(&body.device_path)
        .map_err(AppError::Validation)?;
    require_k8s(&s)?;

    let namespace = body
        .namespace
        .unwrap_or_else(|| s.config.rook_namespace.clone());
    let cluster_name = body
        .cluster_name
        .unwrap_or_else(|| s.config.rook_cluster_name.clone());
    let k8s = s.k8s.as_ref().unwrap();

    if k8s
        .get_node(&body.node_name)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?
        .is_none()
    {
        return Err(AppError::Validation(format!(
            "no such Kubernetes node: {}",
            body.node_name
        )));
    }
    let storage = k8s
        .get_ceph_cluster_storage(&namespace, &cluster_name)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("CephCluster {namespace}/{cluster_name}")))?;
    if storage.use_all_devices {
        return Err(AppError::Validation(
            "this cluster has useAllDevices=true — it already auto-claims every empty device; \
             this API only applies when devices are pinned explicitly per node"
                .into(),
        ));
    }

    // Safety oracle: the gateway has no remote-exec mechanism to shell `lsblk`/`findmnt` against
    // an arbitrary Kubernetes node the way it can for a local ZFS host, so this reads Rook's own
    // device-discovery ConfigMap instead. Fail closed (503) if that data isn't there at all —
    // never assume a device is safe to claim when this oracle is unavailable.
    let device_basename = body.device_path.trim_start_matches("/dev/");
    match k8s
        .ceph_device_reports_empty(&namespace, &body.node_name, device_basename)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Err(AppError::Validation(format!(
                "device {} on node {} is not reported empty by Rook's discovery (already has a \
                 filesystem or is in use)",
                body.device_path, body.node_name
            )))
        }
        Err(atlas_driver_k8s::K8sError::NotFound(msg)) => {
            return Err(AppError::Unavailable(format!(
                "cannot safety-verify {}: {msg} — enable Rook's discovery daemon \
                 (ROOK_ENABLE_DISCOVERY_DAEMON) before using this API",
                body.device_path
            )))
        }
        Err(e) => return Err(AppError::Driver(e.to_string())),
    }

    let spec = JobSpec::CephOsdAddDevice {
        namespace,
        cluster_name,
        node_name: body.node_name.clone(),
        device_path: body.device_path.clone(),
        confirmed_device_path: body.device_path.clone(),
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    // Generous retry budget: Rook's reconciliation can take several minutes. Backoff is capped at
    // 64s/attempt (engine.rs), so 30 retries gives roughly 25-30 minutes of polling before the job
    // is finally marked failed.
    const CEPH_OSD_ADD_MAX_RETRIES: i64 = 30;
    let _ = atlas_inventory::jobs::set_max_retries(&s.pool, &job_id, CEPH_OSD_ADD_MAX_RETRIES).await;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "ceph.osd.add_device.requested",
        "node",
        &body.node_name,
        "accepted",
        Some(json!({ "device": body.device_path })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "node": body.node_name, "device": body.device_path }),
    ))
}

/// `DELETE /ceph/object-stores/{name}[?force=true]` — blocked (409) while any bucket still
/// references the store's StorageClass.
pub(crate) async fn delete_ceph_object_store(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Query(q): Query<ForceParams>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    require_k8s(&s)?;
    let storage_class = format!("zyvor-{name}-bucket");
    let deps = atlas_inventory::buckets::count_by_storage_class(&s.pool, &storage_class).await?;
    if deps > 0 && !q.force {
        return Err(AppError::Conflict(format!(
            "object store {name}'s StorageClass {storage_class} still has {deps} bucket(s); delete them first or pass ?force=true"
        )));
    }
    let spec = JobSpec::CephObjectStoreDelete {
        name: name.clone(),
        namespace: s.config.rook_namespace.clone(),
        storage_class,
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "name": name })))
}
