use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

pub(super) const FILE_LIMIT: usize = 50;
pub(super) const FILE_CUTOFFS: [u64; 3] = [100 << 20, 500 << 20, 1 << 30];

pub(super) struct Volume {
    pub source: String,
    pub path: Option<PathBuf>,
    pub filesystem: String,
    pub status: String,
    pub total: Option<u64>,
    pub used: Option<u64>,
    pub available: Option<u64>,
    pub system: bool,
    pub scannable: bool,
}

impl Volume {
    pub fn used_fraction(&self) -> Option<f32> {
        let total = self.total?;
        let used = self.used?;
        (total != 0).then(|| used as f32 / total as f32)
    }
}

pub(super) struct Inventory {
    pub volumes: Vec<Volume>,
    pub device_error: Option<String>,
}

struct Mount {
    id: Vec<u8>,
    parent: Vec<u8>,
    device: String,
    source: String,
    path: PathBuf,
    filesystem: String,
    readonly: bool,
}

fn mount_field(field: &[u8]) -> OsString {
    let mut decoded = Vec::with_capacity(field.len());
    let mut remaining = field;
    while let Some((&byte, rest)) = remaining.split_first() {
        if byte == b'\\'
            && let [a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7', tail @ ..] = rest
        {
            decoded.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
            remaining = tail;
        } else {
            decoded.push(byte);
            remaining = rest;
        }
    }
    OsString::from_vec(decoded)
}

fn parse_mounts(contents: &[u8]) -> Vec<Mount> {
    contents
        .split(|&byte| byte == b'\n')
        .filter_map(|line| {
            let mut fields = line.split(|&byte| byte == b' ');
            let [id, parent, device, _root, path, options] =
                std::array::from_fn(|_| fields.next().unwrap_or_default());
            fields.find(|field| *field == b"-")?;
            let filesystem = String::from_utf8_lossy(fields.next()?).into_owned();
            Some(Mount {
                id: id.to_vec(),
                parent: parent.to_vec(),
                device: String::from_utf8_lossy(device).into_owned(),
                source: mount_field(fields.next()?).to_string_lossy().into_owned(),
                path: PathBuf::from(mount_field(path)),
                filesystem,
                readonly: options
                    .split(|&byte| byte == b',')
                    .any(|part| part == b"ro"),
            })
        })
        .collect()
}

fn mounts() -> io::Result<Vec<Mount>> {
    Ok(parse_mounts(&fs::read("/proc/self/mountinfo")?))
}

#[derive(Deserialize)]
struct BlockDevices {
    blockdevices: Vec<BlockDevice>,
}

#[derive(Deserialize)]
struct BlockDevice {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    size: Option<u64>,
    fstype: Option<String>,
    mountpoint: Option<String>,
    pkname: Option<String>,
    #[serde(rename = "maj:min")]
    device: String,
}

fn block_devices() -> io::Result<Vec<BlockDevice>> {
    let output = Command::new("lsblk")
        .args([
            "--json",
            "--bytes",
            "--list",
            "--paths",
            "--output",
            "NAME,TYPE,SIZE,FSTYPE,MOUNTPOINT,PKNAME,MAJ:MIN",
        ])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "lsblk exited with {}",
            output.status
        )));
    }
    let parsed: BlockDevices = serde_json::from_slice(&output.stdout)?;
    Ok(parsed
        .blockdevices
        .into_iter()
        .filter(|device| {
            fs::metadata(&device.name).is_ok_and(|metadata| metadata.file_type().is_block_device())
        })
        .collect())
}

fn system_mount(mount: &Mount, total: Option<u64>) -> bool {
    mount.path != Path::new("/")
        && (total == Some(0)
            || matches!(
                mount.filesystem.as_str(),
                "tmpfs"
                    | "devtmpfs"
                    | "proc"
                    | "sysfs"
                    | "devpts"
                    | "cgroup"
                    | "cgroup2"
                    | "autofs"
                    | "mqueue"
                    | "securityfs"
                    | "debugfs"
                    | "tracefs"
                    | "bpf"
                    | "nsfs"
            ))
}

pub(super) fn inventory() -> io::Result<Inventory> {
    let mounts = mounts()?;
    let hidden: HashSet<_> = mounts
        .iter()
        .filter_map(|mount| {
            mounts
                .iter()
                .find(|parent| parent.id == mount.parent && parent.path == mount.path)
                .filter(|parent| parent.id != mount.id)
                .map(|parent| &parent.id)
        })
        .collect();

    let mut volumes = Vec::new();
    for mount in mounts.iter().filter(|mount| !hidden.contains(&mount.id)) {
        // Querying an autofs trigger can mount a remote volume as a side effect.
        let stats = if mount.filesystem == "autofs" {
            None
        } else {
            rustix::fs::statvfs(&mount.path).ok()
        };
        let total = stats
            .as_ref()
            .map(|stats| stats.f_blocks.saturating_mul(stats.f_frsize));
        let used = stats.as_ref().map(|stats| {
            stats
                .f_blocks
                .saturating_sub(stats.f_bfree)
                .saturating_mul(stats.f_frsize)
        });
        let available = stats
            .as_ref()
            .map(|stats| stats.f_bavail.saturating_mul(stats.f_frsize));
        volumes.push(Volume {
            system: system_mount(mount, total),
            scannable: total.is_some_and(|size| size > 0) && mount.path.is_dir(),
            source: mount.source.clone(),
            filesystem: mount.filesystem.clone(),
            path: Some(mount.path.clone()),
            status: if mount.readonly {
                "Read-only"
            } else {
                "Mounted"
            }
            .into(),
            total,
            used,
            available,
        });
    }
    let device_error = match block_devices() {
        Ok(devices) => {
            let parents: HashSet<_> = devices
                .iter()
                .filter_map(|device| device.pkname.as_deref())
                .collect();
            for device in &devices {
                if mounts
                    .iter()
                    .any(|mount| mount.device == device.device || mount.source == device.name)
                    || device
                        .mountpoint
                        .as_deref()
                        .is_some_and(|path| path.starts_with('/'))
                {
                    continue;
                }
                let status = if device.mountpoint.as_deref() == Some("[SWAP]") {
                    "Active swap"
                } else if parents.contains(device.name.as_str()) {
                    "Parent device"
                } else {
                    "Unmounted"
                };
                volumes.push(Volume {
                    source: device.name.clone(),
                    path: None,
                    status: status.into(),
                    filesystem: device.fstype.clone().unwrap_or_else(|| device.kind.clone()),
                    total: device.size,
                    used: None,
                    available: None,
                    system: false,
                    scannable: false,
                });
            }
            None
        }
        Err(error) => Some(error.to_string()),
    };
    Ok(Inventory {
        volumes,
        device_error,
    })
}

