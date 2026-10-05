// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! A filesystem's inode table. Inodes are shared (`Arc`): a read hands out a reference without
//! holding the table, and a snapshot or clone shares every inode until one side changes it.
//! Changes are tracked for the catalog store like the catalog's other maps.

use std::{collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    namespace::{Inode, InodeKind},
    tracked::Tracked,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InodeTable {
    map: Tracked<u64, Arc<Inode>>,
}

impl InodeTable {
    pub fn get(&self, ino: u64) -> Option<Arc<Inode>> {
        self.map.get(&ino).cloned()
    }

    pub fn contains(&self, ino: u64) -> bool {
        self.map.contains_key(&ino)
    }

    pub fn len(&self) -> u64 {
        self.map.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The inode to edit in place; copied first if a snapshot or a reader still shares it.
    pub fn get_mut(&mut self, ino: u64) -> Option<&mut Inode> {
        self.map.get_mut(&ino).map(Arc::make_mut)
    }

    pub fn insert(&mut self, inode: Inode) {
        self.map.insert(inode.ino, Arc::new(inode));
    }

    pub fn remove(&mut self, ino: u64) -> Option<Inode> {
        self.map.remove(&ino).map(Arc::unwrap_or_clone)
    }

    /// Every inode, in inode order.
    pub fn values(&self) -> impl Iterator<Item = &Inode> {
        self.map.values().map(|i| &**i)
    }

    pub fn into_values(self) -> impl Iterator<Item = Inode> {
        self.map.into_values().map(Arc::unwrap_or_clone)
    }

    /// Inodes changed since [`Self::clear_changes`].
    pub(crate) fn touched(&self) -> impl Iterator<Item = u64> + '_ {
        self.map.touched().iter().copied()
    }

    /// Whether `ino` was inserted or removed (rather than edited in place) since the last
    /// [`Self::clear_changes`].
    pub(crate) fn replaced(&self, ino: u64) -> bool {
        self.map.replaced().contains(&ino)
    }

    /// Forgets the changes, including those of the directory entries of changed inodes.
    pub(crate) fn clear_changes(&mut self) {
        self.map.clear_changes_with(|i| {
            if matches!(&i.kind, InodeKind::Dir { entries, .. } if !entries.touched().is_empty()) {
                if let InodeKind::Dir { entries, .. } = &mut Arc::make_mut(i).kind {
                    entries.clear_changes();
                }
            }
        });
    }
}

impl FromIterator<Inode> for InodeTable {
    fn from_iter<I: IntoIterator<Item = Inode>>(iter: I) -> Self {
        Self {
            map: iter.into_iter().map(|i| (i.ino, Arc::new(i))).collect(),
        }
    }
}

impl Serialize for InodeTable {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(self.map.iter().map(|(k, v)| (k, &**v)))
    }
}

impl<'de> Deserialize<'de> for InodeTable {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        BTreeMap::<u64, Inode>::deserialize(d).map(|m| m.into_values().collect())
    }
}
