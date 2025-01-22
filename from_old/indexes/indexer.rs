use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use std::{fs, path::Path};

use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use smallvec::SmallVec;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Query as TantivyQuery, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::Term;
use tantivy::{
    collector::{Collector, MultiFruit},
    schema::Schema,
    tokenizer::NgramTokenizer,
    DocAddress, Document, IndexReader, IndexWriter, Score,
};
use tokio::sync::RwLock;
use tracing::debug;

use crate::application::background::SyncHandle;
use crate::application::config::configuration::Configuration;
use crate::chunking::languages::TSLanguageParsing;
use crate::db::sqlite::SqlDb;
use crate::indexes::code_snippet::CodeSnippetReader;
use crate::repo::state::{RepoError, RepositoryPool};
use crate::semantic_search::client::SemanticClient;
use crate::{
    application::background::SyncPipes,
    repo::types::{RepoMetadata, RepoRef, Repository},
};

use super::caching::FileCache;
use super::code_snippet::CodeSnippetDocument;
use super::query::{case_permutations, trigrams, Query};
use super::schema::{CodeSnippet, CodeSnippetTokenizer, File};

/// A wrapper around `tantivy::IndexReader`.
///
/// This contains the schema, and also additional fields used to enable re-indexing.
pub struct Indexer<T> {
    pub source: T,
    pub index: tantivy::Index,
    pub reader: RwLock<IndexReader>,
    pub reindex_buffer_size: usize,
    pub reindex_threads: usize,
}

