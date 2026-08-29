//! Alert-action domain types.
//!
//! Three concepts, mirroring the alert engine's split:
//! - `ActionBinding` — the operator-defined "when this rule fires, do that".
//!   Lives in `alert_actions`.
//! - `ActionRun` — one append-only row per drafted / attempted / refused
//!   action. Lives in `action_runs`, and doubles as the pending queue for
//!   `manual` bindings.
//! - `ExecutionResult` — what one attempt produced, before it is written
//!   back onto the run row.
//!
//! Nothing here decides *whether* to act — that is the executor's job in
//! `services/actions.rs`. These types carry the vocabulary: what can be run
//! (`ActionKind` + `ActionVerb`), on which transition (`OnEvent`), with how
//! much authority (`ActionMode`), and how it ended (`RunStatus`).

use serde::{Deserialize, Serialize};

/// What sort of thing a binding runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    /// An operator-authored file in the actions directory, executed by the
    /// probe runner (argv only, never a shell).
    Script,
    /// `ServiceManager::{start,stop,restart,reload}` — the same call the
    /// `/services/{name}/{verb}` endpoints make.
    Service,
    /// Docker/Podman container lifecycle — the same calls `/docker/*` makes.
    Container,
}

impl ActionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionKind::Script => "script",
            ActionKind::Service => "service",
            ActionKind::Container => "container",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "script" => Some(ActionKind::Script),
            "service" => Some(ActionKind::Service),
            "container" => Some(ActionKind::Container),
            _ => None,
        }
    }
}

/// Which button the catalog kinds press. Scripts carry no verb — the script
/// itself is the verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionVerb {
    Start,
    Stop,
    Restart,
    Reload,
}

impl ActionVerb {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionVerb::Start => "start",
            ActionVerb::Stop => "stop",
            ActionVerb::Restart => "restart",
            ActionVerb::Reload => "reload",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "start" => Some(ActionVerb::Start),
            "stop" => Some(ActionVerb::Stop),
            "restart" => Some(ActionVerb::Restart),
            "reload" => Some(ActionVerb::Reload),
            _ => None,
        }
    }

    /// Verbs each kind accepts. `reload` is systemd-shaped and has no
    /// container equivalent; the backend may still answer `NotSupported`,
    /// which surfaces as a failed run rather than a rejected binding.
    pub fn allowed_for(kind: ActionKind) -> &'static [ActionVerb] {
        match kind {
            ActionKind::Script => &[],
            ActionKind::Service => &[
                ActionVerb::Start,
                ActionVerb::Stop,
                ActionVerb::Restart,
                ActionVerb::Reload,
            ],
            ActionKind::Container => &[ActionVerb::Start, ActionVerb::Stop, ActionVerb::Restart],
        }
    }
}

/// Which side of the alert lifecycle a binding listens to. `Resolved` is not
/// an afterthought: "scale back down", "re-enable the cron I paused", "clear
/// the maintenance flag" are recovery actions, and wiring them to the same
/// rule keeps the pair legible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnEvent {
    Fired,
    Resolved,
    Both,
}

impl OnEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            OnEvent::Fired => "fired",
            OnEvent::Resolved => "resolved",
            OnEvent::Both => "both",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fired" => Some(OnEvent::Fired),
            "resolved" => Some(OnEvent::Resolved),
            "both" => Some(OnEvent::Both),
            _ => None,
        }
    }

    pub fn covers(&self, trigger: ActionTrigger) -> bool {
        match (self, trigger) {
            (OnEvent::Both, ActionTrigger::Fired | ActionTrigger::Resolved) => true,
            (OnEvent::Fired, ActionTrigger::Fired) => true,
            (OnEvent::Resolved, ActionTrigger::Resolved) => true,
            // A manual run is never *matched* by on_event — the operator
            // asked for this specific binding by id.
            _ => false,
        }
    }
}

