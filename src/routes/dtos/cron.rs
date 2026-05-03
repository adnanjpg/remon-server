use serde::Serialize;

use crate::platform::cron::CronJob;

#[derive(Debug, Serialize)]
pub struct CronJobDto {
    pub schedule: String,
    pub user: Option<String>,
    pub command: String,
    pub source: String,
}

impl From<CronJob> for CronJobDto {
    fn from(j: CronJob) -> Self {
        Self {
            schedule: j.schedule,
            user: j.user,
            command: j.command,
            source: j.source,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListCronJobsResponse {
    pub jobs: Vec<CronJobDto>,
}