impl Indexer<CodeSnippet> {
    pub async fn lexical_search(
        &self,
        repo_ref: &RepoRef,
        query_str: &str,
        limit: usize,
    ) -> Result<Vec<CodeSnippetDocument>> {
        let reader = self.reader.read().await;
        let searcher = reader.searcher();
        let collector = TopDocs::with_limit(5 * limit);
        let code_snippet_source = &self.source;
        let query_parser = QueryParser::for_index(
            searcher.index(),
            vec![code_snippet_source.repo_ref, code_snippet_source.content],
        );
        let tokens = CodeSnippetTokenizer::tokenize_call(query_str);
        let mut query_string = tokens
            .iter()
            .map(|token| format!(r#"content:"{}""#, token.text))
            .collect::<Vec<_>>()
            .join(" OR ");
        let repo_ref_str = repo_ref.to_string();
        query_string = query_string + " AND " + &format!(r#"repo_ref:{repo_ref_str}"#);
        let query = query_parser.parse_query(query_string.as_str())?;
        let top_docs = searcher.search(&query, &collector)?;
        let mut documents_with_score = top_docs
            .into_iter()
            .map(|doc| {
                let retrieved_doc = searcher
                    .doc(doc.1)
                    .expect("failed to get document by address");
                CodeSnippetReader::read_document_with_score(&self.source, retrieved_doc, doc.0)
            })
            .collect::<Vec<_>>();
        documents_with_score.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        if documents_with_score.is_empty() {
            return Ok(Default::default());
        }
        // Here we will also reduce the score to be in a range from 0.5 -> 1 for the
        // lexical search
        let max_score = documents_with_score
            .iter()
            .map(|lexical_search_snippet| lexical_search_snippet.score)
            .max_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap();
        let mut min_score = documents_with_score
            .iter()
            .map(|lexical_search_snippet| lexical_search_snippet.score)
            .min_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap();
        if min_score == max_score {
            min_score = 0.0;
        }
        // Now relativize the scores in this range
        documents_with_score
            .iter_mut()
            .for_each(|lexical_search_snippet| {
                lexical_search_snippet.score =
                    (lexical_search_snippet.score - min_score) / (max_score - min_score) * 0.5
                        + 0.5;
            });

        documents_with_score.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        Ok(documents_with_score.into_iter().take(limit).collect())
    }
}

impl Indexer<File> {
    /// Search this index for paths fuzzily matching a given string.
    ///
    /// For example, the string `Cargo` can return documents whose path is `foo/Cargo.toml`,
    /// or `bar/Cargo.lock`. Constructs regexes that permit an edit-distance of 2.
    ///
    /// If the regex filter fails to build, an empty list is returned.
    pub async fn fuzzy_path_match(
        &self,
        repo_ref: &RepoRef,
        query_str: &str,
        limit: usize,
    ) -> impl Iterator<Item = FileDocument> + '_ {
        // lifted from query::compiler
        let reader = self.reader.read().await;
        let searcher = reader.searcher();
        let collector = TopDocs::with_limit(5 * limit); // TODO: tune this
        let file_source = &self.source;

        // hits is a mapping between a document address and the number of trigrams in it that
        // matched the query
        let repo_ref_term = Term::from_field_text(self.source.repo_ref, &repo_ref.to_string());
        let mut hits = trigrams(query_str)
            .flat_map(|s| case_permutations(s.as_str()))
            .map(|token| Term::from_field_text(self.source.relative_path, token.as_str()))
            .map(|term| {
                let query: Vec<Box<dyn TantivyQuery>> = vec![
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                    Box::new(TermQuery::new(
                        repo_ref_term.clone(),
                        IndexRecordOption::Basic,
                    )),
                ];

                BooleanQuery::intersection(query)
            })
            .flat_map(|query| {
                searcher
                    .search(&query, &collector)
                    .expect("failed to search index")
                    .into_iter()
                    .map(move |(_, addr)| addr)
            })
            .fold(HashMap::new(), |mut map: HashMap<_, usize>, hit| {
                *map.entry(hit).or_insert(0) += 1;
                map
            })
            .into_iter()
            .map(move |(addr, count)| {
                let retrieved_doc = searcher
                    .doc(addr)
                    .expect("failed to get document by address");
                let doc = FileReader.read_document(file_source, retrieved_doc);
                (doc, count)
            })
            .collect::<Vec<_>>();
        hits.sort_by(|(this_doc, this_count), (other_doc, other_count)| {
            let order_count_desc = other_count.cmp(this_count);
            let order_path_asc = this_doc
                .relative_path
                .as_str()
                .cmp(other_doc.relative_path.as_str());

            order_count_desc.then(order_path_asc)
        });

        // order hits in
        // - decsending order of number of matched trigrams
        // - alphabetical order of relative paths to break ties
        //
        //
        // for a list of hits like so:
        //
        //     apple.rs 2
        //     ball.rs  3
        //     cat.rs   2
        //
        // the ordering produced is:
        //
        //     ball.rs  3  -- highest number of hits
        //     apple.rs 2  -- same numeber of hits, but alphabetically preceeds cat.rs
        //     cat.rs   2
        //
        hits.sort_by(|(this_doc, this_count), (other_doc, other_count)| {
            let order_count_desc = other_count.cmp(this_count);
            let order_path_asc = this_doc
                .relative_path
                .as_str()
                .cmp(other_doc.relative_path.as_str());

            order_count_desc.then(order_path_asc)
        });

        let regex_filter = build_fuzzy_regex_filter(query_str);

        // if the regex filter fails to build for some reason, the filter defaults to returning
        // false and zero results are produced
        hits.into_iter()
            .map(|(doc, _)| doc)
            .filter(move |doc| {
                regex_filter
                    .as_ref()
                    .map(|f| f.is_match(&doc.relative_path))
                    .unwrap_or_default()
            })
            .filter(|doc| !doc.relative_path.ends_with('/')) // omit directories
            .take(limit)
    }

    pub async fn get_by_path(
        &self,
        path: &str,
        reporef: &RepoRef,
    ) -> anyhow::Result<Option<FileDocument>> {
        let reader = self.reader.read().await;
        let searcher = reader.searcher();
        let file_index = searcher.index();
        // get the tantivy query here and search for it
        let query_parser = QueryParser::for_index(
            file_index,
            vec![self.source.repo_ref, self.source.relative_path],
        );

        let query_string = format!(r#"repo_ref:"{reporef}" AND relative_path:"{path}""#);
        let query = query_parser
            .parse_query(&query_string)
            .expect("failed to parse tantivy query");
        // Now we use the query along with the searcher and get back the results
        let container = TopDocs::with_limit(1);
        let results = searcher
            .search(&query, &container)
            .expect("search_index to not fail");

        match results.as_slice() {
            [] => Ok(None),
            [(_, doc_address)] => {
                let doc = searcher.doc(*doc_address).expect("doc to exist");
                let file_doc = FileReader.read_document(&self.source, doc);
                Ok(Some(file_doc))
            }
            _ => Err(anyhow::anyhow!("too many results for path: {}", path)),
        }
    }
}

fn build_fuzzy_regex_filter(query_str: &str) -> Option<regex::RegexSet> {
    fn additions(s: &str, i: usize, j: usize) -> String {
        if i > j {
            additions(s, j, i)
        } else {
            let mut s = s.to_owned();
            s.insert_str(j, ".?");
            s.insert_str(i, ".?");
            s
        }
    }

    fn replacements(s: &str, i: usize, j: usize) -> String {
        if i > j {
            replacements(s, j, i)
        } else {
            let mut s = s.to_owned();
            s.remove(j);
            s.insert_str(j, ".?");

            s.remove(i);
            s.insert_str(i, ".?");

            s
        }
    }

    fn one_of_each(s: &str, i: usize, j: usize) -> String {
        if i > j {
            one_of_each(s, j, i)
        } else {
            let mut s = s.to_owned();
            s.remove(j);
            s.insert_str(j, ".?");

            s.insert_str(i, ".?");
            s
        }
    }

    let all_regexes = (query_str.char_indices().map(|(idx, _)| idx))
        .flat_map(|i| (query_str.char_indices().map(|(idx, _)| idx)).map(move |j| (i, j)))
        .filter(|(i, j)| i <= j)
        .flat_map(|(i, j)| {
            let mut v = vec![];
            if j != query_str.len() {
                v.push(one_of_each(query_str, i, j));
                v.push(replacements(query_str, i, j));
            }
            v.push(additions(query_str, i, j));
            v
        });

    regex::RegexSetBuilder::new(all_regexes)
        // Increased from the default to account for long paths. At the time of writing,
        // the default was `10 * (1 << 20)`.
        .size_limit(10 * (1 << 25))
        .case_insensitive(true)
        .build()
        .ok()
}

#[async_trait]
pub trait Indexable: Send + Sync {
    /// This is where files are scanned and indexed.
    async fn index_repository(
        &self,
        reporef: &RepoRef,
        repo: &Repository,
        metadata: &RepoMetadata,
        writer: &IndexWriter,
        pipes: &SyncPipes,
    ) -> Result<()>;

    fn delete_by_repo(&self, writer: &IndexWriter, repo: &Repository);

    /// Return the tantivy `Schema` of the current index
    fn schema(&self) -> Schema;
}

// This is the inner most index write handle, so we can use this to index the
// inner caller and make that work
// the writer here is the tantivy writer which we need to use
pub struct IndexWriteHandle<'a> {
    source: &'a dyn Indexable,
    index: &'a tantivy::Index,
    reader: &'a RwLock<IndexReader>,
    writer: IndexWriter,
}

impl<'a> IndexWriteHandle<'a> {
    pub async fn refresh_reader(&self) -> Result<()> {
        *self.reader.write().await = self.index.reader()?;
        Ok(())
    }

    pub fn delete(&self, repo: &Repository) {
        self.source.delete_by_repo(&self.writer, repo)
    }

    pub async fn index(
        &self,
        reporef: &RepoRef,
        repo: &Repository,
        metadata: &RepoMetadata,
        progress: &SyncPipes,
    ) -> Result<()> {
        self.source
            .index_repository(reporef, repo, metadata, &self.writer, progress)
            .await
    }

    pub async fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        self.refresh_reader().await?;

        Ok(())
    }

    pub fn rollback(&mut self) -> Result<()> {
        self.writer.rollback()?;
        Ok(())
    }
}

#[async_trait]
pub trait DocumentRead: Send + Sync {
    type Schema;
    type Document;

