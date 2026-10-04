// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Engine data-path throughput over real data-node connections on localhost (3 replicas).
//! `cargo test --release -p atlas-native --test datapath_bench -- --ignored --nocapture`

use std::{
    net::TcpListener,
    sync::Arc,
    time::{Duration, Instant},
};

use atlas_native::{
    BlockStore, DataNodeServer, EngineConfig, FailureDomain, MetaBackend, NativeEngine, Node,
    RemoteDevice,
};

const MIB: usize = 1 << 20;

#[test]
#[ignore]
fn datapath_throughput() {
    let total_mib: usize = std::env::var("BENCH_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let io = 8 * MIB;
    let td = tempfile::tempdir().unwrap();
    let servers: Vec<DataNodeServer> = (1..=3)
        .map(|i| {
            DataNodeServer::start(
                format!("n{i}"),
                td.path().join(format!("dn{i}")),
                TcpListener::bind("127.0.0.1:0").unwrap(),
            )
            .unwrap()
        })
        .collect();
    let stores: Vec<(Node, Arc<dyn BlockStore>)> = servers
        .iter()
        .enumerate()
        .map(|(i, s)| {
            (
                Node {
                    id: format!("n{}", i + 1),
                    failure_domain: FailureDomain {
                        zone: "z1".into(),
                        rack: format!("r{i}"),
                        host: format!("h{i}"),
                    },
                    free_bytes: 1 << 40,
                    healthy: true,
                },
                Arc::new(RemoteDevice::new(s.local_addr(), Duration::from_secs(30)))
                    as Arc<dyn BlockStore>,
            )
        })
        .collect();
    let mut cfg = EngineConfig::new(td.path().join("meta"));
    cfg.extent_bytes = MIB;
    let e = NativeEngine::open_with(cfg, stores, MetaBackend::Local).unwrap();
    let size = (total_mib * MIB) as u64;
    let v = e.create_volume("bench", size).unwrap();
    let buf: Vec<u8> = (0..io).map(|i| (i * 13 % 251) as u8).collect();

    let t = Instant::now();
    let mut off = 0u64;
    while off < size {
        e.write(&v, off, &buf).unwrap();
        off += io as u64;
    }
    let w = t.elapsed();

    let t = Instant::now();
    let mut off = 0u64;
    while off < size {
        let got = e.read(&v, off, io).unwrap();
        assert_eq!(got.len(), io);
        off += io as u64;
    }
    let r = t.elapsed();

    let mb = total_mib as f64;
    println!(
        "datapath: {total_mib} MiB, 8 MiB I/Os, 1 MiB extents, 3 replicas: \
         write {:.0} MiB/s, read {:.0} MiB/s",
        mb / w.as_secs_f64(),
        mb / r.as_secs_f64()
    );
}
