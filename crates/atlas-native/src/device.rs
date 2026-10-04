// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::engine::NativeError;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// Byte-addressed extent storage the engine writes replicas to: a local [`FileDevice`] or a
/// [`crate::data_node::RemoteDevice`].
///
/// `fence` is the writer's Raft term. Remote data nodes reject writes whose fence is below the
/// highest they have accepted; local files ignore it.
pub trait BlockStore: std::fmt::Debug + Send + Sync {
    /// Appends `data` at the end of the device and returns its offset.
    fn append(&self, fence: u64, data: &[u8]) -> Result<u64, NativeError>;
    /// Overwrites `data.len()` bytes at `offset`. Never extends the device.
    fn write_at(&self, fence: u64, offset: u64, data: &[u8]) -> Result<(), NativeError>;
    fn read_exact_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, NativeError>;
    fn len(&self) -> Result<u64, NativeError>;
    fn is_empty(&self) -> Result<bool, NativeError> {
        Ok(self.len()? == 0)
    }
    /// Network address a client can read this device at directly, if it is remote.
    fn endpoint(&self) -> Option<&str> {
        None
    }
}

impl BlockStore for FileDevice {
    fn append(&self, _fence: u64, data: &[u8]) -> Result<u64, NativeError> {
        FileDevice::append(self, data)
    }
    fn write_at(&self, _fence: u64, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        FileDevice::write_at(self, offset, data)
    }
    fn read_exact_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, NativeError> {
        FileDevice::read_exact_at(self, offset, len)
    }
    fn len(&self) -> Result<u64, NativeError> {
        FileDevice::len(self)
    }
}

/// Appends are serialized (they extend the file); reads and in-place writes use positional I/O
/// on a second handle and run concurrently.
#[derive(Debug)]
pub struct FileDevice {
    id: DeviceId,
    path: PathBuf,
    file: Mutex<File>,
    positional: File,
}

impl FileDevice {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, NativeError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        let positional = file.try_clone()?;
        Ok(Self {
            id: DeviceId(Uuid::new_v4().to_string()),
            path,
            file: Mutex::new(file),
            positional,
        })
    }

    pub fn id(&self) -> &DeviceId {
        &self.id
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, data: &[u8]) -> Result<u64, NativeError> {
        let mut f = self
            .file
            .lock()
            .map_err(|_| NativeError::Poisoned("device"))?;
        let off = f.seek(SeekFrom::End(0))?;
        f.write_all(data)?;
        f.sync_data()?;
        Ok(off)
    }

    /// Overwrites `data.len()` bytes at `offset` (reuse of a freed range). Never extends the file.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        let end = self.len()?;
        if offset + data.len() as u64 > end {
            return Err(NativeError::Invalid(format!(
                "write_at {offset}+{} past device end {end}",
                data.len()
            )));
        }
        self.positional.write_all_at(data, offset)?;
        self.positional.sync_data()?;
        Ok(())
    }

    pub fn len(&self) -> Result<u64, NativeError> {
        Ok(self.positional.metadata()?.len())
    }

    pub fn is_empty(&self) -> Result<bool, NativeError> {
        Ok(self.len()? == 0)
    }

    pub fn read_exact_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, NativeError> {
        let mut buf = vec![0u8; len];
        self.positional.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }
}