    /// Return whether this reader can process this query.
    fn query_matches(&self, query: &Query<'_>) -> bool;

    /// Compile a set of parsed queries into a single `tantivy` query.
    fn compile<'a, I>(
        &self,
        schema: &Self::Schema,
        queries: I,
        index: &tantivy::Index,
    ) -> Result<Box<dyn tantivy::query::Query>>
    where
        I: Iterator<Item = &'a Query<'a>>;

    /// Read a tantivy document into the specified output type.
    fn read_document(&self, schema: &Self::Schema, doc: Document) -> Self::Document;
}

#[derive(Debug)]
pub struct FileDocument {
    pub relative_path: String,
    pub repo_name: String,
    pub repo_ref: String,
    pub content: String,
}

pub struct FileReader;

impl FileReader {
    fn read_document(&self, schema: &File, doc: tantivy::Document) -> FileDocument {
        let relative_path = get_text_field(&doc, schema.relative_path);
        let repo_ref = get_text_field(&doc, schema.repo_ref);
        let repo_name = get_text_field(&doc, schema.repo_name);
        let content = get_text_field(&doc, schema.content);

        FileDocument {
            relative_path,
            repo_name,
            repo_ref,
            content,
        }
    }
}

pub fn get_text_field(doc: &tantivy::Document, field: Field) -> String {
    doc.get_first(field).unwrap().as_text().unwrap().to_owned()
}

pub fn get_u64_field(doc: &tantivy::Document, field: Field) -> u64 {
    doc.get_first(field).unwrap().as_u64().unwrap().to_owned()
}

impl<T: Indexable> Indexer<T> {
    fn write_handle(&self) -> Result<IndexWriteHandle<'_>> {
        Ok(IndexWriteHandle {
            source: &self.source,
            index: &self.index,
            reader: &self.reader,
            writer: self
                .index
                .writer_with_num_threads(self.reindex_threads, self.reindex_buffer_size)?,
        })
    }

