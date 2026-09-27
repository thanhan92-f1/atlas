// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Provision a raw, unformatted local block device into a brand-new ZFS pool. Mirrors
//! `routes::rook`'s create-pool shape (async job, `202 Accepted`), but this mutation is genuinely
//! destructive (formats a disk), so it additionally requires an explicit `confirm: true`.

use axum::{extract::State, http::StatusCode, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_jobs::JobSpec;

use super::util::accepted;
use crate::auth::Actor;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct CreateZfsPoolFromDeviceBody {
    pool_name: String,
    device_path: String,
    confirm: bool,
}

/// `POST /zfs/pools/from-device` — provision a brand-new zpool on a raw, unformatted local disk
/// (async job). Local-host-only: the gateway process must be running on the host that owns
/// `device_path` (same limitation `RealZfsDriver` itself already has — ZFS has no remote query
/// protocol, unlike Ceph).
pub(crate) async fn create_zfs_pool_from_device(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateZfsPoolFromDeviceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !body.confirm {
        return Err(AppError::Validation(
            "confirm=true is required to format a raw device".into(),
        ));
    }
    atlas_common::device::validate_raw_device_path(&body.device_path)
        .map_err(AppError::Validation)?;
    super::util::validate_k8s_name(&body.pool_name)?;
    // The synthesized pool/volume rows this job writes reference `ZFS_BACKEND_ID` as their
    // backend_id (a NOT NULL foreign key into storage_backends) — that row only exists once the
    // static ZFS backend was actually registered at startup (`ATLAS_ZFS_ENABLE=1`). Fail fast here
    // with a clear message instead of a cryptic FK-violation surfacing deep in job dispatch.
    if !s.config.zfs_enable {
        return Err(AppError::Unavailable(
            "the ZFS backend is not enabled (set ATLAS_ZFS_ENABLE=1 and restart the gateway)"
                .into(),
        ));
    }

    let host = s
        .config
        .zfs_host
        .clone()
        .unwrap_or_else(|| "zfs01.zyvor.lab".into());
    let spec = JobSpec::ZfsPoolCreateFromDevice {
        backend_id: crate::startup::ZFS_BACKEND_ID.into(),
        pool_name: body.pool_name.clone(),
        device_path: body.device_path.clone(),
        confirmed_device_path: body.device_path.clone(),
        host,
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
        "zfs.pool.create_from_device.requested",
        "pool",
        &body.pool_name,
        "accepted",
        Some(json!({ "device": body.device_path })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "pool_name": body.pool_name, "device": body.device_path }),
    ))
}
