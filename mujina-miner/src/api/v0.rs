//! API v0 endpoints.
//!
//! Version 0 signals an unstable API -- breaking changes are expected
//! until the miner reaches 1.0.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::commands::{BoardCommand, FanControlUpdate, SchedulerCommand};
use super::server::SharedState;
use crate::api_client::types::{
    BoardTelemetry, FanControlRequest, MinerPatchRequest, MinerTelemetry, SourceTelemetry,
    TuningRequest,
};

/// Build the v0 API routes with OpenAPI metadata.
pub fn routes() -> OpenApiRouter<SharedState> {
    OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(get_miner, patch_miner))
        .routes(routes!(get_boards))
        .routes(routes!(get_board))
        .routes(routes!(patch_board_fan))
        .routes(routes!(patch_board_tuning))
        .routes(routes!(get_sources))
        .routes(routes!(get_source))
}

/// Health check endpoint.
#[utoipa::path(
    get,
    path = "/health",
    tag = "health",
    responses(
        (status = OK, description = "Server is running", body = String),
    ),
)]
async fn health() -> &'static str {
    "OK"
}

/// Return the current miner state snapshot.
#[utoipa::path(
    get,
    path = "/miner",
    tag = "miner",
    responses(
        (status = OK, description = "Current miner telemetry", body = MinerTelemetry),
    ),
)]
async fn get_miner(State(state): State<SharedState>) -> Json<MinerTelemetry> {
    Json(state.miner_telemetry())
}

/// Apply partial updates to the miner configuration.
#[utoipa::path(
    patch,
    path = "/miner",
    tag = "miner",
    request_body = MinerPatchRequest,
    responses(
        (status = OK, description = "Updated miner telemetry", body = MinerTelemetry),
        (status = INTERNAL_SERVER_ERROR, description = "Command channel error"),
    ),
)]
async fn patch_miner(
    State(state): State<SharedState>,
    Json(req): Json<MinerPatchRequest>,
) -> Result<Json<MinerTelemetry>, StatusCode> {
    if let Some(paused) = req.paused {
        let (tx, rx) = oneshot::channel();
        let cmd = if paused {
            SchedulerCommand::PauseMining { reply: tx }
        } else {
            SchedulerCommand::ResumeMining { reply: tx }
        };
        state
            .scheduler_cmd_tx
            .send(cmd)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        // Result layers: timeout / channel-closed / command-error.
        let Ok(Ok(Ok(()))) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        };
    }

    Ok(Json(state.miner_telemetry()))
}

/// Return all connected boards.
#[utoipa::path(
    get,
    path = "/boards",
    tag = "boards",
    responses(
        (status = OK, description = "List of connected boards", body = Vec<BoardTelemetry>),
    ),
)]
async fn get_boards(State(state): State<SharedState>) -> Json<Vec<BoardTelemetry>> {
    Json(
        state
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .boards(),
    )
}