/// How much authority a binding has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionMode {
    /// Draft a proposal, page an operator, run only on confirmation.
    /// The default, and the reason this feature is safe to ship.
    Manual,
    /// Run unattended, under the binding's guardrails.
    Auto,
    /// Record what would have run. For arming a binding with evidence
    /// instead of optimism.
    DryRun,
}

impl ActionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionMode::Manual => "manual",
            ActionMode::Auto => "auto",
            ActionMode::DryRun => "dry_run",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "manual" => Some(ActionMode::Manual),
            "auto" => Some(ActionMode::Auto),
            "dry_run" => Some(ActionMode::DryRun),
            _ => None,
        }
    }
}

/// What drafted a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionTrigger {
    Fired,
    Resolved,
    /// No transition behind it — an operator ran the binding from the API.
    Manual,
}

impl ActionTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionTrigger::Fired => "fired",
            ActionTrigger::Resolved => "resolved",
            ActionTrigger::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fired" => Some(ActionTrigger::Fired),
            "resolved" => Some(ActionTrigger::Resolved),
            "manual" => Some(ActionTrigger::Manual),
            _ => None,
        }
    }
}

/// Under whose authority a run actually executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOrigin {
    Auto,
    Confirmed,
    DryRun,
}

impl RunOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunOrigin::Auto => "auto",
            RunOrigin::Confirmed => "confirmed",
            RunOrigin::DryRun => "dry_run",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(RunOrigin::Auto),
            "confirmed" => Some(RunOrigin::Confirmed),
            "dry_run" => Some(RunOrigin::DryRun),
            _ => None,
        }
    }
}

/// Terminal and non-terminal states of one run row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    /// A `manual` binding's proposal, awaiting confirmation until `expires_at`.
    Pending,
    /// Claimed by the executor. Also the single-flight latch: a second
    /// transition for the same (binding, label_set) will not start while a
    /// row is in this state.
    Running,
    Succeeded,
    Failed,
    /// Refused before execution — cooldown, hourly ceiling, single-flight,
    /// a disarmed binding, or a dry run. `message` says which.
    Skipped,
    /// A proposal nobody confirmed in time.
    Expired,
    /// A proposal an operator declined.
    Dismissed,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Pending => "pending",
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Skipped => "skipped",
            RunStatus::Expired => "expired",
            RunStatus::Dismissed => "dismissed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(RunStatus::Pending),
            "running" => Some(RunStatus::Running),
            "succeeded" => Some(RunStatus::Succeeded),
            "failed" => Some(RunStatus::Failed),
            "skipped" => Some(RunStatus::Skipped),
            "expired" => Some(RunStatus::Expired),
            "dismissed" => Some(RunStatus::Dismissed),
            _ => None,
        }
    }
}

/// Row mirror of `alert_actions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionBinding {
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
    /// Set when something other than an operator turned this off — today
    /// only the circuit breaker. Cleared when the binding is re-enabled.
    pub disabled_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl ActionBinding {
    /// Human-readable one-liner used in proposals, notifications and the
    /// timeline: "restart service nginx.service", "run script drain-cache".
    pub fn summary(&self) -> String {
        match self.verb {
            Some(v) => format!("{} {} {}", v.as_str(), self.kind.as_str(), self.target),
            None => format!("run {} {}", self.kind.as_str(), self.target),
        }
    }
}

/// Row mirror of `action_runs`. Denormalized on purpose — see the schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRun {
    pub id: i64,
    /// `None` once the binding has been deleted; the run outlives it.
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

/// Outcome of one execution attempt, before it is written back to the row.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: i64,
    pub message: String,
    pub output_tail: Option<String>,
}

impl ExecutionResult {
    pub fn failure(duration_ms: i64, message: impl Into<String>) -> Self {
        Self {
            success: false,
            exit_code: None,
            duration_ms,
            message: message.into(),
            output_tail: None,
        }
    }
}