    fn init_index(schema: Schema, path: &Path, threads: usize) -> Result<tantivy::Index> {
        fs::create_dir_all(path).context("failed to create index dir")?;

        let mut index =
            tantivy::Index::open_or_create(tantivy::directory::MmapDirectory::open(path)?, schema)?;

        index.set_default_multithread_executor()?;
        index.set_multithread_executor(threads)?;
        index
            .tokenizers()
            .register("default", NgramTokenizer::new(1, 3, false)?);

        Ok(index)
    }

    fn init_code_index(schema: Schema, path: &Path, threads: usize) -> Result<tantivy::Index> {
        fs::create_dir_all(path).context("failed to create index dir")?;

        let mut index =
            tantivy::Index::open_or_create(tantivy::directory::MmapDirectory::open(path)?, schema)?;

        index.set_default_multithread_executor()?;
        index.set_multithread_executor(threads)?;
        // register the default ngram tokenizer and also the code snippet one
        index
            .tokenizers()
            .register("default", NgramTokenizer::new(1, 3, false)?);
        index
            .tokenizers()
            .register("code_snippet", CodeSnippetTokenizer {});

        Ok(index)
    }

    /// Create an index using `source` at the specified path.
    pub fn create(source: T, path: &Path, buffer_size: usize, threads: usize) -> Result<Self> {
        let index = Self::init_index(source.schema(), path, threads)?;
        let reader = index.reader()?.into();
        let instance = Self {
            reader,
            index,
            source,
            reindex_threads: threads,
            reindex_buffer_size: buffer_size,
        };

        Ok(instance)
    }

    pub fn create_for_code_snippets(
        source: T,
        path: &Path,
        buffer_size: usize,
        threads: usize,
    ) -> Result<Self> {
        let index = Self::init_code_index(source.schema(), path, threads)?;
        let reader = index.reader()?.into();
        let instance = Self {
            reader,
            index,
            source,
            reindex_buffer_size: buffer_size,
            reindex_threads: threads,
        };

        Ok(instance)
    }

    pub async fn query<'a, R, I, C>(
        &'a self,
        queries: I,
        doc_reader: &'a R,
        collector: C,
    ) -> Result<SearchResults<'_, R::Document>>
    where
        I: Iterator<Item = &'a Query<'a>> + Send,
        C: Collector<Fruit = (Vec<(Score, DocAddress)>, MultiFruit)>,
        R: DocumentRead<Schema = T>,
    {
        let searcher = self.reader.read().await.searcher();
        let queries = queries
            .filter(|q| doc_reader.query_matches(q))
            .collect::<SmallVec<[_; 2]>>();
        let compiled_query =
            doc_reader.compile(&self.source, queries.iter().copied(), &self.index)?;

        let (top_k, metadata) = searcher
            .search(&compiled_query, &collector)
            .context("failed to execute search query")?;

        let iter = top_k.into_iter().map(move |(_score, addr)| {
            let doc = searcher.doc(addr).unwrap();
            doc_reader.read_document(&self.source, doc)
        });

        Ok(SearchResults {
            docs: Box::new(iter),
            metadata,
        })
    }
}

