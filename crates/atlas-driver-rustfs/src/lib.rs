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
//! Credentials are optional. With an access/secret key pair (`ATLAS_RUSTFS_ACCESS_KEY` /
//! `ATLAS_RUSTFS_SECRET_KEY`, see [`RealRustfsDriver::with_credentials_from_env`]) `ListBuckets`
//! is SigV4-signed — required by any RustFS server with authentication on, i.e. every real
//! deployment. Without them the driver falls back to anonymous requests, which only work against
//! a server that allows anonymous listing. (`rusty-s3` has no service-level `ListBuckets`
//! action, so this signs the one request itself — see the `sigv4` module.)

use async_trait::async_trait;
use atlas_api_types::{
    DiscoveryResult, Health, MetricSample, StorageCluster, StorageHealth, StoragePool,
    StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};

mod client;
pub use client::{ClientResponse, RustfsClient};

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

/// Service credentials for SigV4-signed requests. Deliberately not `Debug`/`Clone`-derived so the
/// secret can't leak through a `{:?}` of the driver.
struct Credentials {
    access_key: String,
    secret_key: String,
    region: String,
}

pub struct RealRustfsDriver {
    backend_id: String,
    endpoint: String,
    /// Optional allow-list. Empty means "every bucket ListBuckets returns".
    buckets: Vec<String>,
    client: reqwest::Client,
    credentials: Option<Credentials>,
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
            credentials: None,
        }
    }

    /// Sign `ListBuckets` with this service credential (SigV4, S3 service).
    pub fn with_credentials(
        mut self,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
        region: impl Into<String>,
    ) -> Self {
        self.credentials = Some(Credentials {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            region: region.into(),
        });
        self
    }

    /// [`Self::with_credentials`] from `ATLAS_RUSTFS_ACCESS_KEY` + `ATLAS_RUSTFS_SECRET_KEY`
    /// (region from `ATLAS_RUSTFS_REGION`, default `us-east-1`). A no-op unless *both* keys are
    /// set and non-empty, so an unconfigured deploy keeps the anonymous behavior.
    pub fn with_credentials_from_env(self) -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        match (get("ATLAS_RUSTFS_ACCESS_KEY"), get("ATLAS_RUSTFS_SECRET_KEY")) {
            (Some(access), Some(secret)) => {
                let region = get("ATLAS_RUSTFS_REGION").unwrap_or_else(|| "us-east-1".into());
                self.with_credentials(access, secret, region)
            }
            _ => self,
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
        let mut req = self.client.get(&url);
        if let Some(c) = &self.credentials {
            let parsed = reqwest::Url::parse(&url)
                .map_err(|e| DriverError::Backend(format!("bad rustfs endpoint {url}: {e}")))?;
            let host = parsed.host_str().ok_or_else(|| {
                DriverError::Backend(format!("rustfs endpoint {url} has no host"))
            })?;
            // `Url::port()` is None for the scheme's default port — exactly the value `Host:`
            // carries on the wire, which is what the signature must cover.
            let host = match parsed.port() {
                Some(p) => format!("{host}:{p}"),
                None => host.to_string(),
            };
            for (name, value) in sigv4::signed_get_root_headers(
                &host,
                &c.access_key,
                &c.secret_key,
                &c.region,
                chrono::Utc::now(),
            ) {
                req = req.header(name, value);
            }
        }
        let resp = req
            .send()
            .await
            .map_err(|e| DriverError::Unreachable(format!("list buckets: {e}")))?;
        if !resp.status().is_success() {
            let hint = match (resp.status().as_u16(), self.credentials.is_some()) {
                (401 | 403, true) => " — credentials rejected",
                (401 | 403, false) => " — authentication required; set ATLAS_RUSTFS_ACCESS_KEY/SECRET_KEY",
                _ => "",
            };
            return Err(DriverError::Backend(format!(
                "ListBuckets HTTP {}{hint}",
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

/// Minimal AWS Signature V4 for the one request this driver needs `rusty-s3` can't build
/// (service-level `ListBuckets`, `GET /`). Header-based auth with an empty payload.
pub(crate) mod sigv4 {
    use ring::{digest, hmac};

    /// SHA-256 of the empty string — the payload hash of a bodiless GET.
    const EMPTY_PAYLOAD_SHA256: &str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn hmac_sha256(key: &[u8], data: &[u8]) -> hmac::Tag {
        hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub(crate) fn sha256_hex(data: &[u8]) -> String {
        hex(digest::digest(&digest::SHA256, data).as_ref())
    }

    /// The hex SigV4 signature over an already-built canonical request.
    pub(crate) fn signature(
        secret_key: &str,
        date_stamp: &str,
        amz_date: &str,
        region: &str,
        service: &str,
        canonical_request: &str,
    ) -> String {
        let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date_stamp.as_bytes());
        let k_region = hmac_sha256(k_date.as_ref(), region.as_bytes());
        let k_service = hmac_sha256(k_region.as_ref(), service.as_bytes());
        let k_signing = hmac_sha256(k_service.as_ref(), b"aws4_request");
        hex(hmac_sha256(k_signing.as_ref(), string_to_sign.as_bytes()).as_ref())
    }

    /// The request headers (name, value) that authenticate `GET /` against `host` at `now`.
    pub(super) fn signed_get_root_headers(
        host: &str,
        access_key: &str,
        secret_key: &str,
        region: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<(&'static str, String)> {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "GET\n/\n\nhost:{host}\nx-amz-content-sha256:{EMPTY_PAYLOAD_SHA256}\nx-amz-date:{amz_date}\n\n{signed_headers}\n{EMPTY_PAYLOAD_SHA256}"
        );
        let sig = signature(secret_key, &date_stamp, &amz_date, region, "s3", &canonical_request);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{date_stamp}/{region}/s3/aws4_request, SignedHeaders={signed_headers}, Signature={sig}"
        );
        vec![
            ("x-amz-date", amz_date),
            ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256.to_string()),
            ("authorization", authorization),
        ]
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
        Box::new(RealRustfsDriver::new(backend_id, endpoint, buckets).with_credentials_from_env())
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

    /// The AWS SigV4 test-suite's "get-vanilla" vector (service "service", GET /, no query) — the
    /// exact signature AWS documents for these inputs, so this pins the signing chain itself
    /// (key derivation, string-to-sign, canonical-request hashing) rather than merely checking
    /// that some signature is produced.
    #[test]
    fn sigv4_matches_the_aws_get_vanilla_test_vector() {
        let canonical = "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let sig = sigv4::signature(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "20150830T123600Z",
            "us-east-1",
            "service",
            canonical,
        );
        assert_eq!(
            sig,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn signed_list_buckets_headers_carry_a_well_formed_authorization() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T05:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let headers = sigv4::signed_get_root_headers("10.0.0.1:9000", "AKID", "secret", "us-east-1", now);
        let get = |n: &str| headers.iter().find(|(k, _)| *k == n).map(|(_, v)| v.clone());
        assert_eq!(get("x-amz-date").unwrap(), "20260928T050000Z");
        let auth = get("authorization").unwrap();
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKID/20260928/us-east-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="
        ));
        assert_eq!(auth.rsplit("Signature=").next().unwrap().len(), 64);
    }

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
