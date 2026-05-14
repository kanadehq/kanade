use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HwInventory {
    pub pc_id: String,
    pub hostname: String,
    pub os_name: String,
    pub os_version: String,
    pub os_build: Option<String>,
    pub cpu_model: String,
    pub cpu_cores: u32,
    pub ram_bytes: u64,
    pub disks: Vec<DiskInfo>,
    pub collected_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiskInfo {
    pub device_id: String,
    pub size_bytes: u64,
    pub free_bytes: u64,
    pub file_system: Option<String>,
}
