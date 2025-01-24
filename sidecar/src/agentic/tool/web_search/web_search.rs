use axum::async_trait;
use crate::agentic::tool::{
    errors::ToolError,
    input::ToolInput,
    output::ToolOutput,
    r#type::{Tool, ToolRewardScale},
    web_search::types::WebSearchRequest,
};

use super::{
    cache::{CachedResponse, WebSearchCache},
    exa::{ExaClient, ExaSearchRequest},
    rate_limit::check_rate_limit, types::WebSearchResponse,
};

pub struct WebSearchTool {
    exa_client: ExaClient,
    cache: WebSearchCache,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            exa_client: ExaClient::new(),
            cache: WebSearchCache::with_cache_file("cache.json"),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    async fn invoke(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let context = input.is_web_search()?;

        // TODO: could hash this rather than use the query as the key ? Not sure it's worth it though
        let cache_key = context.query;
        let cached = self.cache.get(&cache_key);

        if let Some(cached_value) = cached {
            println!("Cache hit");
            todo!("todo");
            // TODO extract the summaries from the response JSON
            return Ok(ToolOutput::web_search(
                WebSearchResponse {
                    summaries: vec!["TODO".to_string(); 3]
                }
            ));
        }

        check_rate_limit()?;

        // * TODO the CLI program I wrote used a trait to make this code
        //generic across multiple search APIs, but it's removed here
        // as only one API is supported and KISS where possible
        let search_request = ExaSearchRequest::from(context);
        let text = self.exa_client.perform_web_search(search_request).await?;

        let cached_response = CachedResponse {
            response: text.clone(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        self.cache.set(cache_key, cached_response);

        // TODO: remove caching to disk
        // only keep in-memory cache, writing to disk is just
        // for debugging and to avoid using up a free API quota
        self.cache.save_to_disk()?;

        // TODO extract the summaries from the response JSON
        todo!("todo");
        Ok(ToolOutput::web_search(
        WebSearchResponse {
                summaries: vec!["TODO".to_string(); 3]
            }
        ));
    }

    fn tool_description(&self) -> String {
        format!(
            r#"### web_search
Request to perform a web search, providing up-to-date information from the internet in response to queries.
Web searches are necessary for data that is missing or out-of-date in the LLM training set, but the returned summaries of web pages will not be directly tailored to the query."#
        )
    }

    fn tool_input_format(&self) -> String {
        format!(r#"Parameters:
        - query: (required) A free text query to search the internet for.

        Usage:
        {}"#, WebSearchRequest::web_search_as_string("query here"))
    }

    fn get_evaluation_criteria(&self, _trajectory_length: usize) -> Vec<String> {
        vec![]
    }

    fn get_reward_scale(&self, _trajectory_length: usize) -> Vec<ToolRewardScale> {
        vec![]
    }
}