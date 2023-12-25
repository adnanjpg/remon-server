use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct GetDiskStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct SingleDiskInfo {
    pub id: i64,
    pub frame_id: i64,
    // the id of the disk, consists from key info like name, fs, etc.
    pub disk_id: String,
    pub available: i64,
}

trait SingleDiskInfoUsage {
    fn get_usage_percent(&self, total: i64) -> f64;
}

impl SingleDiskInfoUsage for SingleDiskInfo {
    fn get_usage_percent(&self, total: i64) -> f64 {
        let used_amount = total - self.available;

        let perc = (used_amount as f64 / total as f64) * 100.0;

        perc
    }
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct DiskFrameStatus {
    pub id: i64,
    pub last_check: i64,
    // usage for each disk
    pub disks_usage: Vec<SingleDiskInfo>,
}

// string is disk_id, i64 is total space
pub type DiskTotalSpaceMap = HashMap<String, i64>;
pub type DiskUsageMeanMap = HashMap<String, i64>;
pub trait DiskStatusDataTrait {
    fn disks_usage_means(&self, total_spaces: &DiskTotalSpaceMap) -> DiskUsageMeanMap;
    fn disks_usage_means_percentages(&self, total_spaces: &DiskTotalSpaceMap) -> DiskUsageMeanMap;
}

impl DiskStatusDataTrait for DiskStatusData {
    fn disks_usage_means(&self, total_spaces: &DiskTotalSpaceMap) -> DiskUsageMeanMap {
        let mut res = HashMap::new() as DiskUsageMeanMap;

        for total_space in total_spaces {
            let mut count_avail = 0;
            let usage_mean_sum = &self
                .frames
                .iter()
                .map(|f| {
                    let mut has_any = false;
                    let mut sum_avail = 0;

                    f.disks_usage.iter().for_each(|us| {
                        if us.disk_id == total_space.0.to_string() {
                            count_avail += 1;
                            sum_avail += us.available;

                            has_any = true;
                        }
                    });

                    if !has_any {
                        return -1;
                    }

                    let usage_sum = total_space.1 - sum_avail;

                    return usage_sum;
                })
                .filter(|&e| e != -1)
                .sum::<i64>();

            let usage_mean = usage_mean_sum / count_avail;

            res.insert(total_space.0.to_string(), usage_mean);
        }

        return res;
    }

    fn disks_usage_means_percentages(&self, total_spaces: &DiskTotalSpaceMap) -> DiskUsageMeanMap {
        let means_usages = self.disks_usage_means(total_spaces);

        let mut new_map = HashMap::new() as DiskUsageMeanMap;

        for total_space in total_spaces {
            let usage = means_usages.get(total_space.0);

            if let Some(usage) = usage {
                let per = usage * 100 / total_space.1;

                new_map.insert(total_space.0.to_string(), per);

                continue;
            }
        }

        return new_map;
    }
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct DiskStatusData {
    // usage for each frame, the size
    // of the frame is defined in the config
    // where the user picks the frequency
    // of the monitoring
    pub frames: Vec<DiskFrameStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use maplit::hashmap;

    #[test]
    fn disk_singles_usage_mean_test() {
        let disk1id = "disk1id";
        let disk2id = "disk2id";
        let disk3id = "disk3id";

        let data = DiskStatusData {
            frames: vec![
                DiskFrameStatus {
                    id: -1,
                    last_check: -1,
                    disks_usage: vec![
                        SingleDiskInfo {
                            id: -1,
                            disk_id: disk1id.to_string(),
                            frame_id: -1,
                            available: 30,
                        },
                        SingleDiskInfo {
                            id: -1,
                            disk_id: disk3id.to_string(),
                            frame_id: -1,
                            available: 40,
                        },
                    ],
                },
                DiskFrameStatus {
                    id: -1,
                    last_check: -1,
                    disks_usage: vec![
                        SingleDiskInfo {
                            id: -1,
                            disk_id: disk1id.to_string(),
                            frame_id: -1,
                            available: 20,
                        },
                        SingleDiskInfo {
                            id: -1,
                            disk_id: disk2id.to_string(),
                            frame_id: -1,
                            available: 65,
                        },
                    ],
                },
                DiskFrameStatus {
                    id: -1,
                    last_check: -1,
                    disks_usage: vec![
                        SingleDiskInfo {
                            id: -1,
                            disk_id: disk2id.to_string(),
                            frame_id: -1,
                            available: 90,
                        },
                        SingleDiskInfo {
                            id: -1,
                            disk_id: disk3id.to_string(),
                            frame_id: -1,
                            available: 10,
                        },
                    ],
                },
            ],
        };

        // disk1id: avail (30 + 20) / 2 = 25
        // disk2id: avail (65 + 90) / 2 = 77.5
        // disk3id: avail (40 + 10) / 2 = 25

        let mut totals: DiskTotalSpaceMap = hashmap! {
            // 25 available, 175 used, 87.5% used
            disk1id.to_string() => 200,
            // 77.5 available, 72.5 used, 48.33% used
            disk2id.to_string() => 150,
            // 25 available, 45 used, 64.28% used
            disk3id.to_string() => 70,
        };

        assert_eq!(
            data.disks_usage_means(&mut totals),
            hashmap! {
                disk1id.to_string() => 175,
                disk2id.to_string() => 72,
                disk3id.to_string() => 45,
            }
        );
        assert_eq!(
            data.disks_usage_means_percentages(&mut totals),
            hashmap! {
                disk1id.to_string() => 87,
                disk2id.to_string() => 48,
                disk3id.to_string() => 64,
            }
        );
    }
}
