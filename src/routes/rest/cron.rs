use axum::Json;

use crate::error::AppResult;
use crate::platform::cron;
use crate::routes::dtos::cron::{CronJobDto, ListCronJobsResponse};
use crate::routes::extractors::Claims;

/// GET /cron — list all discoverable cron jobs.
///
/// Reads /etc/crontab, /etc/cron.d/*, and /var/spool/cron/crontabs/*.
/// Unreadable files are silently skipped. Returns an empty list on Windows.
pub async fn list_cron_jobs(_claims: Claims) -> AppResult<Json<ListCronJobsResponse>> {
    let jobs = cron::list().await;
    Ok(Json(ListCronJobsResponse {
        jobs: jobs.into_iter().map(CronJobDto::from).collect(),
    }))
}
