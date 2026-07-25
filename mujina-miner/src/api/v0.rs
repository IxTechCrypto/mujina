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

use super::autotune::{self, AutoTuneStatus, TargetRange, TuneProfile, TuneSetpoint};
use super::commands::{BoardCommand, FanControlUpdate, SchedulerCommand};
use super::server::SharedState;
use crate::api_client::types::{
    AutoTuneRequest, BoardTelemetry, FanControlRequest, MinerPatchRequest, MinerTelemetry,
    PoolSettingsView, SettingsPatch, SettingsResponse, SourceTelemetry, TuningRequest,
};
use crate::asic::bm13xx::chip_profile;
use crate::config::{MinerSettings, PoolSettings, validate_pool_url};
use crate::tracing::prelude::*;

/// Build the v0 API routes with OpenAPI metadata.
pub fn routes() -> OpenApiRouter<SharedState> {
    OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(get_miner, patch_miner))
        .routes(routes!(get_settings, patch_settings))
        .routes(routes!(get_boards))
        .routes(routes!(get_board))
        .routes(routes!(patch_board_fan))
        .routes(routes!(patch_board_tuning))
        .routes(routes!(get_board_autotune, patch_board_autotune))
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

/// Render settings for the wire, redacting the password and reporting
/// whether the saved values differ from the ones this process is running.
fn settings_response(saved: &MinerSettings, running: &MinerSettings) -> SettingsResponse {
    SettingsResponse {
        name: saved.name.clone(),
        pool: saved.pool.as_ref().map(|p| PoolSettingsView {
            url: p.url.clone(),
            user: p.user.clone(),
            password_set: !p.password.is_empty(),
        }),
        worker_username: saved.worker_username(),
        restart_required: !settings_match(saved, running),
    }
}

/// Whether two settings describe the same pool session. Compares only what
/// the daemon acts on at startup: a differing password matters because it
/// is sent at authorize time.
fn settings_match(a: &MinerSettings, b: &MinerSettings) -> bool {
    if a.worker_username() != b.worker_username() || a.name != b.name {
        return false;
    }
    match (&a.pool, &b.pool) {
        (None, None) => true,
        (Some(x), Some(y)) => x.url == y.url && x.password == y.password,
        _ => false,
    }
}

/// Return the miner's name and pool settings.
#[utoipa::path(
    get,
    path = "/settings",
    tag = "miner",
    responses(
        (status = OK, description = "Current miner settings", body = SettingsResponse),
    ),
)]
async fn get_settings(State(state): State<SharedState>) -> Json<SettingsResponse> {
    let saved = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    Json(settings_response(&saved, &state.running_settings))
}

