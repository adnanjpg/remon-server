//! Alert-action REST DTOs.

use serde::{Deserialize, Serialize};

use crate::models::action::{
    ActionBinding, ActionKind, ActionMode, ActionRun, ActionTrigger, ActionVerb, OnEvent,
    RunOrigin, RunStatus,
};

// ===== catalogue =====

/// One thing a binding can point at. Scripts come from the actions directory;
/// catalogue entries are compiled in.
#[derive(Debug, Serialize)]
pub struct ActionCatalogEntry {
    pub kind: ActionKind,
    /// Script name, or the empty string for a catalogue kind whose target is
    /// chosen per binding (any unit name, any container).
    pub target: Option<String>,
    pub description: Option<String>,
    /// Verbs this kind accepts. Empty for scripts.
    pub verbs: Vec<ActionVerb>,
    /// Only meaningful for scripts: where the file came from and how long it
    /// is allowed to run.
    pub source_path: Option<String>,
    pub timeout_ms: Option<u64>,
    pub platforms: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ActionCatalogResponse {
    /// Whether the engine will execute anything at all right now.
    pub enabled: bool,
    /// Whether `auto` bindings are permitted on this host.
    pub auto: bool,
    pub proposal_ttl_secs: u64,
    pub entries: Vec<ActionCatalogEntry>,
}

#[derive(Debug, Serialize)]
pub struct ReloadFailure {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct ReloadActionsResponse {
    pub loaded: Vec<String>,
    pub skipped_disabled: Vec<String>,
    pub skipped_platform: Vec<String>,
    pub failed: Vec<ReloadFailure>,
}

// ===== bindings =====

#[derive(Debug, Serialize)]
pub struct ActionBindingDto {
    pub id: i64,
    pub rule_id: i64,
    pub kind: ActionKind,
    pub target: String,
    pub verb: Option<ActionVerb>,
    pub on_event: OnEvent,
    pub mode: ActionMode,
    pub enabled: bool,
    pub cooldown_secs: i64,
    pub max_runs_per_hour: i64,
    pub failure_limit: i64,
    pub consecutive_failures: i64,
    /// Non-null means the binding turned *itself* off — distinct from an
    /// operator having disabled it, which leaves this null.
    pub disabled_reason: Option<String>,
    /// Rendered one-liner, so a client doesn't have to reassemble
    /// kind/verb/target to show what this does.
    pub summary: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<ActionBinding> for ActionBindingDto {
    fn from(b: ActionBinding) -> Self {
        let summary = b.summary();
        Self {
            id: b.id,
            rule_id: b.rule_id,
            kind: b.kind,
            target: b.target,
            verb: b.verb,
            on_event: b.on_event,
            mode: b.mode,
            enabled: b.enabled,
            cooldown_secs: b.cooldown_secs,
            max_runs_per_hour: b.max_runs_per_hour,
            failure_limit: b.failure_limit,
            consecutive_failures: b.consecutive_failures,
            disabled_reason: b.disabled_reason,
            summary,
            created_at: b.created_at,
            updated_at: b.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListActionBindingsResponse {
    pub bindings: Vec<ActionBindingDto>,
}

#[derive(Debug, Deserialize)]
pub struct CreateActionBindingRequest {
    pub kind: ActionKind,
    pub target: String,
    #[serde(default)]
    pub verb: Option<ActionVerb>,
    #[serde(default = "default_on_event")]
    pub on_event: OnEvent,
    /// Defaults to `manual` — a binding created without saying otherwise
    /// asks before it acts.
    #[serde(default = "default_mode")]
    pub mode: ActionMode,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: i64,
    #[serde(default = "default_max_runs")]
    pub max_runs_per_hour: i64,
    #[serde(default = "default_failure_limit")]
    pub failure_limit: i64,
}

/// Full replace, like `PUT /alerts/{id}`. Every field is required so an
/// update can never half-apply a client's stale view of the binding.
#[derive(Debug, Deserialize)]
pub struct UpdateActionBindingRequest {
    pub kind: ActionKind,
    pub target: String,
    #[serde(default)]
    pub verb: Option<ActionVerb>,
    #[serde(default = "default_on_event")]
    pub on_event: OnEvent,
    #[serde(default = "default_mode")]
    pub mode: ActionMode,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: i64,
    #[serde(default = "default_max_runs")]
    pub max_runs_per_hour: i64,
    #[serde(default = "default_failure_limit")]
    pub failure_limit: i64,
}

fn default_on_event() -> OnEvent {
    OnEvent::Fired
}
fn default_mode() -> ActionMode {
    ActionMode::Manual
}
fn default_true() -> bool {
    true
}
fn default_cooldown() -> i64 {
    300
}
fn default_max_runs() -> i64 {
    3
}
fn default_failure_limit() -> i64 {
    3
}

// ===== runs =====

#[derive(Debug, Serialize)]
pub struct ActionRunDto {
    pub id: i64,
    pub action_id: Option<i64>,
    pub rule_id: Option<i64>,
    pub rule_name: String,
    pub label_set: String,
    pub kind: ActionKind,
    pub target: String,
    pub verb: Option<ActionVerb>,
    pub trigger_event: ActionTrigger,
    pub origin: RunOrigin,
    pub requested_by: Option<String>,
    pub status: RunStatus,
    pub expires_at: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub exit_code: Option<i32>,
    pub message: Option<String>,
    pub output_tail: Option<String>,
    pub created_at: i64,
}

impl From<ActionRun> for ActionRunDto {
    fn from(r: ActionRun) -> Self {
        Self {
            id: r.id,
            action_id: r.action_id,
            rule_id: r.rule_id,
            rule_name: r.rule_name,
            label_set: r.label_set,
            kind: r.kind,
            target: r.target,
            verb: r.verb,
            trigger_event: r.trigger_event,
            origin: r.origin,
            requested_by: r.requested_by,
            status: r.status,
            expires_at: r.expires_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
            duration_ms: r.duration_ms,
            exit_code: r.exit_code,
            message: r.message,
            output_tail: r.output_tail,
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListActionRunsResponse {
    pub runs: Vec<ActionRunDto>,
}

#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    /// One of the `RunStatus` strings; `pending` is the useful one — it is
    /// the operator's inbox.
    pub status: Option<String>,
    pub action_id: Option<i64>,
    pub rule_id: Option<i64>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}
