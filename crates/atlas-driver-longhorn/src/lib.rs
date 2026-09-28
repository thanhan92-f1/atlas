// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Read-only Longhorn discovery through its Kubernetes CRDs. Volume mutations remain on the
//! existing Atlas Kubernetes PVC path; this driver never claims native snapshot/backup support.

use async_trait::async_trait;
use atlas_api_types::{
    DiscoveryResult, Health, MetricSample, StorageCluster, StorageHealth, StoragePool,
    StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};
use kube::api::ListParams;
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client};
use serde_json::Value;

pub struct LonghornDriver {
    backend_id: String,
    namespace: String,
    client: Client,
}

impl LonghornDriver {
    pub fn new(backend_id: impl Into<String>, namespace: impl Into<String>, client: Client) -> Self {
        Self { backend_id: backend_id.into(), namespace: namespace.into(), client }
    }

    fn api(&self, kind: &str, plural: &str) -> Api<DynamicObject> {
        let mut resource = ApiResource::from_gvk(&GroupVersionKind::gvk("longhorn.io", "v1beta2", kind));
        resource.plural = plural.into();
        Api::namespaced_with(self.client.clone(), &self.namespace, &resource)
    }

    async fn list(&self, kind: &str, plural: &str) -> Result<Vec<Value>, DriverError> {
        let api = self.api(kind, plural);
        let mut params = ListParams::default().limit(500);
        let mut result = Vec::new();
        loop {
            let page = api.list(&params).await.map_err(|e| DriverError::Unreachable(format!(
                "Longhorn {plural} in namespace {}: {e}", self.namespace
            )))?;
            result.extend(page.items.into_iter().map(|item| serde_json::to_value(item)
                .map_err(|e| DriverError::Parse(e.to_string()))).collect::<Result<Vec<_>, _>>()?);
            match page.metadata.continue_.as_deref() {
                Some(token) if !token.is_empty() => params = params.continue_token(token),
                _ => return Ok(result),
            }
        }
    }

    async fn snapshot(&self) -> Result<DiscoveryResult, DriverError> {
        let nodes = self.list("Node", "nodes").await?;
        let volumes = self.list("Volume", "volumes").await?;
        Ok(build_discovery(&self.backend_id, &self.namespace, &nodes, &volumes))
    }
}

fn number(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn value_at<'a>(v: &'a Value, path: &str) -> &'a Value {
    v.pointer(path).unwrap_or(&Value::Null)
}

fn name(v: &Value) -> Option<&str> { value_at(v, "/metadata/name").as_str() }

