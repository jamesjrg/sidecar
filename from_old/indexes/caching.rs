// We are going to store caches here for what we have indexed already
// and what we still have to index
//
// Now the way we want to go about doing this:
// we use a fs based system and wrap it in a lock so we are okay with things

use qdrant_client::qdrant::{PointId, PointStruct};
use rayon::iter::ParallelIterator;
use scc::hash_map::Entry;
use sqlx::Sqlite;
use std::collections::HashSet;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tracing::{debug, error, info, trace, warn};

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::db::sqlite::SqlDb;
use crate::embedder::embedder::{EmbedChunk, EmbedQueue};
use crate::repo::types::RepoRef;
use crate::semantic_search::client::SemanticClient;
use crate::semantic_search::schema::Payload;

/// The extra information here which we need for the code snippet is the line_start
/// and the line_end
#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub struct CodeSnippetCacheKeys {
    tantivy: String,
    commit_hash: String,
    file_path: String,
    content_hash: String,
}

impl CodeSnippetCacheKeys {
    pub fn new(
        tantivy: String,
        commit_hash: String,
        file_path: String,
        content_hash: String,
    ) -> Self {
        Self {
            tantivy,
            commit_hash,
            file_path,
            content_hash,
        }
    }

    pub fn tantivy(&self) -> &str {
        &self.tantivy
    }
}

// Indexes might require their own keys, we know tantivy is fucked because
// it expects a key which is unique to the doc schema which you put in...
// so if we query it with the wrong schema it blows in your face :|
// for now we care about tantivy so lets get that working
#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub struct CacheKeys {
    tantivy: String,
    semantic: String,
    commit_hash: String,
    file_path: String,
    file_content_hash: String,
}

impl CacheKeys {
    pub fn new(
        tantivy: String,
        semantic: String,
        commit_hash: String,
        file_path: String,
        file_content_hash: String,
    ) -> Self {
        Self {
            tantivy,
            semantic,
            commit_hash,
            file_path,
            file_content_hash,
        }
    }

    pub fn tantivy(&self) -> &str {
        &self.tantivy
    }

    pub fn semantic(&self) -> &str {
        &self.semantic
    }

    pub fn commit_hash(&self) -> &str {
        &self.commit_hash
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    pub fn file_content_hash(&self) -> &str {
        &self.file_content_hash
    }
}

#[derive(serde::Serialize, serde::Deserialize, Eq)]
pub struct FreshValue<T> {
    // default value is `false` on deserialize
    pub(crate) fresh: bool,
    pub(crate) value: T,
}

impl<T: Default> FreshValue<T> {
    fn fresh_default() -> Self {
        Self {
            fresh: true,
            value: Default::default(),
        }
    }
}

impl<T> PartialEq for FreshValue<T>
where
    T: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.value.eq(&other.value)
    }
}

impl<T> FreshValue<T> {
    fn stale(value: T) -> Self {
        Self {
            fresh: false,
            value,
        }
    }
}

impl<T> From<T> for FreshValue<T> {
    fn from(value: T) -> Self {
        Self { fresh: true, value }
    }
}

/// This is the storage for the underlying struct which we will use to store
/// anything and everything
pub struct FSStorage<T: Serialize + DeserializeOwned + PartialEq> {
    source: T,
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl<T: Serialize + DeserializeOwned + PartialEq> FSStorage<T> {
    pub fn new(source: T, path: PathBuf) -> Self {
        Self {
            source,
            path,
            write_lock: Mutex::new(()),
        }
    }

    // This will store the underlying data to the path we are interested in
    pub async fn store_to_path(&self) -> Result<()> {
        // We take the lock here, since we want to be the only ones writing
        // to it for correctness sake
        let _lock = self.write_lock.lock().await;
        tokio::fs::create_dir_all(self.path.parent().unwrap()).await?;
        let mut file = tokio::fs::File::create(&self.path).await?;
        let data = serde_json::to_vec(&self.source)?;
        file.write_all(&data).await?;
        Ok(())
    }
}

pub struct FileCacheSnapshot<'a> {
    snapshot: Arc<scc::HashMap<CacheKeys, FreshValue<()>>>,
    parent: &'a FileCache<'a>,
}

