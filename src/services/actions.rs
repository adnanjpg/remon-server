//! Alert-action executor — the half of the fanout that *does* something.
//!
//! The notification path answers "who should know". This answers "what should
//! happen", and it hangs off exactly the same transitions, for the same
//! reason: `alert_state` has already decided the violation is real (debounced
//! by `for_duration_secs`, per label_set, surviving restarts). Re-deriving
//! that here would be the same logic in a second place, and the second place
//! is always the one that drifts.
//!
//! ## What it will not do
//!
//! Every unattended execution is a decision made by a machine about a
//! production host, so each one has to get past all of these:
//!
//! - `actions.enabled` — the master switch, config-only.
//! - `actions.auto` — unattended runs are opt-in per *host*, not per binding.
//!   A binding can be `mode = "auto"` on a server where this is off; it then
//!   records a skip that says so rather than quietly acting.
//! - **Single-flight** per (binding, label_set) — a rule that fires again
//!   while the last remediation is still running does not stack a second one.
//! - **Cooldown** per (binding, label_set) — separate from the rule's
//!   notification cooldown, because how often you want to *hear* about
//!   something and how often you want to *act* on it are different questions.
//! - **Hourly ceiling** per binding, across every label_set — the stop against
//!   a flapping rule becoming a restart loop.
//! - **Circuit breaker** — consecutive failures disarm the binding and page.
//!   An automation that cannot fix the problem and will not stop trying is a
//!   second incident stacked on the first.
//! - **Self-targeting** — an action may not stop or restart the unit
//!   supervising this process, for the same reason `/services/{name}/stop`
//!   refuses to.
//!
//! `manual` bindings — the default — clear all of the above and then still do
//! nothing but draft a proposal and page an operator. That is the shape this
//! feature ships in; `auto` is what an operator graduates a binding to once
//! the dry runs have convinced them.

use std::collections::HashMap;
use std::sync::Arc;

use log::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::models::action::{
    ActionBinding, ActionKind, ActionMode, ActionRun, ActionTrigger, ActionVerb, ExecutionResult,
    RunOrigin, RunStatus,
};
use crate::models::alert::AlertSeverity;
use crate::notify::{Notification, NotificationEvent, Severity};
use crate::state::AppState;
use crate::storage::repositories::{ActionRepository, NewActionRun, NewHostEvent};

/// How much of a script's output we keep on the run row. Enough to see the
/// error that explains a failure, not enough to turn the ledger into a log.
const OUTPUT_TAIL_CAP: usize = 2048;

/// How often expired proposals are swept. A proposal's TTL is measured in
/// minutes, so a minute of slack past it costs nothing.
const SWEEP_INTERVAL_SECS: u64 = 60;

/// Everything one execution needs to know about why it is happening. Built
/// once per transition and handed to every binding on that rule.
#[derive(Debug, Clone)]
pub struct AlertContext {
    pub rule_id: i64,
    pub rule_name: String,
    pub severity: AlertSeverity,
    pub label_set: String,
    pub trigger: ActionTrigger,
    pub metric_value: Option<f64>,
}

/// Fire-and-forget entry point from the evaluator. Runs off the eval tick so
/// a slow `systemctl restart` can never stall rule evaluation — the same
/// treatment the incident flight recorder gets.
pub fn spawn_for_alert(state: Arc<AppState>, ctx: AlertContext) {
    tokio::spawn(async move {
        if let Err(e) = dispatch(&state, &ctx).await {
            warn!(
                "action dispatch failed for rule '{}' label={}: {:?}",
                ctx.rule_name, ctx.label_set, e
            );
        }
    });
}

/// Walk every armed binding on the rule and decide what each one does.
///
/// Separate from `spawn_for_alert` so the decision path can be driven
/// directly — the guardrails are the part worth testing, and a fire-and-forget
/// task is the one shape a test cannot observe.
pub async fn dispatch(state: &Arc<AppState>, ctx: &AlertContext) -> AppResult<()> {
    let repo = ActionRepository::new(state.db.clone());
    let bindings = repo.list_armed_for_rule(ctx.rule_id).await?;
    if bindings.is_empty() {
        return Ok(());
    }

    for binding in bindings {
        if !binding.on_event.covers(ctx.trigger) {
            continue;
        }
        if let Err(e) = consider(state, &repo, &binding, ctx).await {
            warn!(
                "action binding {} on rule '{}' errored: {:?}",
                binding.id, ctx.rule_name, e
            );
        }
    }
    Ok(())
}

