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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn hw_inventory_round_trips_with_disks() {
        let inv = HwInventory {
            pc_id: "pc-01".into(),
            hostname: "PC-01".into(),
            os_name: "Windows 11 Pro".into(),
            os_version: "10.0.26200".into(),
            os_build: Some("26200".into()),
            cpu_model: "Intel Core i7".into(),
            cpu_cores: 12,
            ram_bytes: 64 * 1024 * 1024 * 1024,
            disks: vec![DiskInfo {
                device_id: "C:".into(),
                size_bytes: 1_000_000_000_000,
                free_bytes: 500_000_000_000,
                file_system: Some("NTFS".into()),
            }],
            collected_at: chrono::Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0).unwrap(),
        };
        let json = serde_json::to_string(&inv).unwrap();
        let back: HwInventory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.disks.len(), 1);
        assert_eq!(back.disks[0].device_id, "C:");
        assert_eq!(back.cpu_cores, 12);
        assert_eq!(back.ram_bytes, inv.ram_bytes);
    }
}
