use anyhow::Result;
use axum::extract;
use axum::{
    body::{Body, Bytes},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    extract::Request,
};
use http_body_util::BodyExt;
use axum::http::header::AUTHORIZATION;


// reintroduce when necessary
pub async fn auth_middleware<B>(request: extract::Request, next: Next) -> Result<Response, StatusCode> {
    // Get token from Authorization header
    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    dbg!(&auth_header);

    match auth_header {
        Some(token) => {
            // Check if token starts with "Bearer "
            if let Some(token) = token.strip_prefix("Bearer ") {
                // Validate token here
                if _is_valid_token(token).await {
                    Ok(next.run(request).await)
                } else {
                    Err(StatusCode::UNAUTHORIZED)
                }
            } else {
                Err(StatusCode::UNAUTHORIZED)
            }
        }
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

// Token validation function (implement your own logic)
async fn _is_valid_token(token: &str) -> bool {
    println!("webserver::is_valid_token::token({})", token);

    match _validate_workos_token(token).await {
        Ok(_) => true,
        Err(_) => false,
    }
}

async fn _validate_workos_token(token: &str) -> Result<bool> {
    let client = reqwest::Client::new();

    let auth_proxy_endpoint = "";

    let response = client
        .get(auth_proxy_endpoint)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await?;

    dbg!(&response);

    Ok(response.status().is_success())
}

pub async fn print_request_response(request: Request, next: Next) -> Result<impl IntoResponse, (StatusCode, String)> {
    let (parts, body) = request.into_parts();
    let bytes = buffer_and_print("request", body).await?;
    println!("method uri {:?} {:?}", parts.method, parts.uri);
    println!("headers {:?}", parts.headers);
    let request = Request::from_parts(parts, Body::from(bytes));

    let res = next.run(request).await;

    let (parts, body) = res.into_parts();
    let bytes = buffer_and_print("response", body).await?;
    let res = Response::from_parts(parts, Body::from(bytes));

    Ok(res)
}

async fn buffer_and_print<B>(direction: &str, body: B) -> Result<Bytes, (StatusCode, String)>
where
    B: axum::body::HttpBody<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    let bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(err) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("failed to read {direction} body: {err}"),
            ));
        }
    };

    if let Ok(body) = std::str::from_utf8(&bytes) {
        tracing::info!("{direction} body = {body:?}");
    }

    Ok(bytes)
}