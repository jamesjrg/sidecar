use std::{
    path::{Path, PathBuf},
    sync::{atomic::AtomicU64, atomic::Ordering, Arc},
};

use anyhow::{bail, Result};
use async_trait::async_trait;
use tantivy::{doc, schema::Schema, IndexWriter, Term};
use tracing::{debug, info, trace, warn};

use crate::{
    application::background::SyncPipes,
    chunking::languages::TSLanguageParsing,
    repo::{
        filesystem::{BranchFilter, FileWalker, GitWalker},
        iterator::{FileSource, RepoDirectoryEntry, RepositoryFile},
        types::{RepoMetadata, RepoRef, Repository},
    },
    state::schema_version::get_schema_version,
};

use super::{
    caching::{CodeSnippetCache, CodeSnippetCacheKeys, CodeSnippetCacheSnapshot},
    indexer::{get_text_field, get_u64_field, Indexable},
    schema::CodeSnippet,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CodeSnippetDocument {
    pub relative_path: String,
    pub repo_name: String,
    pub repo_ref: String,
    pub content: String,
    pub line_start: u64,
    pub line_end: u64,
    pub score: f32,
}

pub struct CodeSnippetReader;

impl CodeSnippetReader {
    pub fn read_document(schema: &CodeSnippet, doc: tantivy::Document) -> CodeSnippetDocument {
        let path = get_text_field(&doc, schema.relative_path);
        let repo_ref = get_text_field(&doc, schema.repo_ref);
        let content = get_text_field(&doc, schema.content);
        let line_start = get_u64_field(&doc, schema.start_line);
        let line_end = get_u64_field(&doc, schema.end_line);

        CodeSnippetDocument {
            relative_path: path,
            repo_name: "".to_owned(),
            repo_ref,
            content,
            line_start,
            line_end,
            score: 0.0,
        }
    }

    pub fn read_document_with_score(
        schema: &CodeSnippet,
        doc: tantivy::Document,
        score: f32,
    ) -> CodeSnippetDocument {
        let mut code_snippet_doc = Self::read_document(schema, doc);
        code_snippet_doc.score = score;
        code_snippet_doc
    }
}

pub struct Workload<'a> {
    cache: &'a CodeSnippetCacheSnapshot<'a>,
    repo_disk_path: &'a Path,
    repo_name: &'a str,
    repo_metadata: &'a RepoMetadata,
    repo_ref: String,
    relative_path: PathBuf,
    normalized_path: PathBuf,
    commit_hash: String,
}

impl<'a> Workload<'a> {
    pub fn new(
        cache: &'a CodeSnippetCacheSnapshot<'a>,
        repo_disk_path: &'a Path,
        repo_name: &'a str,
        repo_metadata: &'a RepoMetadata,
        repo_ref: String,
        relative_path: PathBuf,
        normalized_path: PathBuf,
        commit_hash: String,
    ) -> Self {
        Self {
            cache,
            repo_disk_path,
            repo_name,
            repo_metadata,
            repo_ref,
            relative_path,
            normalized_path,
            commit_hash,
        }
    }
}

impl<'a> Workload<'a> {
    // These cache keys are important as they also encode information about the
    // the file path in the cache, which implies that for each file we will have
    // a unique cache key.
    fn cache_keys(&self, dir_entry: &RepoDirectoryEntry) -> CodeSnippetCacheKeys {
        let semantic_hash = {
            let mut hash = blake3::Hasher::new();
            hash.update(get_schema_version().as_bytes());
            hash.update(self.relative_path.to_string_lossy().as_ref().as_ref());
            hash.update(self.repo_ref.as_bytes());
            hash.update(dir_entry.buffer().unwrap_or_default().as_bytes());
            hash.finalize().to_hex().to_string()
        };

        let tantivy_hash = {
            let mut hash = blake3::Hasher::new();
            hash.update(semantic_hash.as_ref());
            hash.finalize().to_hex().to_string()
        };

        // We get a unique hash for the file content
        let file_content_hash = match dir_entry.buffer() {
            Some(content) => {
                let mut hash = blake3::Hasher::new();
                hash.update(content.as_bytes())
                    .finalize()
                    .to_hex()
                    .to_string()
            }
            None => "no_content_hash".to_owned(),
        };

        let file_path = dir_entry.path();

        debug!(
            ?tantivy_hash,
            ?semantic_hash,
            ?file_content_hash,
            ?file_path,
            "cache keys"
        );

        CodeSnippetCacheKeys::new(
            tantivy_hash,
            self.commit_hash.to_owned(),
            self.normalized_path
                .to_str()
                .map_or("mangled_path".to_owned(), |path| path.to_owned()),
            file_content_hash,
        )
    }
}

