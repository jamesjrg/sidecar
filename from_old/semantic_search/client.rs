/// This contains the main client we will be using for semantic search
/// The client provides additional support for querying using qdrant and exposes
/// the embedder which we want to use
use std::{env, path::Path, sync::Arc};

use qdrant_client::{
    client::QdrantClient,
    prelude::QdrantClientConfig,
    qdrant::{
        r#match::MatchValue, vectors_config, with_payload_selector, with_vectors_selector,
        CollectionOperationResponse, Condition, CreateCollection, Distance, FieldCondition, Filter,
        Match, ScoredPoint, SearchPoints, VectorParams, VectorsConfig, WithPayloadSelector,
        WithVectorsSelector,
    },
};
use rayon::iter::IntoParallelIterator;
use rayon::prelude::ParallelIterator;
use tracing::{debug, error};

use crate::{
    application::config::configuration::Configuration,
    chunking::languages::TSLanguageParsing,
    embedder::embedder::{Embedder, LocalEmbedder},
    repo::types::RepoRef,
};

use super::schema::Payload;

const EMBEDDING_DIM: usize = 384;

#[derive(Clone)]
pub struct SemanticClient {
    embedder: Arc<dyn Embedder>,
    search_client: Arc<QdrantClient>,
    config: Arc<Configuration>,
    language_parsing: Arc<TSLanguageParsing>,
}

impl SemanticClient {
    pub async fn new(
        config: Arc<Configuration>,
        language_parsing: Arc<TSLanguageParsing>,
    ) -> Option<Self> {
        let qdrant_config = QdrantClientConfig::from_url(&config.qdrant_url);
        let qdrant_client =
            QdrantClient::new(Some(qdrant_config)).expect("client creation to not fail");

        debug!("qdrant client created");

        match qdrant_client.has_collection(&config.collection_name).await {
            Ok(false) => {
                let CollectionOperationResponse { result, time } =
                    create_collection(&config.collection_name, &qdrant_client)
                        .await
                        .unwrap();

                debug!(
                    "Created collection {} in {}ms with result {}",
                    config.collection_name, time, result
                );

                assert!(result);
            }
            Ok(true) => {}
            Err(e) => {
                error!("{}", e.to_string());
                return None;
            }
        }

        // TODO(skcd): we might want to create some indexes here, but we can
        // figure that out as we keep hacking

        // Now we first need to set the ort library up properly
        debug!("initializing ort dylib");
        init_ort_dylib(&config.dylib_directory);
        let embedder = LocalEmbedder::new(&config.model_dir);
        if embedder.is_err() {
            debug!("embedder creation failed");
            return None;
        }
        Some(Self {
            embedder: Arc::new(embedder.expect("is_err check above")),
            search_client: Arc::new(qdrant_client),
            config,
            language_parsing,
        })
    }

    pub fn qdrant_client(&self) -> &QdrantClient {
        &self.search_client
    }

    pub fn collection_name(&self) -> &str {
        &self.config.collection_name
    }

    pub fn get_embedding_queue_size(&self) -> usize {
        self.config.embedding_batch_len.into()
    }

    pub fn get_embedder(&self) -> Arc<dyn Embedder> {
        self.embedder.clone()
    }

    pub async fn delete_points_for_hash(
        &self,
        repo_ref: &str,
        paths: impl Iterator<Item = String>,
    ) {
        let repo_filter = make_kv_keyword_filter("repo_ref", repo_ref).into();
        let file_filter = paths
            .map(|p| make_kv_keyword_filter("content_hash", &p).into())
            .collect::<Vec<_>>();

        let selector = Filter {
            must: vec![repo_filter],
            should: file_filter,
            ..Default::default()
        }
        .into();

        let _ = self
            .qdrant_client()
            .delete_points(&self.config.collection_name, &selector, None)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn chunks_for_buffer<'a>(
        &'a self,
        file_cache_key: String,
        repo_name: &'a str,
        repo_ref: &'a str,
        relative_path: &'a str,
        buffer: &'a str,
        lang_str: &'a str,
        branches: &'a [String],
        file_extension: Option<&'a str>,
    ) -> impl ParallelIterator<Item = (String, Payload)> + 'a {
        let spans = self
            .language_parsing
            .chunk_file(relative_path, buffer, file_extension);
        debug!(chunk_count = spans.len(), relative_path, "found chunks");
        spans.iter().for_each(|span| {
            debug!(?span, relative_path, "span content");
        });