impl<'a> Deref for FileCacheSnapshot<'a> {
    type Target = scc::HashMap<CacheKeys, FreshValue<()>>;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl<'a> FileCacheSnapshot<'a> {
    pub fn parent(&self) -> &'a FileCache<'a> {
        self.parent
    }

    pub fn is_fresh(&self, keys: &CacheKeys) -> bool {
        match self.snapshot.entry(keys.clone()) {
            Entry::Occupied(mut val) => {
                val.get_mut().fresh = true;
                true
            }
            Entry::Vacant(val) => {
                debug!(?keys, "inserting fresh value in File Cache Snapshot");
                _ = val.insert_entry(FreshValue::fresh_default());

                false
            }
        }
    }
}

// This is where we maintain a cache of the file and have a storage layer
// backing up the cache and everything happening here
pub struct FileCache<'a> {
    sqlite: &'a SqlDb,
    reporef: &'a RepoRef,
    semantic: Option<&'a SemanticClient>,
    embed_queue: EmbedQueue,
}

impl<'a> FileCache<'a> {
    pub fn for_repo(
        sqlite: &'a SqlDb,
        reporef: &'a RepoRef,
        semantic: Option<&'a SemanticClient>,
    ) -> Self {
        Self {
            sqlite,
            reporef,
            semantic,
            embed_queue: Default::default(),
        }
    }

