//! API v0 endpoints.
//!
//! Version 0 signals an unstable API -- breaking changes are expected
//! until the miner reaches 1.0.

use axum::{
    Json,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, header::CONTENT_LENGTH},
};
use bytes::{Buf, Bytes};
use futures::StreamExt;
use std::io::Write;
use std::path::{Path as FsPath, PathBuf};
use std::time::Duration;

use tokio::sync::oneshot;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::commands::SchedulerCommand;
use crate::board::nano3s_auto_tune::{AutoTuneRequest, AutoTuneStatus};
use super::server::SharedState;
use crate::api_client::types::{
    AutotuneHashrateRequest, AutotuneHashrateResponse, BoardFanRequest, BoardLedRequest,
    BoardChipClocksRequest, BoardLedState, BoardPauseRequest, BoardPowerTargetRequest, BoardTelemetry,
    BoardTempTargetRequest, BoardTuningRequest, FanCurvePoint, FanCurveRequest, FanCurveResponse,
    FirmwareBundleResponse, FirmwareUploadResponse, MinerPatchRequest, MinerTelemetry,
    PoolConfigRequest, PoolConfigResponse, PsuOverrideRequest, PsuStatusResponse, SourceTelemetry,
};

/// Upper bound on a firmware upload body -- generous headroom over the
/// real binaries (mujina-minerd ~19MB stripped, the harness ~740KB), just
/// large enough to reject obviously-wrong uploads before they hit disk.
const FIRMWARE_MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
/// Reject anything implausibly small -- catches an empty/truncated upload
/// before it ever reaches the ELF-magic check.
const FIRMWARE_MIN_UPLOAD_BYTES: usize = 64 * 1024;
/// Free space required on the target partition beyond the upload's own
/// size, so a swap never runs the partition to 0 free the way a stacked
/// series of manual deploys did earlier in this project's history.
const FIRMWARE_FREE_SPACE_MARGIN_BYTES: u64 = 2 * 1024 * 1024;

/// Build the v0 API routes with OpenAPI metadata.
pub fn routes() -> OpenApiRouter<SharedState> {
    OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(get_miner, patch_miner))
        .routes(routes!(get_boards))
        .routes(routes!(get_board))
        .routes(routes!(patch_board_tuning))
        .routes(routes!(patch_board_chip_clocks))
        .routes(routes!(patch_board_power_target))
        .routes(routes!(patch_board_temp_target))
        .routes(routes!(get_board_psu, patch_board_psu))
        .routes(routes!(get_board_autotune_hashrate, patch_board_autotune_hashrate))
        .routes(routes!(get_board_auto_tune, patch_board_auto_tune))
        .routes(routes!(get_board_tuner, patch_board_tuner))
        .routes(routes!(get_system))
        .routes(routes!(patch_board_fan))
        .routes(routes!(get_board_fan_curve, patch_board_fan_curve))
        .routes(routes!(patch_board_pause))
        .routes(routes!(get_board_led, patch_board_led))
        .routes(routes!(post_reboot))
        .routes(routes!(get_pool, patch_pool))
        .routes(routes!(post_firmware_upload))
        .routes(routes!(post_firmware_bundle))
        .routes(routes!(get_sources))
        .routes(routes!(get_source))
        .layer(DefaultBodyLimit::max(FIRMWARE_MAX_UPLOAD_BYTES))
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