fn build_discovery(backend_id: &str, namespace: &str, nodes: &[Value], volumes: &[Value]) -> DiscoveryResult {
    let cluster_id = format!("cls_{backend_id}");
    let pool_id = format!("pool_{backend_id}");
    // Longhorn disk status reports physical storageMaximum/storageAvailable; these are not
    // equivalent to the sum of provisioned logical volume sizes.
    let mut total: i64 = 0;
    let mut available: i64 = 0;
    let mut disks_seen = 0;
    let mut unhealthy = false;
    let mut faulted = false;
    for node in nodes {
        let ready = value_at(node, "/status/conditions").as_object()
            .and_then(|c| c.get("Ready"))
            .and_then(|c| c.get("status"))
            .and_then(Value::as_str);
        if ready != Some("True") { unhealthy = true; }
        if let Some(disks) = value_at(node, "/status/diskStatus").as_object() {
            for disk in disks.values() {
                if let (Some(max), Some(free)) = (
                    number(value_at(disk, "/storageMaximum")),
                    number(value_at(disk, "/storageAvailable")),
                ) {
                    total = total.saturating_add(max);
                    available = available.saturating_add(free);
                    disks_seen += 1;
                }
            }
        }
    }
    let capacity = (disks_seen > 0).then_some(total);
    let free = (disks_seen > 0).then_some(available);
    let used = (disks_seen > 0).then_some(total.saturating_sub(available));
    let mut mapped = Vec::new();
    for v in volumes {
        let Some(volume_name) = name(v) else { continue };
        let state = value_at(v, "/status/state").as_str().unwrap_or("unknown");
        let robustness = value_at(v, "/status/robustness").as_str().unwrap_or("unknown");
        let health = match robustness {
            "healthy" => Health::Ok,
            "degraded" => { unhealthy = true; Health::Warn },
            "faulted" => { unhealthy = true; faulted = true; Health::Critical },
            _ => Health::Unknown,
        };
        mapped.push(StorageVolume {
            id: format!("vol_{backend_id}_{volume_name}"),
            cluster_id: Some(cluster_id.clone()), pool_id: Some(pool_id.clone()),
            name: volume_name.into(), kind: VolumeKind::Block,
            backend_native_id: match (
                value_at(v, "/status/kubernetesStatus/namespace").as_str(),
                value_at(v, "/status/kubernetesStatus/pvcName").as_str(),
            ) {
                (Some(ns), Some(pvc)) if !ns.is_empty() && !pvc.is_empty() => Some(format!("pvc/{ns}/{pvc}")),
                _ => Some(format!("longhorn/{namespace}/{volume_name}")),
            },
            size_bytes: number(value_at(v, "/spec/size")).unwrap_or(0),
            used_bytes: number(value_at(v, "/status/actualSize")),
            state: state.into(), health,
            kubernetes_namespace: value_at(v, "/status/kubernetesStatus/namespace").as_str().map(str::to_owned),
            pvc_name: value_at(v, "/status/kubernetesStatus/pvcName").as_str().map(str::to_owned),
            storage_class_name: value_at(v, "/status/kubernetesStatus/storageClassName").as_str().map(str::to_owned),
        });
    }
    let status = if faulted { Health::Critical } else if nodes.is_empty() {
        Health::Unknown
    } else if unhealthy { Health::Warn } else { Health::Ok };
    let health = StorageHealth {
        status, summary: format!("Longhorn: {} node(s), {} volume(s)", nodes.len(), mapped.len()),
        raw_capacity_bytes: capacity, used_capacity_bytes: used, available_capacity_bytes: free,
        recovering: volumes.iter().any(|v| value_at(v, "/status/robustness") == "degraded"),
        degraded_objects: mapped.iter().filter(|v| matches!(v.health, Health::Warn | Health::Critical)).count() as i64,
    };
    DiscoveryResult {
        cluster: StorageCluster {
            id: cluster_id.clone(), backend_id: backend_id.into(),
            name: format!("longhorn/{namespace}"), native_fsid: None,
            health: status, raw_capacity_bytes: capacity,
            used_capacity_bytes: used, available_capacity_bytes: free,
        },
        pools: vec![StoragePool {
            id: pool_id, cluster_id, name: "longhorn".into(), kind: "longhorn".into(),
            device_class: None, replica_size: None, used_bytes: used, max_bytes: capacity, health: status,
        }], osds: vec![], volumes: mapped, health,
    }
}

#[async_trait]
impl StorageDriver for LonghornDriver {
    fn backend_id(&self) -> &str { &self.backend_id }
    async fn discover(&self) -> Result<DiscoveryResult, DriverError> { self.snapshot().await }
    async fn health(&self) -> Result<StorageHealth, DriverError> { Ok(self.snapshot().await?.health) }
    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> { Ok(self.snapshot().await?.pools) }
    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        if pool != "longhorn" { return Ok(vec![]); }
        Ok(self.snapshot().await?.volumes)
    }
    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let snapshot = self.snapshot().await?;
        let sample = |name: &str, value: f64| MetricSample {
            name: name.into(), value, labels: Default::default()
        };
        let mut result = vec![sample("longhorn_volumes_total", snapshot.volumes.len() as f64)];
        if let Some(capacity) = snapshot.cluster.raw_capacity_bytes {
            result.push(sample("longhorn_capacity_bytes", capacity as f64));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_capacity_and_volume_health_without_inventing_missing_values() {
        let nodes = [json!({"metadata":{"name":"node-1"},"status":{
            "conditions":{"Ready":{"status":"True"}},
            "diskStatus":{"disk1":{"storageMaximum":1000,"storageAvailable":600}}
        }})];
        let volumes = [json!({"metadata":{"name":"pvc-a"},"spec":{"size":"400"},
            "status":{"state":"attached","robustness":"degraded","actualSize":120,
            "kubernetesStatus":{"namespace":"vms","pvcName":"disk-a","storageClassName":"longhorn"}}
        })];
        let result = build_discovery("bkd_longhorn", "longhorn-system", &nodes, &volumes);
        assert_eq!(result.cluster.raw_capacity_bytes, Some(1000));
        assert_eq!(result.cluster.available_capacity_bytes, Some(600));
        assert_eq!(result.health.status, Health::Warn);
        assert_eq!(result.volumes[0].pvc_name.as_deref(), Some("disk-a"));
        assert_eq!(result.volumes[0].size_bytes, 400);
        assert_eq!(result.volumes[0].health, Health::Warn);
        assert_eq!(result.volumes[0].backend_native_id.as_deref(), Some("pvc/vms/disk-a"));
        let unknown = build_discovery("bkd_longhorn", "longhorn-system", &[], &[]);
        assert_eq!(unknown.cluster.raw_capacity_bytes, None);
        assert_eq!(unknown.health.status, Health::Unknown);
    }
}
