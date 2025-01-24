use std::env;
use logging::new_client;
use anyhow::Result;

use super::types::WebSearchRequest;

#[derive(serde::Serialize, Debug, Clone)]
pub(crate) struct Summary {
    query: Option<String>,
}

#[derive(serde::Serialize, Debug, Clone)]
pub(crate) struct Contents {
    text: bool,
    summary: Summary,
    // There are  other options, see the Exa API documentation
}

#[derive(serde::Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExaSearchRequest {
    pub query: String,
    pub num_results: i32,
    pub contents: Contents,
    pub r#type: String,
    // There are many other options, see the Exa API documentation
}

// * TODO hard coded some of the parameters for now,
// maybe the agent should be free to change them?
// or at least extract the defaults into a struct...
impl From<WebSearchRequest> for ExaSearchRequest {
    fn from(request: WebSearchRequest) -> Self {
        Self {
            query: request.query,
            num_results: 3,
            r#type: "keyword".to_string(),
            contents: Contents {
                text: false,
                summary: Summary {
                    query: None
                },
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct ExaClient {
    client: reqwest_middleware::ClientWithMiddleware,
}

impl ExaClient {
    const API_URL: &'static str = "https://api.exa.ai/search";

    pub fn new() -> Self {
        Self {
            client: new_client(),
        }
    }

    // TODO should requests be proxied by a CodeStory server with
    // an API key?
    // Or should search API key come from the IDE config?
    fn api_key(&self) -> Result<String, anyhow::Error> {
        env::var("AIDE_EXA_API_KEY")
        .map_err(|_| anyhow::anyhow!("Missing AIDE_EXA_API_KEY"))
    }

    pub async fn perform_web_search(
        &self,
        request: ExaSearchRequest,
    ) -> Result<String> {
        let access_token = self.api_key()?;

        let response = self
            .client
            .post(Self::API_URL)
            .header("x-api-key", access_token)
            .json(&request)
            .send()
            .await?;

        Ok(response.text().await?)
    }
}
