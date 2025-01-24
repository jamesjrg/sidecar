#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebSearchRequest {
    pub query: String
}

impl WebSearchRequest {
    pub fn web_search_as_string(query: &str) -> String {
        return format!("<web_search>
<query>
{query}
</query>
</web_search>");
    }

    pub fn to_string(&self) -> String {
        Self::web_search_as_string(&self.query)
    }
}

#[derive(Debug, Clone)]
pub struct WebSearchResponse {
    pub summaries: Vec<String>,
}