/// The guardrail gauntlet for one binding, ending in a proposal, an
/// execution, or a recorded skip. Every refusal leaves a row — an operator
/// asking "why didn't it restart?" gets an answer instead of silence.
async fn consider(
    state: &Arc<AppState>,
    repo: &ActionRepository,
    binding: &ActionBinding,
    ctx: &AlertContext,
) -> AppResult<()> {
    let cfg = &state.actions_config;

    if !cfg.enabled {
        skip(
            repo,
            binding,
            ctx,
            "action engine is disabled (actions.enabled = false)",
        )
        .await?;
        return Ok(());
    }

    // Single-flight before cooldown: an in-flight run is the more specific
    // answer, and the more alarming one to hide.
    if repo.has_in_flight(binding.id, &ctx.label_set).await? {
        skip(
            repo,
            binding,
            ctx,
            "a run for this target is still pending or in flight",
        )
        .await?;
        return Ok(());
    }

    let now = chrono::Utc::now().timestamp();
    if binding.cooldown_secs > 0
        && let Some(last) = repo
            .last_effective_run_at(binding.id, &ctx.label_set)
            .await?
        && now - last < binding.cooldown_secs
    {
        skip(
            repo,
            binding,
            ctx,
            &format!(
                "cooldown: last run {}s ago, needs {}s",
                now - last,
                binding.cooldown_secs
            ),
        )
        .await?;
        return Ok(());
    }

    let recent = repo.runs_since(binding.id, now - 3600).await?;
    if recent >= binding.max_runs_per_hour {
        skip(
            repo,
            binding,
            ctx,
            &format!(
                "hourly ceiling reached ({}/{} in the last hour)",
                recent, binding.max_runs_per_hour
            ),
        )
        .await?;
        return Ok(());
    }

    // Refuse impossible targets before drafting anything. A proposal an
    // operator cannot safely confirm should never reach their phone.
    if let Err(reason) = precheck(state, binding).await {
        skip(repo, binding, ctx, &reason).await?;
        return Ok(());
    }

    match binding.mode {
        ActionMode::DryRun => {
            skip(
                repo,
                binding,
                ctx,
                &format!("dry run: would {}", binding.summary()),
            )
            .await?;
        }
        ActionMode::Manual => {
            propose(state, repo, binding, ctx).await?;
        }
        ActionMode::Auto => {
            if !cfg.auto {
                skip(
                    repo,
                    binding,
                    ctx,
                    "unattended runs are switched off on this host (actions.auto = false)",
                )
                .await?;
                return Ok(());
            }
            let run_id = repo
                .insert_run(&new_run(
                    binding,
                    ctx,
                    RunStatus::Running,
                    RunOrigin::Auto,
                    None,
                    None,
                    None,
                ))
                .await?;
            execute_run(Arc::clone(state), binding.clone(), ctx.clone(), run_id).await;
        }
    }
    Ok(())
}

/// Record a refusal. Never fails the caller — a ledger problem must not turn
/// into a silent skip on top of the skip.
async fn skip(
    repo: &ActionRepository,
    binding: &ActionBinding,
    ctx: &AlertContext,
    reason: &str,
) -> AppResult<()> {
    debug!(
        "action binding {} skipped for rule '{}': {}",
        binding.id, ctx.rule_name, reason
    );
    repo.insert_run(&new_run(
        binding,
        ctx,
        RunStatus::Skipped,
        RunOrigin::Auto,
        None,
        None,
        Some(reason.to_string()),
    ))
    .await?;
    Ok(())
}