#[derive(Default, Deserialize, Serialize)]
pub(super) struct Scan {
    #[serde(with = "super::cache::entries")]
    pub folders: Vec<(u64, PathBuf)>,
    #[serde(with = "super::cache::entries")]
    pub files: Vec<(u64, PathBuf)>,
    pub total: u64,
    pub skipped: usize,
    pub file_count: usize,
    pub large_file_counts: [usize; FILE_CUTOFFS.len()],
}

impl Scan {
    fn count_file(&mut self, size: u64) {
        self.file_count += 1;
        for (cutoff, count) in FILE_CUTOFFS.into_iter().zip(&mut self.large_file_counts) {
            *count += usize::from(size >= cutoff);
        }
    }
}

pub(super) fn scan_path(path: &Path, cancel: &AtomicBool) -> io::Result<Scan> {
    let nested = mounts()?
        .into_iter()
        .filter(|mount| mount.path != path && mount.path.starts_with(path))
        .map(|mount| mount.path)
        .collect();
    scan(path, &nested, cancel)
}

fn scan(path: &Path, nested: &HashSet<PathBuf>, cancel: &AtomicBool) -> io::Result<Scan> {
    let root = fs::symlink_metadata(path)?;
    if !root.is_dir() {
        return Err(io::Error::other(
            "The selected directory is no longer available.",
        ));
    }
    let mut result = Scan::default();
    let mut pending = Vec::new();
    let mut seen_links = HashSet::new();
    let mut largest = BinaryHeap::with_capacity(FILE_LIMIT + 1);
    let mut directory = path.to_path_buf();
    let mut folder = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Scan cancelled.",
            ));
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => Some(entries),
            Err(error) if folder.is_none() => return Err(error),
            Err(_) => {
                result.skipped += 1;
                None
            }
        };
        for entry in entries.into_iter().flatten() {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Scan cancelled.",
                ));
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    result.skipped += 1;
                    continue;
                }
            };
            let child = entry.path();
            if nested.contains(&child) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                result.skipped += 1;
                continue;
            };
            if metadata.dev() != root.dev() {
                continue;
            }
            let size = metadata.blocks().saturating_mul(512);
            if metadata.is_dir() {
                let index = folder.unwrap_or_else(|| {
                    result.folders.push((0, child.clone()));
                    result.folders.len() - 1
                });
                result.folders[index].0 += size;
                result.total += size;
                pending.push((child, index));
            } else if metadata.is_file()
                && (metadata.nlink() <= 1 || seen_links.insert(metadata.ino()))
            {
                if let Some(index) = folder {
                    result.folders[index].0 += size;
                }
                result.total += size;
                result.count_file(size);
                largest.push(Reverse((size, child)));
                if largest.len() > FILE_LIMIT {
                    largest.pop();
                }
            }
        }
        let Some((next, index)) = pending.pop() else {
            break;
        };
        directory = next;
        folder = Some(index);
    }
    result
        .folders
        .sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    result.files = largest
        .into_sorted_vec()
        .into_iter()
        .map(|Reverse(file)| file)
        .collect();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn large_file_counts_cover_the_entire_scan_and_include_the_cutoff() {
        let mut result = Scan::default();
        for _ in 0..FILE_LIMIT + 1 {
            result.count_file(FILE_CUTOFFS[0]);
        }
        result.count_file(FILE_CUTOFFS[2]);
        result.count_file(FILE_CUTOFFS[0] - 1);
        assert_eq!(result.file_count, FILE_LIMIT + 3);
        assert_eq!(result.large_file_counts, [FILE_LIMIT + 2, 1, 1]);
    }

    #[test]
    fn mount_paths_preserve_escaped_spaces_and_backslashes() {
        let mounts =
            parse_mounts(br"36 35 8:1 / /media/My\040Disk\1342 rw shared:1 - exfat /dev/sda1 rw");
        assert_eq!(mounts[0].path, Path::new("/media/My Disk\\2"));
        assert_eq!(mounts[0].source, "/dev/sda1");
    }

    #[test]
    fn scan_skips_nested_mounts_symlinks_and_duplicate_hard_links() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        let nested = path.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("excluded"), vec![1; 8192]).unwrap();
        fs::write(path.join("file"), vec![1; 4096]).unwrap();
        fs::hard_link(path.join("file"), path.join("alias")).unwrap();
        symlink(&nested, path.join("symlink")).unwrap();
        let result = scan(path, &HashSet::from([nested]), &AtomicBool::new(false)).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.file_count, 1);
        let allocated = fs::metadata(path.join("file")).unwrap().blocks() * 512;
        assert_eq!(result.files[0].0, allocated);
        assert!(result.folders.is_empty());
    }
}