/// Apply a live PLL frequency and/or core voltage change over IPC, no
/// reboot required.
///
/// Only implemented for the nano3s board driver (builds without the
/// `nano3s` Cargo feature return 501). At least one of `pll_freq_mhz`/
/// `voltage_mv`/`power_target_w`/`temp_target_c` must be set; values are
/// range-checked before being applied.
#[utoipa::path(
    patch,
    path = "/boards/{name}/tuning",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = BoardTuningRequest,
    responses(
        (status = OK, description = "Tuning command accepted and applied"),
        (status = BAD_REQUEST, description = "No fields set, or a value is outside the allowed range"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support live tuning"),
    ),
)]
async fn patch_board_tuning(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<BoardTuningRequest>,
) -> Result<StatusCode, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }

    if req.pll_freq_mhz.is_none()
        && req.voltage_mv.is_none()
        && req.power_target_w.is_none()
        && req.temp_target_c.is_none()
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Frequency range check; the four PLL ramp domains must be non-decreasing.
    if let Some(f) = req.pll_freq_mhz {
        let in_range = f.iter().all(|&mhz| (100..=520).contains(&mhz));
        let non_decreasing = f[0] <= f[1] && f[1] <= f[2] && f[2] <= f[3];
        if !in_range || !non_decreasing {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    // Voltage range check.
    if let Some(v) = req.voltage_mv
        && !(3300..=3860).contains(&v)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Same range check as the dedicated power-target endpoint.
    if let Some(w) = req.power_target_w
        && !(20.0..=137.0).contains(&w)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Same range check as the dedicated temp-target endpoint.
    if let Some(c) = req.temp_target_c
        && !(30.0..=95.0).contains(&c)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    #[cfg(feature = "nano3s")]
    {
        crate::board::nano3s::write_tuning_command(
            req.pll_freq_mhz,
            req.voltage_mv,
            req.power_target_w,
            req.temp_target_c,
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(StatusCode::OK)
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Set per-chip clock offsets (MHz), layered on the board's base clock.
///
/// `offset_mhz` must hold one in-range value per chip. Applied live with a
/// 10 MHz/s ramp and persisted across reboots. Needs an rtos_core build
/// with per-chip clock support.
#[utoipa::path(
    patch,
    path = "/boards/{name}/chip-clocks",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = BoardChipClocksRequest,
    responses(
        (status = OK, description = "Offsets queued"),
        (status = BAD_REQUEST, description = "Wrong count, or an offset outside the allowed range"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support per-chip clocks"),
    ),
)]
async fn patch_board_chip_clocks(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<BoardChipClocksRequest>,
) -> Result<StatusCode, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }

    #[cfg(feature = "nano3s")]
    {
        use crate::board::nano3s::{CHIP_COUNT, CHIP_OFFSET_RANGE_MHZ};
        let offsets: [i16; CHIP_COUNT] =
            req.offset_mhz.try_into().map_err(|_| StatusCode::BAD_REQUEST)?;
        if !offsets.iter().all(|o| CHIP_OFFSET_RANGE_MHZ.contains(o)) {
            return Err(StatusCode::BAD_REQUEST);
        }
        crate::board::nano3s::write_chip_offset_command(offsets)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(StatusCode::OK)
    }
    #[cfg(not(feature = "nano3s"))]
    {
        let _ = req;
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Live-edit the power-target voltage loop's target, no reboot required.
///
/// `target_w: null`/absent disables the loop (voltage stays wherever it
/// last was); a present value takes effect on the loop's next check.
#[utoipa::path(
    patch,
    path = "/boards/{name}/power-target",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = BoardPowerTargetRequest,
    responses(
        (status = OK, description = "Power target updated (or cleared)"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support the power-target loop"),
    ),
)]
async fn patch_board_power_target(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<BoardPowerTargetRequest>,
) -> Result<StatusCode, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }
    // Range check before the value reaches the power-target loop.
    if let Some(w) = req.target_w
        && !(20.0..=137.0).contains(&w)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    #[cfg(feature = "nano3s")]
    {
        crate::board::nano3s::write_power_target_command(req.target_w)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(StatusCode::OK)
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Live-edit the temp-target PLL frequency auto-throttle's target, no
/// reboot required.
///
/// `target_c: null`/absent disables the loop (frequency stays wherever it
/// last was, no automatic recovery); a present value takes effect on the
/// loop's next status refresh (~15s). Steps frequency down when the
/// hottest chip (`temp_max`) exceeds target and back up when comfortably
/// under, recovering only up to the last dashboard/API-commanded
/// frequency. Distinct from `PATCH /boards/{name}/fan`'s
/// `target_temp_c`, which drives fan duty instead and never touches
/// frequency.
#[utoipa::path(
    patch,
    path = "/boards/{name}/temp-target",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = BoardTempTargetRequest,
    responses(
        (status = OK, description = "Temp target updated (or cleared)"),
        (status = BAD_REQUEST, description = "target_c outside the allowed range"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support the temp-target loop"),
    ),
)]
async fn patch_board_temp_target(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<BoardTempTargetRequest>,
) -> Result<StatusCode, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }
    // Range check before the value reaches the temp-target loop.
    if let Some(c) = req.target_c
        && !(30.0..=95.0).contains(&c)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    #[cfg(feature = "nano3s")]
    {
        crate::board::nano3s::write_temp_target_command(req.target_c)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(StatusCode::OK)
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Read the current USB-C PD power-contract state -- live INA226 bus
/// voltage, HUSB238A attach status, the auto-detected contract ceiling,
/// any manual override, and the ceiling actually being enforced.
#[utoipa::path(
    get,
    path = "/boards/{name}/psu",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    responses(
        (status = OK, description = "Current PSU/PD status", body = PsuStatusResponse),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support PD detection"),
    ),
)]
async fn get_board_psu(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<PsuStatusResponse>, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }

    #[cfg(feature = "nano3s")]
    {
        let (bus_v, attached, detected_ceiling_w, override_max_w, effective_ceiling_w) =
            crate::board::nano3s::read_psu_status();
        Ok(Json(PsuStatusResponse {
            bus_v,
            attached,
            detected_ceiling_w,
            override_max_w,
            effective_ceiling_w,
        }))
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Set or clear the manual PSU power-contract override -- see
/// `PsuOverrideRequest`'s doc comment. Only ever tightens the ceiling
/// the power-target safety trip enforces, never loosens it.
#[utoipa::path(
    patch,
    path = "/boards/{name}/psu",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = PsuOverrideRequest,
    responses(
        (status = OK, description = "Override updated (or cleared)"),
        (status = BAD_REQUEST, description = "override_max_w outside the allowed range"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support PD detection"),
    ),
)]
async fn patch_board_psu(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<PsuOverrideRequest>,
) -> Result<StatusCode, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }
    if let Some(w) = req.override_max_w
        && !(15.0..=140.0).contains(&w)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    #[cfg(feature = "nano3s")]
    {
        crate::board::nano3s::write_psu_override_command(req.override_max_w)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(StatusCode::OK)
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Read the current hashrate-mode autotune state.
#[utoipa::path(
    get,
    path = "/boards/{name}/autotune-hashrate",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    responses(
        (status = OK, description = "Current autotune state", body = AutotuneHashrateResponse),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support autotune"),
    ),
)]
async fn get_board_autotune_hashrate(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<AutotuneHashrateResponse>, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }

    #[cfg(feature = "nano3s")]
    {
        let (enabled, target_ths, level, level_freq_mhz, level_voltage_mv) =
            crate::board::nano3s::read_autotune_hashrate_status();
        Ok(Json(AutotuneHashrateResponse {
            enabled,
            target_ths,
            level,
            level_freq_mhz,
            level_voltage_mv,
        }))
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Enable/edit/disable hashrate-mode autotune -- see
/// `AutotuneHashrateRequest`'s doc comment.
#[utoipa::path(
    patch,
    path = "/boards/{name}/autotune-hashrate",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = AutotuneHashrateRequest,
    responses(
        (status = OK, description = "Autotune target updated (or disabled)"),
        (status = BAD_REQUEST, description = "target_ths outside a plausible range"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support autotune"),
    ),
)]
async fn patch_board_autotune_hashrate(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<AutotuneHashrateRequest>,
) -> Result<StatusCode, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }
    // This board's real range is roughly 0.5-6 TH/s across LOW-HIGH;
    // generous headroom either side to allow testing/future hardware,
    // but reject an obviously wrong value (e.g. a watts figure typed
    // into the wrong field).
    if let Some(t) = req.target_ths
        && !(0.1..=20.0).contains(&t)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    #[cfg(feature = "nano3s")]
    {
        crate::board::nano3s::write_autotune_hashrate_command(req.target_ths)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(StatusCode::OK)
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Read AutoTune state.
#[utoipa::path(
    get,
    path = "/boards/{name}/auto-tune",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    responses(
        (status = OK, description = "Current AutoTune state", body = AutoTuneStatus),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support AutoTune"),
    ),
)]
async fn get_board_auto_tune(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<AutoTuneStatus>, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }
    #[cfg(feature = "nano3s")]
    {
        Ok(Json(crate::board::nano3s_auto_tune::read_status()))
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Enable (`quiet`/`efficient`/`balanced`/`max_hash`) or disable (`null`) the
/// AutoTune. Enabling turns off power-target, temp-target and
/// autotune-hashrate; enabling any of those, or a manual tune, turns it off.
/// Shares the single-slot control file: space writes at least 5 s apart.
#[utoipa::path(
    patch,
    path = "/boards/{name}/auto-tune",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = AutoTuneRequest,
    responses(
        (status = OK, description = "AutoTune directive queued"),
        (status = BAD_REQUEST, description = "chip_temp_target_c outside 60..88, or target outside 20..140 W / 0.5..12 TH/s"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support AutoTune"),
    ),
)]
async fn patch_board_auto_tune(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<AutoTuneRequest>,
) -> Result<StatusCode, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }
    if req.validate().is_err() {
        return Err(StatusCode::BAD_REQUEST);
    }
    #[cfg(feature = "nano3s")]
    {
        crate::board::nano3s::write_auto_tune_command(
            req.profile_change(),
            req.chip_temp_target_c,
            req.per_chip,
            req.continuous,
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(StatusCode::OK)
    }
    #[cfg(not(feature = "nano3s"))]
    {
        let _ = req;
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Unified-dashboard alias of `GET /boards/{name}/auto-tune`.
#[utoipa::path(
    get,
    path = "/boards/{name}/tuner",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    responses(
        (status = OK, description = "Current AutoTune state", body = AutoTuneStatus),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support AutoTune"),
    ),
)]
async fn get_board_tuner(
    state: State<SharedState>,
    name: Path<String>,
) -> Result<Json<AutoTuneStatus>, StatusCode> {
    get_board_auto_tune(state, name).await
}

/// Unified-dashboard alias of `PATCH /boards/{name}/auto-tune`.
#[utoipa::path(
    patch,
    path = "/boards/{name}/tuner",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = AutoTuneRequest,
    responses(
        (status = OK, description = "Tuner directive queued"),
        (status = BAD_REQUEST, description = "chip_temp_target_c outside 60..88, or target outside 20..140 W / 0.5..12 TH/s"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support AutoTune"),
    ),
)]
async fn patch_board_tuner(
    state: State<SharedState>,
    name: Path<String>,
    req: Json<AutoTuneRequest>,
) -> Result<StatusCode, StatusCode> {
    patch_board_auto_tune(state, name, req).await
}