    // Retrieve a file-level snapshot of the cache for the repository in scope.
    pub(crate) async fn retrieve(&'a self) -> FileCacheSnapshot<'a> {
        let repo_str = self.reporef.to_string();
        let rows = sqlx::query! {
            "SELECT tantivy_cache_key, file_content_hash, file_path, commit_hash, semantic_search_hash FROM file_cache \
             WHERE repo_ref = ?",
            repo_str,
        }
        .fetch_all(self.sqlite.as_ref())
        .await;

        let output = scc::HashMap::default();
        for row in rows.into_iter().flatten() {
            let tantivy_cache_key = row.tantivy_cache_key;
            _ = output.insert(
                CacheKeys {
                    tantivy: tantivy_cache_key,
                    semantic: row.semantic_search_hash,
                    commit_hash: row.commit_hash,
                    file_path: row.file_path,
                    file_content_hash: row.file_content_hash,
                },
                FreshValue::stale(()),
            );
        }

        FileCacheSnapshot {
            snapshot: output.into(),
            parent: self,
        }
    }

    async fn delete_files(&self, tx: &mut sqlx::Transaction<'_, Sqlite>) -> anyhow::Result<()> {
        let repo_str = self.reporef.to_string();
        sqlx::query! {
            "DELETE FROM file_cache \
                 WHERE repo_ref = ?",
            repo_str
        }
        .execute(&mut **tx)
        .await?;

        Ok(())
    }

    async fn delete_chunks(&self, tx: &mut sqlx::Transaction<'_, Sqlite>) -> anyhow::Result<()> {
        let repo_str = self.reporef.to_string();
        sqlx::query! {
            "DELETE FROM chunk_cache WHERE repo_ref = ?",
            repo_str
        }
        .execute(&mut **tx)
        .await?;

        Ok(())
    }

    pub(crate) async fn synchronize(
        &'a self,
        cache: FileCacheSnapshot<'a>,
        delete_tantivy: impl Fn(&str),
    ) -> anyhow::Result<()> {
        debug!(?self.reporef, "synchronizing file cache");
        let mut tx = self.sqlite.begin().await?;
        // First we clean-up our cache here by calling delete files
        self.delete_files(&mut tx).await?;

        // files that are no longer tracked by the git index are to be removed
        // from the tantivy & qdrant indices
        let qdrant_stale = {
            let mut semantic_fresh = HashSet::new();
            let mut semantic_all = HashSet::new();

            cache.retain(|k, v| {
                // check if it's already in to avoid unnecessary copies
                if v.fresh && !semantic_fresh.contains(k.semantic()) {
                    semantic_fresh.insert(k.semantic().to_owned());
                }

                if !semantic_all.contains(k.semantic()) {
                    semantic_all.insert(k.semantic().to_owned());
                }

                // just call the passed closure for tantivy
                if !v.fresh {
                    // Here we are removing the file from the tantivy index since
                    // its stale and we no longer want to keep track of it
                    delete_tantivy(k.tantivy())
                }

                v.fresh
            });

            semantic_all
                .difference(&semantic_fresh)
                .cloned()
                .collect::<Vec<_>>()
        };

        // generate a transaction to push the remaining entries
        // into the sql cache
        {
            let mut next = cache.first_occupied_entry_async().await;
            while let Some(entry) = next {
                let key = entry.key();
                let tantivy_key = key.tantivy().to_owned();
                let semantic_key = key.semantic().to_owned();
                let commit_hash = key.commit_hash.to_owned();
                let file_path = key.file_path();
                let file_content_hash = key.file_content_hash();
                let repo_str = self.reporef.to_string();
                sqlx::query!(
                    "INSERT INTO file_cache \
                 (repo_ref, tantivy_cache_key, semantic_search_hash, commit_hash, file_path, file_content_hash) \
                         VALUES (?, ?, ?, ?, ?, ?)",
                    repo_str,
                    tantivy_key,
                    semantic_key,
                    commit_hash,
                    file_path,
                    file_content_hash,
                )
                .execute(&mut *tx)
                .await?;

                next = entry.next();
            }

            tx.commit().await?;
        }

        // batch-delete points from qdrant index
        if !qdrant_stale.is_empty() {
            if let Some(semantic) = self.semantic {
                let semantic = semantic.clone();
                let reporef = self.reporef.to_string();
                tokio::spawn(async move {
                    semantic
                        .delete_points_for_hash(reporef.as_str(), qdrant_stale.into_iter())
                        .await;
                });
            }
        }

        // make sure we generate & commit all remaining embeddings
        self.batched_embed_or_flush_queue(true).await?;

        Ok(())
    }

    async fn batched_embed_or_flush_queue(&self, flush: bool) -> anyhow::Result<()> {
        let Some(semantic) = self.semantic else {
            debug!(?self.reporef, "no semantic search client configured");
            return Ok(());
        };

        let new_points = self.embed_queued_points(semantic, flush).await?;

        if !new_points.is_empty() {
            if let Err(err) = semantic
                .qdrant_client()
                .upsert_points(&semantic.collection_name(), new_points, None)
                .await
            {
                error!(?err, "failed to write new points into qdrant");
            }
        }
        Ok(())
    }

    /// Empty the queue in batches, and generate embeddings using the
    /// configured embedder
    async fn embed_queued_points(
        &self,
        semantic: &SemanticClient,
        flush: bool,
    ) -> Result<Vec<PointStruct>, anyhow::Error> {
        let batch_size = semantic.get_embedding_queue_size();
        let log = &self.embed_queue;
        debug!(?batch_size, ?self.reporef, "embedding queue");
        let mut output = vec![];

        loop {
            // if we're not currently flushing the log, only process full batches
            if log.is_empty() || (log.len() < batch_size && !flush) {
                return Ok(output);
            }

            let mut batch = vec![];

            // fill this batch with embeddings
            while let Some(embedding) = log.pop() {
                batch.push(embedding);

                if batch.len() == batch_size {
                    break;
                }
            }

            let (elapsed, res) = {
                let time = Instant::now();
                let res = semantic
                    .get_embedder()
                    .batch_embed(batch.iter().map(|c| c.data.as_ref()).collect::<Vec<_>>())
                    .await;

                (time.elapsed(), res)
            };

            match res {
                Ok(res) => {
                    trace!(?elapsed, size = batch.len(), "batch embedding successful");
                    output.extend(
                        res.into_iter()
                            .zip(batch)
                            .map(|(embedding, src)| PointStruct {
                                id: Some(PointId::from(src.id)),
                                vectors: Some(embedding.into()),
                                payload: src.payload,
                            }),
                    )
                }
                Err(err) => {
                    error!(
                        ?err,
                        ?elapsed,
                        size = batch.len(),
                        "remote batch embeddings failed"
                    )
                }
            }
        }
    }

    /// Process the next chunk from the embedding queue if the batch size is met.
    pub fn process_embedding_queue(&self) -> anyhow::Result<()> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { self.batched_embed_or_flush_queue(false).await })
        })
    }

    pub async fn process_chunks(
        &self,
        cache_keys: &CacheKeys,
        repo_name: &str,
        repo_ref: &str,
        relative_path: &str,
        buffer: &str,
        lang_str: &str,
        branches: &[String],
        file_extension: Option<&str>,
    ) -> anyhow::Result<()> {
        let chunk_cache = self.chunks_for_file(cache_keys, relative_path).await;
        let semantic = self.semantic.expect("uninitialized semantic db");

        semantic
            .chunks_for_buffer(
                cache_keys.semantic().into(),
                repo_name,
                repo_ref,
                relative_path,
                buffer,
                lang_str,
                branches,
                file_extension,
            )
            .for_each(|(data, payload)| {
                let cached = chunk_cache.update_or_embed(&data, payload);
                if let Err(err) = cached {
                    warn!(?err, %repo_name, %relative_path, "embedding failed");
                }
            });

        match chunk_cache.commit().await {
            Ok((new, updated, deleted)) => {
                info!(
                    repo_name,
                    relative_path, new, updated, deleted, "Successful commit"
                )
            }
            Err(err) => {
                warn!(repo_name, relative_path, ?err, "Failed to upsert vectors")
            }
        }
        Ok(())
    }

    async fn chunks_for_file(
        &'a self,
        key: &'a CacheKeys,
        relative_path: &'a str,
    ) -> ChunkCache<'a> {
        ChunkCache::for_file(
            self.sqlite,
            self.semantic
                .expect("we shouldn't get here without semantic db configured"),
            self.reporef,
            &self.embed_queue,
            key.semantic(),
            relative_path,
        )
        .await
    }

    pub async fn delete(&self) -> Result<()> {
        // For deleting, we have to do the following:
        // - 1. clenup the current file cache and the chunk cache
        // - 2. cleanup the adrant data which we have as its not required
        let mut tx = self.sqlite.begin().await?;
        // First we clean-up our cache here by calling delete files
        self.delete_files(&mut tx).await?;
        // Next delete the chunk cache
        self.delete_chunks(&mut tx).await?;
        Ok(())
    }
}

