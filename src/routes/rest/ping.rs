//! Anonymous heartbeat ping endpoints — the capability-URL side.
//!
//! The slug in the path IS the credential: 128 bits of server-generated
//! entropy, stored blake3-hashed, shown once at create/rotate. Unknown or
//! malformed slugs get a uniform 404 with no detail. The whole group sits
//! behind its own per-IP governor (more generous than the auth one — a
//! fleet of cron jobs behind one NAT is the normal case, not abuse) and a
//! small body limit; see `create_routes`.
//!
//! - `GET|POST /ping/{slug}`          — success ping
//! - `GET|POST /ping/{slug}/fail`     — explicit failure (latches until next success)
//! - `GET|POST /ping/{slug}/{code}`   — exit-code report: 0 = success, else fail
//! - `POST     /ping/{slug}/pause`    — service-announced downtime
//! - `POST     /ping/{slug}/resume`   — end a service-announced pause early
//!
//! `pause`/`resume` are POST-only: a pasted URL must not let a link
//! prefetcher mutate monitoring state. Plain pings tolerate GET so the
//! one-liner integration stays `curl <url>`. Fail bodies are read capped
//! (4 KiB) and the rest dropped — an oversized stack trace truncates
//! instead of 413-ing away the fail signal it rides on; only the
//! app-wide 64 KiB limit rejects outright.

use axum::{
    Json,
    body::Body,
    extract::{ConnectInfo, Path, Query, State},
    http::HeaderMap,
};
use chrono::Utc;
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::models::heartbeat::{HeartbeatCheck, HeartbeatPing, PauseOrigin, PingKind};
use crate::state::AppState;
use crate::storage::repositories::HeartbeatRepository;

/// Ceiling on service-announced pauses. Bounds the blast radius of a
/// leaked slug (a thief mutes at most 24h at a time, visibly) and stops a
/// miswired deploy script from silencing a check forever. Longer windows
/// are an operator decision — use the JWT pause endpoint.
const SERVICE_PAUSE_CAP_SECS: i64 = 86_400;

/// Stored ping-body ceiling. Capture is fail/nonzero-exit only — that is
/// where a stack trace earns its bytes; success pings stay row-only.
const PING_BODY_CAPTURE_BYTES: usize = 4096;

/// Uniform rejection: wrong shape, unknown slug, disabled check — all the
/// same 404, so the endpoint is not an oracle for probing slug validity.
fn not_found() -> AppError {
    AppError::NotFound("Ping target".to_string())
}

/// Hash a presented slug for lookup. Slugs are high-entropy random values
/// (not passwords), so a fast keyless hash is the right primitive —
/// Argon2 here would only tax the ping hot path.
fn slug_hash(slug: &str) -> Result<String, AppError> {
    let well_formed = slug.len() == 32
        && slug
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !well_formed {
        return Err(not_found());
    }
    Ok(blake3::hash(slug.as_bytes()).to_hex().to_string())
}

async fn resolve_check(state: &AppState, slug: &str) -> AppResult<HeartbeatCheck> {
    let hash = slug_hash(slug)?;
    let repo = HeartbeatRepository::new(state.db.clone());
    repo.get_by_slug_hash(&hash)
        .await?
        .filter(|c| c.enabled)
        .ok_or_else(not_found)
}

/// Persist a log row, mapping a FK failure (check deleted mid-request)
/// to the uniform 404 instead of a 500.
async fn log_ping(repo: &HeartbeatRepository, row: &HeartbeatPing) -> AppResult<()> {
    match repo.insert_ping(row).await {
        Err(AppError::DatabaseError(msg)) if msg.contains("FOREIGN KEY") => Err(not_found()),
        other => other,
    }
}

fn log_row(
    check_id: i64,
    now: i64,
    kind: PingKind,
    exit_code: Option<i32>,
    source_ip: String,
    headers: &HeaderMap,
    body: Option<String>,
) -> HeartbeatPing {
    HeartbeatPing {
        id: 0,
        check_id,
        received_at: now,
        kind,
        exit_code,
        // XFF is attacker-controlled text until the proxy is trusted;
        // bound it like every other free-text field on this row.
        source_ip: Some(source_ip.chars().take(64).collect()),
        user_agent: headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.chars().take(256).collect()),
        body,
    }
}

