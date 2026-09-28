<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Licensing

Atlas is licensed under the **[Apache License, Version 2.0](../LICENSE)**.

You may use, modify and redistribute it — including in production, in SaaS and managed services, and in
products you sell — under the terms of that license (attribution, a copy of the license, a statement of
changes, and the patent grant/termination terms in §3). There is no runtime license key and no usage
restriction beyond the license itself.

SPDX identifier used in source headers and Cargo/npm metadata: `Apache-2.0` (see
[`LICENSES/Apache-2.0.txt`](../LICENSES/Apache-2.0.txt), [`NOTICE`](../NOTICE)).

Contributions require the [CLA](../CLA.md) and [DCO](../DCO.md) (`git commit -s`). Header checks: `make headers`.

## History

| Release | License |
|---|---|
| up to v0.3.0 | AGPL-3.0 + Atlas Commercial License (dual) — text preserved at [`LICENSES/LicenseRef-Atlas-Commercial-v0.3.0.md`](../LICENSES/LicenseRef-Atlas-Commercial-v0.3.0.md) |
| v0.4.0 (development builds) | Zyvor Production License v1.0 (non-production free, production needs a commercial license) |
| from the Apache-2.0 relicense onward | Apache License 2.0 |

Releases already published under an earlier license remain available under the terms they were
published with; see the git tags and the licensing notes at those tags.

## Bundled third-party software

The gateway images include, under their own licenses: Helm (Apache-2.0) and the official RustFS Helm chart
(Apache-2.0), used by the console's "Deploy RustFS" feature, and the Oracle Instant Client (Oracle's
redistributable Basic Lite license) for the DataBridge Oracle connector — see [`NOTICE`](../NOTICE) and the
Dockerfiles. Rust and frontend dependency licenses are policed by `cargo deny` (`deny.toml`) and declared in
`package-lock.json`.

**Contact:** [https://zyvor.dev](https://zyvor.dev)