/// Draft a proposal and page an operator. Nothing has happened to the host at
/// this point and nothing will until someone confirms.
async fn propose(
    state: &Arc<AppState>,
    repo: &ActionRepository,
    binding: &ActionBinding,
    ctx: &AlertContext,
) -> AppResult<()> {
    let expires_at = chrono::Utc::now().timestamp() + state.actions_config.proposal_ttl_secs as i64;
    let run_id = repo
        .insert_run(&new_run(
            binding,
            ctx,
            RunStatus::Pending,
            RunOrigin::Auto,
            None,
            Some(expires_at),
            Some(format!("awaiting confirmation to {}", binding.summary())),
        ))
        .await?;

    let server_name = state.effective_config.read().await.server_name.clone();
    let n = Notification {
        title: format!("[{}] Action needed: {}", server_name, ctx.rule_name),
        body: format!(
            "{} is firing{}.\nProposed: {}\nConfirm with POST /actions/runs/{}/confirm — expires in {}m.",
            ctx.rule_name,
            label_suffix(&ctx.label_set),
            binding.summary(),
            run_id,
            state.actions_config.proposal_ttl_secs / 60,
        ),
        severity: severity_of(ctx.severity),
        event: NotificationEvent::ActionRequired,
    };
    state.notify_queue.dispatch(n, None);

    record_event(
        state,
        "action_proposed",
        severity_str(ctx.severity),
        format!(
            "Proposed action for '{}': {}",
            ctx.rule_name,
            binding.summary()
        ),
        run_id,
        serde_json::json!({
            "action_id": binding.id,
            "rule": ctx.rule_name,
            "label_set": ctx.label_set,
            "summary": binding.summary(),
            "expires_at": expires_at,
        }),
    );
    info!(
        "action proposed (run {}) for rule '{}': {}",
        run_id,
        ctx.rule_name,
        binding.summary()
    );
    Ok(())
}

/// Execute a claimed run and write back its outcome. Holds a concurrency
/// permit for the duration — actions restart services, and twenty at once is
/// how a remediation becomes an outage.
pub async fn execute_run(
    state: Arc<AppState>,
    binding: ActionBinding,
    ctx: AlertContext,
    run_id: i64,
) {
    let repo = ActionRepository::new(state.db.clone());
    let permit = state.action_gate.clone().acquire_owned().await;
    let result = match permit {
        Ok(_permit) => run_target(&state, &binding, &ctx, run_id).await,
        // The semaphore is never closed while the process lives; if it ever
        // is, refusing to run is the safe reading.
        Err(_) => ExecutionResult::failure(0, "action concurrency gate is closed"),
    };

    let status = if result.success {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    };
    if let Err(e) = repo
        .finish_run(
            run_id,
            status,
            result.exit_code,
            result.duration_ms,
            &result.message,
            result.output_tail.as_deref(),
        )
        .await
    {
        warn!("could not write outcome for action run {}: {:?}", run_id, e);
    }

    if result.success {
        if let Err(e) = repo.clear_failures(binding.id).await {
            warn!(
                "could not reset failure streak on binding {}: {:?}",
                binding.id, e
            );
        }
        record_event(
            &state,
            "action_ran",
            "info",
            format!(
                "Action succeeded for '{}': {}",
                ctx.rule_name,
                binding.summary()
            ),
            run_id,
            serde_json::json!({
                "action_id": binding.id,
                "rule": ctx.rule_name,
                "label_set": ctx.label_set,
                "summary": binding.summary(),
                "duration_ms": result.duration_ms,
            }),
        );
        info!(
            "action run {} succeeded ({} for rule '{}')",
            run_id,
            binding.summary(),
            ctx.rule_name
        );
        return;
    }

    warn!(
        "action run {} failed ({} for rule '{}'): {}",
        run_id,
        binding.summary(),
        ctx.rule_name,
        result.message
    );
    let breaker_reason = format!(
        "disarmed after {} consecutive failures; last: {}",
        binding.failure_limit, result.message
    );
    let (streak, tripped) = match repo.record_failure(binding.id, &breaker_reason).await {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "could not record failure on binding {}: {:?}",
                binding.id, e
            );
            (0, false)
        }
    };

    record_event(
        &state,
        "action_failed",
        "warn",
        format!(
            "Action failed for '{}': {} — {}",
            ctx.rule_name,
            binding.summary(),
            result.message
        ),
        run_id,
        serde_json::json!({
            "action_id": binding.id,
            "rule": ctx.rule_name,
            "label_set": ctx.label_set,
            "summary": binding.summary(),
            "consecutive_failures": streak,
            "exit_code": result.exit_code,
        }),
    );

    if tripped {
        let server_name = state.effective_config.read().await.server_name.clone();
        state.notify_queue.dispatch(
            Notification {
                title: format!("[{}] Action disarmed: {}", server_name, binding.summary()),
                body: format!(
                    "'{}' failed {} times in a row and has been disabled. Last failure: {}",
                    binding.summary(),
                    streak,
                    result.message
                ),
                severity: Severity::Crit,
                event: NotificationEvent::HostEvent,
            },
            None,
        );
        record_event(
            &state,
            "action_disarmed",
            "error",
            format!(
                "Action '{}' disabled itself after {} consecutive failures",
                binding.summary(),
                streak
            ),
            run_id,
            serde_json::json!({ "action_id": binding.id, "consecutive_failures": streak }),
        );
    }
}