/// Capability document for the unified dashboard (docs/unified-dashboard-spec.md).
#[utoipa::path(
    get,
    path = "/system",
    tag = "system",
    responses(
        (status = OK, description = "Mode, device, power and per-board capabilities"),
    ),
)]
async fn get_system(State(state): State<SharedState>) -> Json<serde_json::Value> {
    use super::system::{BoardCaps, SystemBoard, SystemInfo, build_sha, build_system};
    let names: Vec<(String, String)> = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .map(|b| (b.name, b.model))
        .collect();
    #[cfg(feature = "nano3s")]
    let info = {
        let (_bus_v, _attached, detected_w, override_w, effective_w) = crate::board::nano3s::read_psu_status();
        let contract_w = override_w.or(detected_w);
        let ceiling = Some(effective_w);
        let chips = crate::board::nano3s::get_detail_snapshot().map(|d| d.chips.len() as u32).filter(|n| *n > 0).unwrap_or(super::system::NANO3S_CHIPS);
        SystemInfo {
            mode: "embedded",
            platform: "nano3s-k230".into(),
            version: env!("CARGO_PKG_VERSION"),
            build: build_sha(),
            update: "bundle",
            device_model: Some("Avalon Nano 3S".into()),
            can_reboot: true,
            can_restart_miner: false,
            pool_apply: "reboot",
            shared_rail: true,
            ceiling_w: ceiling,
            psu_contract_w: contract_w,
            boards: names
                .into_iter()
                .map(|(name, model)| SystemBoard {
                    caps: BoardCaps::nano3s(&name),
                    name,
                    model: Some(model),
                    chips,
                })
                .collect(),
        }
    };
    #[cfg(not(feature = "nano3s"))]
    let info = SystemInfo {
        mode: "host",
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        version: env!("CARGO_PKG_VERSION"),
        build: build_sha(),
        update: "none",
        device_model: None,
        can_reboot: false,
        can_restart_miner: false,
        pool_apply: "restart",
        shared_rail: false,
        ceiling_w: None,
        psu_contract_w: None,
        boards: names
            .into_iter()
            .map(|(name, model)| SystemBoard { name, model: Some(model), chips: 0, caps: BoardCaps::generic() })
            .collect(),
    };
    Json(build_system(&info))
}

/// Live-edit fan control, no reboot required.
///
/// Fan commands never touch `power_en`, so they can be sent at any time
/// without affecting the mining chain.
#[utoipa::path(
    patch,
    path = "/boards/{name}/fan",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = BoardFanRequest,
    responses(
        (status = OK, description = "Fan command accepted and applied"),
        (status = BAD_REQUEST, description = "mode=\"manual\" without manual_duty_percent, an invalid mode string, or a value out of range"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support live fan control"),
    ),
)]
async fn patch_board_fan(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<BoardFanRequest>,
) -> Result<StatusCode, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }
    if req.mode.is_none() && req.manual_duty_percent.is_none() && req.chip_temp_target_c.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(t) = req.chip_temp_target_c
        && !(40.0..=88.0).contains(&t)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    #[cfg(feature = "nano3s")]
    {
        if req.mode.is_some() || req.manual_duty_percent.is_some() {
            // Always exactly 2 comma-separated fields (mode, duty);
            // empty means "don't change". Parsed by
            // mujina_test_harness.c's control_poll_loop().
            let pct = match req.mode.as_deref() {
                Some("manual") => Some(req.manual_duty_percent.ok_or(StatusCode::BAD_REQUEST)?),
                Some("auto") => None,
                Some(_) => return Err(StatusCode::BAD_REQUEST),
                None => None,
            };
            if req.mode.as_deref() == Some("manual") && pct.is_some_and(|p| p > 100) {
                return Err(StatusCode::BAD_REQUEST);
            }
            let fields = [
                req.mode.clone().unwrap_or_default(),
                pct.map(|p| p.to_string()).unwrap_or_default(),
            ];
            crate::board::nano3s::write_fan_control_command(&format!("fan:{}", fields.join(",")))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        }
        if let Some(t) = req.chip_temp_target_c {
            crate::board::nano3s::write_fan_control_command(&format!("fan_chip_temp_target:{t}"))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        }
        Ok(StatusCode::OK)
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Return the currently persisted/applied fan curve.
#[utoipa::path(
    get,
    path = "/boards/{name}/fan-curve",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    responses(
        (status = OK, description = "Current fan curve points, sorted ascending by temp_c", body = FanCurveResponse),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support a fan curve"),
    ),
)]
async fn get_board_fan_curve(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<FanCurveResponse>, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }

    #[cfg(feature = "nano3s")]
    {
        let points = crate::board::nano3s::read_fan_curve()
            .into_iter()
            .map(|(temp_c, duty_pct)| FanCurvePoint { temp_c, duty_pct })
            .collect();
        Ok(Json(FanCurveResponse { points }))
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Set the fan's outlet-temp baseline curve, no reboot required.
///
/// 2-8 points, any order (sorted by `temp_c` before being applied). This
/// is only the baseline -- it's overridden by 100% whenever the hottest
/// chip crosses the active mode's real tuning limit
/// (`PATCH /boards/{name}/temp-target`'s design), independent of outlet
/// temp. Applied live within ~1s and persisted across reboots.
#[utoipa::path(
    patch,
    path = "/boards/{name}/fan-curve",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = FanCurveRequest,
    responses(
        (status = OK, description = "Fan curve updated", body = FanCurveResponse),
        (status = BAD_REQUEST, description = "Fewer than 2 or more than 8 points, or a point outside the allowed range"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support a fan curve"),
    ),
)]
async fn patch_board_fan_curve(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<FanCurveRequest>,
) -> Result<Json<FanCurveResponse>, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }
    if req.points.len() < 2 || req.points.len() > 8 {
        return Err(StatusCode::BAD_REQUEST);
    }
    for p in &req.points {
        if !(0.0..=150.0).contains(&p.temp_c) || !(0.0..=100.0).contains(&p.duty_pct) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    #[cfg(feature = "nano3s")]
    {
        let points: Vec<(f64, f64)> = req.points.iter().map(|p| (p.temp_c, p.duty_pct)).collect();
        crate::board::nano3s::write_fan_curve_command(&points)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let mut sorted = req.points;
        sorted.sort_by(|a, b| a.temp_c.total_cmp(&b.temp_c));
        Ok(Json(FanCurveResponse { points: sorted }))
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Pause or resume mining, no reboot required.
///
/// Goes through `board::nano3s::write_pause_command()`, an
/// IPC-coordinated path distinct from the generic
/// `PATCH /api/v0/miner {"paused": ...}` endpoint (which only sets a
/// scheduler flag and does not reach hardware for this board). Pause
/// idles the chain; resume reapplies its operating frequency and voltage.
#[utoipa::path(
    patch,
    path = "/boards/{name}/pause",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = BoardPauseRequest,
    responses(
        (status = OK, description = "Pause/resume command accepted"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support live pause control"),
    ),
)]
async fn patch_board_pause(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<BoardPauseRequest>,
) -> Result<StatusCode, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }

    #[cfg(feature = "nano3s")]
    {
        crate::board::nano3s::write_pause_command(req.paused)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(StatusCode::OK)
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Return the currently commanded LED state.
#[utoipa::path(
    get,
    path = "/boards/{name}/led",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    responses(
        (status = OK, description = "Current LED state", body = BoardLedState),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support LED control"),
    ),
)]
async fn get_board_led(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<BoardLedState>, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }

    #[cfg(feature = "nano3s")]
    {
        Ok(Json(crate::board::nano3s::get_led_state()))
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Live-edit the status LED strip, no reboot required.
///
/// `effect="auto"` (the default) returns the strip to automatic status
/// indication; any other effect hands it to manual control until
/// switched back. Fields left unset keep their current value.
#[utoipa::path(
    patch,
    path = "/boards/{name}/led",
    tag = "boards",
    params(
        ("name" = String, Path, description = "Board name"),
    ),
    request_body = BoardLedRequest,
    responses(
        (status = OK, description = "LED command accepted and applied"),
        (status = BAD_REQUEST, description = "Unknown effect name or malformed color"),
        (status = NOT_FOUND, description = "Board not found"),
        (status = NOT_IMPLEMENTED, description = "This build's board driver doesn't support LED control"),
    ),
)]
async fn patch_board_led(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<BoardLedRequest>,
) -> Result<StatusCode, StatusCode> {
    let known = state
        .board_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boards()
        .into_iter()
        .any(|b| b.name == name);
    if !known {
        return Err(StatusCode::NOT_FOUND);
    }

    #[cfg(feature = "nano3s")]
    {
        crate::board::nano3s::write_led_command(req.effect, req.color, req.brightness, req.speed)
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        Ok(StatusCode::OK)
    }
    #[cfg(not(feature = "nano3s"))]
    {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

/// Reboot the device.
///
/// Shells out to the system `reboot` command. Responds `OK` immediately;
/// the actual reboot is spawned after a short delay on a detached task so
/// the HTTP response reaches the client before the connection drops.
#[utoipa::path(
    post,
    path = "/reboot",
    tag = "miner",
    responses(
        (status = OK, description = "Reboot initiated"),
    ),
)]
async fn post_reboot() -> StatusCode {
    trigger_system_reboot();
    StatusCode::OK
}

