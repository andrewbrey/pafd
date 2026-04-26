use axum::Router;
use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::routing::get;
use axum::routing::post;
use futures_util::StreamExt;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::signal;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal as unix_signal;
use tokio::sync::watch;

const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const STREAM_MAX_DURATION: Duration = Duration::from_mins(1);

#[derive(Clone)]
struct AppState {
    token: Option<String>,
    shutdown: watch::Receiver<bool>,
}

/// Runs the playback daemon, listening on `bind` for HTTP requests.
///
/// # Panics
///
/// Panics if the listener fails to bind, the local address cannot be read, or
/// the server task itself errors.
pub async fn serve(bind: &str, token: Option<String>) {
    if token.is_some() {
        tracing::info!("bearer token auth enabled for playback");
    } else {
        tracing::warn!("no bearer token configured; playback is unauthenticated");
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = AppState {
        token,
        shutdown: shutdown_rx,
    };

    let app = Router::new()
        .route("/ping", get(ping))
        .route("/stream", post(stream))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state);

    let listener = TcpListener::bind(bind).await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    tracing::info!("listening on http://{local_addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            wait_for_signal().await;
            let _ = shutdown_tx.send(true);
        })
        .await
        .expect("serve");
    tracing::info!("shutdown complete");
}

async fn wait_for_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install SIGINT handler");
    };

    let terminate = async {
        unix_signal(SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        () = ctrl_c => tracing::info!("received SIGINT; shutting down"),
        () = terminate => tracing::info!("received SIGTERM; shutting down"),
    }
}

async fn ping() -> &'static str {
    "pong"
}

async fn stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<&'static str, (StatusCode, String)> {
    check_auth(&state, &headers)?;

    let mut shutdown = state.shutdown.clone();

    let work = async {
        let mut child = Command::new("paplay")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("spawn paplay: {e}"),
                )
            })?;

        let mut stdin = child.stdin.take().expect("paplay stdin piped above");
        let mut data_stream = body.into_data_stream();

        let pump = async {
            let mut wrote = false;
            while let Some(chunk) = data_stream.next().await {
                let chunk =
                    chunk.map_err(|e| (StatusCode::BAD_REQUEST, format!("body read: {e}")))?;
                if chunk.is_empty() {
                    continue;
                }
                stdin.write_all(&chunk).await.map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("write to paplay: {e}"),
                    )
                })?;
                wrote = true;
            }
            Ok::<bool, (StatusCode, String)>(wrote)
        };

        let (wrote_anything, body_err) = match tokio::time::timeout(STREAM_MAX_DURATION, pump).await
        {
            Ok(Ok(w)) => (w, None),
            Ok(Err(e)) => (true, Some(e)),
            Err(_) => (
                true,
                Some((
                    StatusCode::REQUEST_TIMEOUT,
                    format!("stream exceeded {}s", STREAM_MAX_DURATION.as_secs()),
                )),
            ),
        };

        let _ = stdin.shutdown().await;
        drop(stdin);

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("wait: {e}")))?;

        if let Some(err) = body_err {
            return Err(err);
        }

        if !wrote_anything {
            return Err((StatusCode::BAD_REQUEST, "empty body".into()));
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err((
                StatusCode::BAD_REQUEST,
                format!("paplay exited {}: {stderr}", output.status),
            ));
        }

        Ok::<&'static str, (StatusCode, String)>("ok")
    };

    tokio::select! {
        res = work => res,
        _ = shutdown.changed() => {
            tracing::info!("shutdown during /stream; killed paplay");
            Err((StatusCode::SERVICE_UNAVAILABLE, "shutting down".into()))
        }
    }
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let Some(expected) = state.token.as_deref() else {
        return Ok(());
    };
    let provided = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    let ok = provided.is_some_and(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()));
    if ok {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "unauthorized".into()))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