/// Manage both the SQL cache and the underlying qdrant database to
/// ensure consistency.
///
/// Operates on a single file's level.
/// This is keeping the cache of the chunk using some parameters from the chunk
/// and also using some information about the semantic layer.
/// We are also taking into consideration the branches which are present (weird?)
/// and using that as part of the caching algorithm and keeping the state using that
/// we try to commit them in that order and use that as our unique key.
///
/// What we want to maintain over here is the cache consistency with the qdrant
/// layer where we store the vectors.
///
/// For that, we want to map this to the semantic_key of the file and use that to
/// populate the entries and check what's going to happen
pub struct ChunkCache<'a> {
    sql: &'a SqlDb,
    semantic: &'a SemanticClient,
    reporef: &'a RepoRef,
    // this is the unique cache key for the file which can be used to identify
    // the file and it only belongs to a single file
    file_cache_key: &'a str,
    file_path: &'a str,
    // We are keeping the hash of the chunk and the value of the branch hash I think?
    // Yeah I think its branch hash and we mark it as stale if required.
    // This should go from the chunk hash to the commit id maybe?
    cache_to_commit_hash: scc::HashMap<String, FreshValue<String>>,
    // key here is the list of branches and the hash for it, and we keep the list of chunks
    // and their hashes here
    // This maps keeps track of the commit hash and the hash of the chunks (or their ids)
    // as these need to be updated, and not inserted a new (these chunks were already present in
    // the repo but they were part of a different commit)
    update_to_commit_hash: scc::HashMap<String, Vec<String>>,
    // for the new sql, here we are keeping track of the rows we have to add
    // so here we are going for a mapping from
    new_sql: RwLock<Vec<(String, String)>>,
    embed_queue: &'a EmbedQueue,
}