/// Live path this binary is actually deployed to, and its holding
/// filesystem -- used for the pre-write free-space check.
fn firmware_target_path(target: &str) -> Option<(PathBuf, PathBuf)> {
    match target {
        "mujina-minerd" => Some((PathBuf::from("/data/mujina-minerd"), PathBuf::from("/data"))),
        "harness" => Some((
            PathBuf::from("/mntapp/release/linux/app/mujina_test_harness"),
            PathBuf::from("/mntapp"),
        )),
        _ => None,
    }
}

/// Remove any orphaned `<live_path>.new` staging files left behind by an
/// interrupted or OOM-killed OTA upload so they don't consume `/data` or
/// `/mntapp` space on subsequent boots.
pub(crate) fn cleanup_stale_staged_firmware() {
    for target in ["mujina-minerd", "harness"] {
        if let Some((live_path, _)) = firmware_target_path(target) {
            let staged_path = staged_firmware_path(&live_path);
            match std::fs::remove_file(&staged_path) {
                Ok(()) => {
                    tracing::info!(path = %staged_path.display(), "Removed stale staged firmware file");
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(
                        path = %staged_path.display(),
                        error = %e,
                        "Failed to remove stale staged firmware file"
                    );
                }
            }
        }
    }
}

fn staged_firmware_path(live_path: &FsPath) -> PathBuf {
    let mut s = live_path.as_os_str().to_os_string();
    s.push(".new");
    PathBuf::from(s)
}

/// Free space on `mount_point`, in bytes, via `df -k` -- no new crate
/// dependency, consistent with this codebase's existing pattern of
/// shelling out to system tools (i2cset, reboot, etc.) for one-off system
/// queries rather than linking libc/statvfs directly.
fn free_space_bytes(mount_point: &FsPath) -> Option<u64> {
    let out = std::process::Command::new("df").arg("-k").arg(mount_point).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    // `df -k` columns: Filesystem, 1K-blocks, Used, Available, Use%, Mounted.
    fields.get(3)?.parse::<u64>().ok().map(|kb| kb * 1024)
}

/// Drop guard that unlinks any staged `<live_path>.new` files if an upload
/// aborts or returns an error before all sections are swapped into place.
struct StagedCleanupGuard {
    paths: Vec<PathBuf>,
    active: bool,
}

impl StagedCleanupGuard {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            active: true,
        }
    }

    fn track(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    fn defuse(&mut self) {
        self.active = false;
    }
}

impl Drop for StagedCleanupGuard {
    fn drop(&mut self) {
        if self.active {
            for path in &self.paths {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Incremental byte stream reader over `axum::body::Body::into_data_stream()`
/// that never buffers the entire upload in RAM.
struct StreamReader<S> {
    stream: S,
    current: Bytes,
}

impl<S, E> StreamReader<S>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    fn new(stream: S) -> Self {
        Self {
            stream,
            current: Bytes::new(),
        }
    }

    async fn next_chunk(&mut self) -> Result<Option<Bytes>, (StatusCode, String)> {
        if !self.current.is_empty() {
            return Ok(Some(std::mem::take(&mut self.current)));
        }
        while let Some(res) = self.stream.next().await {
            let chunk = res.map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("malformed upload: stream read error: {e}"),
                )
            })?;
            if !chunk.is_empty() {
                return Ok(Some(chunk));
            }
        }
        Ok(None)
    }

    fn push_back(&mut self, rest: Bytes) {
        if !rest.is_empty() {
            debug_assert!(self.current.is_empty());
            self.current = rest;
        }
    }

    /// Fill `buf` completely from the stream, returning `Ok(false)` if EOF is
    /// reached before `buf` is full.
    async fn read_exact_or_eof(&mut self, buf: &mut [u8]) -> Result<bool, (StatusCode, String)> {
        let mut filled = 0;
        while filled < buf.len() {
            let Some(mut chunk) = self.next_chunk().await? else {
                return Ok(false);
            };
            let take = (buf.len() - filled).min(chunk.len());
            buf[filled..filled + take].copy_from_slice(&chunk[..take]);
            chunk.advance(take);
            if !chunk.is_empty() {
                self.push_back(chunk);
            }
            filled += take;
        }
        Ok(true)
    }
}

/// Streaming validator for a single firmware ELF binary: verifies `\x7fELF` on
/// the first 4 bytes and, for `mujina-minerd`, scans for `nano3s_ipc_` across
/// arbitrary chunk boundaries using a 10-byte rolling tail.
struct SectionValidator<'a> {
    target: &'a str,
    elf_header: [u8; 4],
    elf_len: usize,
    check_nano3s_ipc: bool,
    found_ipc: bool,
    tail: [u8; 10],
    tail_len: usize,
}

impl<'a> SectionValidator<'a> {
    const IPC_NEEDLE: &'static [u8] = b"nano3s_ipc_";
    const TAIL_CAP: usize = 10; // IPC_NEEDLE.len() - 1

    fn new(target: &'a str) -> Self {
        Self {
            target,
            elf_header: [0; 4],
            elf_len: 0,
            check_nano3s_ipc: target == "mujina-minerd",
            found_ipc: false,
            tail: [0; Self::TAIL_CAP],
            tail_len: 0,
        }
    }

    fn feed(&mut self, data: &[u8]) -> Result<(), (StatusCode, String)> {
        if data.is_empty() {
            return Ok(());
        }
        if self.elf_len < 4 {
            let take = (4 - self.elf_len).min(data.len());
            self.elf_header[self.elf_len..self.elf_len + take].copy_from_slice(&data[..take]);
            self.elf_len += take;
            if self.elf_len == 4 && &self.elf_header != b"\x7fELF" {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("{}: not an ELF binary (bad magic)", self.target),
                ));
            }
        }

        if self.check_nano3s_ipc && !self.found_ipc {
            if self.tail_len > 0 {
                let head_take = Self::TAIL_CAP.min(data.len());
                let mut overlap = [0u8; Self::TAIL_CAP * 2];
                overlap[..self.tail_len].copy_from_slice(&self.tail[..self.tail_len]);
                overlap[self.tail_len..self.tail_len + head_take]
                    .copy_from_slice(&data[..head_take]);
                if overlap[..self.tail_len + head_take]
                    .windows(Self::IPC_NEEDLE.len())
                    .any(|w| w == Self::IPC_NEEDLE)
                {
                    self.found_ipc = true;
                }
            }

            if !self.found_ipc && data.len() >= Self::IPC_NEEDLE.len() {
                if data
                    .windows(Self::IPC_NEEDLE.len())
                    .any(|w| w == Self::IPC_NEEDLE)
                {
                    self.found_ipc = true;
                }
            }

            if !self.found_ipc {
                if data.len() >= Self::TAIL_CAP {
                    self.tail.copy_from_slice(&data[data.len() - Self::TAIL_CAP..]);
                    self.tail_len = Self::TAIL_CAP;
                } else {
                    let combined = self.tail_len + data.len();
                    if combined > Self::TAIL_CAP {
                        let drop = combined - Self::TAIL_CAP;
                        self.tail.copy_within(drop..self.tail_len, 0);
                        self.tail_len -= drop;
                    }
                    self.tail[self.tail_len..self.tail_len + data.len()].copy_from_slice(data);
                    self.tail_len += data.len();
                }
            }
        }

        Ok(())
    }

    fn finish(self) -> Result<(), (StatusCode, String)> {
        if self.elf_len < 4 || &self.elf_header != b"\x7fELF" {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{}: not an ELF binary (bad magic)", self.target),
            ));
        }
        if self.check_nano3s_ipc && !self.found_ipc {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "{}: no nano3s_ipc_* symbols found in this binary -- it was very likely built \
                     without the nano3s Cargo feature and would silently run with no board driver at all",
                    self.target
                ),
            ));
        }
        Ok(())
    }
}

