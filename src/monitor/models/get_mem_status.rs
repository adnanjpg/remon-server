use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct GetCpuStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GetMemStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}

// we actually only fetch a single mem data, but we're cloning this into 2 structs for convenience, so it would be read the same as the cpu and disk
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct SingleMemInfo {
    pub id: i64,
    pub frame_id: i64,
    // the id of the mem, currently we only have a single mem so this is going to be a constant
    pub mem_id: String,
    pub available: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MemFrameStatus {
    pub id: i64,
    pub last_check: i64,
    // usage for each mem
    pub mems_usage: Vec<SingleMemInfo>,
}

// string is mem_id, i64 is total space
pub type MemTotalSpaceMap = HashMap<String, i64>;
pub type MemUsageMeanMap = HashMap<String, i64>;
pub trait MemStatusDataTrait {
    fn mems_usage_means(&self, total_spaces: &MemTotalSpaceMap) -> MemUsageMeanMap;
    fn mems_usage_means_percentages(&self, total_spaces: &MemTotalSpaceMap) -> MemUsageMeanMap;
}

impl MemStatusDataTrait for MemStatusData {
    fn mems_usage_means(&self, total_spaces: &MemTotalSpaceMap) -> MemUsageMeanMap {
        let mut res = HashMap::new() as MemUsageMeanMap;

        for total_space in total_spaces {
            let mut count_avail = 0;
            let usage_mean_sum = &self
                .frames
                .iter()
                .map(|f| {
                    let mut has_any = false;
                    let mut sum_avail = 0;

                    f.mems_usage.iter().for_each(|us| {
                        if us.mem_id == total_space.0.to_string() {
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

    fn mems_usage_means_percentages(&self, total_spaces: &MemTotalSpaceMap) -> MemUsageMeanMap {
        let means_usages = self.mems_usage_means(total_spaces);

        let mut new_map = HashMap::new() as MemUsageMeanMap;

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
pub struct MemStatusData {
    pub frames: Vec<MemFrameStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use maplit::hashmap;

    #[test]
    fn mem_singles_usage_mean_test() {
        let mem1id = "mem1id";
        let mem2id = "mem2id";
        let mem3id = "mem3id";

        let data = MemStatusData {
            frames: vec![
                MemFrameStatus {
                    id: -1,
                    last_check: -1,
                    mems_usage: vec![
                        SingleMemInfo {
                            id: -1,
                            mem_id: mem1id.to_string(),
                            frame_id: -1,
                            available: 30,
                        },
                        SingleMemInfo {
                            id: -1,
                            mem_id: mem3id.to_string(),
                            frame_id: -1,
                            available: 40,
                        },
                    ],
                },
                MemFrameStatus {
                    id: -1,
                    last_check: -1,
                    mems_usage: vec![
                        SingleMemInfo {
                            id: -1,
                            mem_id: mem1id.to_string(),
                            frame_id: -1,
                            available: 20,
                        },
                        SingleMemInfo {
                            id: -1,
                            mem_id: mem2id.to_string(),
                            frame_id: -1,
                            available: 65,
                        },
                    ],
                },
                MemFrameStatus {
                    id: -1,
                    last_check: -1,
                    mems_usage: vec![
                        SingleMemInfo {
                            id: -1,
                            mem_id: mem2id.to_string(),
                            frame_id: -1,
                            available: 90,
                        },
                        SingleMemInfo {
                            id: -1,
                            mem_id: mem3id.to_string(),
                            frame_id: -1,
                            available: 10,
                        },
                    ],
                },
            ],
        };

        // mem1id: avail (30 + 20) / 2 = 25
        // mem2id: avail (65 + 90) / 2 = 77.5
        // mem3id: avail (40 + 10) / 2 = 25

        let mut totals: MemTotalSpaceMap = hashmap! {
            // 25 available, 175 used, 87.5% used
            mem1id.to_string() => 200,
            // 77.5 available, 72.5 used, 48.33% used
            mem2id.to_string() => 150,
            // 25 available, 45 used, 64.28% used
            mem3id.to_string() => 300,
        };

        assert_eq!(
            data.mems_usage_means(&mut totals),
            hashmap! {
                mem1id.to_string() => 175,
                mem2id.to_string() => 72,
                mem3id.to_string() => 45,
            }
        );
        assert_eq!(
            data.mems_usage_means_percentages(&mut totals),
            hashmap! {
                mem1id.to_string() => 87,
                mem2id.to_string() => 48,
                mem3id.to_string() => 64,
            }
        );
    }
}
