//! HTTP server lifecycle and router construction.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::tracing::prelude::*;
use utoipa_axum::router::OpenApiRouter;
use utoipa_swagger_ui::SwaggerUi;

use super::{
    commands::SchedulerCommand,
    dashboard::HashrateHistory,
    registry::{BoardRegistration, BoardRegistry},
    v0,
};
use crate::api_client::types::MinerTelemetry;

/// API server configuration.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Address and port to bind the API server to.
    pub bind_addr: String,
}

/// Shared application state available to all handlers.
#[derive(Clone)]
pub(crate) struct SharedState {
    pub miner_telemetry_rx: watch::Receiver<MinerTelemetry>,
    pub board_registry: Arc<Mutex<BoardRegistry>>,
    pub scheduler_cmd_tx: mpsc::Sender<SchedulerCommand>,
    /// Rolling hashrate history behind the dashboard's fleet/board charts.
    /// See [`HashrateHistory`] for why this lives here rather than being
    /// derived per-request.
    pub hashrate_history: Arc<Mutex<HashrateHistory>>,
}

impl SharedState {
    /// Build a complete MinerTelemetry by combining scheduler data with board
    /// snapshots from the registry.
    pub fn miner_telemetry(&self) -> MinerTelemetry {
        let mut telemetry = self.miner_telemetry_rx.borrow().clone();
        telemetry.boards = self
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .boards();
        telemetry
    }
}

/// Default token file on the Nano 3S (the writable /data partition).
const API_TOKEN_FILE: &str = "/data/userconfig/api_token";

/// Shared token required for writes: `MUJINA_API_TOKEN`, else the contents
/// of the file named by `MUJINA_API_TOKEN_FILE` (default [`API_TOKEN_FILE`]).
/// `None` (nothing set) leaves writes open, the behavior before tokens.
pub(crate) fn load_api_token() -> Option<Arc<str>> {
    let from_file = || {
        let path = std::env::var("MUJINA_API_TOKEN_FILE").unwrap_or_else(|_| API_TOKEN_FILE.into());
        std::fs::read_to_string(path).ok()
    };
    std::env::var("MUJINA_API_TOKEN")
        .ok()
        .or_else(from_file)
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .map(Arc::from)
}

/// With a token configured, every request other than GET/HEAD/OPTIONS must
/// carry `Authorization: Bearer <token>`. Reads stay open.
async fn require_token(State(token): State<Option<Arc<str>>>, req: Request, next: Next) -> Response {
    let read = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let Some(expected) = token.filter(|_| !read) else {
        return next.run(req).await;
    };
    let given = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if given.is_some_and(|g| ct_eq(g.as_bytes(), expected.as_bytes())) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "API token required").into_response()
    }
}

/// Byte comparison whose time doesn't depend on where the inputs differ.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Start the API server.
///
/// This function starts the HTTP API server and runs until the provided
/// cancellation token is triggered. It binds to localhost only by default for
/// security.
///
/// Board registrations arrive via `board_reg_rx` as boards connect. The
/// server manages the collection internally and cleans up when boards
/// disconnect.
pub async fn serve(
    config: ApiConfig,
    shutdown: CancellationToken,
    miner_telemetry_rx: watch::Receiver<MinerTelemetry>,
    mut board_reg_rx: mpsc::Receiver<BoardRegistration>,
    scheduler_cmd_tx: mpsc::Sender<SchedulerCommand>,
) -> Result<()> {
    let board_registry = Arc::new(Mutex::new(BoardRegistry::new()));

    // Drain board registrations into the registry as they arrive.
    // Exits when the sender is dropped (backplane shutdown).
    tokio::spawn({
        let registry = board_registry.clone();
        async move {
            while let Some(reg) = board_reg_rx.recv().await {
                registry.lock().unwrap_or_else(|e| e.into_inner()).push(reg);
            }
        }
    });

    let api_token = load_api_token();
    if api_token.is_some() {
        info!("API token set: writes require Authorization: Bearer <token>.");
    }
    let app = build_router(miner_telemetry_rx, board_registry, scheduler_cmd_tx, api_token);

    let listener = TcpListener::bind(&config.bind_addr).await?;
    let actual_addr = listener.local_addr()?;

    info!(url = %format!("http://{}", actual_addr), "API server listening.");

    // Warn if binding to non-localhost addresses
    if !actual_addr.ip().is_loopback() {
        warn!(
            "API server is bound to a non-localhost address ({}). \
             This exposes the API to the network without authentication.",
            actual_addr.ip()
        );
    }

    // Run server with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
        })
        .await?;

    Ok(())
}

