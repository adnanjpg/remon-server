use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct GetCpuStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct CpuCoreInfo {
    pub id: i64,
    pub frame_id: i64,
    // the id of the cpu chip, consists from key info like vendor_id, brand, etc.
    pub cpu_id: String,
    pub freq: i64,
    pub usage: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct CpuFrameStatus {
    pub id: i64,
    pub last_check: i64,
    pub cores_usage: Vec<CpuCoreInfo>,
}

pub trait CpuFrameStatusTrait {
    fn cores_usage_mean(&self) -> Option<f64>;
}

impl CpuFrameStatusTrait for CpuFrameStatus {
    fn cores_usage_mean(&self) -> Option<f64> {
        let crs_usg = &self.cores_usage;

        let sum: Option<f64> = crs_usg.iter().map(|u| u.usage as f64).reduce(|a, b| a + b);

        if let Some(sum) = sum {
            let mean = sum / crs_usg.len() as f64;

            return Some(mean);
        }

        None
    }
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct CpuStatusData {
    pub frames: Vec<CpuFrameStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_cores_usage_mean_test() {
        let stat = CpuFrameStatus {
            id: -1,
            last_check: -1,
            cores_usage: vec![
                CpuCoreInfo {
                    id: -1,
                    cpu_id: "".to_string(),
                    frame_id: -1,
                    freq: -1,
                    usage: 30,
                },
                CpuCoreInfo {
                    id: -1,
                    cpu_id: "".to_string(),
                    frame_id: -1,
                    freq: -1,
                    usage: 20,
                },
                CpuCoreInfo {
                    id: -1,
                    cpu_id: "".to_string(),
                    frame_id: -1,
                    freq: -1,
                    usage: 10,
                },
            ],
        };

        assert_eq!(stat.cores_usage_mean().unwrap(), 20.0);
    }
}
