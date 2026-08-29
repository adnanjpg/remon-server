//! Alert-action REST endpoints.
//!
//! Surface:
//! - `GET    /actions`                       — what can be run, and the
//!   host's ceilings on running it
//! - `POST   /actions/reload`                — rescan the actions directory
//! - `GET    /alerts/{id}/actions`           — bindings on one rule
//! - `POST   /alerts/{id}/actions`           — bind an action to a rule
//! - `GET    /actions/bindings`              — every binding
//! - `GET    /actions/bindings/{id}`         — one binding
//! - `PUT    /actions/bindings/{id}`         — replace a binding
//! - `DELETE /actions/bindings/{id}`         — remove a binding
//! - `POST   /actions/bindings/{id}/run`     — run it now, on request
//! - `GET    /actions/runs?status=pending`   — the ledger, and the inbox
//! - `GET    /actions/runs/{id}`             — one run
//! - `POST   /actions/runs/{id}/confirm`     — approve a proposal
//! - `POST   /actions/runs/{id}/dismiss`     — decline a proposal
//!
//! Validation lives here rather than in the executor: a binding that could
//! never work should fail at write time, when an operator is looking at the
//! error, not at 3am on the tick that needed it.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};

use crate::error::{AppError, AppResult};
use crate::models::action::{ActionKind, ActionVerb, RunStatus};
use crate::routes::dtos::actions::{
    ActionBindingDto, ActionCatalogEntry, ActionCatalogResponse, ActionRunDto,
    CreateActionBindingRequest, ListActionBindingsResponse, ListActionRunsResponse,
    ReloadActionsResponse, ReloadFailure, RunsQuery, UpdateActionBindingRequest,
};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::{
    ActionRepository, AlertRepository, RunQuery, UpsertActionBinding,
};

const DEFAULT_RUN_LIMIT: u32 = 100;
const MAX_RUN_LIMIT: u32 = 1000;
const MAX_RUN_OFFSET: u32 = 100_000;

/// Ceiling on a binding's own cooldown. A day is already "this basically
/// runs once"; beyond it the binding is disabled in all but name.
const MAX_COOLDOWN: i64 = 86_400;
/// Nobody legitimately wants more than this many automated interventions an
/// hour on one binding — past it the automation is the problem.
const MAX_RUNS_PER_HOUR: i64 = 60;
const MAX_FAILURE_LIMIT: i64 = 20;

/// Same charset rule as `/services/{name}`: unit names, container names and
/// script names all fit inside it, and anything outside is an injection probe.
fn validate_target(kind: ActionKind, target: &str) -> AppResult<()> {
    if target.is_empty() || target.len() > 256 {
        return Err(AppError::BadRequest(
            "target must be 1–256 characters".into(),
        ));
    }
    let ok = match kind {
        // Script names are manifest names: the probe/action charset.
        ActionKind::Script => target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')),
        ActionKind::Service | ActionKind::Container => target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | ':' | '/')),
    };
    if !ok {
        return Err(AppError::BadRequest(format!(
            "target '{}' contains characters not allowed for a {} action",
            target,
            kind.as_str()
        )));
    }
    Ok(())
}

