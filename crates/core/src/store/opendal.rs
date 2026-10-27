use std::time::SystemTime;

use anyhow::{Context, Result, bail};

/// OpenDAL connection settings: a service scheme (`azblob`, `gcs`, `sftp`,
/// `ftp`, `gdrive`, `webdav`, `onedrive`, `fs`, ...) plus its key=value
/// options, passed straight through to `Operator::via_iter`.
#[derive(Debug, Clone, Default)]
pub struct OpendalConfig {
    pub scheme: String,
    pub options: Vec<(String, String)>,
    /// Directory chunks live under; normalized to end with `/`.
    pub prefix: String,
}

/// Chunk store over any OpenDAL service. Same API shape as `S3Store`:
/// keys are `{prefix}{hex}`, objects are envelopes, `put*` verifies
/// before admitting (upload-poisoning guard).
pub struct OpendalStore {
    op: opendal::Operator,
    prefix: String,
}

impl OpendalStore {
    pub fn connect(config: &OpendalConfig) -> Result<OpendalStore> {
        opendal::init_default_registry();
        let mut prefix = config.prefix.clone();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            // OpenDAL paths are directory-shaped; a bare "chunks" prefix
            // must be the directory "chunks/" to be listable.
            prefix.push('/');
        }
        let op = opendal::Operator::via_iter(&config.scheme, config.options.clone())
            .with_context(|| format!("opendal: building '{}' operator", config.scheme))?;
        Ok(OpendalStore { op, prefix })
    }

    fn key(&self, hash: &blake3::Hash) -> String {
        format!("{}{}", self.prefix, hash.to_hex())
    }

    pub async fn has(&self, hash: &blake3::Hash) -> Result<bool> {
        self.op
            .exists(&self.key(hash))
            .await
            .context("opendal exists")
    }

    /// Verifies `blake3(data) == hash` before admitting — same guard as
    /// `DiskStore::put`. Objects land as envelopes (compressed when it pays).
    pub async fn put(&self, hash: &blake3::Hash, data: &[u8]) -> Result<()> {
        if blake3::hash(data) != *hash {
            bail!("chunk data does not match hash {}", hash.to_hex());
        }
        self.put_encoded(hash, super::envelope::encode(data)).await
    }

    /// Store an already-enveloped object after verifying it.
    pub async fn put_encoded(&self, hash: &blake3::Hash, encoded: Vec<u8>) -> Result<()> {
        super::envelope::decode(&encoded, hash)?;
        self.op
            .write(&self.key(hash), encoded)
            .await
            .context("opendal write")?;
        Ok(())
    }

    pub async fn get(&self, hash: &blake3::Hash) -> Result<Vec<u8>> {
        super::envelope::decode(&self.get_encoded(hash).await?, hash)
    }

    /// The enveloped bytes as stored.
    pub async fn get_encoded(&self, hash: &blake3::Hash) -> Result<Vec<u8>> {
        Ok(self
            .op
            .read(&self.key(hash))
            .await
            .with_context(|| format!("chunk {} not in opendal store", hash.to_hex()))?
            .to_vec())
    }

    pub async fn remove(&self, hash: &blake3::Hash) -> Result<()> {
        self.op
            .delete(&self.key(hash))
            .await
            .context("opendal delete")?;
        Ok(())
    }

    /// Use service-native batches where available (for example S3's 1,000
    /// keys), with bounded concurrency for both batches and single deletes.
    pub async fn remove_many(&self, hashes: &[blake3::Hash]) -> Result<()> {
        const CONCURRENT_DELETES: usize = 16;
        let batch_size = self
            .op
            .info()
            .full_capability()
            .delete_max_size
            .unwrap_or(1)
            .clamp(1, 1000);
        let mut tasks = tokio::task::JoinSet::new();
        for batch in hashes.chunks(batch_size) {
            let op = self.op.clone();
            let keys: Vec<_> = batch.iter().map(|hash| self.key(hash)).collect();
            tasks.spawn(async move { op.delete_iter(keys).await });
            if tasks.len() == CONCURRENT_DELETES {
                tasks
                    .join_next()
                    .await
                    .unwrap()
                    .context("opendal delete task")??;
            }
        }
        while let Some(result) = tasks.join_next().await {
            result.context("opendal delete task")??;
        }
        Ok(())
    }

    /// Chunk names without per-object metadata requests. Transfers and local
    /// eviction only need presence; only GC needs last-modified timestamps.
    pub async fn list_names(&self) -> Result<Vec<blake3::Hash>> {
        Ok(self
            .chunk_entries()
            .await?
            .into_iter()
            .map(|(hash, _)| hash)
            .collect())
    }

    async fn chunk_entries(&self) -> Result<Vec<(blake3::Hash, opendal::Entry)>> {
        let path = if self.prefix.is_empty() {
            "/"
        } else {
            &self.prefix
        };
        let entries = match self.op.list(path).await {
            Ok(entries) => entries,
            // A store that has never seen a put has no prefix directory yet.
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).context("opendal list"),
        };
        Ok(entries
            .into_iter()
            .filter(|entry| entry.metadata().is_file())
            .filter_map(|entry| {
                // Foreign objects under the prefix are not ours to GC.
                blake3::Hash::from_hex(entry.name())
                    .ok()
                    .map(|hash| (hash, entry))
            })
            .collect())
    }

    /// All chunks under the prefix with their last-modified age — GC's
    /// grace period needs it. Falls back to a per-entry stat when the
    /// service's listing omits timestamps; `None` if that fails too.
    pub async fn list(&self) -> Result<Vec<(blake3::Hash, Option<SystemTime>)>> {
        let mut out = Vec::new();
        for (hash, entry) in self.chunk_entries().await? {
            let mut modified = entry.metadata().last_modified();
            if modified.is_none() {
                modified = self
                    .op
                    .stat(entry.path())
                    .await
                    .ok()
                    .and_then(|m| m.last_modified());
            }
            out.push((hash, modified.map(SystemTime::from)));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::raw::{Access, AccessorInfo, OpList, OpStat, RpList, RpStat, oio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct MissingTimestamps {
        stats: Arc<AtomicUsize>,
    }

    impl Access for MissingTimestamps {
        type Reader = oio::Reader;
        type Writer = oio::Writer;
        type Lister = oio::Lister;
        type Deleter = oio::Deleter;
        type Copier = oio::Copier;

        fn info(&self) -> Arc<AccessorInfo> {
            let info = AccessorInfo::default();
            info.set_scheme("test");
            info.set_native_capability(opendal::Capability {
                list: true,
                stat: true,
                ..Default::default()
            });
            Arc::new(info)
        }

        async fn list(&self, _: &str, _: OpList) -> opendal::Result<(RpList, Self::Lister)> {
            let hash = blake3::hash(b"listed chunk");
            Ok((
                RpList::default(),
                Box::new(Entries(Some(oio::Entry::new(
                    &format!("chunks/{}", hash.to_hex()),
                    opendal::Metadata::new(opendal::EntryMode::FILE),
                )))),
            ))
        }

        async fn stat(&self, _: &str, _: OpStat) -> opendal::Result<RpStat> {
            self.stats.fetch_add(1, Ordering::SeqCst);
            Err(opendal::Error::new(
                opendal::ErrorKind::PermissionDenied,
                "unavailable timestamp",
            ))
        }
    }

    struct Entries(Option<oio::Entry>);

    impl oio::List for Entries {
        async fn next(&mut self) -> opendal::Result<Option<oio::Entry>> {
            Ok(self.0.take())
        }
    }

    #[tokio::test]
    async fn names_only_listing_never_stats_and_gc_keeps_unknown_ages() {
        let stats = Arc::new(AtomicUsize::new(0));
        let store = OpendalStore {
            op: opendal::Operator::from_inner(Arc::new(MissingTimestamps {
                stats: stats.clone(),
            })),
            prefix: "chunks/".into(),
        };
        let hash = blake3::hash(b"listed chunk");
        assert_eq!(store.list_names().await.unwrap(), vec![hash]);
        assert_eq!(stats.load(Ordering::SeqCst), 0);
        assert_eq!(store.list().await.unwrap(), vec![(hash, None)]);
        assert_eq!(stats.load(Ordering::SeqCst), 1);
    }
}
