// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! The catalog on disk (`catalog.redb`): one record per volume, snapshot, extent, filesystem,
//! inode and filesystem snapshot, plus one for the rest of the catalog's state. A checkpoint
//! writes only the records the catalog's [`Tracked`](crate::tracked::Tracked) maps report as
//! changed, in one transaction, so its cost follows the changes since the last checkpoint rather
//! than the size of the catalog. A catalog the store doesn't hold yet (new, read from
//! `catalog.json`, installed from a Raft snapshot) is written in full.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use redb::{
    Database, ReadableDatabase, ReadableTable, TableDefinition, TableError, WriteTransaction,
};
use serde::{Deserialize, Serialize};

use crate::{
    alloc::FreeList,
    membership::Membership,
    metadata::{Catalog, SnapshotId},
    namespace::{FsId, FsMeta, Inode, InodeKind},
    tracked::Tracked,
};

const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("state");
const VOLUMES: TableDefinition<&str, &[u8]> = TableDefinition::new("volumes");
const SNAPSHOTS: TableDefinition<&str, &[u8]> = TableDefinition::new("snapshots");
const EXTENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("extents");
const FILESYSTEMS: TableDefinition<&str, &[u8]> = TableDefinition::new("filesystems");
const INODES: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("inodes");
/// Directory entries, one record each, so a create in a large directory writes one small record.
const DIR_ENTRIES: TableDefinition<(&str, u64, &str), u64> = TableDefinition::new("dir_entries");
const FS_SNAPSHOTS: TableDefinition<&str, &[u8]> = TableDefinition::new("fs_snapshots");
const STATE_KEY: &str = "catalog";

/// Everything in [`Catalog`] outside its tracked maps; small, rewritten at every checkpoint.
#[derive(Serialize)]
struct StateRef<'a> {
    applied_index: u64,
    current_term: u64,
    free: &'a FreeList,
    membership: &'a Option<Membership>,
    raft_addrs: &'a BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct State {
    applied_index: u64,
    current_term: u64,
    free: FreeList,
    membership: Option<Membership>,
    raft_addrs: BTreeMap<String, String>,
}

/// A filesystem without its inodes, which have records of their own.
#[derive(Serialize, Deserialize)]
struct FsHeader {
    id: FsId,
    name: String,
    next_ino: u64,
    source_snapshot: Option<SnapshotId>,
    extent_bytes: Option<u64>,
}

fn err(e: impl Into<redb::Error>) -> io::Error {
    io::Error::other(e.into())
}

fn json<T: Serialize + ?Sized>(v: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(io::Error::other)
}

