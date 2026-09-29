// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! A `reqwest::Client` that additionally trusts a CA certificate from `ATLAS_RUSTFS_CA_CERT`, for
//! talking to a RustFS server whose TLS certificate (`RUSTFS_TLS_PATH`, see `docs/RUSTFS.md`) was
//! not issued by a CA already in the system trust store — a private CA, or a self-signed lab cert.
//! The system/OS root store stays trusted regardless (reqwest's own default); this only adds one
//! more root, it never removes the others and never disables verification. Shared by
//! `atlas-driver-rustfs` (the admin/S3 proxy client) and `atlas-driver-rgw` (`S3Target`, used for
//! both RustFS and Ceph RGW buckets) so both speak the same TLS trust policy.
use std::time::Duration;

use crate::DriverError;

pub fn trusted_http_client(timeout: Option<Duration>) -> Result<reqwest::Client, DriverError> {
    let mut builder = reqwest::Client::builder();
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    if let Ok(path) = std::env::var("ATLAS_RUSTFS_CA_CERT") {
        if !path.trim().is_empty() {
            let pem = std::fs::read(&path)
                .map_err(|e| DriverError::Backend(format!("ATLAS_RUSTFS_CA_CERT {path}: {e}")))?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| {
                DriverError::Backend(format!(
                    "ATLAS_RUSTFS_CA_CERT {path} is not a valid PEM certificate: {e}"
                ))
            })?;
            builder = builder.add_root_certificate(cert);
        }
    }
    builder
        .build()
        .map_err(|e| DriverError::Backend(format!("http client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // cargo test runs these in parallel threads of the same process, and all three mutate the
    // same process-global ATLAS_RUSTFS_CA_CERT — without this lock one test's remove_var can race
    // another's set_var (found live in CI, 2026-09-28: invalid_pem_errors_not_panics intermittently
    // saw no env var set and got Ok instead of Err).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn no_env_var_builds_a_plain_client() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: test-only env mutation; serialized by ENV_LOCK.
        unsafe { std::env::remove_var("ATLAS_RUSTFS_CA_CERT") };
        assert!(trusted_http_client(Some(Duration::from_secs(1))).is_ok());
    }

    #[test]
    fn missing_ca_cert_file_errors_not_panics() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: test-only env mutation; serialized by ENV_LOCK.
        unsafe { std::env::set_var("ATLAS_RUSTFS_CA_CERT", "/nonexistent/path/ca.pem") };
        let result = trusted_http_client(None);
        unsafe { std::env::remove_var("ATLAS_RUSTFS_CA_CERT") };
        assert!(result.is_err());
    }

    #[test]
    fn invalid_pem_errors_not_panics() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("atlas-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.pem");
        std::fs::write(&path, b"not a certificate").unwrap();
        // SAFETY: test-only env mutation; serialized by ENV_LOCK.
        unsafe { std::env::set_var("ATLAS_RUSTFS_CA_CERT", path.to_str().unwrap()) };
        let result = trusted_http_client(None);
        unsafe { std::env::remove_var("ATLAS_RUSTFS_CA_CERT") };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(result.is_err());
    }
}