impl<'a> ChunkCache<'a> {
    async fn for_file(
        sql: &'a SqlDb,
        semantic: &'a SemanticClient,
        reporef: &'a RepoRef,
        embed_log: &'a EmbedQueue,
        file_cache_key: &'a str,
        file_path: &'a str,
    ) -> ChunkCache<'a> {
        // First we need to read from the table what all caches with the file_cache_key
        // already exist and mark those as stable, using the query below
        let rows = sqlx::query! {
            "select chunk_hash, commit_hash from chunk_cache where file_cache_key = ?", file_cache_key
        }
        .fetch_all(sql.as_ref())
        .await;

        let cache = scc::HashMap::<String, FreshValue<_>>::default();
        for row in rows.into_iter().flatten() {
            _ = cache.insert(row.chunk_hash, FreshValue::stale(row.commit_hash));
        }

        Self {
            sql,
            semantic,
            reporef,
            file_cache_key,
            file_path,
            cache_to_commit_hash: cache,
            embed_queue: embed_log,
            update_to_commit_hash: Default::default(),
            new_sql: Default::default(),
        }
    }

    /// Update a cache entry with the details from `payload`, or create a new embedding.
    ///
    /// New insertions are queued, and stored on the repository-level
    /// `FileCache` instance that created this.
    fn update_or_embed(&self, data: &'a str, payload: Payload) -> anyhow::Result<()> {
        let id = self.derive_chunk_uuid(data, &payload);
        let commit_hash = payload.commit_hash.to_owned();

        match self.cache_to_commit_hash.entry(id) {
            scc::hash_map::Entry::Occupied(mut existing) => {
                // The existing commit hash does not match the new commit hash,
                // so we need to update things
                if existing.get().value != commit_hash {
                    // We also want to add the chunk's id and link it with the
                    // commit hash, so we can keep tabs on it
                    self.update_to_commit_hash
                        .entry(commit_hash.to_owned())
                        .or_insert_with(Vec::new)
                        .get_mut()
                        // existing.key here is the hash for the file we want to
                        // embed
                        .push(existing.key().to_owned());
                }
                // For the current cache_to_commit_hash we want to set the commit
                // has to the latest one which we have
                *existing.get_mut() = commit_hash.into();
            }
            scc::hash_map::Entry::Vacant(vacant) => {
                self.new_sql
                    .write()
                    .unwrap()
                    .push((vacant.key().to_owned(), commit_hash.clone()));

                self.embed_queue.push(EmbedChunk {
                    id: vacant.key().clone(),
                    data: data.into(),
                    payload: payload.into_qdrant(),
                });

                vacant.insert_entry(commit_hash.into());
            }
        }

        Ok(())
    }

    /// Commit both qdrant and cache changes to the respective databases.
    ///
    /// The SQLite operations mirror qdrant changes 1:1, so any
    /// discrepancy between the 2 should be minimized.
    ///
    /// In addition, the SQLite cache is committed only AFTER all
    /// qdrant writes have successfully completed, meaning they're in
    /// qdrant's pipelines.
    ///
    /// Since qdrant changes are pipelined on their end, data written
    /// here is not necessarily available for querying when the
    /// commit's completed.
    pub async fn commit(self) -> anyhow::Result<(usize, usize, usize)> {
        let mut tx = self.sql.begin().await?;

        let update_size = self.commit_commit_hash_updates(&mut tx).await?;
        let delete_size = self.commit_deletes(&mut tx).await?;
        let new_size = self.commit_inserts(&mut tx).await?;

        tx.commit().await?;

        Ok((new_size, update_size, delete_size))
    }

    /// Insert new additions to sqlite
    ///
    /// Note this step will update the cache before changes are
    /// actually written to qdrant in batches.
    ///
    /// All qdrant operations are executed in batches through a call
    /// to [`FileCache::commit_embed_log`].
    async fn commit_inserts(
        &self,
        tx: &mut sqlx::Transaction<'_, Sqlite>,
    ) -> Result<usize, anyhow::Error> {
        let new_sql = std::mem::take(&mut *self.new_sql.write().unwrap());
        let new_size = new_sql.len();
        let _file_path = self.file_path;

        let repo_str = self.reporef.to_string();
        for (chunk_hash, commit_hash) in new_sql {
            sqlx::query! {
                "insert into chunk_cache (chunk_hash, commit_hash, file_cache_key, repo_ref, file_path) \
                VALUES (?, ?, ?, ?, ?)",
                chunk_hash, commit_hash, self.file_cache_key, repo_str, self.file_path,
            }.execute(&mut **tx)
            .await?;
        }

        Ok(new_size)
    }

    /// Delete points that have expired in the latest index.
    async fn commit_deletes(
        &self,
        tx: &mut sqlx::Transaction<'_, Sqlite>,
    ) -> Result<usize, anyhow::Error> {
        let mut to_delete = vec![];
        self.cache_to_commit_hash
            .scan_async(|id, p| {
                if !p.fresh {
                    to_delete.push(id.to_owned());
                }
            })
            .await;

        let delete_size = to_delete.len();
        for chunk_hash in to_delete.iter() {
            sqlx::query! {
                "DELETE FROM chunk_cache \
                 WHERE chunk_hash = ? AND file_cache_key = ?",
                 chunk_hash,
                self.file_cache_key
            }
            .execute(&mut **tx)
            .await?;
        }

        if !to_delete.is_empty() {
            self.semantic
                .qdrant_client()
                .delete_points(
                    self.semantic.collection_name(),
                    &to_delete
                        .into_iter()
                        .map(PointId::from)
                        .collect::<Vec<_>>()
                        .into(),
                    None,
                )
                .await?;
        }
        Ok(delete_size)
    }

    /// Update points where the commit hash is which they're
    /// searchable has changed.
    async fn commit_commit_hash_updates(
        &self,
        tx: &mut sqlx::Transaction<'_, Sqlite>,
    ) -> Result<usize, anyhow::Error> {
        let mut update_size = 0;
        let mut qdrant_updates = vec![];

        let mut next = self.update_to_commit_hash.first_occupied_entry();
        while let Some(entry) = next {
            let commit_hash = entry.key();
            let points = entry.get();
            update_size += points.len();

            for chunk_hash_id in entry.get() {
                sqlx::query! {
                    "UPDATE chunk_cache SET commit_hash = ? \
                     WHERE chunk_hash = ?",
                     commit_hash,
                     chunk_hash_id
                }
                .execute(&mut **tx)
                .await?;
            }

            let id = points
                .iter()
                .cloned()
                .map(PointId::from)
                .collect::<Vec<_>>()
                .into();

            let payload = qdrant_client::client::Payload::new_from_hashmap(
                [("commit_hash".to_string(), commit_hash.to_owned().into())].into(),
            );

            qdrant_updates.push(async move {
                self.semantic
                    .qdrant_client()
                    .set_payload(self.semantic.collection_name(), &id, payload, None)
                    .await
            });
            next = entry.next();
        }

        // Note these actions aren't actually parallel, merely
        // concurrent.
        //
        // This should be fine since the number of updates would be
        // reasonably small.
        futures::future::join_all(qdrant_updates.into_iter())
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;

        Ok(update_size)
    }

    /// Generate a content hash from the embedding data, and pin it to
    /// the containing file's content id.
    fn derive_chunk_uuid(&self, data: &str, payload: &Payload) -> String {
        let id = {
            let mut bytes = [0; 16];
            let mut hasher = blake3::Hasher::new();
            hasher.update(&payload.start_line.to_le_bytes());
            hasher.update(&payload.end_line.to_le_bytes());
            hasher.update(self.file_cache_key.as_bytes());
            hasher.update(data.as_ref());
            bytes.copy_from_slice(&hasher.finalize().as_bytes()[16..32]);
            Uuid::from_bytes(bytes).to_string()
        };
        id
    }
}