/// Build the application router with all API routes.
pub(crate) fn build_router(
    miner_telemetry_rx: watch::Receiver<MinerTelemetry>,
    board_registry: Arc<Mutex<BoardRegistry>>,
    scheduler_cmd_tx: mpsc::Sender<SchedulerCommand>,
    api_token: Option<Arc<str>>,
) -> Router {
    let state = SharedState {
        miner_telemetry_rx,
        board_registry,
        scheduler_cmd_tx,
        hashrate_history: Arc::new(Mutex::new(HashrateHistory::default())),
    };

    let dashboard_router = super::dashboard::routes().with_state(state.clone());

    let (router, api) = OpenApiRouter::new()
        .nest("/api/v0", v0::routes())
        .with_state(state)
        .split_for_parts();

    router
        .route("/", routing::get(Redirect::permanent("/dashboard")))
        .route("/api", routing::get(Redirect::permanent("/swagger-ui")))
        .merge(dashboard_router)
        .merge(SwaggerUi::new("/swagger-ui").url("/api/v0/openapi.json", api))
        .layer(middleware::from_fn_with_state(api_token, require_token))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::TRACE))
                .on_response(DefaultOnResponse::new().level(Level::TRACE)),
        )
}

#[cfg(test)]
mod tests {
    use http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::api::commands::SchedulerCommand;
    use crate::api::registry::BoardRegistration;
    use crate::api_client::types::{BoardTelemetry, SourceTelemetry};

    /// Test fixtures returned by the router builder.
    struct TestFixtures {
        router: Router,
        /// Keep alive to prevent board watch channels from closing.
        _board_senders: Vec<watch::Sender<BoardTelemetry>>,
        /// Publish updated miner telemetry (e.g. after handling a command).
        _miner_tx: watch::Sender<MinerTelemetry>,
        /// Receives commands sent by PATCH handlers.
        _cmd_rx: mpsc::Receiver<SchedulerCommand>,
    }

    fn build_test_router(
        miner_state: MinerTelemetry,
        board_states: Vec<BoardTelemetry>,
    ) -> TestFixtures {
        build_test_router_with_token(miner_state, board_states, None)
    }

    fn build_test_router_with_token(
        miner_state: MinerTelemetry,
        board_states: Vec<BoardTelemetry>,
        api_token: Option<Arc<str>>,
    ) -> TestFixtures {
        let (miner_tx, miner_rx) = watch::channel(miner_state);
        let (cmd_tx, cmd_rx) = mpsc::channel::<SchedulerCommand>(16);

        let mut registry = BoardRegistry::new();
        let mut board_senders = Vec::new();
        for state in board_states {
            let (tx, rx) = watch::channel(state);
            registry.push(BoardRegistration { telemetry_rx: rx });
            board_senders.push(tx);
        }

        TestFixtures {
            router: build_router(miner_rx, Arc::new(Mutex::new(registry)), cmd_tx, api_token),
            _board_senders: board_senders,
            _miner_tx: miner_tx,
            _cmd_rx: cmd_rx,
        }
    }