/// Shared validation for create and replace. Anything that would make the
/// binding permanently unrunnable is a 400 here.
async fn validate_binding(
    state: &Arc<AppState>,
    kind: ActionKind,
    target: &str,
    verb: Option<ActionVerb>,
    cooldown_secs: i64,
    max_runs_per_hour: i64,
    failure_limit: i64,
) -> AppResult<()> {
    validate_target(kind, target)?;

    let allowed = ActionVerb::allowed_for(kind);
    match (kind, verb) {
        (ActionKind::Script, Some(v)) => {
            return Err(AppError::BadRequest(format!(
                "a script action takes no verb (got '{}'); the script is the verb",
                v.as_str()
            )));
        }
        (ActionKind::Script, None) => {}
        (_, None) => {
            return Err(AppError::BadRequest(format!(
                "a {} action needs a verb: one of {}",
                kind.as_str(),
                allowed
                    .iter()
                    .map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        (_, Some(v)) => {
            if !allowed.contains(&v) {
                return Err(AppError::BadRequest(format!(
                    "'{}' is not a {} verb; expected one of {}",
                    v.as_str(),
                    kind.as_str(),
                    allowed
                        .iter()
                        .map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
    }

    if !(0..=MAX_COOLDOWN).contains(&cooldown_secs) {
        return Err(AppError::BadRequest(format!(
            "cooldown_secs {} out of range [0..{}]",
            cooldown_secs, MAX_COOLDOWN
        )));
    }
    if !(1..=MAX_RUNS_PER_HOUR).contains(&max_runs_per_hour) {
        return Err(AppError::BadRequest(format!(
            "max_runs_per_hour {} out of range [1..{}]",
            max_runs_per_hour, MAX_RUNS_PER_HOUR
        )));
    }
    if !(1..=MAX_FAILURE_LIMIT).contains(&failure_limit) {
        return Err(AppError::BadRequest(format!(
            "failure_limit {} out of range [1..{}]",
            failure_limit, MAX_FAILURE_LIMIT
        )));
    }

    // A binding naming a script that isn't loaded is almost always a typo,
    // and a typo that only announces itself the next time the rule fires is
    // the worst possible time to learn about it.
    if kind == ActionKind::Script {
        let reg = state.action_registry.read().await;
        if !reg.actions.contains_key(target) {
            let mut known: Vec<&String> = reg.actions.keys().collect();
            known.sort();
            return Err(AppError::BadRequest(format!(
                "no action script named '{}' is loaded. Available: [{}]. Drop the file in the \
                 actions directory and POST /actions/reload.",
                target,
                known
                    .into_iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }

    // Self-targeting is refused at write time as well as run time — an
    // operator should find out now, not from a skipped run later.
    if kind == ActionKind::Service
        && matches!(verb, Some(ActionVerb::Stop) | Some(ActionVerb::Restart))
        && crate::platform::identity::is_own_service(target)
    {
        return Err(AppError::Conflict(format!(
            "'{}' is remon-server itself; an action may not stop or restart it",
            target
        )));
    }

    Ok(())
}

// ===== catalogue =====

/// `GET /actions` — everything a binding could point at, plus the host-level
/// ceilings a client needs to explain why a binding might not run.
pub async fn list_catalog(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ActionCatalogResponse>> {
    let mut entries: Vec<ActionCatalogEntry> = {
        let reg = state.action_registry.read().await;
        reg.actions
            .values()
            .map(|m| ActionCatalogEntry {
                kind: ActionKind::Script,
                target: Some(m.name.clone()),
                description: m.description.clone(),
                verbs: Vec::new(),
                source_path: Some(m.source_path.display().to_string()),
                timeout_ms: Some(m.timeout.as_millis() as u64),
                platforms: m.platforms.clone(),
            })
            .collect()
    };
    entries.sort_by(|a, b| a.target.cmp(&b.target));

    // The catalogue kinds have no fixed target — any unit, any container —
    // so they are listed once each with their verb set.
    entries.push(ActionCatalogEntry {
        kind: ActionKind::Service,
        target: None,
        description: Some(
            "Service lifecycle, through the same manager /services/{name}/{verb} uses".into(),
        ),
        verbs: ActionVerb::allowed_for(ActionKind::Service).to_vec(),
        source_path: None,
        timeout_ms: None,
        platforms: Vec::new(),
    });
    entries.push(ActionCatalogEntry {
        kind: ActionKind::Container,
        target: None,
        description: Some("Container lifecycle, through the same calls /docker/* uses".into()),
        verbs: ActionVerb::allowed_for(ActionKind::Container).to_vec(),
        source_path: None,
        timeout_ms: None,
        platforms: Vec::new(),
    });

    Ok(Json(ActionCatalogResponse {
        enabled: state.actions_config.enabled,
        auto: state.actions_config.auto,
        proposal_ttl_secs: state.actions_config.proposal_ttl_secs,
        entries,
    }))
}

/// `POST /actions/reload` — rescan the actions directory.
pub async fn reload_actions(
    claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ReloadActionsResponse>> {
    let dir = &crate::paths::get().actions_dir;
    let report = crate::actions::registry::load(dir, &state.action_registry).await;
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "actions_reloaded",
        format!(
            "Action definitions reloaded ({} loaded, {} failed)",
            report.loaded.len(),
            report.failed.len()
        ),
        None,
        None,
        Some(serde_json::json!({
            "loaded": report.loaded.len(),
            "skipped_disabled": report.skipped_disabled.len(),
            "skipped_platform": report.skipped_platform.len(),
            "failed": report.failed.len(),
        })),
    );
    Ok(Json(ReloadActionsResponse {
        loaded: report.loaded,
        skipped_disabled: report.skipped_disabled,
        skipped_platform: report.skipped_platform,
        failed: report
            .failed
            .into_iter()
            .map(|(path, error)| ReloadFailure {
                path: path.display().to_string(),
                error,
            })
            .collect(),
    }))
}

// ===== bindings =====

/// `GET /actions/bindings` — every binding on the host.
pub async fn list_bindings(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ListActionBindingsResponse>> {
    let repo = ActionRepository::new(state.db.clone());
    Ok(Json(ListActionBindingsResponse {
        bindings: repo
            .list_bindings()
            .await?
            .into_iter()
            .map(ActionBindingDto::from)
            .collect(),
    }))
}

/// `GET /alerts/{id}/actions` — bindings on one rule.
pub async fn list_bindings_for_rule(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(rule_id): Path<i64>,
) -> AppResult<Json<ListActionBindingsResponse>> {
    let alerts = AlertRepository::new(state.db.clone());
    if alerts.get(rule_id).await?.is_none() {
        return Err(AppError::NotFound(format!("Alert rule {}", rule_id)));
    }
    let repo = ActionRepository::new(state.db.clone());
    Ok(Json(ListActionBindingsResponse {
        bindings: repo
            .list_for_rule(rule_id)
            .await?
            .into_iter()
            .map(ActionBindingDto::from)
            .collect(),
    }))
}

pub async fn get_binding(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<ActionBindingDto>> {
    let repo = ActionRepository::new(state.db.clone());
    let b = repo
        .get_binding(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Action binding {}", id)))?;
    Ok(Json(ActionBindingDto::from(b)))
}

/// `POST /alerts/{id}/actions` — bind an action to a rule.
pub async fn create_binding(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(rule_id): Path<i64>,
    Json(req): Json<CreateActionBindingRequest>,
) -> AppResult<(StatusCode, Json<ActionBindingDto>)> {
    let alerts = AlertRepository::new(state.db.clone());
    let rule = alerts
        .get(rule_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Alert rule {}", rule_id)))?;

    validate_binding(
        &state,
        req.kind,
        &req.target,
        req.verb,
        req.cooldown_secs,
        req.max_runs_per_hour,
        req.failure_limit,
    )
    .await?;

    let repo = ActionRepository::new(state.db.clone());
    let upsert = UpsertActionBinding {
        rule_id,
        kind: req.kind,
        target: req.target.clone(),
        verb: req.verb,
        on_event: req.on_event,
        mode: req.mode,
        enabled: req.enabled,
        cooldown_secs: req.cooldown_secs,
        max_runs_per_hour: req.max_runs_per_hour,
        failure_limit: req.failure_limit,
    };
    let id = repo.insert_binding(&upsert).await?;
    let created = repo
        .get_binding(id)
        .await?
        .ok_or_else(|| AppError::Internal("binding vanished after insert".into()))?;

    // Arming an automation is an operator decision worth a timeline row, the
    // same as restarting a service by hand.
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "action_binding_created",
        format!(
            "Action bound to '{}': {} ({})",
            rule.name,
            created.summary(),
            created.mode.as_str()
        ),
        Some("alert_rule"),
        Some(rule_id.to_string()),
        Some(serde_json::json!({
            "action_id": id,
            "mode": created.mode.as_str(),
            "on_event": created.on_event.as_str(),
        })),
    );

    Ok((StatusCode::CREATED, Json(ActionBindingDto::from(created))))
}

/// `PUT /actions/bindings/{id}` — full replace.
pub async fn update_binding(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateActionBindingRequest>,
) -> AppResult<Json<ActionBindingDto>> {
    let repo = ActionRepository::new(state.db.clone());
    let existing = repo
        .get_binding(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Action binding {}", id)))?;

    validate_binding(
        &state,
        req.kind,
        &req.target,
        req.verb,
        req.cooldown_secs,
        req.max_runs_per_hour,
        req.failure_limit,
    )
    .await?;

    let upsert = UpsertActionBinding {
        rule_id: existing.rule_id,
        kind: req.kind,
        target: req.target.clone(),
        verb: req.verb,
        on_event: req.on_event,
        mode: req.mode,
        enabled: req.enabled,
        cooldown_secs: req.cooldown_secs,
        max_runs_per_hour: req.max_runs_per_hour,
        failure_limit: req.failure_limit,
    };
    if !repo.update_binding(id, &upsert).await? {
        return Err(AppError::NotFound(format!("Action binding {}", id)));
    }
    let updated = repo
        .get_binding(id)
        .await?
        .ok_or_else(|| AppError::Internal("binding vanished after update".into()))?;

    // Only the transitions that change what the host will do unattended are
    // worth a row; renaming a cooldown is not.
    if existing.mode != updated.mode || existing.enabled != updated.enabled {
        crate::services::events::record_operator(
            &state,
            &claims.device_id,
            "action_binding_updated",
            format!(
                "Action '{}' is now {} ({})",
                updated.summary(),
                if updated.enabled { "armed" } else { "disabled" },
                updated.mode.as_str()
            ),
            Some("alert_rule"),
            Some(updated.rule_id.to_string()),
            Some(serde_json::json!({
                "action_id": id,
                "mode": updated.mode.as_str(),
                "enabled": updated.enabled,
            })),
        );
    }

    Ok(Json(ActionBindingDto::from(updated)))
}

pub async fn delete_binding(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let repo = ActionRepository::new(state.db.clone());
    let existing = repo
        .get_binding(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Action binding {}", id)))?;
    if !repo.delete_binding(id).await? {
        return Err(AppError::NotFound(format!("Action binding {}", id)));
    }
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "action_binding_deleted",
        format!("Action unbound: {}", existing.summary()),
        Some("alert_rule"),
        Some(existing.rule_id.to_string()),
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /actions/bindings/{id}/run` — run this binding now.
///
/// The "does it actually work" button. Deliberately not subject to the
/// cooldown, the hourly ceiling or `actions.auto`: those restrain unattended
/// repetition, and this is one run a person asked for by id. Single-flight
/// and the target precheck still apply.
pub async fn run_binding(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<ActionRunDto>> {
    let repo = ActionRepository::new(state.db.clone());
    let binding = repo
        .get_binding(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Action binding {}", id)))?;

    let run_id =
        crate::services::actions::run_binding_now(&state, &binding, &claims.device_id).await?;

    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "action_run_requested",
        format!("Action run requested by operator: {}", binding.summary()),
        Some("action_run"),
        Some(run_id.to_string()),
        None,
    );

    let run = repo
        .get_run(run_id)
        .await?
        .ok_or_else(|| AppError::Internal("run vanished after execution".into()))?;
    Ok(Json(ActionRunDto::from(run)))
}

// ===== runs =====

/// `GET /actions/runs` — the ledger. `?status=pending` is the operator's
/// inbox of proposals awaiting an answer.
pub async fn list_runs(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<RunsQuery>,
) -> AppResult<Json<ListActionRunsResponse>> {
    let status = match q.status.as_deref() {
        None => None,
        Some(s) => Some(RunStatus::parse(s).ok_or_else(|| {
            AppError::BadRequest(format!(
                "unknown status '{}': expected pending, running, succeeded, failed, skipped, \
                 expired or dismissed",
                s
            ))
        })?),
    };
    let repo = ActionRepository::new(state.db.clone());
    let runs = repo
        .list_runs(&RunQuery {
            status,
            action_id: q.action_id,
            rule_id: q.rule_id,
            limit: q.limit.unwrap_or(DEFAULT_RUN_LIMIT).min(MAX_RUN_LIMIT),
            offset: q.offset.unwrap_or(0).min(MAX_RUN_OFFSET),
        })
        .await?;
    Ok(Json(ListActionRunsResponse {
        runs: runs.into_iter().map(ActionRunDto::from).collect(),
    }))
}

pub async fn get_run(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<ActionRunDto>> {
    let repo = ActionRepository::new(state.db.clone());
    let run = repo
        .get_run(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Action run {}", id)))?;
    Ok(Json(ActionRunDto::from(run)))
}

/// `POST /actions/runs/{id}/confirm` — approve a proposal and run it.
///
/// Responds only once the run has finished, so the operator who pressed the
/// button sees the outcome rather than an acknowledgement.
pub async fn confirm_run(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<ActionRunDto>> {
    let run = crate::services::actions::confirm_run(&state, id, &claims.device_id).await?;
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "action_confirmed",
        format!(
            "Action confirmed for '{}': {} — {}",
            run.rule_name,
            run.target,
            run.message.as_deref().unwrap_or("no detail")
        ),
        Some("action_run"),
        Some(id.to_string()),
        Some(serde_json::json!({ "status": run.status.as_str() })),
    );
    Ok(Json(ActionRunDto::from(run)))
}

/// `POST /actions/runs/{id}/dismiss` — decline a proposal.
pub async fn dismiss_run(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<ActionRunDto>> {
    let run = crate::services::actions::dismiss_run(&state, id, &claims.device_id).await?;
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "action_dismissed",
        format!(
            "Proposed action declined for '{}': {}",
            run.rule_name, run.target
        ),
        Some("action_run"),
        Some(id.to_string()),
        None,
    );
    Ok(Json(ActionRunDto::from(run)))
}