/// Read at most `PING_BODY_CAPTURE_BYTES` of the request body and drop
/// the rest — an oversized stack trace must truncate, not 413 away the
/// fail signal it rides on. Lossy UTF-8; NUL bytes stripped so the TEXT
/// column stays queryable. (The app-wide 64 KiB body limit still rejects
/// truly hostile payloads before this runs.)
async fn capture_body(body: Body) -> Option<String> {
    use futures_util::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        let room = PING_BODY_CAPTURE_BYTES - buf.len();
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if buf.len() >= PING_BODY_CAPTURE_BYTES {
            break;
        }
    }
    if buf.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).replace('\0', ""))
}

async fn apply_success(
    state: &AppState,
    check: &HeartbeatCheck,
    source_ip: String,
    headers: &HeaderMap,
) -> AppResult<()> {
    let repo = HeartbeatRepository::new(state.db.clone());
    let now = Utc::now().timestamp();
    repo.record_success(check.id, now).await?;
    log_ping(
        &repo,
        &log_row(check.id, now, PingKind::Success, None, source_ip, headers, None),
    )
    .await?;
    Ok(())
}

async fn apply_fail(
    state: &AppState,
    check: &HeartbeatCheck,
    exit_code: Option<i32>,
    source_ip: String,
    headers: &HeaderMap,
    body: Option<String>,
) -> AppResult<()> {
    let repo = HeartbeatRepository::new(state.db.clone());
    let now = Utc::now().timestamp();
    repo.record_fail(check.id, now).await?;
    log_ping(
        &repo,
        &log_row(check.id, now, PingKind::Fail, exit_code, source_ip, headers, body),
    )
    .await?;
    Ok(())
}

