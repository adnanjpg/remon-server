mod actions;
mod alerts;
mod config;
mod devices;
#[cfg(feature = "docker")]
mod docker;
mod heartbeats;
mod host_events;
mod incidents;
pub mod logs;
mod metrics;
mod notifications;
mod probes;
mod process;
mod resolutions;
mod retention;
mod rollup_state;
mod runtime_state;
mod smart;

pub use actions::{ActionRepository, NewActionRun, RunQuery, UpsertActionBinding};
pub use alerts::{AlertEventWithRule, AlertRepository, UpsertAlertRule};
pub use config::{ConfigRepository, RuntimeOverrides};
pub use devices::DeviceRepository;
#[cfg(feature = "docker")]
pub use docker::{DockerMetricsRepository, DockerStatsRow};
pub use heartbeats::{HeartbeatRepository, UpsertHeartbeatCheck};
pub use host_events::{HostEventRepository, HostEventRow, NewHostEvent};
pub use incidents::{IncidentRepository, IncidentSummaryRow, NewIncident};
pub use logs::LogRepository;
pub use metrics::MetricsRepository;
pub use notifications::{NotificationChannelRepository, NotificationChannelRow};
pub use probes::{ProbeDefinitionRow, ProbeRepository};
pub use process::{ProcessGroupRow, ProcessMetricsRepository};
pub use resolutions::{Resolution, ResolutionRepository};
pub use retention::RetentionRepository;
pub use rollup_state::RollupStateRepository;
pub use runtime_state::RuntimeStateRepository;
pub use smart::{SmartDeviceRow, SmartRepository};

/// `["a","b"]` → `",a,b,"` — the shape the `instr(csv, ',' || col || ',')`
/// membership test expects. One bound parameter instead of an `IN` list built
/// by string concatenation, so the query stays a checked `query!` macro and no
/// caller-supplied text ever reaches the SQL text itself.
pub(crate) fn wrap_csv(items: &[String]) -> String {
    format!(",{},", items.join(","))
}