/// Here we are going to create snapshots and cache for the code snippets which
/// we will also use as a lexical search input

pub struct CodeSnippetCacheSnapshot<'a> {
    snapshot: Arc<scc::HashMap<CodeSnippetCacheKeys, FreshValue<()>>>,
    parent: &'a CodeSnippetCache<'a>,
}

impl<'a> Deref for CodeSnippetCacheSnapshot<'a> {
    type Target = scc::HashMap<CodeSnippetCacheKeys, FreshValue<()>>;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl<'a> CodeSnippetCacheSnapshot<'a> {
    pub fn parent(&self) -> &'a CodeSnippetCache<'a> {
        self.parent
    }

    pub fn is_fresh(&self, keys: &CodeSnippetCacheKeys) -> bool {
        match self.snapshot.entry(keys.clone()) {
            Entry::Occupied(mut val) => {
                val.get_mut().fresh = true;
                true
            }
            Entry::Vacant(val) => {
                debug!(
                    ?keys,
                    "inserting fresh value in the code snippet snapshot cache"
                );
                _ = val.insert_entry(FreshValue::fresh_default());
                false
            }
        }
    }
}

pub struct CodeSnippetCache<'a> {
    sqlite: &'a SqlDb,
    reporef: &'a RepoRef,
}

impl<'a> CodeSnippetCache<'a> {
    pub fn for_repo(sqlite: &'a SqlDb, reporef: &'a RepoRef) -> Self {
        Self { sqlite, reporef }
    }

