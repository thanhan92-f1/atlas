// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! RustFS storage driver — an S3-compatible object backend behind `StorageDriver`.
//!
//! RustFS (https://github.com/rustfs/rustfs) is treated as a one-cluster backend:
//! the endpoint is the cluster, each **bucket** is a `StoragePool` (`kind = "s3"`),
//! and each object under an optional prefix is a `StorageVolume` of kind object.
//!
//! Two implementations, selected by `ATLAS_RUSTFS_DRIVER_MODE` (`fake` default, or `real`):
//! - [`FakeRustfsDriver`] — deterministic fixtures, no network. Safe for `make run` / CI.
//! - [`RealRustfsDriver`] — `GET {endpoint}/health` (or `/minio/health/live`) plus S3
//!   `ListBuckets` / `ListObjectsV2`. Never fabricates a bucket the server did not return.
//!
//! Credentials are optional. RustFS anonymous read works for public buckets; signed
//! requests are left to a follow-up that reuses `atlas-driver-rgw`'s `rusty-s3` signer.
//! Secret material is never stored on the driver beyond the process env reference.

use async_trait::async_trait;
use atlas_api_types::{
    DiscoveryResult, Health, MetricSample, StorageCluster, StorageHealth, StoragePool,
    StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};

fn sanitize(s: &str) -> String {
    s.trim_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

// ---------------------------------------------------------------------------
// Fake
// ---------------------------------------------------------------------------

const BUCKET_CAPACITY: i64 = 4_000_000_000_000;
const BUCKET_USED: i64 = 900_000_000_000;

pub struct FakeRustfsDriver {
    backend_id: String,
    endpoint: String,
    buckets: Vec<String>,
}

impl FakeRustfsDriver {
    pub fn new(
        backend_id: impl Into<String>,
        endpoint: impl Into<String>,
        buckets: Vec<String>,
    ) -> Self {
        Self {
            backend_id: backend_id.into(),
            endpoint: endpoint.into(),
            buckets,
        }
    }

    pub fn with_defaults(backend_id: impl Into<String>) -> Self {
        Self::new(
            backend_id,
            "http://rustfs.zyvor.lab:9000",
            vec!["vm-images".into(), "backups".into(), "databridge".into()],
        )
    }

    fn cluster_id(&self) -> String {
        format!("cls_rustfs_{}", sanitize(&self.endpoint))
    }

    fn cluster(&self) -> StorageCluster {
        let n = self.buckets.len().max(1) as i64;
        StorageCluster {
            id: self.cluster_id(),
            backend_id: self.backend_id.clone(),
            name: format!("rustfs://{}", self.endpoint),
            native_fsid: None,
            health: Health::Ok,
            raw_capacity_bytes: Some(BUCKET_CAPACITY * n),
            used_capacity_bytes: Some(BUCKET_USED * n),
            available_capacity_bytes: Some((BUCKET_CAPACITY - BUCKET_USED) * n),
        }
    }

    fn pool_for(&self, bucket: &str) -> StoragePool {
        StoragePool {
            id: format!("pool_rustfs_{}", sanitize(bucket)),
            cluster_id: self.cluster_id(),
            name: bucket.to_string(),
            kind: "s3".into(),
            device_class: Some("rustfs".into()),
            replica_size: None,
            used_bytes: Some(BUCKET_USED),
            max_bytes: Some(BUCKET_CAPACITY),
            health: Health::Ok,
        }
    }

    fn pools(&self) -> Vec<StoragePool> {
        self.buckets.iter().map(|b| self.pool_for(b)).collect()
    }
}

#[async_trait]
impl StorageDriver for FakeRustfsDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    fn is_fixture(&self) -> bool {
        true
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        let mut volumes = Vec::new();
        for bucket in &self.buckets {
            volumes.extend(self.list_volumes(bucket).await?);
        }
        Ok(DiscoveryResult {
            cluster: self.cluster(),
            pools: self.pools(),
            osds: vec![],
            volumes,
            health: self.health().await?,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        let n = self.buckets.len().max(1) as i64;
        Ok(StorageHealth {
            status: Health::Ok,
            summary: format!(
                "{} bucket(s) on RustFS {}",
                self.buckets.len(),
                self.endpoint
            ),
            raw_capacity_bytes: Some(BUCKET_CAPACITY * n),
            used_capacity_bytes: Some(BUCKET_USED * n),
            available_capacity_bytes: Some((BUCKET_CAPACITY - BUCKET_USED) * n),
            recovering: false,
            degraded_objects: 0,
        })
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        Ok(self.pools())
    }

    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        if !self.buckets.iter().any(|b| b == pool) {
            return Ok(vec![]);
        }
        let slug = sanitize(pool);
        Ok(vec![StorageVolume {
            id: format!("vol_rustfs_{slug}_catalog"),
            cluster_id: Some(self.cluster_id()),
            pool_id: Some(format!("pool_rustfs_{slug}")),
            name: format!("{pool}/catalog"),
            kind: VolumeKind::Object,
            backend_native_id: Some(format!("{}/{pool}/catalog", self.endpoint)),
            size_bytes: BUCKET_CAPACITY,
            used_bytes: Some(BUCKET_USED),
            state: "available".into(),
            health: Health::Ok,
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: None,
        }])
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let n = self.buckets.len().max(1) as f64;
        let m = |name: &str, value: f64| MetricSample {
            name: name.into(),
            value,
            labels: Default::default(),
        };
        Ok(vec![
            m("rustfs_buckets_total", self.buckets.len() as f64),
            m("rustfs_capacity_bytes", BUCKET_CAPACITY as f64 * n),
            m("rustfs_used_bytes", BUCKET_USED as f64 * n),
        ])
    }
}

// ---------------------------------------------------------------------------
// Real
// ---------------------------------------------------------------------------

pub struct RealRustfsDriver {
    backend_id: String,
    endpoint: String,
    /// Optional allow-list. Empty means "every bucket ListBuckets returns".
    buckets: Vec<String>,
    client: reqwest::Client,
}

impl RealRustfsDriver {
    pub fn new(
        backend_id: impl Into<String>,
        endpoint: impl Into<String>,
        buckets: Vec<String>,
    ) -> Self {
        Self {
            backend_id: backend_id.into(),
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            buckets,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    fn cluster_id(&self) -> String {
        format!("cls_rustfs_{}", sanitize(&self.endpoint))
    }

    async fn probe_health(&self) -> Result<(), DriverError> {
        // RustFS serves MinIO-compatible liveness plus its own /health.
        for path in ["/health", "/minio/health/live"] {
            let url = format!("{}{path}", self.endpoint);
            match self.client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                Ok(_) => continue,
                Err(err) => {
                    return Err(DriverError::Unreachable(format!(
                        "rustfs {url}: {err}"
                    )))
                }
            }
        }
        Err(DriverError::Unreachable(format!(
            "rustfs {} health endpoints failed",
            self.endpoint
        )))
    }

    async fn list_buckets_live(&self) -> Result<Vec<String>, DriverError> {
        let url = format!("{}/", self.endpoint);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| DriverError::Unreachable(format!("list buckets: {e}")))?;
        if !resp.status().is_success() {
            return Err(DriverError::Backend(format!(
                "ListBuckets HTTP {}",
                resp.status()
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| DriverError::Parse(e.to_string()))?;
        let names = parse_bucket_names(&body)?;
        if self.buckets.is_empty() {
            Ok(names)
        } else {
            Ok(names
                .into_iter()
                .filter(|n| self.buckets.iter().any(|w| w == n))
                .collect())
        }
    }
}

fn parse_bucket_names(xml: &str) -> Result<Vec<String>, DriverError> {
    // Minimal ListBuckets parser: <Name>bucket</Name> inside <Bucket>.
    let mut names = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Name>") {
        rest = &rest[start + 6..];
        if let Some(end) = rest.find("</Name>") {
            let name = rest[..end].trim();
            if !name.is_empty() && !name.contains('<') {
                names.push(name.to_string());
            }
            rest = &rest[end + 7..];
        } else {
            break;
        }
    }
    if names.is_empty() && xml.contains("Error") {
        return Err(DriverError::Backend(
            "ListBuckets returned an S3 error (auth required?)".into(),
        ));
    }
    Ok(names)
}

#[async_trait]
impl StorageDriver for RealRustfsDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        self.probe_health().await?;
        let buckets = self.list_buckets_live().await?;
        let cluster_id = self.cluster_id();
        let pools: Vec<StoragePool> = buckets
            .iter()
            .map(|b| StoragePool {
                id: format!("pool_rustfs_{}", sanitize(b)),
                cluster_id: cluster_id.clone(),
                name: b.clone(),
                kind: "s3".into(),
                device_class: Some("rustfs".into()),
                replica_size: None,
                used_bytes: None,
                max_bytes: None,
                health: Health::Ok,
            })
            .collect();
        let volumes = pools
            .iter()
            .map(|p| StorageVolume {
                id: format!("vol_rustfs_{}", sanitize(&p.name)),
                cluster_id: Some(cluster_id.clone()),
                pool_id: Some(p.id.clone()),
                name: p.name.clone(),
                kind: VolumeKind::Object,
                backend_native_id: Some(format!("{}/{}", self.endpoint, p.name)),
                size_bytes: 0,
                used_bytes: None,
                state: "available".into(),
                health: Health::Ok,
                kubernetes_namespace: None,
                pvc_name: None,
                storage_class_name: None,
            })
            .collect();
        let health = self.health().await?;
        Ok(DiscoveryResult {
            cluster: StorageCluster {
                id: cluster_id,
                backend_id: self.backend_id.clone(),
                name: format!("rustfs://{}", self.endpoint),
                native_fsid: None,
                health: health.status,
                raw_capacity_bytes: None,
                used_capacity_bytes: None,
                available_capacity_bytes: None,
            },
            pools,
            osds: vec![],
            volumes,
            health,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        self.probe_health().await?;
        Ok(StorageHealth {
            status: Health::Ok,
            summary: format!("RustFS reachable at {}", self.endpoint),
            raw_capacity_bytes: None,
            used_capacity_bytes: None,
            available_capacity_bytes: None,
            recovering: false,
            degraded_objects: 0,
        })
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        Ok(self.discover().await?.pools)
    }

    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        let all = self.discover().await?.volumes;
        Ok(all.into_iter().filter(|v| v.name == pool).collect())
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let n = self.list_buckets_live().await?.len() as f64;
        Ok(vec![MetricSample {
            name: "rustfs_buckets_total".into(),
            value: n,
            labels: Default::default(),
        }])
    }
}

/// Mode from `ATLAS_RUSTFS_DRIVER_MODE` (`fake` default).
pub fn from_env(backend_id: impl Into<String>) -> Box<dyn StorageDriver> {
    let mode = std::env::var("ATLAS_RUSTFS_DRIVER_MODE").unwrap_or_else(|_| "fake".into());
    let endpoint = std::env::var("ATLAS_RUSTFS_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:9000".into());
    let buckets = std::env::var("ATLAS_RUSTFS_BUCKETS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if mode.eq_ignore_ascii_case("real") {
        Box::new(RealRustfsDriver::new(backend_id, endpoint, buckets))
    } else {
        let driver = if buckets.is_empty() {
            FakeRustfsDriver::with_defaults(backend_id)
        } else {
            FakeRustfsDriver::new(backend_id, endpoint, buckets)
        };
        Box::new(driver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_buckets_xml() {
        let xml = r#"<?xml version="1.0"?>
            <ListAllMyBucketsResult>
              <Buckets>
                <Bucket><Name>vm-images</Name></Bucket>
                <Bucket><Name>backups</Name></Bucket>
              </Buckets>
            </ListAllMyBucketsResult>"#;
        let names = parse_bucket_names(xml).unwrap();
        assert_eq!(names, vec!["vm-images", "backups"]);
    }

    #[tokio::test]
    async fn fake_discover_is_stable() {
        let d = FakeRustfsDriver::with_defaults("be_rustfs");
        let got = d.discover().await.unwrap();
        assert_eq!(got.pools.len(), 3);
        assert!(d.is_fixture());
        assert_eq!(got.volumes.len(), 3);
    }
}
