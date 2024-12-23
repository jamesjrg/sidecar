use std::collections::HashMap;

use crate::reqwest_tee_middleware::new_tee_client;

#[derive(Clone)]
pub struct PareaClient {
    client: reqwest_middleware::ClientWithMiddleware,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PareaLogMessage {
    role: String,
    content: String,
}

impl PareaLogMessage {
    pub fn new(role: String, content: String) -> Self {
        Self { role, content }
    }
}

#[derive(Debug, Clone)]
pub struct PareaLogCompletion {
    messages: Vec<PareaLogMessage>,
    metadata: HashMap<String, String>,
    response: String,
    temperature: f32,
    parent_trace_id: String,
    trace_id: String,
    root_trace_id: String,
    llm: String,
    provider: String,
    trace_name: String,
}

#[derive(Debug, Clone)]
pub struct PareaLogEvent {
    event_name: String,
    parent_trace_id: String,
    trace_id: String,
    metadata: HashMap<String, String>,
}

impl PareaLogEvent {
    pub fn new(
        event_name: String,
        parent_trace_id: String,
        trace_id: String,
        metadata: HashMap<String, String>,
    ) -> Self {
        Self {
            event_name,
            parent_trace_id,
            trace_id,
            metadata,
        }
    }
}

impl PareaLogCompletion {
    pub fn new(
        messages: Vec<PareaLogMessage>,
        metadata: HashMap<String, String>,
        response: String,
        temperature: f32,
        trace_id: String,
        parent_trace_id: String,
        root_trace_id: String,
        llm: String,
        provider: String,
        trace_name: String,
    ) -> Self {
        Self {
            messages,
            metadata,
            response,
            temperature,
            parent_trace_id,
            trace_id,
            root_trace_id,
            llm,
            provider,
            trace_name,
        }
    }
}

impl PareaClient {
    pub fn new() -> Self {
        Self {
            client: new_tee_client(),
        }
    }

    pub async fn log_event(&self, event: PareaLogEvent) {
        let url =
            "https://parea-ai-backend-us-9ac16cdbc7a7b006.onporter.run/api/parea/v1/trace_log";

        let metadata = event.metadata.clone();

        let body = serde_json::json!({
            "trace_id": event.trace_id,
            "root_trace_id": event.parent_trace_id,
            "parent_trace_id": event.parent_trace_id,
            "trace_name": event.event_name,
            "project_name": "default",
            "inputs": event.metadata,
            "output": "logging".to_owned(),
            "start_timestamp": chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "end_timestamp": chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "status": "success",
            "metadata": metadata,
            "depth": 0,
            "execution_order": 0,
            "evaluation_metric_names": vec!["XML Checker"],
            "fill_children": true,
        });

        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .header(
                "x-api-key",
                "pai-fadddd1b1f5ad7b39b082541ef715fb9b0017a77125b0225c3e778acfc43c206",
            )
            .body(serde_json::to_string(&body).expect("conversion should never fail for logging"))
            .send()
            .await;
        println!("{:?}", response.map(|response| response.status()));
    }

    pub async fn log_completion(&self, completion: PareaLogCompletion) {
        let url =
            "https://parea-ai-backend-us-9ac16cdbc7a7b006.onporter.run/api/parea/v1/trace_log";

        let mut metadata = completion.metadata.clone();
        metadata.insert(
            "evaluation_metric_names".to_owned(),
            "XML Checker".to_owned(),
        );

        let body = serde_json::json!({
            "trace_id": completion.trace_id,
            "root_trace_id": completion.root_trace_id,
            "parent_trace_id": completion.parent_trace_id,
            "trace_name": completion.trace_name,
            "project_name": "default",
            "inputs": completion.metadata,
            "output": completion.response,
            "configuration": {
                "model": completion.llm,
                "provider": completion.provider,
                "messages": completion.messages,
                "temperature": completion.temperature,
            },
            "start_timestamp": chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "end_timestamp": chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "status": "success",
            "metadata": metadata,
            "depth": 0,
            "execution_order": 0,
            "evaluation_metric_names": vec!["XML Checker"],
            "fill_children": true,
        });

        let _ = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .header(
                "x-api-key",
                "pai-fadddd1b1f5ad7b39b082541ef715fb9b0017a77125b0225c3e778acfc43c206",
            )
            .body(serde_json::to_string(&body).expect("conversion should never fail for logging"))
            .send()
            .await;
    }
}