/// Update the miner's name and/or pool settings.
///
/// The new values are persisted immediately but are **not** applied to the
/// running daemon, which reads them once at startup; the response's
/// `restart_required` says whether a restart is now pending.
#[utoipa::path(
    patch,
    path = "/settings",
    tag = "miner",
    request_body = SettingsPatch,
    responses(
        (status = OK, description = "Updated miner settings", body = SettingsResponse),
        (status = BAD_REQUEST, description = "Empty pool user, or an unusable pool URL"),
        (status = INTERNAL_SERVER_ERROR, description = "Settings could not be saved"),
    ),
)]
async fn patch_settings(
    State(state): State<SharedState>,
    Json(req): Json<SettingsPatch>,
) -> Result<Json<SettingsResponse>, (StatusCode, &'static str)> {
    let mut saved = state.settings.lock().unwrap_or_else(|e| e.into_inner());

    // Built as a candidate and only committed to shared state once the
    // write succeeds. Mutating in place first would leave a failed save
    // reporting the new values back on the next GET, as though they had
    // been stored.
    let mut candidate = saved.clone();

    if let Some(name) = req.name {
        // An empty field is how a client clears the name; storing it as
        // Some("") would append a bare dot to the worker string.
        let name = name.trim();
        candidate.name = (!name.is_empty()).then(|| name.to_string());
    }

    if let Some(pool) = req.pool {
        let url = pool.url.trim();
        let user = pool.user.trim();
        if user.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "pool user must not be empty"));
        }
        // Checked here rather than at connect time: this is only read at
        // startup, so an unusable URL would otherwise surface after a
        // restart, with the miner down and unable to explain itself.
        validate_pool_url(url).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        // An absent password keeps the stored one, so a client that was
        // never shown the password can still edit the URL.
        let password = pool
            .password
            .or_else(|| candidate.pool.as_ref().map(|p| p.password.clone()))
            .unwrap_or_else(|| "x".to_string());
        candidate.pool = Some(PoolSettings {
            url: url.to_string(),
            user: user.to_string(),
            password,
        });
    }

    if let Err(e) = candidate.save() {
        error!(error = %e, "Failed to save miner settings");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "settings could not be written to disk",
        ));
    }
    *saved = candidate;

    let response = settings_response(&saved, &state.running_settings);
    if response.restart_required {
        info!("Miner settings saved; restart required to apply them");
    }
    Ok(Json(response))
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
    // Via miner_telemetry so each board carries its per-thread hashrate,
    // which only the scheduler measures.
    Json(state.miner_telemetry().boards)
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
        .miner_telemetry()
        .boards
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
        .miner_telemetry()
        .boards
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
            // No thread data needed: only the tuning fields are read.
            .boards(&[])
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
        .miner_telemetry()
        .boards
        .into_iter()
        .find(|b| b.name == name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Current frequency/voltage operating point of a board, from telemetry.
fn board_setpoint(state: &SharedState, name: &str) -> Option<TuneSetpoint> {
    let board = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        // No thread data needed: only the tuning fields are read.
        .boards(&[])
        .into_iter()
        .find(|b| b.name == name)?;
    let frequency_mhz = board.frequency_mhz?;
    let core_voltage_mv = board
        .powers
        .iter()
        .find(|p| p.name == "core")
        .and_then(|p| p.voltage_v)
        .map(|v| (v * 1000.0).round() as u16)?;
    Some(TuneSetpoint {
        frequency_mhz,
        core_voltage_mv,
    })
}

fn parse_profile(s: Option<&str>) -> TuneProfile {
    match s {
        Some("quiet") => TuneProfile::Quiet,
        Some("efficient") => TuneProfile::Efficient,
        Some("max_hash") => TuneProfile::MaxHash,
        _ => TuneProfile::Balanced,
    }
}

/// The board's hashrate-target slider bounds, from its reported chip
/// model and count. `None` if the board isn't found or its chip model
/// isn't recognized.
fn board_hashrate_target_range(state: &SharedState, name: &str) -> Option<TargetRange> {
    let board = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        // No thread data needed: only the tuning fields are read.
        .boards(&[])
        .into_iter()
        .find(|b| b.name == name)?;
    let chip = chip_profile::profile_for(board.chip_model.as_deref()?)?;
    Some(autotune::hashrate_target_range(chip, board.chip_count?))
}

/// Get a board's auto-tuning status and recent activity log.
#[utoipa::path(
    get,
    path = "/boards/{name}/autotune",
    tag = "boards",
    params(("name" = String, Path, description = "Board name")),
    responses((status = OK, description = "Auto-tuning status", body = AutoTuneStatus)),
)]
async fn get_board_autotune(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Json<AutoTuneStatus> {
    let setpoint = board_setpoint(&state, &name);
    let range = board_hashrate_target_range(&state, &name);
    let status = state
        .autotuner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&name)
        .status(setpoint, range);
    Json(status)
}

/// Enable or disable auto-tuning for a board, choosing either a cap-based
/// profile or a power/hashrate setpoint to converge to.
#[utoipa::path(
    patch,
    path = "/boards/{name}/autotune",
    tag = "boards",
    params(("name" = String, Path, description = "Board name")),
    request_body = AutoTuneRequest,
    responses((status = OK, description = "Updated auto-tuning status", body = AutoTuneStatus)),
)]
async fn patch_board_autotune(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<AutoTuneRequest>,
) -> Json<AutoTuneStatus> {
    let setpoint = board_setpoint(&state, &name);
    let range = board_hashrate_target_range(&state, &name);
    let mut all = state.autotuner.lock().unwrap_or_else(|e| e.into_inner());
    // Only this board's tuner is touched; the others keep running.
    let tuner = all.get_mut(&name);
    if req.enabled {
        if let Some(target) = req.target {
            tuner.enable_target(target);
        } else {
            tuner.enable_profile(parse_profile(req.profile.as_deref()));
        }
    } else {
        tuner.disable();
    }
    Json(tuner.status(setpoint, range))
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