fn check_free_space(
    target: &str,
    mount_point: &FsPath,
    needed_upload_bytes: u64,
    free_space: &(dyn Fn(&FsPath) -> Option<u64> + Sync),
) -> Result<(), (StatusCode, String)> {
    let needed = needed_upload_bytes + FIRMWARE_FREE_SPACE_MARGIN_BYTES;
    match free_space(mount_point) {
        Some(free) if free < needed => Err((
            StatusCode::INSUFFICIENT_STORAGE,
            format!(
                "{target}: not enough free space on {}: {free} bytes free, need {needed} \
                 (upload size + {FIRMWARE_FREE_SPACE_MARGIN_BYTES}-byte margin)",
                mount_point.display()
            ),
        )),
        Some(_) => Ok(()),
        None => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{target}: couldn't determine free space on {}", mount_point.display()),
        )),
    }
}

/// Process-wide mutual exclusion for firmware uploads so concurrent requests
/// (e.g. a user retry after client timeout, or a double-click) cannot unlink
/// and overwrite `<live_path>.new` while another upload is staging or swapping.
static FIRMWARE_UPLOAD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn finalize_staged_file(
    target: &str,
    mut file: std::fs::File,
    staged_path: &FsPath,
) -> Result<(), (StatusCode, String)> {
    file.flush().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{target}: failed to flush {}: {e}", staged_path.display()),
        )
    })?;
    file.sync_all().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{target}: failed to sync {}: {e}", staged_path.display()),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(staged_path, std::fs::Permissions::from_mode(0o755)).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{target}: failed to chmod {}: {e}", staged_path.display()),
            )
        })?;
    }
    Ok(())
}

async fn finalize_staged_file_async(
    target: &'static str,
    file: std::fs::File,
    staged_path: PathBuf,
) -> Result<(), (StatusCode, String)> {
    tokio::task::spawn_blocking(move || finalize_staged_file(target, file, &staged_path))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{target}: finalize task failed: {e}"),
            )
        })?
}

async fn swap_staged_files_async(
    pairs: Vec<(String, PathBuf, PathBuf)>,
) -> Result<(), (StatusCode, String)> {
    tokio::task::spawn_blocking(move || {
        for (target, live_path, staged_path) in &pairs {
            std::fs::rename(staged_path, live_path).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "{target}: staged but failed to swap in {}: {e}",
                        live_path.display()
                    ),
                )
            })?;
            if let Some(parent) = live_path.parent()
                && let Ok(dir) = std::fs::File::open(parent)
            {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("swap task failed: {e}"),
        )
    })?
}

/// Upload and apply a new `mujina-minerd` or harness binary, then reboot.
///
/// `target` is `mujina-minerd` or `harness`. Body is the raw ELF binary
/// (`application/octet-stream`), no multipart wrapper. Streams directly to
/// `<path>.new` without buffering the upload in RAM, validating ELF magic and,
/// for `mujina-minerd` specifically, the presence of `nano3s_ipc_` symbol
/// strings -- the same sanity check `build_mujina_minerd.sh` itself runs,
/// catching a build that silently compiled out the `nano3s` Cargo feature.
///
/// One-step by design: on success, the binary is already swapped in when
/// this responds, and a full device reboot is already in flight (not a
/// process-only restart -- swapping either binary in place and only
/// killing the process leaves the RT-Smart core's IPC handle stale, a
/// real, previously-hit failure mode for `mujina-minerd`; the harness
/// holds no such handle but reboots the same way for consistency and
/// because a fresh boot is the only state this endpoint has actually
/// verified working). There is no confirmation step -- a successful
/// upload commits to applying it.
#[utoipa::path(
    post,
    path = "/firmware/{target}",
    tag = "miner",
    params(
        ("target" = String, Path, description = "\"mujina-minerd\" or \"harness\""),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = OK, description = "Binary swapped in, reboot initiated", body = FirmwareUploadResponse),
        (status = BAD_REQUEST, description = "Unknown target, empty/oversized body, bad ELF magic, or (mujina-minerd) missing nano3s feature symbols"),
        (status = INSUFFICIENT_STORAGE, description = "Not enough free space on the target partition"),
        (status = INTERNAL_SERVER_ERROR, description = "Write or rename failed"),
    ),
)]
async fn post_firmware_upload(
    Path(target): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<FirmwareUploadResponse>, (StatusCode, String)> {
    let content_length = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    handle_firmware_upload_with(
        target,
        content_length,
        body,
        &FIRMWARE_UPLOAD_LOCK,
        &firmware_target_path,
        &free_space_bytes,
        schedule_reboot,
    )
    .await
}

async fn handle_firmware_upload_with<F>(
    target: String,
    content_length: Option<u64>,
    body: Body,
    upload_lock: &tokio::sync::Mutex<()>,
    resolve_target: &(dyn Fn(&str) -> Option<(PathBuf, PathBuf)> + Sync),
    free_space: &(dyn Fn(&FsPath) -> Option<u64> + Sync),
    on_reboot: F,
) -> Result<Json<FirmwareUploadResponse>, (StatusCode, String)>
where
    F: FnOnce() + Send,
{
    let _upload_guard = upload_lock.try_lock().map_err(|_| {
        (
            StatusCode::CONFLICT,
            "firmware update already in progress".to_string(),
        )
    })?;

    let Some((live_path, mount_point)) = resolve_target(&target) else {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown target {target:?} (want \"mujina-minerd\" or \"harness\")"),
        ));
    };

    let staged_path = staged_firmware_path(&live_path);
    // Unlink any stale `.new` BEFORE checking free space so a previous aborted
    // or OOM-killed upload never causes a false 507 INSUFFICIENT_STORAGE.
    let _ = std::fs::remove_file(&staged_path);

    if let Some(len) = content_length {
        if len < FIRMWARE_MIN_UPLOAD_BYTES as u64 {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{target}: upload too small ({len} bytes) -- looks empty or truncated"),
            ));
        }
        if len > FIRMWARE_MAX_UPLOAD_BYTES as u64 {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{target}: upload too large ({len} bytes)"),
            ));
        }
        check_free_space(&target, &mount_point, len, free_space)?;
    } else {
        check_free_space(&target, &mount_point, FIRMWARE_MIN_UPLOAD_BYTES as u64, free_space)?;
    }

    let mut guard = StagedCleanupGuard::new();
    let mut file = std::fs::File::create(&staged_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{target}: failed to create {}: {e}", staged_path.display()),
        )
    })?;
    guard.track(staged_path.clone());

    let mut reader = StreamReader::new(body.into_data_stream());
    let mut validator = SectionValidator::new(&target);
    let mut total_bytes: usize = 0;

    while let Some(chunk) = reader.next_chunk().await? {
        let next_total = total_bytes.saturating_add(chunk.len());
        if next_total > FIRMWARE_MAX_UPLOAD_BYTES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{target}: upload too large (> {FIRMWARE_MAX_UPLOAD_BYTES} bytes)"),
            ));
        }
        validator.feed(&chunk)?;
        file.write_all(&chunk).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{target}: failed to write {}: {e}", staged_path.display()),
            )
        })?;
        total_bytes = next_total;
    }

    if total_bytes < FIRMWARE_MIN_UPLOAD_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{target}: upload too small ({total_bytes} bytes) -- looks empty or truncated"),
        ));
    }
    if let Some(expected) = content_length
        && total_bytes as u64 != expected
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{target}: upload truncated ({total_bytes} of {expected} bytes)"),
        ));
    }
    if content_length.is_none() {
        check_free_space(&target, &mount_point, 0, free_space)?;
    }

    validator.finish()?;
    let target_label: &'static str = if target == "mujina-minerd" {
        "mujina-minerd"
    } else {
        "harness"
    };
    finalize_staged_file_async(target_label, file, staged_path.clone()).await?;
    swap_staged_files_async(vec![(target.clone(), live_path, staged_path)]).await?;
    guard.defuse();

    on_reboot();

    Ok(Json(FirmwareUploadResponse {
        ok: true,
        target,
        size: total_bytes,
        detail: "swapped in, rebooting now".to_string(),
    }))
}

