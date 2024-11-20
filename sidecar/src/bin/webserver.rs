// This is where we will create the default webserver for running the binary
// locally

use anyhow::Result;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::Extension;
use clap::Parser;
use sidecar::application::{application::Application, config::configuration::Configuration};
use sidecar::webserver;
use std::net::SocketAddr;
use tokio::signal;
use tokio::sync::oneshot;
use tower_http::{catch_panic::CatchPanicLayer, cors::CorsLayer};
use tracing::{debug, error, info};

pub type Router<S = Application> = axum::Router<S>;

#[tokio::main]
async fn main() -> Result<()> {
    info!("CodeStory 🚀");
    let configuration = Configuration::parse();

    // We get the logging setup first
    debug!("installing logging to local file");
    Application::install_logging(&configuration);

    // We create our scratch-pad directory
    Application::setup_scratch_pad(&configuration).await;

    // Create a oneshot channel
    let (tx, rx) = oneshot::channel();

    // Spawn a task to listen for signals
    tokio::spawn(async move {
        signal::ctrl_c().await.expect("failed to listen for event");
        let _ = tx.send(());
    });

    // We initialize the logging here
    let application = Application::initialize(configuration).await?;
    println!("initialized application");
    debug!("initialized application");

    // Main logic
    tokio::select! {
        // Start the webserver
        _ = run(application) => {
            // Your server logic
        }
        _ = rx => {
            // Signal received, this block will be executed.
            // Drop happens automatically when variables go out of scope.
            debug!("Signal received, cleaning up...");
        }
    }

    Ok(())
}

pub async fn run(application: Application) -> Result<()> {
    let mut joins = tokio::task::JoinSet::new();

    joins.spawn(start(application));

    while let Some(result) = joins.join_next().await {
        if let Ok(Err(err)) = result {
            error!(?err, "sidecar failed");
            return Err(err);
        }
    }

    Ok(())
}

// TODO(skcd): Add routes here which can do the following:
// - when a file changes, it should still be logged and tracked
// - when a file is opened, it should be tracked over here too
pub async fn start(app: Application) -> anyhow::Result<()> {
    println!("Port: {}", app.config.port);
    let bind = SocketAddr::new(app.config.host.parse()?, app.config.port);

    // routes through middleware
    let protected_routes = Router::new()
        .nest("/inline_completion", inline_completion())
        .nest("/agentic", agentic_router())
        .nest("/plan", plan_router())
        .nest("/agent", agent_router());
    // .layer(from_fn(auth_middleware)); // routes through middleware

    // no middleware check
    let public_routes = Router::new()
        .route("/config", get(webserver::config::get))
        .route(
            "/reach_the_devs",
            get(webserver::config::reach_the_devs),
        )
        .route("/version", get(webserver::config::version))
        .nest("/in_editor", in_editor_router())
        .nest("/tree_sitter", tree_sitter_router())
        .nest("/file", file_operations_router());

    // both protected and public merged into api
    let mut api = Router::new().merge(protected_routes).merge(public_routes);

    api = api.route("/health", get(webserver::health::health));

    let api = api
        .layer(Extension(app.clone()))
        .with_state(app.clone())
        .layer(CorsLayer::permissive())
        .layer(CatchPanicLayer::new())
        // I want to set the bytes limit here to 20 MB
        .layer(DefaultBodyLimit::max(20 * 1024 * 1024));

    #[cfg(feature = "print_request_response")]
    let api = api.layer(axum::middleware::from_fn(webserver::middleware::print_request_response));

    let router = Router::new().nest("/api", api);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, router.into_make_service()).await?;

    Ok(())
}

fn plan_router() -> Router {
    use axum::routing::*;
    Router::new()
    // Probe request routes
    // These routes handle starting and stopping probe requests
}