/// Route one binding to the thing that actually performs it.
async fn run_target(
    state: &Arc<AppState>,
    binding: &ActionBinding,
    ctx: &AlertContext,
    run_id: i64,
) -> ExecutionResult {
    match binding.kind {
        ActionKind::Script => run_script(state, binding, ctx, run_id).await,
        ActionKind::Service => run_service(state, binding).await,
        ActionKind::Container => run_container(binding).await,
    }
}

async fn run_script(
    state: &Arc<AppState>,
    binding: &ActionBinding,
    ctx: &AlertContext,
    run_id: i64,
) -> ExecutionResult {
    let manifest = {
        let reg = state.action_registry.read().await;
        reg.actions.get(&binding.target).cloned()
    };
    let Some(manifest) = manifest else {
        // The binding outlived its script — a reload dropped it, or the file
        // was renamed. Loud beats a no-op that looks like success.
        return ExecutionResult::failure(
            0,
            format!(
                "no action script named '{}' is loaded (check the actions directory, then POST /actions/reload)",
                binding.target
            ),
        );
    };

    let context = script_env(state, binding, ctx, run_id).await;
    let env = manifest.env_with(&context);
    let cap = crate::probes::runner::execute_capture(&manifest.exec_spec(&env)).await;

    let success = cap.spawn_error.is_none() && !cap.timed_out && cap.exit_code == Some(0);
    ExecutionResult {
        success,
        exit_code: cap.exit_code,
        duration_ms: cap.duration_ms,
        message: if success {
            format!("script '{}' exited 0", binding.target)
        } else {
            cap.failure_message()
        },
        output_tail: cap.output_tail(OUTPUT_TAIL_CAP),
    }
}

/// The environment an action script reads its situation from. Everything a
/// remediation needs to know without parsing anything: which rule, which
/// target, how bad, and whether this is the problem starting or ending.
async fn script_env(
    state: &Arc<AppState>,
    binding: &ActionBinding,
    ctx: &AlertContext,
    run_id: i64,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("REMON_EVENT".into(), ctx.trigger.as_str().to_string());
    env.insert("REMON_RULE".into(), ctx.rule_name.clone());
    env.insert("REMON_RULE_ID".into(), ctx.rule_id.to_string());
    env.insert("REMON_SEVERITY".into(), ctx.severity.as_str().to_string());
    env.insert("REMON_LABELS".into(), ctx.label_set.clone());
    env.insert("REMON_ACTION".into(), binding.target.clone());
    env.insert("REMON_ACTION_ID".into(), binding.id.to_string());
    env.insert("REMON_RUN_ID".into(), run_id.to_string());
    env.insert("REMON_MODE".into(), binding.mode.as_str().to_string());
    env.insert(
        "REMON_SERVER_NAME".into(),
        state.effective_config.read().await.server_name.clone(),
    );
    if let Some(v) = ctx.metric_value {
        env.insert("REMON_VALUE".into(), v.to_string());
    }

    // Individual labels as their own vars, so a script can read
    // `$REMON_LABEL_MOUNT_POINT` instead of parsing JSON in sh. Only keys
    // that are already identifier-shaped are promoted — anything else stays
    // available in REMON_LABELS and is not smuggled into the environment
    // under a mangled name.
    if let Ok(serde_json::Value::Object(map)) =
        serde_json::from_str::<serde_json::Value>(&ctx.label_set)
    {
        for (k, v) in map {
            if is_env_safe_key(&k)
                && let Some(s) = v.as_str()
            {
                env.insert(format!("REMON_LABEL_{}", k.to_uppercase()), s.to_string());
            }
        }
    }
    env
}

