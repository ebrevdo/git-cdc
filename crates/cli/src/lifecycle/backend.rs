//! Cache operations are separate from worktree policy and remote garbage
//! collection. An LFS object ID must never be used as a legacy BLAKE3 key.

use std::collections::HashMap;
use std::fs;

use anyhow::{Result, bail};
use git_cdc_core::store::{ChunkStore, DiskStore};

use super::{BackendKind, Object, ObjectId};
use crate::{git, sync};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Presence {
    /// Safe remote backing for eviction, including retention where required.
    Present,
    Missing,
    Unknown,
}

pub(super) struct CachedObject {
    pub id: ObjectId,
    pub stored_bytes: u64,
}

pub(super) trait CacheBackend {
    fn cached_bytes(&self, object: &Object) -> Result<Option<u64>>;
    fn fetch(&self, objects: &[Object]) -> Result<()>;
    fn read(&self, object: &Object) -> Result<Vec<u8>>;
    fn inventory(&self) -> Result<Vec<CachedObject>>;
    /// Read-only. Neither upload content nor publish retention refs here.
    fn remote_presence(&self, objects: &[Object]) -> Result<HashMap<ObjectId, Presence>>;
    fn confirm_eviction(&self) -> Result<()> {
        Ok(())
    }
    fn evict(&self, object: &Object) -> Result<()>;
}

pub(super) struct Backends {
    legacy: Legacy,
    lfs: Lfs,
}

impl Backends {
    pub fn new() -> Result<Self> {
        Ok(Self {
            legacy: Legacy(git::local_store()?),
            lfs: Lfs(crate::lfs::cache::Cache::new()?),
        })
    }

    pub fn get(&self, kind: BackendKind) -> &dyn CacheBackend {
        match kind {
            BackendKind::Legacy => &self.legacy,
            BackendKind::Lfs => &self.lfs,
        }
    }
}

struct Legacy(DiskStore);

fn legacy_hash(id: &ObjectId) -> Result<blake3::Hash> {
    match id {
        ObjectId::Legacy(hash) => Ok(*hash),
        ObjectId::Lfs(_) => bail!("LFS object sent to legacy cache adapter"),
    }
}

fn legacy_chunks(objects: &[Object]) -> Result<HashMap<blake3::Hash, u64>> {
    objects
        .iter()
        .map(|object| Ok((legacy_hash(&object.id)?, object.size)))
        .collect()
}

impl CacheBackend for Legacy {
    fn cached_bytes(&self, object: &Object) -> Result<Option<u64>> {
        match fs::metadata(self.0.path_for(&legacy_hash(&object.id)?)) {
            Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn fetch(&self, objects: &[Object]) -> Result<()> {
        sync::fetch_legacy(&legacy_chunks(objects)?)
    }

    fn read(&self, object: &Object) -> Result<Vec<u8>> {
        self.0.get(&legacy_hash(&object.id)?)
    }

    fn inventory(&self) -> Result<Vec<CachedObject>> {
        self.0
            .list()?
            .into_iter()
            .map(|hash| {
                Ok(CachedObject {
                    id: ObjectId::Legacy(hash),
                    stored_bytes: fs::metadata(self.0.path_for(&hash))?.len(),
                })
            })
            .collect()
    }

    fn remote_presence(&self, objects: &[Object]) -> Result<HashMap<ObjectId, Presence>> {
        let present = sync::remote_present(&legacy_chunks(objects)?)?;
        objects
            .iter()
            .map(|object| {
                let state = if present.contains(&legacy_hash(&object.id)?) {
                    Presence::Present
                } else {
                    Presence::Missing
                };
                Ok((object.id.clone(), state))
            })
            .collect()
    }

    fn evict(&self, object: &Object) -> Result<()> {
        self.0.remove(&legacy_hash(&object.id)?)
    }
}

struct Lfs(crate::lfs::cache::Cache);

fn lfs_oid(id: &ObjectId) -> Result<&str> {
    match id {
        ObjectId::Lfs(oid) => Ok(oid),
        ObjectId::Legacy(_) => bail!("legacy object sent to LFS cache adapter"),
    }
}

fn lfs_objects(objects: &[Object]) -> Result<Vec<(String, u64)>> {
    objects
        .iter()
        .map(|object| Ok((lfs_oid(&object.id)?.to_owned(), object.size)))
        .collect()
}

impl CacheBackend for Lfs {
    fn cached_bytes(&self, object: &Object) -> Result<Option<u64>> {
        self.0.cached_bytes(lfs_oid(&object.id)?)
    }

    fn fetch(&self, objects: &[Object]) -> Result<()> {
        self.0.fetch(&lfs_objects(objects)?)
    }

    fn read(&self, object: &Object) -> Result<Vec<u8>> {
        self.0.read(lfs_oid(&object.id)?, object.size)
    }

    fn inventory(&self) -> Result<Vec<CachedObject>> {
        Ok(self
            .0
            .inventory()?
            .into_iter()
            .map(|(oid, stored_bytes)| CachedObject {
                id: ObjectId::Lfs(oid),
                stored_bytes,
            })
            .collect())
    }

    fn remote_presence(&self, objects: &[Object]) -> Result<HashMap<ObjectId, Presence>> {
        Ok(self
            .0
            .remote_presence(&lfs_objects(objects)?)?
            .into_iter()
            .map(|oid| (ObjectId::Lfs(oid), Presence::Present))
            .collect())
    }

    fn confirm_eviction(&self) -> Result<()> {
        self.0.confirm_eviction()
    }

    fn evict(&self, object: &Object) -> Result<()> {
        self.0.evict(lfs_oid(&object.id)?)
    }
}
