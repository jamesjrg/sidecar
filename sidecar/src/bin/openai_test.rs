/// Binary to check if we can call openai
use async_openai::config::AzureConfig;
use async_openai::types::ChatCompletionRequestMessageArgs;
use async_openai::types::CreateChatCompletionRequestArgs;
use async_openai::types::Role;
use async_openai::Client;
use futures::StreamExt;
use sidecar::reporting::posthog::client::client;
use sidecar::reporting::posthog::client::PosthogClient;
use sidecar::reporting::posthog::client::PosthogEvent;

// Note: This does not work as posthog uses an internal blocking reqwest client
// we should not be using that and instead fork it and create our own
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let posthog_client = posthog_client();
    let _ = main_func(posthog_client).await;
    Ok(())
}

async fn main_func(posthog_client: PosthogClient) -> anyhow::Result<()> {
    // try invoking it with the llm client
    // llm_request().await;

    // these are with the api keys
    let api_base = "https://codestory-gpt4.openai.azure.com".to_owned();
    let api_key = "89ca8a49a33344c9b794b3dabcbbc5d0".to_owned();
    let api_version = "2023-08-01-preview".to_owned();
    let deployment_id = "gpt4-access".to_owned();
    let azure_config = AzureConfig::new()
        .with_api_base(api_base)
        .with_api_key(api_key)
        .with_api_version(api_version)
        .with_deployment_id(deployment_id);

    let event = PosthogEvent::new("rust_event");
    let start_time = std::time::Instant::now();
    let _ = posthog_client.capture(event).await;
    let _ = start_time.elapsed().as_millis();

    let client = Client::with_config(azure_config);

    let mut request_args = CreateChatCompletionRequestArgs::default();
    let mut message_builder = ChatCompletionRequestMessageArgs::default();
    let system_message = message_builder
        .role(Role::System)
        .content("message")
        .build()
        .unwrap();
    let _ = ChatCompletionRequestMessageArgs::default()
        .role(Role::User)
        .content("can you please write me a song")
        .build()
        .unwrap();
    let chat_request_args = request_args
        .model("gpt-3.5-turbo-16k-0613".to_owned())
        .messages(vec![system_message])
        .build()
        .unwrap();
    let mut event = PosthogEvent::new("rust_something");
    let start_time = std::time::Instant::now();
    let _ = event.insert_prop("request", chat_request_args.clone());
    let _ = posthog_client.capture(event).await;
    let _ = start_time.elapsed().as_millis();
    let stream_messages = client.chat().create_stream(chat_request_args).await?;

    let _ = stream_messages
        .for_each(|value| {
            println!("values: {:?}", value);
            futures::future::ready(())
        })
        .await;

    Ok(())
}

fn posthog_client() -> PosthogClient {
    client(
        "phc_dKVAmUNwlfHYSIAH1kgnvq3iEw7ovE5YYvGhTyeRlaB",
        "codestory".to_owned(),
    )
}
