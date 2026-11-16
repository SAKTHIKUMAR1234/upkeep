use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nix::sys::statvfs::statvfs;

use super::{Level, Observation, evaluate, percent};
use crate::monitor::settings::MonitorSettings;

/// File system types that hold data. Pseudo and image file systems are skipped.
const DISK_TYPES: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "f2fs", "vfat", "nfs", "nfs4",
];

/// Disk and inode use of each mount, then memory, swap and load.
pub fn observe(settings: &MonitorSettings) -> Vec<Observation> {
    let mut observations = Vec::new();
    let mounts = if settings.disk_mounts.is_empty() {
        let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        disk_mounts(&text)
    } else {
        settings.disk_mounts.clone()
    };
    for mount in mounts {
        observations.extend(mount_use(&mount, settings));
    }

    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let memory = MemInfo::parse(&meminfo);
    if settings.is_enabled("memory") {
        observations.push(memory.memory(settings));
    }
    if settings.is_enabled("swap") {
        observations.push(memory.swap(settings));
    }
    if settings.is_enabled("load") {
        let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let cpus = std::thread::available_parallelism().map_or(1, |count| count.get());
        observations.push(load(&loadavg, cpus, settings));
    }
    observations
}

fn mount_use(mount: &Path, settings: &MonitorSettings) -> Vec<Observation> {
    let name = mount.display();
    let stats = match statvfs(mount) {
        Ok(stats) => stats,
        Err(error) => {
            return vec![Observation::new(
                format!("disk:{name}"),
                Level::Critical,
                format!("cannot read: {error}"),
            )];
        }
    };

    let mut observations = Vec::new();
    let used = stats.blocks() - stats.blocks_free();
    let usable = used + stats.blocks_available();
    if settings.is_enabled("disk") && usable > 0 {
        let used_percent = used as f64 * 100.0 / usable as f64;
        observations.push(percent(format!("disk:{name}"), used_percent, settings.disk));
    }
    let files = stats.files();
    if settings.is_enabled("inodes") && files > 0 {
        let used_percent = (files - stats.files_free()) as f64 * 100.0 / files as f64;
        observations.push(percent(
            format!("inodes:{name}"),
            used_percent,
            settings.inodes,
        ));
    }
    observations
}

/// Mount points of disk file systems in `/proc/self/mounts`, one per device.
pub fn disk_mounts(mounts: &str) -> Vec<PathBuf> {
    let mut devices = BTreeSet::new();
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?, fields.next()?, fields.next()?))
        })
        .filter(|(_, _, kind)| DISK_TYPES.contains(kind))
        .filter(|(device, _, _)| devices.insert(device.to_owned()))
        .map(|(_, mount, _)| PathBuf::from(mount.replace("\\040", " ")))
        .collect()
}

/// The `/proc/meminfo` values the checks use, in KiB.
#[derive(Debug, Default, PartialEq)]
pub struct MemInfo {
    total: u64,
    available: u64,
    swap_total: u64,
    swap_free: u64,
}

impl MemInfo {
    pub fn parse(text: &str) -> Self {
        let mut info = Self::default();
        for line in text.lines() {
            let Some((name, rest)) = line.split_once(':') else {
                continue;
            };
            let value = rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            match name {
                "MemTotal" => info.total = value,
                "MemAvailable" => info.available = value,
                "SwapTotal" => info.swap_total = value,
                "SwapFree" => info.swap_free = value,
                _ => {}
            }
        }
        info
    }

    pub fn memory(&self, settings: &MonitorSettings) -> Observation {
        let used = self.total.saturating_sub(self.available) as f64 * 100.0;
        percent(
            "memory".into(),
            used / self.total.max(1) as f64,
            settings.memory,
        )
    }

    pub fn swap(&self, settings: &MonitorSettings) -> Observation {
        if self.swap_total == 0 {
            return Observation::new("swap", Level::Ok, "no swap");
        }
        let used = self.swap_total.saturating_sub(self.swap_free) as f64 * 100.0;
        percent("swap".into(), used / self.swap_total as f64, settings.swap)
    }
}

/// The 5-minute load average per CPU.
pub fn load(loadavg: &str, cpus: usize, settings: &MonitorSettings) -> Observation {
    let five_minutes: f64 = loadavg
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.0);
    let per_cpu = five_minutes / cpus.max(1) as f64;
    Observation::new(
        "load",
        evaluate(per_cpu, settings.load),
        format!("{five_minutes:.2} on {cpus} CPUs ({per_cpu:.2} per CPU)"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "\
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
proc /proc proc rw 0 0
tmpfs /run tmpfs rw 0 0
/dev/nvme0n1p1 /boot/efi vfat rw 0 0
/dev/loop3 /snap/core/1 squashfs ro 0 0
/dev/nvme0n1p2 /var/lib/docker ext4 rw 0 0
/dev/sdb1 /mnt/backup\\040disk xfs rw 0 0
";

    #[test]
    fn lists_disk_mounts_once_per_device() {
        let mounts: Vec<String> = disk_mounts(MOUNTS)
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        assert_eq!(mounts, ["/", "/boot/efi", "/mnt/backup disk"]);
    }

    #[test]
    fn memory_uses_available_and_swap_can_be_absent() {
        let settings = MonitorSettings::default();
        let info = MemInfo::parse(
            "MemTotal: 1000 kB\nMemFree: 10 kB\nMemAvailable: 80 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        );
        assert_eq!(
            info.memory(&settings),
            Observation::new("memory", Level::Warn, "92% used")
        );
        assert_eq!(info.swap(&settings).level, Level::Ok);

        let info = MemInfo::parse(
            "MemTotal: 1000 kB\nMemAvailable: 500 kB\nSwapTotal: 100 kB\nSwapFree: 40 kB\n",
        );
        assert_eq!(info.memory(&settings).level, Level::Ok);
        assert_eq!(info.swap(&settings).level, Level::Warn);
    }

    #[test]
    fn load_is_per_cpu() {
        let settings = MonitorSettings::default();
        let busy = load("9.00 8.40 7.00 3/900 1234", 2, &settings);
        assert_eq!(busy.level, Level::Critical);
        assert_eq!(busy.value, "8.40 on 2 CPUs (4.20 per CPU)");
        assert_eq!(
            load("9.00 8.40 7.00 3/900 1234", 8, &settings).level,
            Level::Ok
        );
    }
}