fn is_env_safe_key(k: &str) -> bool {
    !k.is_empty()
        && k.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

async fn run_service(state: &Arc<AppState>, binding: &ActionBinding) -> ExecutionResult {
    let start = std::time::Instant::now();
    let verb = match binding.verb {
        Some(v) => v,
        None => return ExecutionResult::failure(0, "service action has no verb"),
    };
    let mgr = &state.service_manager;
    let name = binding.target.as_str();
    let outcome = match verb {
        ActionVerb::Start => mgr.start(name).await,
        ActionVerb::Stop => mgr.stop(name).await,
        ActionVerb::Restart => mgr.restart(name).await,
        ActionVerb::Reload => mgr.reload(name).await,
    };
    let duration_ms = start.elapsed().as_millis() as i64;
    match outcome {
        Ok(()) => ExecutionResult {
            success: true,
            exit_code: Some(0),
            duration_ms,
            message: format!("{} {}", verb.as_str(), name),
            output_tail: None,
        },
        Err(e) => {
            ExecutionResult::failure(duration_ms, format!("{} {}: {}", verb.as_str(), name, e))
        }
    }
}

#[cfg(feature = "docker")]
async fn run_container(binding: &ActionBinding) -> ExecutionResult {
    let start = std::time::Instant::now();
    let verb = match binding.verb {
        Some(v) => v,
        None => return ExecutionResult::failure(0, "container action has no verb"),
    };
    let name = binding.target.as_str();
    let outcome = match verb {
        ActionVerb::Start => crate::services::docker::start_container(name).await,
        ActionVerb::Stop => crate::services::docker::stop_container(name).await,
        ActionVerb::Restart => crate::services::docker::restart_container(name).await,
        ActionVerb::Reload => {
            return ExecutionResult::failure(0, "containers have no reload; use restart");
        }
    };
    let duration_ms = start.elapsed().as_millis() as i64;
    match outcome {
        Ok(()) => ExecutionResult {
            success: true,
            exit_code: Some(0),
            duration_ms,
            message: format!("{} container {}", verb.as_str(), name),
            output_tail: None,
        },
        Err(e) => ExecutionResult::failure(
            duration_ms,
            format!("{} container {}: {}", verb.as_str(), name, e),
        ),
    }
}

#[cfg(not(feature = "docker"))]
async fn run_container(_binding: &ActionBinding) -> ExecutionResult {
    ExecutionResult::failure(0, "this build has no docker support")
}

/// Refusals that are properties of the target rather than of the moment.
/// Checked before drafting a proposal so an operator is never handed a
/// confirm button for something the server would then decline.
pub async fn precheck(state: &Arc<AppState>, binding: &ActionBinding) -> Result<(), String> {
    match binding.kind {
        ActionKind::Script => {
            let reg = state.action_registry.read().await;
            if !reg.actions.contains_key(&binding.target) {
                return Err(format!(
                    "no action script named '{}' is loaded",
                    binding.target
                ));
            }
        }
        ActionKind::Service => {
            // Same refusal `/services/{name}/stop` makes, for the same
            // reason: a stop the supervisor does not undo is permanent, and
            // an *automated* one is permanent and unattended.
            if matches!(
                binding.verb,
                Some(ActionVerb::Stop) | Some(ActionVerb::Restart)
            ) && crate::platform::identity::is_own_service(&binding.target)
            {
                return Err(format!(
                    "'{}' is remon-server itself; an action may not stop or restart it",
                    binding.target
                ));
            }
        }
        ActionKind::Container => {}
    }
    Ok(())
}

// ===== operator-driven paths =====

/// Confirm a pending proposal and run it. The claim is a conditional UPDATE,
/// so two operators tapping confirm at the same moment produce one execution.
pub async fn confirm_run(
    state: &Arc<AppState>,
    run_id: i64,
    device_id: &str,
) -> AppResult<ActionRun> {
    let repo = ActionRepository::new(state.db.clone());
    let run = repo
        .get_run(run_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Action run {}", run_id)))?;
    if run.status != RunStatus::Pending {
        return Err(AppError::Conflict(format!(
            "action run {} is already {}",
            run_id,
            run.status.as_str()
        )));
    }
    if !state.actions_config.enabled {
        return Err(AppError::Conflict(
            "the action engine is disabled (actions.enabled = false)".into(),
        ));
    }
    let binding_id = run
        .action_id
        .ok_or_else(|| AppError::Conflict("this proposal's binding has been deleted".into()))?;
    let binding = repo
        .get_binding(binding_id)
        .await?
        .ok_or_else(|| AppError::Conflict("this proposal's binding has been deleted".into()))?;

    // Re-check the target: the proposal may have sat in a queue while the
    // script was removed or the unit renamed.
    precheck(state, &binding)
        .await
        .map_err(AppError::Conflict)?;

    if !repo.claim_pending(run_id, device_id).await? {
        return Err(AppError::Conflict(format!(
            "action run {} was already handled",
            run_id
        )));
    }

    let ctx = AlertContext {
        rule_id: run.rule_id.unwrap_or(0),
        rule_name: run.rule_name.clone(),
        severity: AlertSeverity::Warn,
        label_set: run.label_set.clone(),
        trigger: run.trigger_event,
        metric_value: None,
    };
    execute_run(Arc::clone(state), binding, ctx, run_id).await;

    repo.get_run(run_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Action run {}", run_id)))
}

/// Decline a pending proposal.
pub async fn dismiss_run(
    state: &Arc<AppState>,
    run_id: i64,
    device_id: &str,
) -> AppResult<ActionRun> {
    let repo = ActionRepository::new(state.db.clone());
    if !repo.dismiss_pending(run_id, device_id).await? {
        let existing = repo.get_run(run_id).await?;
        return Err(match existing {
            Some(r) => AppError::Conflict(format!(
                "action run {} is already {}",
                run_id,
                r.status.as_str()
            )),
            None => AppError::NotFound(format!("Action run {}", run_id)),
        });
    }
    repo.get_run(run_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Action run {}", run_id)))
}

/// Run a binding right now, on an operator's say-so, with no transition
/// behind it. The "does this actually work" button — it bypasses the
/// guardrails that exist to restrain *unattended* repetition (cooldown,
/// hourly ceiling, `actions.auto`), because a human asked for exactly one
/// run, but it keeps single-flight and the target precheck.
pub async fn run_binding_now(
    state: &Arc<AppState>,
    binding: &ActionBinding,
    device_id: &str,
) -> AppResult<i64> {
    if !state.actions_config.enabled {
        return Err(AppError::Conflict(
            "the action engine is disabled (actions.enabled = false)".into(),
        ));
    }
    let repo = ActionRepository::new(state.db.clone());
    if repo.has_in_flight(binding.id, "{}").await? {
        return Err(AppError::Conflict(
            "a run for this binding is already pending or in flight".into(),
        ));
    }
    precheck(state, binding).await.map_err(AppError::Conflict)?;

    let ctx = AlertContext {
        rule_id: binding.rule_id,
        rule_name: rule_name_for(state, binding.rule_id).await,
        severity: AlertSeverity::Warn,
        label_set: "{}".to_string(),
        trigger: ActionTrigger::Manual,
        metric_value: None,
    };
    let run_id = repo
        .insert_run(&new_run(
            binding,
            &ctx,
            RunStatus::Running,
            RunOrigin::Confirmed,
            Some(device_id.to_string()),
            None,
            Some(format!("operator-requested: {}", binding.summary())),
        ))
        .await?;
    execute_run(Arc::clone(state), binding.clone(), ctx, run_id).await;
    Ok(run_id)
}

// ===== background upkeep =====

/// Retire proposals nobody answered, on a slow loop. An expired proposal is
/// put on the timeline rather than dropped: "we offered to restart nginx and
/// nobody said yes" is a fact about the incident.
pub fn spawn_proposal_sweeper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let repo = ActionRepository::new(state.db.clone());
        let mut shutdown = state.shutdown.subscribe();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS)) => {}
                _ = shutdown.changed() => break,
            }
            let now = chrono::Utc::now().timestamp();
            match repo.expire_stale_proposals(now).await {
                Ok(expired) => {
                    for run in expired {
                        info!(
                            "action proposal {} expired unconfirmed ({})",
                            run.id, run.target
                        );
                        record_event(
                            &state,
                            "action_expired",
                            "warn",
                            format!(
                                "Proposed action for '{}' expired unconfirmed: {}",
                                run.rule_name, run.target
                            ),
                            run.id,
                            serde_json::json!({
                                "action_id": run.action_id,
                                "rule": run.rule_name,
                                "label_set": run.label_set,
                            }),
                        );
                    }
                }
                Err(e) => warn!("proposal sweep failed: {:?}", e),
            }
        }
    });
}