/// GET|POST /ping/{slug} — success ping.
pub async fn ping_success(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let check = resolve_check(&state, &slug).await?;
    let ip = super::auth::extract_client_ip(&headers, &addr, state.trusted_proxy);
    apply_success(&state, &check, ip, &headers).await?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// GET|POST /ping/{slug}/fail — explicit failure report.
pub async fn ping_fail(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    body: Body,
) -> AppResult<Json<serde_json::Value>> {
    let check = resolve_check(&state, &slug).await?;
    let ip = super::auth::extract_client_ip(&headers, &addr, state.trusted_proxy);
    let captured = capture_body(body).await;
    apply_fail(&state, &check, None, ip, &headers, captured).await?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// GET|POST /ping/{slug}/{code} — exit-code report; `sh -c 'job; curl
/// $URL/$?'` wiring. 0 succeeds, anything else fails with the code kept
/// on the log row.
pub async fn ping_exit_code(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((slug, code)): Path<(String, String)>,
    body: Body,
) -> AppResult<Json<serde_json::Value>> {
    // Uniform 404 on a non-exit-code tail — this route also catches
    // arbitrary junk after the slug.
    let code: u8 = code.parse().map_err(|_| not_found())?;
    let check = resolve_check(&state, &slug).await?;
    let ip = super::auth::extract_client_ip(&headers, &addr, state.trusted_proxy);
    if code == 0 {
        apply_success(&state, &check, ip, &headers).await?;
    } else {
        let captured = capture_body(body).await;
        apply_fail(&state, &check, Some(code as i32), ip, &headers, captured).await?;
    }
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

#[derive(Debug, Deserialize)]
pub struct ServicePauseQuery {
    /// `"3h"`, `"90m"`, `"45s"`, `"1d"` or bare seconds.
    pub duration: Option<String>,
    /// Absolute unix-epoch end. Mutually exclusive with `duration`.
    pub until: Option<i64>,
    pub reason: Option<String>,
}

/// POST /ping/{slug}/pause — service-announced downtime.
///
/// Three forms, one policy split:
/// - `?duration=` / `?until=` — a declared window. Never auto-resumes on
///   ping (deploys flap; one boot-time ping must not re-arm alerting mid
///   window). Magnitude over the cap clamps with `"clamped": true`;
///   malformed shape is a hard 400.
/// - bare — "quiet until I ping again": the only auto-resuming form,
///   still capped so a job that dies mid-deploy can't stay muted forever.
pub async fn ping_pause(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(q): Query<ServicePauseQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let check = resolve_check(&state, &slug).await?;
    let now = Utc::now().timestamp();
    let cap = now + SERVICE_PAUSE_CAP_SECS;

    if q.duration.is_some() && q.until.is_some() {
        return Err(AppError::BadRequest(
            "duration and until are mutually exclusive".to_string(),
        ));
    }
    let (requested_until, until_ping) = if let Some(d) = &q.duration {
        let secs = parse_duration_secs(d)
            .ok_or_else(|| AppError::BadRequest(format!("invalid duration '{}'", d)))?;
        (now.saturating_add(secs), false)
    } else if let Some(until) = q.until {
        if until <= now {
            return Err(AppError::BadRequest("until is in the past".to_string()));
        }
        (until, false)
    } else {
        (cap, true)
    };
    let until = requested_until.min(cap);
    let reason = q
        .reason
        .as_deref()
        .map(|r| r.chars().take(256).collect::<String>());

    let repo = HeartbeatRepository::new(state.db.clone());
    let placed = repo
        .service_pause(check.id, now, until, until_ping, reason.as_deref())
        .await?;
    if !placed {
        // The guarded UPDATE also misses when the check was deleted
        // between lookup and write — re-fetch to tell the two apart.
        return match repo.get(check.id).await? {
            None => Err(not_found()),
            Some(_) => Err(AppError::Conflict(
                "an operator pause is active on this check".to_string(),
            )),
        };
    }
    let ip = super::auth::extract_client_ip(&headers, &addr, state.trusted_proxy);
    log_ping(
        &repo,
        &log_row(check.id, now, PingKind::Pause, None, ip, &headers, None),
    )
    .await?;

    Ok(Json(serde_json::json!({
        "status": "paused",
        "paused_until": until,
        "clamped": requested_until > cap,
    })))
}

/// POST /ping/{slug}/resume — end a service-announced pause early.
/// Idempotent when nothing service-owned is active; 409 only when the
/// active pause belongs to the operator.
pub async fn ping_resume(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let check = resolve_check(&state, &slug).await?;
    let now = Utc::now().timestamp();
    let repo = HeartbeatRepository::new(state.db.clone());

    let resumed = repo.service_resume(check.id, now).await?;
    if !resumed {
        // Decide from a fresh row: the snapshot predates the UPDATE.
        let fresh = repo.get(check.id).await?.ok_or_else(not_found)?;
        if fresh.is_paused(now) && fresh.pause_origin == Some(PauseOrigin::Operator) {
            return Err(AppError::Conflict(
                "the active pause was placed by the operator".to_string(),
            ));
        }
    } else {
        let ip = super::auth::extract_client_ip(&headers, &addr, state.trusted_proxy);
        log_ping(
            &repo,
            &log_row(check.id, now, PingKind::Resume, None, ip, &headers, None),
        )
        .await?;
    }
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `"3h"` / `"90m"` / `"45s"` / `"1d"` / bare seconds → seconds.
/// Zero and overflow are rejected; fractions are not accepted.
fn parse_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, mult) = match s.as_bytes().last()? {
        b's' => (&s[..s.len() - 1], 1),
        b'm' => (&s[..s.len() - 1], 60),
        b'h' => (&s[..s.len() - 1], 3600),
        b'd' => (&s[..s.len() - 1], 86_400),
        b'0'..=b'9' => (s, 1),
        _ => return None,
    };
    let n: i64 = num.parse().ok()?;
    if n <= 0 {
        return None;
    }
    n.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::parse_duration_secs;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration_secs("45s"), Some(45));
        assert_eq!(parse_duration_secs("90m"), Some(5400));
        assert_eq!(parse_duration_secs("3h"), Some(10800));
        assert_eq!(parse_duration_secs("1d"), Some(86_400));
        assert_eq!(parse_duration_secs("120"), Some(120));
    }

    #[test]
    fn junk_rejected() {
        assert_eq!(parse_duration_secs(""), None);
        assert_eq!(parse_duration_secs("0"), None);
        assert_eq!(parse_duration_secs("-5m"), None);
        assert_eq!(parse_duration_secs("3x"), None);
        assert_eq!(parse_duration_secs("1.5h"), None);
        assert_eq!(parse_duration_secs("h"), None);
    }
}