pub type WriteHandleForIndexersRef<'a> = [IndexWriteHandle<'a>];

pub struct WriteHandleForIndexers<'a> {
    handles: Vec<IndexWriteHandle<'a>>,
    _write_lock: tokio::sync::MutexGuard<'a, ()>,
}

impl<'a> Deref for WriteHandleForIndexers<'a> {
    type Target = WriteHandleForIndexersRef<'a>;

    fn deref(&self) -> &Self::Target {
        &self.handles
    }
}

impl<'a> WriteHandleForIndexers<'a> {
    pub async fn commit(self) -> Result<()> {
        for mut handle in self.handles {
            handle.commit().await?
        }

        Ok(())
    }

    pub async fn index(
        &self,
        sync_handle: &SyncHandle,
        repo: &Repository,
    ) -> Result<Arc<RepoMetadata>, RepoError> {
        let metadata = repo.get_repo_metadata().await;

        futures::future::join_all(self.handles.iter().map(|handle| {
            handle.index(&sync_handle.reporef, repo, &metadata, sync_handle.pipes())
        }))
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

        debug!("writerhandleforindexers finished");

        Ok(metadata)
    }

    pub fn rollback(self) -> Result<()> {
        for mut handle in self.handles {
            handle.rollback()?;
        }
        Ok(())
    }
}

pub struct SearchResults<'a, T> {
    pub docs: Box<dyn Iterator<Item = T> + Sync + Send + 'a>,
    pub metadata: MultiFruit,
}

pub struct Indexes {
    pub file: Indexer<File>,
    pub code_snippet: Indexer<CodeSnippet>,
    write_mutex: tokio::sync::Mutex<()>,
}

impl Indexes {
    pub async fn new(
        repo_pool: RepositoryPool,
        sql_db: SqlDb,
        semantic: Option<SemanticClient>,
        config: Arc<Configuration>,
        language_parsing: Arc<TSLanguageParsing>,
    ) -> Result<Self> {
        debug!("creating indexes");
        // Figure out how to do version mismatch
        if config.state_source.index_version_mismatch() {
            debug!("deleting cache because of mismatch with tantivy");
            // we don't support old schemas, and tantivy will hard
            // error if we try to open a db with a different schema.
            // std::fs::remove_dir_all(config.index_path("repo"))?;
            std::fs::remove_dir_all(config.index_path("content"))?;

            let mut refs = vec![];
            // knocking out our current file caches will force re-indexing qdrant
            repo_pool.for_each(|reporef, repo| {
                refs.push(reporef.to_owned());
                repo.last_index_unix_secs = 0;
            });

            for reporef in refs {
                FileCache::for_repo(&sql_db, &reporef, semantic.as_ref())
                    .delete()
                    .await?;
            }

            if let Some(ref semantic) = semantic {
                semantic.delete_collection().await?;
            }
        }
        config.state_source.save_index_version()?;

        debug!("indexes created");

        Ok(Self {
            file: Indexer::create(
                File::new(sql_db.clone(), semantic),
                config.index_path("content").as_ref(),
                config.buffer_size,
                config.max_threads,
            )?,
            code_snippet: Indexer::create_for_code_snippets(
                CodeSnippet::new(sql_db, language_parsing),
                config.index_path("code_snippets").as_ref(),
                config.buffer_size,
                config.max_threads,
            )?,
            write_mutex: Default::default(),
        })
    }

    pub async fn writers(&self) -> Result<WriteHandleForIndexers<'_>> {
        let id: u64 = rand::random();
        debug!(id, "waiting for other writers to finish");
        let _write_lock = self.write_mutex.lock().await;
        debug!(id, "lock acquired");

        Ok(WriteHandleForIndexers {
            handles: vec![self.file.write_handle()?, self.code_snippet.write_handle()?],
            _write_lock,
        })
    }
}