    pub(crate) async fn retrieve(&'a self) -> CodeSnippetCacheSnapshot<'a> {
        let repo_str = self.reporef.to_string();
        let rows = sqlx::query! {
            "SELECT tantivy_cache_key, content_hash, file_path, commit_hash FROM code_snippet_cache \
             WHERE repo_ref = ?",
             repo_str,
        }
        .fetch_all(self.sqlite.as_ref())
        .await;

        let output = scc::HashMap::default();
        for row in rows.into_iter().flatten() {
            let tantivy_cache_key = row.tantivy_cache_key;
            _ = output.insert(
                CodeSnippetCacheKeys {
                    tantivy: tantivy_cache_key,
                    commit_hash: row.commit_hash,
                    file_path: row.file_path,
                    content_hash: row.content_hash,
                },
                FreshValue::stale(()),
            );
        }

        CodeSnippetCacheSnapshot {
            snapshot: output.into(),
            parent: self,
        }
    }

    async fn delete_files(&self, tx: &mut sqlx::Transaction<'_, Sqlite>) -> anyhow::Result<()> {
        let repo_str = self.reporef.to_string();
        sqlx::query! {
            "DELETE FROM code_snippet_cache \
                 WHERE repo_ref = ?",
            repo_str
        }
        .execute(&mut **tx)
        .await?;

        Ok(())
    }

    pub(crate) async fn synchronize(
        &'a self,
        cache: CodeSnippetCacheSnapshot<'a>,
        delete_tantivy: impl Fn(&str),
    ) -> anyhow::Result<()> {
        debug!(?self.reporef, "synchronizing code snippet cache");
        let mut tx = self.sqlite.begin().await?;

        self.delete_files(&mut tx).await?;

        // Now we try to get the files which are no longer tracked by git index
        // and remove them from tantivy
        {
            cache.retain(|k, v| {
                if !v.fresh {
                    delete_tantivy(k.tantivy());
                }
                v.fresh
            });
        }

        // generate a transaction to push the remaining entries
        {
            let mut next = cache.first_occupied_entry_async().await;
            while let Some(entry) = next {
                let key = entry.key();
                let tantivy_key = key.tantivy.to_owned();
                let commit_hash = key.commit_hash.to_owned();
                let file_path = key.file_path.to_owned();
                let content_hash = key.content_hash.to_owned();
                let repo_str = self.reporef.to_string();
                sqlx::query!(
                    "INSERT INTO code_snippet_cache \
                    (repo_ref, tantivy_cache_key, commit_hash, file_path, content_hash) \
                         VALUES (?, ?, ?, ?, ?)",
                    repo_str,
                    tantivy_key,
                    commit_hash,
                    file_path,
                    content_hash,
                )
                .execute(&mut *tx)
                .await?;

                next = entry.next();
            }

            tx.commit().await?;
        }
        Ok(())
    }

    pub async fn delete(&self) -> Result<()> {
        // for deleting we have to do the following:
        // - 1. cleanup the code snippet cache
        let mut tx = self.sqlite.begin().await?;
        self.delete_files(&mut tx).await?;
        Ok(())
    }
}