/// Startup recovery. A `running` row can only belong to a process that died
/// mid-execution; left alone it holds the single-flight latch shut forever
/// and the binding never acts again.
pub async fn recover_orphaned_runs(db: &sqlx::SqlitePool) {
    let repo = ActionRepository::new(db.clone());
    match repo.fail_orphaned_runs().await {
        Ok(0) => {}
        Ok(n) => warn!(
            "marked {} action run(s) failed: they were in flight when the server last stopped",
            n
        ),
        Err(e) => warn!("could not recover orphaned action runs: {:?}", e),
    }
}

// ===== helpers =====

#[allow(clippy::too_many_arguments)]
fn new_run(
    binding: &ActionBinding,
    ctx: &AlertContext,
    status: RunStatus,
    origin: RunOrigin,
    requested_by: Option<String>,
    expires_at: Option<i64>,
    message: Option<String>,
) -> NewActionRun {
    NewActionRun {
        action_id: Some(binding.id),
        rule_id: Some(ctx.rule_id).filter(|id| *id > 0),
        rule_name: ctx.rule_name.clone(),
        label_set: ctx.label_set.clone(),
        kind: binding.kind,
        target: binding.target.clone(),
        verb: binding.verb,
        trigger_event: ctx.trigger,
        origin,
        requested_by,
        status,
        expires_at,
        message,
    }
}