fn trigger_system_reboot() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Signal supervisor watchdog via /sharefs
        let _ = std::fs::write("/sharefs/reboot_request", b"reboot");
        // Flush filesystem buffers
        let _ = std::process::Command::new("/bin/sync").status();
        let _ = std::process::Command::new("sync").status();
        // Execute immediate hardware reboot
        let res = std::process::Command::new("/sbin/reboot").arg("-f").status();
        if res.is_err() {
            let _ = std::process::Command::new("reboot").arg("-f").status();
        }
    });
}

fn schedule_reboot() {
    trigger_system_reboot();
}

/// Bundle format for `POST /firmware/bundle`:
/// 8-byte magic `b"MJBUNDL1"`, then for each of `mujina-minerd` and `harness`
/// in that fixed order: a 4-byte little-endian length followed by that
/// many bytes (length 0 means "not included in this bundle" -- the
/// bundle only needs to touch one binary if that's all the caller wants
/// to change, while still going through the single-reboot bundle path).
/// See `pack_firmware_bundle.py` for the matching packer.
const BUNDLE_MAGIC: &[u8; 8] = b"MJBUNDL1";

/// Upload one file that updates mujina-minerd and/or the harness in a
/// single reboot, instead of running the single-target endpoint twice
/// (which would reboot twice -- once per upload). See `pack_firmware_bundle.py`
/// for the packer that builds one from locally-built binaries.
///
/// Both included binaries are streamed directly to `<path>.new`, validated
/// incrementally (same checks as the single-target endpoint), and synced
/// *before* either is renamed into place, so a bad second binary can't leave
/// the pair mismatched -- either both apply or neither does.
#[utoipa::path(
    post,
    path = "/firmware/bundle",
    tag = "miner",
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = OK, description = "All included binaries swapped in, reboot initiated", body = FirmwareBundleResponse),
        (status = BAD_REQUEST, description = "Malformed bundle, or a section failed the same checks the single-target endpoint runs"),
        (status = INSUFFICIENT_STORAGE, description = "Not enough free space for one of the included binaries"),
        (status = INTERNAL_SERVER_ERROR, description = "Write, chmod, or rename failed"),
    ),
)]
async fn post_firmware_bundle(body: Body) -> Result<Json<FirmwareBundleResponse>, (StatusCode, String)> {
    handle_firmware_bundle_with(
        body,
        &FIRMWARE_UPLOAD_LOCK,
        &firmware_target_path,
        &free_space_bytes,
        schedule_reboot,
    )
    .await
}

async fn handle_firmware_bundle_with<F>(
    body: Body,
    upload_lock: &tokio::sync::Mutex<()>,
    resolve_target: &(dyn Fn(&str) -> Option<(PathBuf, PathBuf)> + Sync),
    free_space: &(dyn Fn(&FsPath) -> Option<u64> + Sync),
    on_reboot: F,
) -> Result<Json<FirmwareBundleResponse>, (StatusCode, String)>
where
    F: FnOnce() + Send,
{
    let _upload_guard = upload_lock.try_lock().map_err(|_| {
        (
            StatusCode::CONFLICT,
            "firmware update already in progress".to_string(),
        )
    })?;

    let bad = |msg: &str| (StatusCode::BAD_REQUEST, format!("malformed bundle: {msg}"));
    let mut reader = StreamReader::new(body.into_data_stream());

    let mut magic = [0u8; 8];
    if !reader.read_exact_or_eof(&mut magic).await? || &magic != BUNDLE_MAGIC {
        return Err(bad(
            "bad magic (expected \"MJBUNDL1\") -- build the bundle with pack_firmware_bundle.py, don't concatenate the binaries by hand",
        ));
    }

    let mut guard = StagedCleanupGuard::new();
    let mut staged: Vec<(&'static str, PathBuf, PathBuf)> = Vec::new();

    for target in ["mujina-minerd", "harness"] {
        let mut len_bytes = [0u8; 4];
        if !reader.read_exact_or_eof(&mut len_bytes).await? {
            return Err(bad("truncated -- missing a length field"));
        }
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len == 0 {
            continue;
        }

        if len < FIRMWARE_MIN_UPLOAD_BYTES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{target}: upload too small ({len} bytes) -- looks empty or truncated"),
            ));
        }
        if len > FIRMWARE_MAX_UPLOAD_BYTES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{target}: upload too large ({len} bytes)"),
            ));
        }

        let Some((live_path, mount_point)) = resolve_target(target) else {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("unknown target {target:?}"),
            ));
        };
        let staged_path = staged_firmware_path(&live_path);

        // Unlink any stale `.new` BEFORE checking free space so a previous
        // aborted upload doesn't cause a false 507 INSUFFICIENT_STORAGE.
        let _ = std::fs::remove_file(&staged_path);
        check_free_space(target, &mount_point, len as u64, free_space)?;

        let mut file = std::fs::File::create(&staged_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{target}: failed to create {}: {e}", staged_path.display()),
            )
        })?;
        guard.track(staged_path.clone());

        let mut validator = SectionValidator::new(target);
        let mut remaining = len;
        while remaining > 0 {
            let Some(mut chunk) = reader.next_chunk().await? else {
                return Err(bad("truncated -- section length runs past end of file"));
            };
            let take = remaining.min(chunk.len());
            let slice = &chunk[..take];
            validator.feed(slice)?;
            file.write_all(slice).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("{target}: failed to write {}: {e}", staged_path.display()),
                )
            })?;
            chunk.advance(take);
            if !chunk.is_empty() {
                reader.push_back(chunk);
            }
            remaining -= take;
        }

        validator.finish()?;
        finalize_staged_file_async(target, file, staged_path.clone()).await?;
        staged.push((target, live_path, staged_path));
    }

    if staged.is_empty() {
        return Err(bad("both sections empty -- nothing to apply"));
    }

    if reader.next_chunk().await?.is_some() {
        return Err(bad("trailing bytes after section 2"));
    }

    let swap_pairs = staged
        .iter()
        .map(|(target, live, staged_p)| ((*target).to_string(), live.clone(), staged_p.clone()))
        .collect();
    swap_staged_files_async(swap_pairs).await?;
    guard.defuse();

    on_reboot();

    Ok(Json(FirmwareBundleResponse {
        ok: true,
        applied: staged.iter().map(|(target, _, _)| (*target).to_string()).collect(),
        detail: "swapped in, rebooting now".to_string(),
    }))
}

/// Return the currently persisted pool configuration.
///
/// This is what will apply on the miner's *next* restart, not necessarily
/// what the running process connected with at its own last startup -- see
/// `GET /api/v0/sources` for the live connection's actual URL.
#[utoipa::path(
    get,
    path = "/pool",
    tag = "miner",
    responses(
        (status = OK, description = "Currently persisted pool config", body = PoolConfigResponse),
    ),
)]
async fn get_pool() -> Json<PoolConfigResponse> {
    let cfg = crate::pool_config::load();
    Json(PoolConfigResponse {
        url: cfg.url,
        user: cfg.user,
        password_set: cfg.pass.is_some(),
    })
}

