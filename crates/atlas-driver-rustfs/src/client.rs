// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! A generic SigV4-signed HTTP client for one RustFS server: the native admin API
//! (`/rustfs/admin/v3/...`, plain JSON) and the S3 bucket sub-resources (versioning, lifecycle,
//! policy, ...). RustFS's own `rustfs-madmin` client signs the same way (service `s3`, region
//! `us-east-1` unless the server has one configured); it is not depended on directly because it
//! pulls in the whole `s3s`/`rustfs-*` tree for a handful of heal/scanner calls, and Atlas passes
//! RustFS's JSON through unchanged rather than re-declaring every payload type.

use std::time::Duration;

use atlas_driver_core::DriverError;
use reqwest::Method;

use crate::sigv4;

/// Bytes are RFC 3986 unreserved, or `%XX`-encoded. `keep_slash` keeps `/` for URI paths.
fn uri_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The canonical query string: pairs sorted by encoded key (then value), each encoded.
fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// The full canonical request for a header-signed call (signed headers: host, content type when
/// present, `x-amz-content-sha256`, `x-amz-date`).
fn canonical_request(
    method: &str,
    path: &str,
    query: &str,
    host: &str,
    content_type: Option<&str>,
    payload_sha256: &str,
    amz_date: &str,
) -> (String, String) {
    let mut headers = String::new();
    let mut names = Vec::new();
    if let Some(ct) = content_type {
        headers.push_str(&format!("content-type:{ct}\n"));
        names.push("content-type");
    }
    headers.push_str(&format!("host:{host}\n"));
    names.push("host");
    headers.push_str(&format!("x-amz-content-sha256:{payload_sha256}\n"));
    names.push("x-amz-content-sha256");
    headers.push_str(&format!("x-amz-date:{amz_date}\n"));
    names.push("x-amz-date");
    let signed = names.join(";");
    (
        format!("{method}\n{path}\n{query}\n{headers}\n{signed}\n{payload_sha256}"),
        signed,
    )
}

/// What a RustFS call returned, verbatim: the caller decides what the status and body mean.
#[derive(Debug, Clone)]
pub struct ClientResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// A signed client for one RustFS endpoint.
pub struct RustfsClient {
    endpoint: reqwest::Url,
    access_key: String,
    secret_key: String,
    region: String,
    http: reqwest::Client,
}

impl RustfsClient {
    pub fn new(
        endpoint: &str,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
        region: impl Into<String>,
    ) -> Result<Self, DriverError> {
        let endpoint = reqwest::Url::parse(endpoint)
            .map_err(|e| DriverError::Backend(format!("invalid RustFS endpoint: {e}")))?;
        let region = region.into();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| DriverError::Backend(format!("http client: {e}")))?;
        Ok(Self {
            endpoint,
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            region: if region.is_empty() { "us-east-1".into() } else { region },
            http,
        })
    }

    /// Build from `endpoint` plus `ATLAS_RUSTFS_ACCESS_KEY` / `ATLAS_RUSTFS_SECRET_KEY` /
    /// `ATLAS_RUSTFS_REGION`. Errors when the credentials are not configured — the admin API is
    /// never called anonymously.
    pub fn from_env(endpoint: &str) -> Result<Self, DriverError> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        match (get("ATLAS_RUSTFS_ACCESS_KEY"), get("ATLAS_RUSTFS_SECRET_KEY")) {
            (Some(access), Some(secret)) => {
                Self::new(endpoint, access, secret, get("ATLAS_RUSTFS_REGION").unwrap_or_default())
            }
            _ => Err(DriverError::Unreachable(
                "RustFS credentials are not configured (ATLAS_RUSTFS_ACCESS_KEY / ATLAS_RUSTFS_SECRET_KEY)"
                    .into(),
            )),
        }
    }

    /// Send one signed request. `path` is the absolute URI path (`/rustfs/admin/v3/info`,
    /// `/my-bucket`); `query` holds decoded key/value pairs (a value-less sub-resource such as
    /// `versioning` is `("versioning", "")`).
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<ClientResponse, DriverError> {
        let host = match (self.endpoint.host_str(), self.endpoint.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => return Err(DriverError::Backend("RustFS endpoint has no host".into())),
        };
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let payload_sha256 = sigv4::sha256_hex(&body);
        let canonical_path = uri_encode(path, true);
        let canonical_qs = canonical_query(query);
        let (canonical, signed_headers) = canonical_request(
            method.as_str(),
            &canonical_path,
            &canonical_qs,
            &host,
            content_type,
            &payload_sha256,
            &amz_date,
        );
        let signature = sigv4::signature(
            &self.secret_key,
            &date_stamp,
            &amz_date,
            &self.region,
            "s3",
            &canonical,
        );
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{date_stamp}/{}/s3/aws4_request, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key, self.region
        );

        let mut url = self.endpoint.clone();
        url.set_path(&canonical_path);
        url.set_query(if canonical_qs.is_empty() { None } else { Some(&canonical_qs) });
        let mut req = self
            .http
            .request(method, url)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_sha256)
            .header("authorization", authorization);
        if let Some(ct) = content_type {
            req = req.header("content-type", ct);
        }
        if !body.is_empty() {
            req = req.body(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| DriverError::Unreachable(format!("RustFS request failed: {e}")))?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp
            .bytes()
            .await
            .map_err(|e| DriverError::Unreachable(format!("RustFS response: {e}")))?
            .to_vec();
        Ok(ClientResponse {
            status,
            content_type,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encoding_follows_rfc3986_unreserved() {
        assert_eq!(uri_encode("a b/c~d", true), "a%20b/c~d");
        assert_eq!(uri_encode("a b/c", false), "a%20b%2Fc");
        assert_eq!(uri_encode("k=v&x", false), "k%3Dv%26x");
    }

    #[test]
    fn canonical_query_is_sorted_and_encoded() {
        let q = vec![
            ("versioning".to_string(), String::new()),
            ("accessKey".to_string(), "AB C".to_string()),
        ];
        assert_eq!(canonical_query(&q), "accessKey=AB%20C&versioning=");
    }

    #[test]
    fn canonical_request_layout() {
        let (c, signed) = canonical_request(
            "PUT",
            "/b",
            "versioning=",
            "h:9000",
            Some("application/xml"),
            "abc",
            "20260928T000000Z",
        );
        assert_eq!(signed, "content-type;host;x-amz-content-sha256;x-amz-date");
        assert_eq!(
            c,
            "PUT\n/b\nversioning=\ncontent-type:application/xml\nhost:h:9000\nx-amz-content-sha256:abc\nx-amz-date:20260928T000000Z\n\ncontent-type;host;x-amz-content-sha256;x-amz-date\nabc"
        );
    }

    #[tokio::test]
    async fn unreachable_endpoint_errors_not_panics() {
        let c = RustfsClient::new("http://127.0.0.1:1", "ak", "sk", "").unwrap();
        assert!(c
            .request(Method::GET, "/rustfs/admin/v3/info", &[], Vec::new(), None)
            .await
            .is_err());
    }
}