fn parse<'a, T: Deserialize<'a>>(b: &'a [u8]) -> io::Result<T> {
    serde_json::from_slice(b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The store's file under an engine or Raft root.
pub const CATALOG_STORE: &str = "catalog.redb";
/// Where catalogs were checkpointed before the store; read once, then removed.
const LEGACY_CATALOG: &str = "catalog.json";

/// The checkpointed catalog: from the store, else from a legacy `catalog.json`, else empty.
pub fn load_checkpoint(root: &Path, store: &CatalogStore) -> io::Result<Catalog> {
    if let Some(c) = store.load()? {
        return Ok(c);
    }
    let legacy = root.join(LEGACY_CATALOG);
    if legacy.exists() {
        parse(&fs::read(&legacy)?)
    } else {
        Ok(Catalog::default())
    }
}

/// Drops a legacy `catalog.json` once the store holds a checkpoint, so it can never be read
/// over newer state.
pub fn remove_legacy_catalog(root: &Path) -> io::Result<()> {
    match fs::remove_file(root.join(LEGACY_CATALOG)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

pub struct CatalogStore {
    db: Database,
    path: PathBuf,
}

impl std::fmt::Debug for CatalogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogStore")
            .field("path", &self.path)
            .finish()
    }
}

impl CatalogStore {
    /// Opens or creates the store. Only one process may hold it open.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        Ok(Self {
            db: Database::create(&path).map_err(err)?,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The last checkpointed catalog, or `None` if nothing was checkpointed yet.
    pub fn load(&self) -> io::Result<Option<Catalog>> {
        let tx = self.db.begin_read().map_err(err)?;
        let state: State = match tx.open_table(STATE) {
            Ok(t) => match t.get(STATE_KEY).map_err(err)? {
                Some(v) => parse(v.value())?,
                None => return Ok(None),
            },
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(err(e)),
        };
        let mut c = Catalog {
            applied_index: state.applied_index,
            current_term: state.current_term,
            free: state.free,
            membership: state.membership,
            raft_addrs: state.raft_addrs,
            ..Catalog::default()
        };
        c.volumes = read_table(&tx, VOLUMES)?;
        c.snapshots = read_table(&tx, SNAPSHOTS)?;
        c.extents = read_table(&tx, EXTENTS)?;
        c.fs_snapshots = read_table(&tx, FS_SNAPSHOTS)?;
        let headers: BTreeMap<FsId, FsHeader> = read_table::<FsHeader>(&tx, FILESYSTEMS)?
            .into_values()
            .map(|h| (h.id.clone(), h))
            .collect();
        let mut inodes: BTreeMap<FsId, BTreeMap<u64, Inode>> = BTreeMap::new();
        match tx.open_table(INODES) {
            Ok(t) => {
                for row in t.iter().map_err(err)? {
                    let (k, v) = row.map_err(err)?;
                    let (fs, ino) = k.value();
                    inodes
                        .entry(fs.to_string())
                        .or_default()
                        .insert(ino, parse(v.value())?);
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(err(e)),
        }
        match tx.open_table(DIR_ENTRIES) {
            Ok(t) => {
                let mut dirs: BTreeMap<(String, u64), BTreeMap<String, u64>> = BTreeMap::new();
                for row in t.iter().map_err(err)? {
                    let (k, v) = row.map_err(err)?;
                    let (fs, dir, name) = k.value();
                    dirs.entry((fs.to_string(), dir))
                        .or_default()
                        .insert(name.to_string(), v.value());
                }
                for ((fs, dir), names) in dirs {
                    let inode = inodes.get_mut(&fs).and_then(|m| m.get_mut(&dir));
                    match inode.map(|i| &mut i.kind) {
                        Some(InodeKind::Dir { entries, .. }) => *entries = names.into(),
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "directory entries for {fs}/{dir}, which is not a directory"
                                ),
                            ))
                        }
                    }
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(err(e)),
        }
        c.filesystems = headers
            .into_values()
            .map(|h| {
                let fs = FsMeta {
                    inodes: inodes.remove(&h.id).unwrap_or_default().into(),
                    id: h.id.clone(),
                    name: h.name,
                    next_ino: h.next_ino,
                    source_snapshot: h.source_snapshot,
                    extent_bytes: h.extent_bytes,
                };
                (h.id, fs)
            })
            .collect();
        c.in_store = true;
        Ok(Some(c))
    }

    /// Writes `c`'s changes (all of it unless `c.in_store`) durably, then forgets them. Returns
    /// how many records were written or removed.
    pub fn checkpoint(&self, c: &mut Catalog) -> io::Result<u64> {
        let tx = self.db.begin_write().map_err(err)?;
        let full = !c.in_store;
        if full {
            for t in [
                STATE,
                VOLUMES,
                SNAPSHOTS,
                EXTENTS,
                FILESYSTEMS,
                FS_SNAPSHOTS,
            ] {
                tx.delete_table(t).map_err(err)?;
            }
            tx.delete_table(INODES).map_err(err)?;
            tx.delete_table(DIR_ENTRIES).map_err(err)?;
        }
        write_state(&tx, c)?;
        let records = 1
            + write_map(&tx, VOLUMES, &c.volumes, full)?
            + write_map(&tx, SNAPSHOTS, &c.snapshots, full)?
            + write_map(&tx, EXTENTS, &c.extents, full)?
            + write_map(&tx, FS_SNAPSHOTS, &c.fs_snapshots, full)?
            + write_filesystems(&tx, &c.filesystems, full)?;
        tx.commit().map_err(err)?;

        c.volumes.clear_changes();
        c.snapshots.clear_changes();
        c.extents.clear_changes();
        c.fs_snapshots.clear_changes();
        c.filesystems.clear_changes_with(|f| {
            f.inodes.clear_changes_with(|i| {
                if let InodeKind::Dir { entries, .. } = &mut i.kind {
                    entries.clear_changes();
                }
            })
        });
        c.in_store = true;
        Ok(records)
    }
}

/// An inode's record: a directory's entries are stored separately.
fn inode_record(i: &Inode) -> io::Result<Vec<u8>> {
    match &i.kind {
        InodeKind::Dir { parent, .. } => json(&Inode {
            kind: InodeKind::Dir {
                parent: *parent,
                entries: Tracked::default(),
            },
            op_id: i.op_id.clone(),
            xattrs: i.xattrs.clone(),
            ..*i
        }),
        _ => json(i),
    }
}

fn read_table<V: for<'a> Deserialize<'a>>(
    tx: &redb::ReadTransaction,
    def: TableDefinition<&str, &[u8]>,
) -> io::Result<Tracked<String, V>> {
    let t = match tx.open_table(def) {
        Ok(t) => t,
        Err(TableError::TableDoesNotExist(_)) => return Ok(Tracked::default()),
        Err(e) => return Err(err(e)),
    };
    let mut out = BTreeMap::new();
    for row in t.iter().map_err(err)? {
        let (k, v) = row.map_err(err)?;
        out.insert(k.value().to_string(), parse(v.value())?);
    }
    Ok(out.into())
}

fn write_state(tx: &WriteTransaction, c: &Catalog) -> io::Result<()> {
    let state = json(&StateRef {
        applied_index: c.applied_index,
        current_term: c.current_term,
        free: &c.free,
        membership: &c.membership,
        raft_addrs: &c.raft_addrs,
    })?;
    tx.open_table(STATE)
        .map_err(err)?
        .insert(STATE_KEY, state.as_slice())
        .map_err(err)?;
    Ok(())
}

fn write_map<V: Serialize>(
    tx: &WriteTransaction,
    def: TableDefinition<&str, &[u8]>,
    map: &Tracked<String, V>,
    full: bool,
) -> io::Result<u64> {
    let mut t = tx.open_table(def).map_err(err)?;
    let keys: Box<dyn Iterator<Item = &String>> = if full {
        Box::new(map.keys())
    } else {
        Box::new(map.touched().iter())
    };
    let mut n = 0;
    for k in keys {
        n += 1;
        match map.get(k) {
            Some(v) => {
                t.insert(k.as_str(), json(v)?.as_slice()).map_err(err)?;
            }
            None => {
                t.remove(k.as_str()).map_err(err)?;
            }
        }
    }
    Ok(n)
}

fn write_filesystems(
    tx: &WriteTransaction,
    filesystems: &Tracked<FsId, FsMeta>,
    full: bool,
) -> io::Result<u64> {
    let mut headers = tx.open_table(FILESYSTEMS).map_err(err)?;
    let mut inodes = tx.open_table(INODES).map_err(err)?;
    let mut dirents = tx.open_table(DIR_ENTRIES).map_err(err)?;
    let ids: Box<dyn Iterator<Item = &FsId>> = if full {
        Box::new(filesystems.keys())
    } else {
        Box::new(filesystems.touched().iter())
    };
    let mut n = 0;
    for id in ids {
        n += 1;
        let fs = id.as_str();
        // A filesystem replaced or removed since the last checkpoint keeps none of its records.
        let rewrite = full || filesystems.replaced().contains(id);
        if rewrite && !full {
            inodes
                .retain_in((fs, 0)..=(fs, u64::MAX), |_, _| false)
                .map_err(err)?;
            dirents
                .retain_in((fs, 0, "")..(fs, u64::MAX, ""), |_, _| false)
                .map_err(err)?;
        }
        let Some(f) = filesystems.get(id) else {
            headers.remove(fs).map_err(err)?;
            continue;
        };
        let header = json(&FsHeader {
            id: f.id.clone(),
            name: f.name.clone(),
            next_ino: f.next_ino,
            source_snapshot: f.source_snapshot.clone(),
            extent_bytes: f.extent_bytes,
        })?;
        headers.insert(fs, header.as_slice()).map_err(err)?;
        let changed: Box<dyn Iterator<Item = &u64>> = if rewrite {
            Box::new(f.inodes.keys())
        } else {
            Box::new(f.inodes.touched().iter())
        };
        for &ino in changed {
            n += 1;
            // An inode inserted or removed (not just edited) keeps none of its old entries.
            let fresh = rewrite || f.inodes.replaced().contains(&ino);
            if fresh && !rewrite {
                dirents
                    .retain_in((fs, ino, "")..(fs, ino + 1, ""), |_, _| false)
                    .map_err(err)?;
            }
            let Some(i) = f.inodes.get(&ino) else {
                inodes.remove((fs, ino)).map_err(err)?;
                continue;
            };
            inodes
                .insert((fs, ino), inode_record(i)?.as_slice())
                .map_err(err)?;
            if let InodeKind::Dir { entries, .. } = &i.kind {
                let names: Box<dyn Iterator<Item = &String>> = if fresh {
                    Box::new(entries.keys())
                } else {
                    Box::new(entries.touched().iter())
                };
                for name in names {
                    n += 1;
                    match entries.get(name) {
                        Some(child) => {
                            dirents
                                .insert((fs, ino, name.as_str()), *child)
                                .map_err(err)?;
                        }
                        None => {
                            dirents.remove((fs, ino, name.as_str())).map_err(err)?;
                        }
                    }
                }
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        metadata::{ExtentRef, MetaCommand, ReplicaRef},
        namespace::{FsOp, NodeType, ROOT_INO},
    };

    struct Harness {
        _td: tempfile::TempDir,
        store: CatalogStore,
        catalog: Catalog,
        index: u64,
    }

    impl Harness {
        fn new() -> Self {
            let td = tempfile::tempdir().unwrap();
            let store = CatalogStore::open(td.path().join(CATALOG_STORE)).unwrap();
            Self {
                _td: td,
                store,
                catalog: Catalog::default(),
                index: 0,
            }
        }

        fn apply(&mut self, cmd: MetaCommand) {
            self.index += 1;
            self.catalog.apply(1, self.index, &cmd).unwrap();
        }

        fn fs(&mut self, op: FsOp) {
            self.apply(MetaCommand::Fs { op });
        }

        fn mknode(&mut self, fs: &str, parent: u64, name: &str, node_type: NodeType) {
            self.fs(FsOp::Mknode {
                fs: fs.into(),
                parent,
                name: name.into(),
                op_id: format!("{fs}-{name}"),
                node_type,
                target: None,
                rdev: 0,
                mode: 0o644,
                uid: 0,
                gid: 0,
                now_ns: self.index as i64,
            });
        }

        fn ino(&self, fs: &str, name: &str) -> u64 {
            self.catalog.filesystems[fs].entries(ROOT_INO).unwrap()[name]
        }

        /// Checkpoints, then reads the store back and compares it with memory.
        fn check(&mut self) {
            self.store.checkpoint(&mut self.catalog).unwrap();
            assert!(self.catalog.in_store);
            let loaded = self.store.load().unwrap().unwrap();
            assert_eq!(
                serde_json::to_value(&loaded).unwrap(),
                serde_json::to_value(&self.catalog).unwrap()
            );
        }
    }

    fn extent(id: &str, offset: u64) -> ExtentRef {
        ExtentRef {
            id: id.into(),
            logical_offset: 0,
            len: 4096,
            checksum: [7; 32],
            replicas: vec![ReplicaRef {
                node_id: "n1".into(),
                device_index: 0,
                offset,
            }],
        }
    }

    #[test]
    fn incremental_checkpoints_round_trip() {
        let mut h = Harness::new();
        assert!(h.store.load().unwrap().is_none());
        h.apply(MetaCommand::CreateVolume {
            id: "vol".into(),
            name: "vol".into(),
            size_bytes: 1 << 20,
        });
        h.fs(FsOp::CreateFs {
            fs: "f".into(),
            name: "f".into(),
            now_ns: 1,
            extent_bytes: None,
        });
        h.mknode("f", ROOT_INO, "a", NodeType::File);
        h.mknode("f", ROOT_INO, "b", NodeType::File);
        h.mknode("f", ROOT_INO, "d", NodeType::Dir);
        h.check();

        // In-place edits, a removal and a new extent.
        let (a, b, d) = (h.ino("f", "a"), h.ino("f", "b"), h.ino("f", "d"));
        h.fs(FsOp::Unlink {
            fs: "f".into(),
            parent: ROOT_INO,
            name: "a".into(),
            now_ns: 5,
        });
        h.mknode("f", d, "inner", NodeType::File);
        h.fs(FsOp::InstallFileExtent {
            fs: "f".into(),
            ino: b,
            logical_offset: 0,
            extent: extent("e1", 0),
            size: 4096,
            now_ns: 6,
        });
        h.check();
        assert!(!h.catalog.filesystems["f"].inodes.contains_key(&a));

        // Renames across directories and an rmdir move and drop entry records.
        h.mknode("f", ROOT_INO, "gone", NodeType::Dir);
        h.fs(FsOp::Rename {
            fs: "f".into(),
            parent: d,
            name: "inner".into(),
            new_parent: ROOT_INO,
            new_name: "moved".into(),
            now_ns: 6,
        });
        h.fs(FsOp::Rmdir {
            fs: "f".into(),
            parent: ROOT_INO,
            name: "gone".into(),
            now_ns: 6,
        });
        h.check();

        // A snapshot, a clone of it, and an edit in the clone.
        h.fs(FsOp::SnapshotFs {
            id: "s1".into(),
            fs: "f".into(),
            name: "s1".into(),
            now_ns: 7,
        });
        h.fs(FsOp::CloneFs {
            id: "g".into(),
            name: "g".into(),
            snapshot_id: "s1".into(),
        });
        h.mknode("g", ROOT_INO, "only-in-g", NodeType::File);
        h.check();

        // A filesystem deleted and recreated under the same id keeps none of its old inodes.
        h.fs(FsOp::DeleteFs { fs: "g".into() });
        h.fs(FsOp::CreateFs {
            fs: "g".into(),
            name: "g2".into(),
            now_ns: 8,
            extent_bytes: None,
        });
        h.mknode("g", ROOT_INO, "fresh", NodeType::File);
        h.check();
        assert_eq!(h.catalog.filesystems["g"].inodes.len(), 2);

        h.fs(FsOp::DeleteFs { fs: "g".into() });
        h.apply(MetaCommand::DeleteVolume {
            volume_id: "vol".into(),
        });
        h.check();
    }

    #[test]
    fn a_create_in_a_large_directory_writes_a_few_records() {
        let mut h = Harness::new();
        h.fs(FsOp::CreateFs {
            fs: "f".into(),
            name: "f".into(),
            now_ns: 1,
            extent_bytes: None,
        });
        for i in 0..1000 {
            h.mknode("f", ROOT_INO, &format!("file-{i}"), NodeType::File);
        }
        h.check();
        h.mknode("f", ROOT_INO, "one-more", NodeType::File);
        // State, filesystem header, the new inode, the directory inode and its new entry.
        let written = h.store.checkpoint(&mut h.catalog).unwrap();
        assert_eq!(written, 5);
        h.check();
    }

    #[test]
    fn a_catalog_not_from_the_store_is_written_in_full() {
        let mut h = Harness::new();
        h.fs(FsOp::CreateFs {
            fs: "f".into(),
            name: "f".into(),
            now_ns: 1,
            extent_bytes: None,
        });
        h.mknode("f", ROOT_INO, "a", NodeType::File);
        h.check();
        // A Raft snapshot (or catalog.json) arrives as plain JSON with no change records.
        let mut other = Harness::new();
        other.fs(FsOp::CreateFs {
            fs: "x".into(),
            name: "x".into(),
            now_ns: 1,
            extent_bytes: None,
        });
        h.catalog = serde_json::from_value(serde_json::to_value(&other.catalog).unwrap()).unwrap();
        assert!(!h.catalog.in_store);
        h.check();
        assert!(!h
            .store
            .load()
            .unwrap()
            .unwrap()
            .filesystems
            .contains_key("f"));
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::{
        metadata::MetaCommand,
        namespace::{FsOp, NodeType, ROOT_INO},
    };

    /// `cargo test --release -p atlas-native --lib store::bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn checkpoint_time_as_the_store_grows() {
        let td = tempfile::tempdir().unwrap();
        let store = CatalogStore::open(td.path().join(CATALOG_STORE)).unwrap();
        let mut c = Catalog::default();
        let mut index = 1;
        c.apply(
            1,
            index,
            &MetaCommand::Fs {
                op: FsOp::CreateFs {
                    fs: "f".into(),
                    name: "f".into(),
                    now_ns: 1,
                    extent_bytes: None,
                },
            },
        )
        .unwrap();
        for round in 0..64 {
            let t = std::time::Instant::now();
            for i in 0..1024 {
                index += 1;
                let name = format!("file-{round}-{i}");
                c.apply(
                    1,
                    index,
                    &MetaCommand::Fs {
                        op: FsOp::Mknode {
                            fs: "f".into(),
                            parent: ROOT_INO,
                            op_id: name.clone(),
                            name,
                            node_type: NodeType::File,
                            target: None,
                            rdev: 0,
                            mode: 0o644,
                            uid: 0,
                            gid: 0,
                            now_ns: 2,
                        },
                    },
                )
                .unwrap();
            }
            let applied = t.elapsed();
            let t = std::time::Instant::now();
            let n = store.checkpoint(&mut c).unwrap();
            if round % 8 == 7 {
                println!(
                    "{} inodes: apply 1024 in {applied:?}, checkpoint {n} records in {:?}, file {} KiB",
                    c.filesystems["f"].inodes.len(),
                    t.elapsed(),
                    std::fs::metadata(store.path()).unwrap().len() / 1024
                );
            }
        }
    }
}