/// Return a single board by name, or 404 if not found.
#[utoipa::path(
    get,
    path = "/boards/{name}",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    responses(
        (status = OK, description = "Board details", body = BoardTelemetry),
        (status = NOT_FOUND, description = "Board not found"),
    ),
)]
async fn get_board(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<BoardTelemetry>, StatusCode> {
    state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .find(|b| b.name == name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Update a board's fan control policy.
#[utoipa::path(
    patch,
    path = "/boards/{name}/fan",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = FanControlRequest,
    responses(
        (status = OK, description = "Updated board details", body = BoardTelemetry),
        (status = NOT_FOUND, description = "Board not found or accepts no commands"),
        (status = INTERNAL_SERVER_ERROR, description = "Command channel error"),
    ),
)]
async fn patch_board_fan(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<FanControlRequest>,
) -> Result<Json<BoardTelemetry>, StatusCode> {
    let sender = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .command_sender(&name)
        .ok_or(StatusCode::NOT_FOUND)?;

    let (tx, rx) = oneshot::channel();
    let cmd = BoardCommand::SetFanControl {
        update: FanControlUpdate {
            auto: req.auto,
            target_c: req.target_c,
            min_percent: req.min_percent,
            percent: req.percent,
        },
        reply: tx,
    };
    sender
        .send(cmd)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Result layers: timeout / channel-closed / command-error.
    let Ok(Ok(Ok(()))) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .find(|b| b.name == name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Send one board command and await its reply, mapping failures to 500.
async fn dispatch_board_command(
    sender: &mpsc::Sender<BoardCommand>,
    make: impl FnOnce(oneshot::Sender<anyhow::Result<()>>) -> BoardCommand,
) -> Result<(), StatusCode> {
    let (tx, rx) = oneshot::channel();
    // Bound the enqueue too: if the board's command buffer is full and its
    // monitor is wedged, we must not await the send forever.
    let Ok(Ok(())) = tokio::time::timeout(Duration::from_secs(5), sender.send(make(tx))).await
    else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    // A live frequency ramp can take a couple of seconds; allow headroom.
    let Ok(Ok(Ok(()))) = tokio::time::timeout(Duration::from_secs(15), rx).await else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    Ok(())
}

/// Set a board's ASIC frequency and/or core voltage (manual tuning).
#[utoipa::path(
    patch,
    path = "/boards/{name}/tuning",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = TuningRequest,
    responses(
        (status = OK, description = "Updated board details", body = BoardTelemetry),
        (status = NOT_FOUND, description = "Board not found or accepts no commands"),
        (status = INTERNAL_SERVER_ERROR, description = "Command channel error"),
    ),
)]
async fn patch_board_tuning(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<TuningRequest>,
) -> Result<Json<BoardTelemetry>, StatusCode> {
    let (sender, current_freq) = {
        let mut registry = state
            .board_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let sender = registry
            .command_sender(&name)
            .ok_or(StatusCode::NOT_FOUND)?;
        let current_freq = registry
            .boards()
            .into_iter()
            .find(|b| b.name == name)
            .and_then(|b| b.frequency_mhz);
        (sender, current_freq)
    };

    // Order the two changes by frequency direction so the chip is never
    // under-volted at a high clock during the multi-second transition:
    // when raising the clock, raise voltage first; when lowering, drop the
    // clock first, then the voltage. Default to voltage-first when the
    // current clock is unknown (the safer assumption).
    let raising = match (current_freq, req.frequency_mhz) {
        (Some(cur), Some(target)) => target > cur,
        _ => true,
    };

    for step_is_voltage in [raising, !raising] {
        if step_is_voltage {
            if let Some(mv) = req.core_voltage_mv {
                dispatch_board_command(&sender, |reply| BoardCommand::SetCoreVoltage {
                    millivolts: mv,
                    reply,
                })
                .await?;
            }
        } else if let Some(mhz) = req.frequency_mhz {
            dispatch_board_command(&sender, |reply| BoardCommand::SetFrequency { mhz, reply })
                .await?;
        }
    }

    state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .find(|b| b.name == name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Return all registered job sources.
#[utoipa::path(
    get,
    path = "/sources",
    tag = "sources",
    responses(
        (status = OK, description = "List of job sources", body = Vec<SourceTelemetry>),
    ),
)]
async fn get_sources(State(state): State<SharedState>) -> Json<Vec<SourceTelemetry>> {
    Json(state.miner_telemetry_rx.borrow().sources.clone())
}

/// Return a single source by name, or 404 if not found.
#[utoipa::path(
    get,
    path = "/sources/{name}",
    tag = "sources",
    params(
        ("name" = String, Path, description = "Source name"),
    ),
    responses(
        (status = OK, description = "Source details", body = SourceTelemetry),
        (status = NOT_FOUND, description = "Source not found"),
    ),
)]
async fn get_source(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<SourceTelemetry>, StatusCode> {
    state
        .miner_telemetry_rx
        .borrow()
        .sources
        .iter()
        .find(|s| s.name == name)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}