/// Put an action on the `/events` timeline, through the same ledger every
/// other producer uses — so a fire, the action it drafted, and the outcome
/// read as one sequence rather than three places to look.
///
/// `ref_type = "action_run"` lets a client jump from the timeline row to the
/// run detail (and from there to the binding that drafted it).
fn record_event(
    state: &Arc<AppState>,
    kind: &'static str,
    severity: &'static str,
    message: String,
    run_id: i64,
    details: serde_json::Value,
) {
    crate::services::events::record(
        state,
        NewHostEvent {
            created_at: None,
            source: "system",
            kind,
            severity,
            message,
            actor_device_id: None,
            actor_name: None,
            ref_type: Some("action_run"),
            ref_id: Some(run_id.to_string()),
            details: Some(details.to_string()),
        },
    );
}

async fn rule_name_for(state: &Arc<AppState>, rule_id: i64) -> String {
    use crate::storage::repositories::AlertRepository;
    let repo = AlertRepository::new(state.db.clone());
    match repo.get(rule_id).await {
        Ok(Some(r)) => r.name,
        _ => format!("rule {}", rule_id),
    }
}

fn severity_of(s: AlertSeverity) -> Severity {
    match s {
        AlertSeverity::Warn => Severity::Warn,
        AlertSeverity::Crit => Severity::Crit,
    }
}

/// Alert severities onto the host-event ledger's narrower set: it has no
/// `crit` tier, and `error` is the one that means "this needed someone".
fn severity_str(s: AlertSeverity) -> &'static str {
    match s {
        AlertSeverity::Warn => "warn",
        AlertSeverity::Crit => "error",
    }
}

/// `{"mount_point":"/"}` → ` for {"mount_point":"/"}`; `{}` → ``. Keeps the
/// unlabelled case from reading like a bug in the message.
fn label_suffix(label_set: &str) -> String {
    if label_set.trim() == "{}" || label_set.trim().is_empty() {
        String::new()
    } else {
        format!(" for {}", label_set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_suffix_is_quiet_when_unlabelled() {
        assert_eq!(label_suffix("{}"), "");
        assert_eq!(
            label_suffix(r#"{"mount_point":"/"}"#),
            r#" for {"mount_point":"/"}"#
        );
    }

    #[test]
    fn only_identifier_shaped_labels_reach_the_environment() {
        assert!(is_env_safe_key("mount_point"));
        assert!(is_env_safe_key("probe_name"));
        // Leading digit, dashes, dots and anything that would need quoting
        // stay out — they are still readable in REMON_LABELS.
        assert!(!is_env_safe_key("2fast"));
        assert!(!is_env_safe_key("mount-point"));
        assert!(!is_env_safe_key("dev/sda"));
        assert!(!is_env_safe_key(""));
    }
}