/// Update the persisted pool configuration.
///
/// Saved to `/data/userconfig/pool.conf` immediately; takes effect on the
/// miner's next startup since the stratum client has no hot-reload path.
/// Applying it requires a full device reboot (`POST /api/v0/reboot`), not
/// just restarting the `mujina-minerd` process -- killing and relaunching
/// the process in place leaves the RT-Smart core's IPC handle stale
/// (hashrate stuck at 0 even though the API looks healthy), so this API
/// deliberately does not offer a lighter process-only restart.
#[utoipa::path(
    patch,
    path = "/pool",
    tag = "miner",
    request_body = PoolConfigRequest,
    responses(
        (status = OK, description = "Pool config saved", body = PoolConfigResponse),
        (status = BAD_REQUEST, description = "No fields set, or url isn't a stratum+tcp://\
            or stratum+ssl:// URL"),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to write the config file"),
    ),
)]
async fn patch_pool(Json(req): Json<PoolConfigRequest>) -> Result<Json<PoolConfigResponse>, StatusCode> {
    if req.url.is_none() && req.user.is_none() && req.password.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(url) = &req.url
        && !(url.starts_with("stratum+tcp://") || url.starts_with("stratum+ssl://"))
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let cfg = crate::pool_config::save_merged(req.url.as_deref(), req.user.as_deref(), req.password.as_deref())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(PoolConfigResponse {
        url: cfg.url,
        user: cfg.user,
        password_set: cfg.pass.is_some(),
    }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestTempDir {
        path: PathBuf,
    }

    impl TestTempDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mujina-v0-firmware-test-{}-{}-{id}",
                std::process::id(),
                label
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp test dir");
            Self { path }
        }

        fn minerd_live(&self) -> PathBuf {
            self.path.join("mujina-minerd")
        }

        fn harness_live(&self) -> PathBuf {
            self.path.join("mujina_test_harness")
        }

        fn resolver(&self) -> impl Fn(&str) -> Option<(PathBuf, PathBuf)> + '_ {
            move |target| match target {
                "mujina-minerd" => Some((self.minerd_live(), self.path.clone())),
                "harness" => Some((self.harness_live(), self.path.clone())),
                _ => None,
            }
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn make_elf(size: usize, marker: Option<(&[u8], usize)>) -> Vec<u8> {
        assert!(size >= 4);
        let mut buf = vec![0xAAu8; size];
        buf[..4].copy_from_slice(b"\x7fELF");
        if let Some((bytes, offset)) = marker {
            buf[offset..offset + bytes.len()].copy_from_slice(bytes);
        }
        buf
    }

    fn make_bundle(minerd: Option<&[u8]>, harness: Option<&[u8]>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(BUNDLE_MAGIC);
        for sec in [minerd, harness] {
            match sec {
                Some(data) => {
                    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                    out.extend_from_slice(data);
                }
                None => {
                    out.extend_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        out
    }

    fn chunked_body(data: Vec<u8>, chunk_size: usize) -> Body {
        let chunks: Vec<Result<Bytes, std::io::Error>> = data
            .chunks(chunk_size)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        Body::from_stream(futures::stream::iter(chunks))
    }

    fn split_at_offsets_body(data: &[u8], split_offsets: &[usize]) -> Body {
        let mut chunks = Vec::new();
        let mut prev = 0;
        for &off in split_offsets {
            if off > prev && off <= data.len() {
                chunks.push(Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(
                    &data[prev..off],
                )));
                prev = off;
            }
        }
        if prev < data.len() {
            chunks.push(Ok(Bytes::copy_from_slice(&data[prev..])));
        }
        Body::from_stream(futures::stream::iter(chunks))
    }

    #[tokio::test]
    async fn valid_streaming_bundle_stages_and_swaps() {
        let dir = TestTempDir::new("valid_bundle");
        let minerd = make_elf(FIRMWARE_MIN_UPLOAD_BYTES, Some((b"nano3s_ipc_send", 1024)));
        let harness = make_elf(FIRMWARE_MIN_UPLOAD_BYTES + 123, None);
        let bundle = make_bundle(Some(&minerd), Some(&harness));

        let rebooted = AtomicBool::new(false);
        let res = handle_firmware_bundle_with(
            chunked_body(bundle, 1024),
            &tokio::sync::Mutex::new(()),
            &dir.resolver(),
            &|_| Some(256 * 1024 * 1024),
            || rebooted.store(true, Ordering::SeqCst),
        )
        .await
        .expect("valid bundle should succeed");

        assert!(res.0.ok);
        assert_eq!(res.0.applied, vec!["mujina-minerd", "harness"]);
        assert!(rebooted.load(Ordering::SeqCst));
        assert_eq!(std::fs::read(dir.minerd_live()).unwrap(), minerd);
        assert_eq!(std::fs::read(dir.harness_live()).unwrap(), harness);
        assert!(!staged_firmware_path(&dir.minerd_live()).exists());
        assert!(!staged_firmware_path(&dir.harness_live()).exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.minerd_live())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755);
        }
    }

    #[tokio::test]
    async fn bundle_bad_magic_rejected() {
        let dir = TestTempDir::new("bad_magic");
        let mut bundle = make_bundle(None, Some(&make_elf(FIRMWARE_MIN_UPLOAD_BYTES, None)));
        bundle[..8].copy_from_slice(b"BADMAGIC");

        let rebooted = AtomicBool::new(false);
        let (status, msg) = handle_firmware_bundle_with(
            chunked_body(bundle, 512),
            &tokio::sync::Mutex::new(()),
            &dir.resolver(),
            &|_| Some(256 * 1024 * 1024),
            || rebooted.store(true, Ordering::SeqCst),
        )
        .await
        .unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("bad magic"), "unexpected msg: {msg}");
        assert!(!rebooted.load(Ordering::SeqCst));
        assert!(!dir.harness_live().exists());
    }

    #[tokio::test]
    async fn bundle_truncated_stream_cleans_up_all_staged_files() {
        let dir = TestTempDir::new("truncated");
        let minerd = make_elf(FIRMWARE_MIN_UPLOAD_BYTES, Some((b"nano3s_ipc_send", 512)));
        let harness = make_elf(FIRMWARE_MIN_UPLOAD_BYTES + 1000, None);
        let mut bundle = make_bundle(Some(&minerd), Some(&harness));
        // Truncate inside section 2 after section 1 has already been staged.
        bundle.truncate(bundle.len() - 500);

        let rebooted = AtomicBool::new(false);
        let (status, msg) = handle_firmware_bundle_with(
            chunked_body(bundle, 4096),
            &tokio::sync::Mutex::new(()),
            &dir.resolver(),
            &|_| Some(256 * 1024 * 1024),
            || rebooted.store(true, Ordering::SeqCst),
        )
        .await
        .unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("truncated"), "unexpected msg: {msg}");
        assert!(!rebooted.load(Ordering::SeqCst));
        assert!(!dir.minerd_live().exists());
        assert!(!dir.harness_live().exists());
        assert!(!staged_firmware_path(&dir.minerd_live()).exists());
        assert!(!staged_firmware_path(&dir.harness_live()).exists());
    }

    #[tokio::test]
    async fn bundle_trailing_bytes_rejected_and_cleaned_up() {
        let dir = TestTempDir::new("trailing");
        let minerd = make_elf(FIRMWARE_MIN_UPLOAD_BYTES, Some((b"nano3s_ipc_send", 512)));
        let harness = make_elf(FIRMWARE_MIN_UPLOAD_BYTES, None);
        let mut bundle = make_bundle(Some(&minerd), Some(&harness));
        bundle.extend_from_slice(b"EXTRA_TRAILING_BYTES");

        let rebooted = AtomicBool::new(false);
        let (status, msg) = handle_firmware_bundle_with(
            chunked_body(bundle, 4096),
            &tokio::sync::Mutex::new(()),
            &dir.resolver(),
            &|_| Some(256 * 1024 * 1024),
            || rebooted.store(true, Ordering::SeqCst),
        )
        .await
        .unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("trailing bytes"), "unexpected msg: {msg}");
        assert!(!rebooted.load(Ordering::SeqCst));
        assert!(!dir.minerd_live().exists());
        assert!(!dir.harness_live().exists());
        assert!(!staged_firmware_path(&dir.minerd_live()).exists());
        assert!(!staged_firmware_path(&dir.harness_live()).exists());
    }

    #[tokio::test]
    async fn bundle_missing_nano3s_ipc_rejected() {
        let dir = TestTempDir::new("missing_ipc");
        let minerd = make_elf(FIRMWARE_MIN_UPLOAD_BYTES, None);
        let bundle = make_bundle(Some(&minerd), None);

        let rebooted = AtomicBool::new(false);
        let (status, msg) = handle_firmware_bundle_with(
            chunked_body(bundle, 4096),
            &tokio::sync::Mutex::new(()),
            &dir.resolver(),
            &|_| Some(256 * 1024 * 1024),
            || rebooted.store(true, Ordering::SeqCst),
        )
        .await
        .unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("nano3s_ipc_"), "unexpected msg: {msg}");
        assert!(!rebooted.load(Ordering::SeqCst));
        assert!(!staged_firmware_path(&dir.minerd_live()).exists());
    }

    #[tokio::test]
    async fn bundle_nano3s_ipc_split_across_chunk_boundaries() {
        let dir = TestTempDir::new("split_ipc");
        // Place "nano3s_ipc_" near the middle and split right inside it,
        // plus test 1-byte / 3-byte splits around the symbol.
        let sym_offset = 1000;
        let minerd = make_elf(FIRMWARE_MIN_UPLOAD_BYTES, Some((b"nano3s_ipc_init", sym_offset)));
        let bundle = make_bundle(Some(&minerd), None);
        // Bundle header is 8 (magic) + 4 (len) = 12 bytes before section 1.
        let abs_sym = 12 + sym_offset;
        let body = split_at_offsets_body(
            &bundle,
            &[
                3,
                10,
                abs_sym - 2,
                abs_sym + 4, // splits "nano" | "3s_ipc_init"
                abs_sym + 7, // splits "3s_" | "ipc_init"
                abs_sym + 9, // splits "ip" | "c_init"
            ],
        );

        let rebooted = AtomicBool::new(false);
        let res = handle_firmware_bundle_with(
            body,
            &tokio::sync::Mutex::new(()),
            &dir.resolver(),
            &|_| Some(256 * 1024 * 1024),
            || rebooted.store(true, Ordering::SeqCst),
        )
        .await
        .expect("split nano3s_ipc_ across chunks should still be detected");

        assert!(res.0.ok);
        assert_eq!(res.0.applied, vec!["mujina-minerd"]);
        assert!(rebooted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn bundle_unlinks_stale_new_before_free_space_check() {
        let dir = TestTempDir::new("stale_new");
        let stale_path = staged_firmware_path(&dir.minerd_live());
        std::fs::write(&stale_path, vec![0u8; 1024]).unwrap();
        assert!(stale_path.exists());

        let minerd = make_elf(FIRMWARE_MIN_UPLOAD_BYTES, Some((b"nano3s_ipc_send", 512)));
        let bundle = make_bundle(Some(&minerd), None);

        let rebooted = AtomicBool::new(false);
        let res = handle_firmware_bundle_with(
            chunked_body(bundle, 4096),
            &tokio::sync::Mutex::new(()),
            &dir.resolver(),
            &|_| {
                if stale_path.exists() {
                    // Simulate /data being too full until the orphaned .new is unlinked.
                    Some(1024)
                } else {
                    Some(FIRMWARE_MIN_UPLOAD_BYTES as u64 + FIRMWARE_FREE_SPACE_MARGIN_BYTES)
                }
            },
            || rebooted.store(true, Ordering::SeqCst),
        )
        .await
        .expect("stale .new should be unlinked before free_space check");

        assert!(res.0.ok);
        assert!(rebooted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn single_upload_streams_and_rejects_bad_elf_early() {
        let dir = TestTempDir::new("single_upload");
        let minerd = make_elf(FIRMWARE_MIN_UPLOAD_BYTES, Some((b"nano3s_ipc_send", 512)));

        let rebooted = AtomicBool::new(false);
        let res = handle_firmware_upload_with(
            "mujina-minerd".to_string(),
            None, // chunked upload without Content-Length
            chunked_body(minerd.clone(), 1024),
            &tokio::sync::Mutex::new(()),
            &dir.resolver(),
            &|_| Some(256 * 1024 * 1024),
            || rebooted.store(true, Ordering::SeqCst),
        )
        .await
        .expect("single upload without Content-Length should succeed");

        assert!(res.0.ok);
        assert_eq!(res.0.size, minerd.len());
        assert!(rebooted.load(Ordering::SeqCst));
        assert_eq!(std::fs::read(dir.minerd_live()).unwrap(), minerd);

        // Bad ELF magic should fail and clean up .new.
        let mut bad = make_elf(FIRMWARE_MIN_UPLOAD_BYTES, Some((b"nano3s_ipc_send", 512)));
        bad[..4].copy_from_slice(b"NOPE");
        let (status, msg) = handle_firmware_upload_with(
            "mujina-minerd".to_string(),
            Some(bad.len() as u64),
            chunked_body(bad, 1024),
            &tokio::sync::Mutex::new(()),
            &dir.resolver(),
            &|_| Some(256 * 1024 * 1024),
            || {},
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("not an ELF binary"), "unexpected msg: {msg}");
        assert!(!staged_firmware_path(&dir.minerd_live()).exists());
    }

    #[tokio::test]
    async fn concurrent_uploads_return_409_conflict() {
        let dir = TestTempDir::new("conflict_409");
        let lock = tokio::sync::Mutex::new(());
        let minerd = make_elf(FIRMWARE_MIN_UPLOAD_BYTES, Some((b"nano3s_ipc_send", 512)));
        let bundle = make_bundle(Some(&minerd), None);

        {
            let _held = lock.try_lock().expect("lock should start unlocked");

            let (status, msg) = handle_firmware_bundle_with(
                chunked_body(bundle.clone(), 1024),
                &lock,
                &dir.resolver(),
                &|_| Some(256 * 1024 * 1024),
                || {},
            )
            .await
            .unwrap_err();
            assert_eq!(status, StatusCode::CONFLICT);
            assert!(msg.contains("already in progress"), "unexpected msg: {msg}");

            let (status2, msg2) = handle_firmware_upload_with(
                "mujina-minerd".to_string(),
                Some(minerd.len() as u64),
                chunked_body(minerd.clone(), 1024),
                &lock,
                &dir.resolver(),
                &|_| Some(256 * 1024 * 1024),
                || {},
            )
            .await
            .unwrap_err();
            assert_eq!(status2, StatusCode::CONFLICT);
            assert!(msg2.contains("already in progress"), "unexpected msg: {msg2}");
        }

        // Once the first upload finishes and drops the guard, the next upload succeeds.
        let res = handle_firmware_bundle_with(
            chunked_body(bundle, 1024),
            &lock,
            &dir.resolver(),
            &|_| Some(256 * 1024 * 1024),
            || {},
        )
        .await
        .expect("upload after lock release should succeed");
        assert!(res.0.ok);
    }
}