    async fn patch_status(app: Router, uri: &str, auth: Option<&str>) -> http::StatusCode {
        let mut req = Request::builder()
            .method("PATCH")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let req = req.body(axum::body::Body::from("{}")).unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn writes_need_the_token_when_one_is_set() {
        let token: Option<Arc<str>> = Some(Arc::from("s3cret"));
        let app = || build_test_router_with_token(MinerTelemetry::default(), vec![], token.clone()).router;
        let (status, _) = get(app(), "/api/v0/health").await;
        assert_eq!(status, http::StatusCode::OK, "reads stay open");
        assert_eq!(patch_status(app(), "/api/v0/miner", None).await, http::StatusCode::UNAUTHORIZED);
        assert_eq!(
            patch_status(app(), "/api/v0/miner", Some("Bearer wrong")).await,
            http::StatusCode::UNAUTHORIZED
        );
        assert_eq!(patch_status(app(), "/api/v0/miner", Some("s3cret")).await, http::StatusCode::UNAUTHORIZED);
        assert_ne!(
            patch_status(app(), "/api/v0/miner", Some("Bearer s3cret")).await,
            http::StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn writes_stay_open_without_a_token() {
        let app = build_test_router(MinerTelemetry::default(), vec![]).router;
        assert_ne!(patch_status(app, "/api/v0/miner", None).await, http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn ct_eq_matches_only_equal_bytes() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }

    async fn get(app: Router, uri: &str) -> (http::StatusCode, String) {
        let req = Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let fixtures = build_test_router(MinerTelemetry::default(), vec![]);
        let (status, body) = get(fixtures.router.clone(), "/api/v0/health").await;
        assert_eq!(status, 200);
        assert_eq!(body, "OK");
    }

    #[tokio::test]
    async fn miner_includes_boards_and_sources() {
        let miner_state = MinerTelemetry {
            uptime_secs: 42,
            hashrate: 1_000_000,
            shares_submitted: 5,
            sources: vec![SourceTelemetry {
                name: "pool".into(),
                url: Some("stratum+tcp://localhost:3333".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let board = BoardTelemetry {
            name: "test-board".into(),
            model: "TestModel".into(),
            ..Default::default()
        };
        let fixtures = build_test_router(miner_state, vec![board]);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/miner").await;
        assert_eq!(status, 200);

        let state: MinerTelemetry = serde_json::from_str(&body).unwrap();
        assert_eq!(state.uptime_secs, 42);
        assert_eq!(state.hashrate, 1_000_000);
        assert_eq!(state.shares_submitted, 5);
        assert_eq!(state.boards.len(), 1);
        assert_eq!(state.boards[0].name, "test-board");
        assert_eq!(state.sources.len(), 1);
        assert_eq!(state.sources[0].name, "pool");
    }

    #[tokio::test]
    async fn boards_returns_list() {
        let boards = vec![
            BoardTelemetry {
                name: "board-a".into(),
                model: "A".into(),
                ..Default::default()
            },
            BoardTelemetry {
                name: "board-b".into(),
                model: "B".into(),
                ..Default::default()
            },
        ];
        let fixtures = build_test_router(MinerTelemetry::default(), boards);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/boards").await;
        assert_eq!(status, 200);

        let boards: Vec<BoardTelemetry> = serde_json::from_str(&body).unwrap();
        assert_eq!(boards.len(), 2);
        assert_eq!(boards[0].name, "board-a");
        assert_eq!(boards[1].name, "board-b");
    }

    #[tokio::test]
    async fn board_by_name_returns_match() {
        let board = BoardTelemetry {
            name: "bitaxe-abc123".into(),
            model: "Bitaxe".into(),
            serial: Some("abc123".into()),
            ..Default::default()
        };
        let fixtures = build_test_router(MinerTelemetry::default(), vec![board]);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/boards/bitaxe-abc123").await;
        assert_eq!(status, 200);

        let board: BoardTelemetry = serde_json::from_str(&body).unwrap();
        assert_eq!(board.name, "bitaxe-abc123");
        assert_eq!(board.serial, Some("abc123".into()));
    }

    #[tokio::test]
    async fn board_by_name_returns_404_when_missing() {
        let fixtures = build_test_router(MinerTelemetry::default(), vec![]);
        let (status, _body) = get(fixtures.router.clone(), "/api/v0/boards/nonexistent").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn sources_returns_list() {
        let miner_state = MinerTelemetry {
            sources: vec![
                SourceTelemetry {
                    name: "pool-a".into(),
                    url: Some("stratum+tcp://a:3333".into()),
                    ..Default::default()
                },
                SourceTelemetry {
                    name: "pool-b".into(),
                    url: None,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let fixtures = build_test_router(miner_state, vec![]);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/sources").await;
        assert_eq!(status, 200);

        let sources: Vec<SourceTelemetry> = serde_json::from_str(&body).unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].name, "pool-a");
        assert_eq!(sources[0].url.as_deref(), Some("stratum+tcp://a:3333"));
        assert_eq!(sources[1].name, "pool-b");
        assert_eq!(sources[1].url, None);
    }

    #[tokio::test]
    async fn source_by_name_returns_match() {
        let miner_state = MinerTelemetry {
            sources: vec![SourceTelemetry {
                name: "my-pool".into(),
                url: Some("stratum+tcp://pool:3333".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fixtures = build_test_router(miner_state, vec![]);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/sources/my-pool").await;
        assert_eq!(status, 200);

        let source: SourceTelemetry = serde_json::from_str(&body).unwrap();
        assert_eq!(source.name, "my-pool");
        assert_eq!(source.url.as_deref(), Some("stratum+tcp://pool:3333"));
    }

    #[tokio::test]
    async fn source_by_name_returns_404_when_missing() {
        let fixtures = build_test_router(MinerTelemetry::default(), vec![]);
        let (status, _body) = get(fixtures.router.clone(), "/api/v0/sources/nonexistent").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn source_difficulty_serializes_as_f64() {
        let miner_state = MinerTelemetry {
            sources: vec![SourceTelemetry {
                name: "pool".into(),
                difficulty: Some(2048.5),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fixtures = build_test_router(miner_state, vec![]);

        let (status, body) = get(fixtures.router.clone(), "/api/v0/sources/pool").await;
        assert_eq!(status, 200);

        let source: SourceTelemetry = serde_json::from_str(&body).unwrap();
        assert_eq!(source.difficulty, Some(2048.5));
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let fixtures = build_test_router(MinerTelemetry::default(), vec![]);
        let (status, _body) = get(fixtures.router.clone(), "/api/v0/nope").await;
        assert_eq!(status, 404);
    }
}