        spans
            .into_par_iter()
            .filter(|span| span.data.is_some())
            .map(move |span| {
                let data_content = span.data.unwrap();
                let data = format!("{repo_name}\t{relative_path}\n{}", data_content);
                let payload = Payload {
                    repo_name: repo_name.to_owned(),
                    repo_ref: repo_ref.to_owned(),
                    relative_path: relative_path.to_owned(),
                    content_hash: file_cache_key.to_string(),
                    text: data_content.to_owned(),
                    lang: lang_str.to_ascii_lowercase(),
                    branches: branches.to_owned(),
                    start_line: span.start as u64,
                    end_line: span.end as u64,
                    ..Default::default()
                };

                (data, payload)
            })
    }

    pub async fn delete_collection(&self) -> anyhow::Result<()> {
        // There might be race conditions here with the qdrant binary and we might
        // not end up deleting the collection which we are tracking, so we should
        // ideally be careful about that.
        let _ = self
            .qdrant_client()
            .delete_collection(&self.config.collection_name)
            .await?;
        Ok(())
    }

    pub async fn search<'a>(
        &self,
        query: &'a str,
        reporef: &'a RepoRef,
        limit: u64,
        offset: u64,
        threshold: f32,
        get_more: bool,
    ) -> anyhow::Result<Vec<Payload>> {
        let vector = self.embedder.embed(&query)?;

        // TODO: Remove the need for `retrieve_more`. It's here because:
        // In /q `limit` is the maximum number of results returned (the actual number will often be lower due to deduplication)
        // In /answer we want to retrieve `limit` results exactly
        let results = self
            .search_with(
                query,
                reporef,
                vector.clone(),
                if get_more { limit * 2 } else { limit }, // Retrieve double `limit` and deduplicate
                offset,
                threshold,
            )
            .await
            .map(|raw| {
                raw.into_iter()
                    .map(Payload::from_qdrant)
                    .collect::<Vec<_>>()
            })?;
        // We should also deduplicate things here, when required
        // TODO(skcd): deduplicate the snippets here and also rank them properly
        // with how much more relevant they are
        Ok(results)
    }

    pub async fn search_with<'a>(
        &self,
        query: &'a str,
        reporef: &'a RepoRef,
        vector: Vec<f32>,
        limit: u64,
        offset: u64,
        threshold: f32,
    ) -> anyhow::Result<Vec<ScoredPoint>> {
        let response = self
            .qdrant_client()
            .search_points(&SearchPoints {
                limit,
                vector,
                collection_name: self.config.collection_name.to_string(),
                offset: Some(offset),
                score_threshold: Some(threshold),
                with_payload: Some(WithPayloadSelector {
                    selector_options: Some(with_payload_selector::SelectorOptions::Enable(true)),
                }),
                filter: Some(Filter {
                    must: build_conditions(query, reporef),
                    ..Default::default()
                }),
                with_vectors: Some(WithVectorsSelector {
                    selector_options: Some(with_vectors_selector::SelectorOptions::Enable(true)),
                }),
                ..Default::default()
            })
            .await?;

        Ok(response.result)
    }

    pub async fn health_check(&self) -> anyhow::Result<()> {
        self.qdrant_client().health_check().await?;
        Ok(())
    }
}

fn build_conditions<'a>(parsed_query: &'a str, reporef: &'a RepoRef) -> Vec<Condition> {
    vec![Filter {
        must: vec![make_kv_keyword_filter("repo_ref", &reporef.to_string()).into()],
        ..Default::default()
    }]
    .into_iter()
    .map(Into::into)
    .collect()
}

fn make_kv_keyword_filter(key: &str, value: &str) -> FieldCondition {
    let key = key.to_owned();
    let value = value.to_owned();
    FieldCondition {
        key,
        r#match: Some(Match {
            match_value: MatchValue::Keyword(value).into(),
        }),
        ..Default::default()
    }
}

/// Initialize the `ORT_DYLIB_PATH` variable, consumed by the `ort` crate.
///
/// This is required because we need the dylib library to be present when we are
/// starting out the embedder as this is required by the ort runtime.
fn init_ort_dylib(dylib_dir: impl AsRef<Path>) {
    {
        #[cfg(target_os = "linux")]
        let lib_name = "libonnxruntime.so";
        #[cfg(target_os = "macos")]
        let lib_name = "libonnxruntime.dylib";
        #[cfg(target_os = "windows")]
        let lib_name = "onnxruntime.dll";

        let ort_dylib_path = dylib_dir.as_ref().join(lib_name);

        if env::var("ORT_DYLIB_PATH").is_err() {
            env::set_var("ORT_DYLIB_PATH", ort_dylib_path);
        }
    }
}

pub(super) async fn create_collection(
    name: &str,
    qdrant: &QdrantClient,
) -> anyhow::Result<CollectionOperationResponse> {
    qdrant
        .create_collection(&CreateCollection {
            collection_name: name.to_string(),
            vectors_config: Some(VectorsConfig {
                config: Some(vectors_config::Config::Params(VectorParams {
                    size: EMBEDDING_DIM as u64,
                    distance: Distance::Cosine.into(),
                    on_disk: Some(true),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        })
        .await
}
