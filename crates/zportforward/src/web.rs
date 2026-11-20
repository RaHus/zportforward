//! The ports page: a small web UI served by the daemon on 127.0.0.1.
//!
//! It is a thin front for the same requests the CLI sends. Because any web
//! page the user visits can make their browser talk to localhost, every
//! request is checked:
//!
//! * the `Host` header must name this server on a loopback address, which
//!   defeats DNS rebinding;
//! * anything that changes state must be a POST carrying an `X-Zportforward`
//!   header (browsers refuse to send custom headers cross-site without a
//!   CORS preflight, which is never granted) and, when the browser sends an
//!   `Origin`, it must be this page's own.

use axum::extract::{Request as HttpRequest, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response as HttpResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

use crate::control::Request;
use crate::daemon::Event;
use crate::forwards;

/// Tried first; nearby ports are used when it is taken.
pub const DEFAULT_PORT: u16 = 4480;

const PAGE: &str = include_str!("ports.html");
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
                   connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

type Events = mpsc::Sender<Event>;

/// Start serving and return the page's URL. `ZPORTFORWARD_UI_PORT` picks the port;
/// `0` turns the page off.
pub async fn start(events: Events) -> Option<String> {
    let wanted = match std::env::var("ZPORTFORWARD_UI_PORT") {
        Ok(value) => value.parse().unwrap_or(DEFAULT_PORT),
        Err(_) => DEFAULT_PORT,
    };
    if wanted == 0 {
        return None;
    }
    let (listener, port) = match forwards::bind(wanted, false).await {
        Ok(bound) => bound,
        Err(reason) => {
            crate::log!("ports page not available: {reason}");
            return None;
        }
    };
    let app = Router::new()
        .route("/", get(page))
        .route("/api/state", get(state))
        .route("/api/forward", post(forward))
        .route("/api/stop", post(stop))
        .route("/api/auto", post(auto))
        .layer(middleware::from_fn_with_state(port, guard))
        .with_state(events);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            crate::log!("ports page stopped: {e}");
        }
    });
    Some(format!("http://127.0.0.1:{port}/"))
}

async fn guard(State(port): State<u16>, request: HttpRequest, next: Next) -> HttpResponse {
    let headers = request.headers();
    let text = |name| headers.get(name).and_then(|v: &HeaderValue| v.to_str().ok());
    let host = text(header::HOST).unwrap_or_default();
    let mut allowed = host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}");
    if request.method() != Method::GET {
        allowed &= headers.contains_key("x-zportforward");
        if let Some(origin) = text(header::ORIGIN) {
            allowed &= origin == format!("http://{host}");
        }
    }
    if !allowed {
        return (StatusCode::FORBIDDEN, "forbidden\n").into_response();
    }
    next.run(request).await
}

async fn page() -> impl IntoResponse {
    (
        [
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Html(PAGE),
    )
}

#[derive(Deserialize)]
struct ForwardBody {
    container: String,
    port: u16,
    host_port: Option<u16>,
    host: Option<String>,
}

#[derive(Deserialize)]
struct StopBody {
    container: String,
    port: u16,
}

#[derive(Deserialize)]
struct AutoBody {
    on: bool,
}

async fn state(State(events): State<Events>) -> HttpResponse {
    call(&events, Request::List).await
}

async fn forward(State(events): State<Events>, Json(body): Json<ForwardBody>) -> HttpResponse {
    let request = Request::Add {
        container: Some(body.container),
        cwd: None,
        port: body.port,
        host_port: body.host_port,
        host: body.host.filter(|h| !h.is_empty()),
    };
    call(&events, request).await
}

async fn stop(State(events): State<Events>, Json(body): Json<StopBody>) -> HttpResponse {
    call(&events, Request::Remove { container: Some(body.container), cwd: None, port: body.port }).await
}

async fn auto(State(events): State<Events>, Json(body): Json<AutoBody>) -> HttpResponse {
    call(&events, Request::SetAuto { on: body.on }).await
}

/// Hand a request to the daemon loop and turn its answer into JSON.
async fn call(events: &Events, request: Request) -> HttpResponse {
    let (tx, rx) = oneshot::channel();
    if events.send(Event::Control(request, tx)).await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match rx.await {
        Ok(response) => {
            let status = if response.error.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
            (status, [(header::CACHE_CONTROL, "no-store")], Json(response)).into_response()
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