#[async_trait]
impl Indexable for CodeSnippet {
    async fn index_repository(
        &self,
        reporef: &RepoRef,
        repo: &Repository,
        repo_metadata: &RepoMetadata,
        writer: &IndexWriter,
        pipes: &SyncPipes,
    ) -> Result<()> {
        let code_snippet_cache = Arc::new(CodeSnippetCache::for_repo(&self.sql, reporef));
        let cache = code_snippet_cache.retrieve().await;
        let repo_name = reporef.indexed_name();
        let processed = &AtomicU64::new(0);

        let file_worker = |count: usize| {
            let cache = &cache;
            move |dir_entry: RepoDirectoryEntry| {
                let completed = processed.fetch_add(1, Ordering::Relaxed);

                let entry_disk_path = dir_entry.path().unwrap().to_owned();
                debug!(
                    entry_disk_path,
                    "processing entry for indexing code snippet"
                );
                let relative_path = {
                    let entry_srcpath = PathBuf::from(&entry_disk_path);
                    entry_srcpath
                        .strip_prefix(&repo.disk_path)
                        .map(ToOwned::to_owned)
                        .unwrap_or(entry_srcpath)
                };
                debug!(?relative_path, "relative path for indexing code snippets");
                let normalized_path = repo.disk_path.join(&relative_path);

                let workload = Workload::new(
                    cache,
                    &repo.disk_path,
                    &repo_name,
                    repo_metadata,
                    reporef.to_string(),
                    relative_path,
                    normalized_path,
                    repo_metadata.commit_hash.clone(),
                );

                trace!(entry_disk_path, "queueing entry for code snippet indexing");
                if let Err(err) = self.worker(dir_entry, workload, writer) {
                    warn!(%err, entry_disk_path, "indexing failed code snippet; finished");
                }
                debug!(entry_disk_path, "finished indexing code snippet");
                pipes.index_percent(((completed as f32 / count as f32) * 100f32) as u8);
            }
        };

        let start = std::time::Instant::now();

        if repo_metadata.last_commit_unix_secs.is_some() {
            let walker = GitWalker::open_repository(reporef, &repo.disk_path, BranchFilter::Head)?;
            let count = walker.len();
            walker.for_each(pipes, file_worker(count));
        } else {
            let walker = FileWalker::index_directory(&repo.disk_path);
            let count = walker.len();
            walker.for_each(pipes, file_worker(count));
        }

        if pipes.is_cancelled() {
            bail!("cancelled code snippet indexing");
        }

        info!(?repo.disk_path, "code snippet indexing finished, took {:?}", start.elapsed());

        code_snippet_cache
            .synchronize(cache, |key| {
                writer.delete_term(Term::from_field_text(self.unique_hash, key));
            })
            .await?;

        pipes.index_percent(100);
        Ok(())
    }

    fn delete_by_repo(&self, writer: &IndexWriter, repo: &Repository) {
        writer.delete_term(Term::from_field_text(
            self.repo_disk_path,
            &repo.disk_path.to_string_lossy(),
        ));
    }

    /// Return the tantivy `Schema` of the current index
    fn schema(&self) -> Schema {
        self.schema.clone()
    }
}

impl CodeSnippet {
    fn worker(
        &self,
        dir_entry: RepoDirectoryEntry,
        workload: Workload<'_>,
        writer: &IndexWriter,
    ) -> Result<()> {
        let cache_keys = workload.cache_keys(&dir_entry);
        let last_commit = workload
            .repo_metadata
            .last_commit_unix_secs
            .unwrap_or_default();
        trace!("processing file for code snippets");
        match dir_entry {
            _ if workload.cache.is_fresh(&cache_keys) => {
                info!(?cache_keys, "code snippet cache is fresh");
                return Ok(());
            }
            RepoDirectoryEntry::Dir(dir) => {
                debug!("not indexing snippets from the directory {:?}", dir);
            }
            RepoDirectoryEntry::File(file) => {
                // Here we get back a list of documents all of which we have to write
                // to the index
                let documents = file.build_documents(
                    self,
                    &workload,
                    &cache_keys,
                    last_commit,
                    self.language_parsing.clone(),
                    workload.cache.parent(),
                );
                // add all the generated code snippets to the index
                documents.into_iter().for_each(|document| {
                    // TODO(codestory): This kind of expect is bad, but we need
                    // it for now while we are testing
                    let _ = writer
                        .add_document(document)
                        .expect("writer adding code snippet should always work");
                });
            }
            RepoDirectoryEntry::Other => {
                bail!("found an entry which is neither a file or a document");
            }
        }
        Ok(())
    }
}

impl RepositoryFile {
    // Here we will return multiple documents all of which are the code snippets
    // we are interested in
    fn build_documents(
        mut self,
        schema: &CodeSnippet,
        workload: &Workload<'_>,
        cache_keys: &CodeSnippetCacheKeys,
        last_commit: i64,
        language_parsing: Arc<TSLanguageParsing>,
        file_cache: &CodeSnippetCache,
    ) -> Vec<tantivy::schema::Document> {
        // Now we need to parse the content of the file and then get the documents
        // which will be generated

        let Workload {
            relative_path,
            repo_name,
            repo_disk_path,
            repo_ref,
            ..
        } = workload;

        let relative_path_str = format!("{}", relative_path.to_string_lossy());
        #[cfg(windows)]
        let relative_path_str = relative_path_str.replace('\\', "/");

        let file_extension = self
            .pathbuf
            .extension()
            .map(|extension| extension.to_str())
            .flatten();

        let chunks = language_parsing
            .chunk_file(
                &relative_path.to_string_lossy().to_string(),
                &self.buffer,
                file_extension,
            )
            .into_iter()
            .filter(|span| span.data.is_some())
            .collect::<Vec<_>>();

        // Now that we have the chunks, we can prepare the documents here we have
        // to match the schema of the document carefully with what we expect
        chunks
            .into_iter()
            .map(|chunk| {
                let data = chunk.data.unwrap_or_default();
                doc!(
                    schema.raw_content => data.to_owned().as_bytes(),
                    schema.raw_repo_name => repo_name.as_bytes(),
                    schema.raw_relative_path => relative_path_str.as_bytes(),
                    schema.unique_hash => cache_keys.tantivy(),
                    schema.repo_disk_path => repo_disk_path.to_string_lossy().as_ref(),
                    schema.relative_path => relative_path_str.to_owned(),
                    schema.repo_ref => repo_ref.as_str(),
                    schema.repo_name => *repo_name,
                    schema.last_commit_unix_seconds => last_commit,
                    schema.content => data,
                    schema.start_line => chunk.start as u64,
                    schema.end_line => chunk.end as u64,
                )
            })
            .collect::<Vec<_>>()
    }
}