// Define routes for agentic operations
// Define the router for agentic operations
// This router handles various AI-assisted code operations and benchmarking
fn agentic_router() -> Router {
    use axum::routing::*;
    Router::new()
        .route(
            "/probe_request_stop",
            post(webserver::agentic::probe_request_stop),
        )
        .route(
            "/code_sculpting_followup",
            post(webserver::agentic::code_sculpting),
        )
        .route(
            "/code_sculpting_heal",
            post(webserver::agentic::code_sculpting_heal),
        )
        // route for push events coming from the editor
        .route(
            "/diagnostics",
            post(webserver::agentic::push_diagnostics),
        )
        // SWE bench route
        // This route is for software engineering benchmarking
        .route("/swe_bench", get(webserver::agentic::swe_bench))
        .route(
            "/agent_session_chat",
            post(webserver::agentic::agent_session_chat),
        )
        .route(
            "/agent_session_edit_anchored",
            post(webserver::agentic::agent_session_edit_anchored),
        )
        .route(
            "/agent_session_edit_agentic",
            post(webserver::agentic::agent_session_edit_agentic),
        )
        .route(
            "/agent_session_plan",
            post(webserver::agentic::agent_session_plan),
        )
        .route(
            "/agent_session_plan_iterate",
            post(webserver::agentic::agent_session_plan_iterate),
        )
        .route(
            "/agent_tool_use",
            post(webserver::agentic::agent_tool_use),
        )
        .route(
            "/verify_model_config",
            post(sidecar::webserver::agentic::verify_model_config),
        )
        .route(
            "/cancel_running_event",
            post(webserver::agentic::cancel_running_exchange),
        )
        .route(
            "/user_feedback_on_exchange",
            post(webserver::agentic::user_feedback_on_exchange),
        )
        .route(
            "/user_handle_session_undo",
            post(webserver::agentic::handle_session_undo),
        )
}

fn agent_router() -> Router {
    use axum::routing::*;
    Router::new()
        .route(
            "/search_agent",
            get(webserver::agent::search_agent),
        )
        .route(
            "/hybrid_search",
            get(webserver::agent::hybrid_search),
        )
        .route("/explain", get(webserver::agent::explain))
        .route(
            "/followup_chat",
            post(webserver::agent::followup_chat),
        )
}

fn in_editor_router() -> Router {
    use axum::routing::*;
    Router::new().route(
        "/answer",
        post(webserver::in_line_agent::reply_to_user),
    )
}

fn tree_sitter_router() -> Router {
    use axum::routing::*;
    Router::new()
        .route(
            "/documentation_parsing",
            post(webserver::tree_sitter::extract_documentation_strings),
        )
        .route(
            "/diagnostic_parsing",
            post(webserver::tree_sitter::extract_diagnostics_range),
        )
        .route(
            "/tree_sitter_valid",
            post(webserver::tree_sitter::tree_sitter_node_check),
        )
        .route(
            "/valid_xml",
            post(webserver::tree_sitter::check_valid_xml),
        )
}

fn file_operations_router() -> Router {
    use axum::routing::*;
    Router::new().route("/edit_file", post(webserver::file_edit::file_edit))
}

fn inline_completion() -> Router {
    use axum::routing::*;
    Router::new()
        .route(
            "/inline_completion",
            post(webserver::inline_completion::inline_completion),
        )
        .route(
            "/cancel_inline_completion",
            post(webserver::inline_completion::cancel_inline_completion),
        )
        .route(
            "/document_open",
            post(webserver::inline_completion::inline_document_open),
        )
        .route(
            "/document_content_changed",
            post(webserver::inline_completion::inline_completion_file_content_change),
        )
        .route(
            "/get_document_content",
            post(webserver::inline_completion::inline_completion_file_content),
        )
        .route(
            "/get_identifier_nodes",
            post(webserver::inline_completion::get_identifier_nodes),
        )
        .route(
            "/get_symbol_history",
            post(webserver::inline_completion::symbol_history),
        )
}
