//! Storage-class detection (rotational HDD vs SSD/NVMe) for adaptive worker
//! tuning.
//!
//! The sync engine adapts its concurrency to the storage it writes (or
//! hashes): a single spinning disk is a serial device — parallel writers
//! cause seek thrash — while SSD/NVMe scale with concurrency. Detection is
//! best-effort: any failure or unknown filesystem (network mounts, virtual
//! filesystems, unsupported platforms) yields [`StorageClass::Unknown`], and
//! callers fall back to their SSD defaults. An explicit user override
//! (`--storage hdd|ssd`) always wins over detection.
//!
//! Detection per platform: Linux reads `/sys/dev/block/<maj>:<min>/queue/
//! rotational` (device-mapper/md/loop aggregates via `slaves/`); Windows
//! queries `IOCTL_STORAGE_QUERY_PROPERTY` (seek-penalty) on the path's
//! volume handle — no admin rights or PowerShell needed; macOS shells out to
//! Apple's `diskutil info` (`Solid State` field). Other platforms report
//! `Unknown`.

use std::path::Path;
use std::str::FromStr;

/// Detected storage class of a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageClass {
    /// Rotational media: seeks are expensive, keep concurrency low.
    Hdd,
    /// Non-rotational media (SSD/NVMe): parallel I/O helps.
    Ssd,
    /// Not determinable (non-Linux, network/virtual fs, missing path, ...).
    Unknown,
}

/// User preference for storage class (`--storage`).
///
/// `Auto` (the default) enables detection; `Hdd`/`Ssd` force the class,
/// skipping detection entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StoragePreference {
    #[default]
    Auto,
    Hdd,
    Ssd,
}

impl FromStr for StoragePreference {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "hdd" => Ok(Self::Hdd),
            "ssd" => Ok(Self::Ssd),
            other => Err(format!(
                "invalid storage '{other}' (expected auto, hdd, or ssd)"
            )),
        }
    }
}

impl std::fmt::Display for StoragePreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Lowercase so the output round-trips through `FromStr` (which is how
        // the forwarded `--storage` flag is parsed on the remote side).
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Hdd => f.write_str("hdd"),
            Self::Ssd => f.write_str("ssd"),
        }
    }
}

impl From<StorageClass> for StoragePreference {
    fn from(class: StorageClass) -> Self {
        match class {
            StorageClass::Hdd => Self::Hdd,
            StorageClass::Ssd => Self::Ssd,
            StorageClass::Unknown => Self::Auto,
        }
    }
}

/// Detect the storage class backing `path` (the filesystem it lives on).
///
/// Linux: `stat` the path, decode `st_dev` into a major:minor device number,
/// and read `/sys/dev/block/<major>:<minor>/queue/rotational`. Windows: open
/// the path's volume and query the seek-penalty property. macOS: run
/// `diskutil info` and read the `Solid State` field. Other platforms always
/// report [`StorageClass::Unknown`].
#[must_use]
pub fn detect_storage(path: &Path) -> StorageClass {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::metadata(path) else {
            return StorageClass::Unknown;
        };
        let dev = meta.dev();
        let sys_dev =
            Path::new("/sys/dev/block").join(format!("{}:{}", dev_major(dev), dev_minor(dev)));
        classify_sysfs_device(&sys_dev)
    }
    #[cfg(target_os = "windows")]
    {
        detect_storage_windows(path)
    }
    #[cfg(target_os = "macos")]
    {
        detect_storage_macos(path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = path;
        StorageClass::Unknown
    }
}

/// Detect the storage class on macOS via `diskutil info <path>`.
///
/// Apple's own tool reports `Solid State: Yes|No` (plus `Media Type` on
/// newer systems); parsing its fixed English output is far less risky than a
/// raw IOKit/CoreFoundation walk (APFS container chains, whole-disk parent
/// traversal, CF memory management), and the spawn happens once per sync at
/// negligible cost. Any failure → [`StorageClass::Unknown`].
#[cfg(target_os = "macos")]
fn detect_storage_macos(path: &Path) -> StorageClass {
    let output = std::process::Command::new("diskutil")
        .arg("info")
        .arg(path)
        .output();
    match output {
        Ok(out) if out.status.success() => {
            parse_diskutil_class(&String::from_utf8_lossy(&out.stdout))
                .unwrap_or(StorageClass::Unknown)
        }
        _ => StorageClass::Unknown,
    }
}

/// Parse `diskutil info` text output into a storage class.
///
/// Prefers the `Solid State: Yes|No` field (stable across macOS versions),
/// falling back to `Media Type: SSD|HDD`. Pure so it is unit-testable on any
/// platform; compiled only on macOS and in test builds.
#[cfg(any(target_os = "macos", test))]
fn parse_diskutil_class(text: &str) -> Option<StorageClass> {
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("Solid State:") {
            return match value.trim() {
                "Yes" => Some(StorageClass::Ssd),
                "No" => Some(StorageClass::Hdd),
                _ => None,
            };
        }
    }
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("Media Type:") {
            return match value.trim() {
                "SSD" => Some(StorageClass::Ssd),
                "HDD" => Some(StorageClass::Hdd),
                _ => None,
            };
        }
    }
    None
}

#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::HANDLE;

/// Detect the storage class on Windows: resolve `path` to its volume mount
/// point (`GetVolumePathNameW` → `GetVolumeNameForVolumeMountPointW`), open
/// the volume, and query `StorageDeviceSeekPenaltyProperty`. An SSD/NVMe
/// reports no seek penalty; a spinning disk reports one. Any failure (UNC
/// path without a local volume, permissions, exotic filesystems) →
/// [`StorageClass::Unknown`].
#[cfg(target_os = "windows")]
fn detect_storage_windows(path: &Path) -> StorageClass {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeNameForVolumeMountPointW,
        GetVolumePathNameW, OPEN_EXISTING,
    };

    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    // Resolve the path to its volume mount point (handles drive letters and
    // volumes mounted on folders).
    let mut mount_point = vec![0u16; 512];
    // SAFETY: `path_wide` is a NUL-terminated wide string; `mount_point`
    // points to a 512-element buffer whose length is passed as `cchBuffer`.
    let ok = unsafe {
        GetVolumePathNameW(
            path_wide.as_ptr(),
            mount_point.as_mut_ptr(),
            mount_point.len() as u32,
        )
    };
    if ok == 0 {
        return StorageClass::Unknown;
    }

    // Map the mount point to a volume GUID (`\\?\Volume{...}\`) we can open.
    let mut volume_guid = vec![0u16; 512];
    // SAFETY: `mount_point` is a NUL-terminated wide string; `volume_guid`
    // points to a 512-element buffer whose length is passed as `cchBuffer`.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(
            mount_point.as_ptr(),
            volume_guid.as_mut_ptr(),
            volume_guid.len() as u32,
        )
    };
    if ok == 0 {
        return StorageClass::Unknown;
    }

    // SAFETY: `volume_guid` is a NUL-terminated wide string; the remaining
    // arguments are null pointers/zeroes (no security attributes, no template).
    let handle = unsafe {
        CreateFileW(
            volume_guid.as_ptr(),
            0, // no access rights needed for the property query
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return StorageClass::Unknown;
    }
    let class = match query_seek_penalty(handle) {
        Some(true) => StorageClass::Hdd,
        Some(false) => StorageClass::Ssd,
        None => StorageClass::Unknown,
    };
    // SAFETY: `handle` is the valid open handle returned by `CreateFileW`
    // above (checked against `INVALID_HANDLE_VALUE`).
    unsafe { CloseHandle(handle) };
    class
}

/// Query whether the device behind `handle` incurs a seek penalty
/// (`true` = rotating disk, `false` = SSD/NVMe) via
/// `IOCTL_STORAGE_QUERY_PROPERTY` / `StorageDeviceSeekPenaltyProperty`.
#[cfg(target_os = "windows")]
fn query_seek_penalty(handle: HANDLE) -> Option<bool> {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
    };

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut descriptor = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut bytes_returned = 0u32;
    // SAFETY: `query` and `descriptor` are valid objects of the exact types
    // and sizes passed as `nInBufferSize`/`nOutBufferSize`; `bytes_returned`
    // is a valid out-pointer; `handle` is an open handle from `CreateFileW`.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &raw const query as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            &raw mut descriptor as *mut _ as *mut core::ffi::c_void,
            core::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some(descriptor.IncursSeekPenalty)
}

/// Classify a block device from its sysfs directory
/// (`/sys/dev/block/<major>:<minor>`).
///
/// A `queue/rotational` file of `1` marks rotational media (HDD); `0` marks
/// SSD/NVMe. Aggregated devices (device-mapper, md RAID, loop) often lack
/// `queue/rotational` and expose their backing devices under `slaves/` —
/// recurse and aggregate: any rotational slave makes the aggregate an HDD.
#[cfg(target_os = "linux")]
pub(crate) fn classify_sysfs_device(sys_dev: &Path) -> StorageClass {
    if let Ok(rotational) = std::fs::read_to_string(sys_dev.join("queue/rotational")) {
        return match rotational.trim() {
            "1" => StorageClass::Hdd,
            "0" => StorageClass::Ssd,
            // An empty or unparseable value is not evidence of an SSD.
            _ => StorageClass::Unknown,
        };
    }
    let Ok(slaves) = std::fs::read_dir(sys_dev.join("slaves")) else {
        return StorageClass::Unknown;
    };
    let mut seen = false;
    for entry in slaves.flatten() {
        seen = true;
        if classify_sysfs_device(&entry.path()) == StorageClass::Hdd {
            return StorageClass::Hdd;
        }
    }
    if seen {
        StorageClass::Ssd
    } else {
        StorageClass::Unknown
    }
}

/// Decode the major number from a Linux `st_dev` (`new_encode_dev` layout).
///
/// The kernel encodes `mkdev(major, minor)` into `st_dev` with
/// `new_encode_dev`; the inverse (as used by GNU coreutils `stat`) is:
/// major = `((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)`.
// The bit masks bound the result to 20 bits, so the u32 cast cannot truncate.
#[cfg(target_os = "linux")]
#[expect(clippy::cast_possible_truncation)]
fn dev_major(dev: u64) -> u32 {
    (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as u32
}

/// Decode the minor number from a Linux `st_dev` (`new_encode_dev` layout).
// The bit masks bound the result to 20 bits, so the u32 cast cannot truncate.
#[cfg(target_os = "linux")]
#[expect(clippy::cast_possible_truncation)]
fn dev_minor(dev: u64) -> u32 {
    ((dev & 0xff) | ((dev >> 12) & !0xff)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_storage_missing_path_is_unknown() {
        assert_eq!(
            detect_storage(Path::new("/nonexistent/cp2-storage-test")),
            StorageClass::Unknown
        );
    }

    #[test]
    fn storage_preference_parses() {
        assert_eq!(
            "auto".parse::<StoragePreference>().unwrap(),
            StoragePreference::Auto
        );
        assert_eq!(
            "HDD".parse::<StoragePreference>().unwrap(),
            StoragePreference::Hdd
        );
        assert_eq!(
            "ssd".parse::<StoragePreference>().unwrap(),
            StoragePreference::Ssd
        );
        assert!("raid".parse::<StoragePreference>().is_err());
    }

    #[test]
    fn storage_class_maps_to_preference() {
        assert_eq!(
            StoragePreference::from(StorageClass::Hdd),
            StoragePreference::Hdd
        );
        assert_eq!(
            StoragePreference::from(StorageClass::Ssd),
            StoragePreference::Ssd
        );
        assert_eq!(
            StoragePreference::from(StorageClass::Unknown),
            StoragePreference::Auto
        );
    }

    #[test]
    fn diskutil_solid_state_yes_is_ssd() {
        let out = "\
   Device Identifier:         disk3s1s1
   Device Node:               /dev/disk3s1s1
   Solid State:               Yes
   Media Type:                SSD
";
        assert_eq!(parse_diskutil_class(out), Some(StorageClass::Ssd));
    }

    #[test]
    fn diskutil_solid_state_no_is_hdd() {
        let out = "\
   Solid State:               No
";
        assert_eq!(parse_diskutil_class(out), Some(StorageClass::Hdd));
    }

    #[test]
    fn diskutil_media_type_fallback() {
        // Older diskutil variants without the Solid State field.
        assert_eq!(
            parse_diskutil_class("   Media Type:                SSD\n"),
            Some(StorageClass::Ssd)
        );
        assert_eq!(
            parse_diskutil_class("   Media Type:                HDD\n"),
            Some(StorageClass::Hdd)
        );
    }

    #[test]
    fn diskutil_solid_state_wins_over_media_type() {
        // The explicit Solid State field takes precedence.
        let out = "Solid State: No\nMedia Type: SSD\n";
        assert_eq!(parse_diskutil_class(out), Some(StorageClass::Hdd));
    }

    #[test]
    fn diskutil_unrecognized_output_is_none() {
        assert_eq!(parse_diskutil_class("diskutil: No such file"), None);
        assert_eq!(parse_diskutil_class(""), None);
        assert_eq!(parse_diskutil_class("Solid State: Maybe\n"), None);
    }

    /// Sysfs-walk tests: the `/sys/dev/block` probe is Linux-only, so these
    /// exercise `classify_sysfs_device` against tempdir fixtures.
    #[cfg(target_os = "linux")]
    mod sysfs_tests {
        use super::*;

        /// Build a fake sysfs device tree under `dir`.
        fn sysfs(dir: &std::path::Path, dev: &str) -> std::path::PathBuf {
            let root = dir.join("sys").join("dev").join("block").join(dev);
            std::fs::create_dir_all(root.join("queue")).unwrap();
            root
        }

        /// Add a `queue/rotational` file to a (possibly nested) sysfs device dir.
        fn mark_rotational(dev: &std::path::Path, value: &str) {
            std::fs::create_dir_all(dev.join("queue")).unwrap();
            std::fs::write(dev.join("queue/rotational"), value).unwrap();
        }

        /// Add a `slaves/<name>` backing device to `dev`, returning its dir.
        fn add_slave(dev: &std::path::Path, name: &str) -> std::path::PathBuf {
            let slave = dev.join("slaves").join(name);
            std::fs::create_dir_all(&slave).unwrap();
            slave
        }

        #[test]
        fn rotational_one_is_hdd() {
            let dir = tempfile::tempdir().unwrap();
            let dev = sysfs(dir.path(), "8:0");
            std::fs::write(dev.join("queue/rotational"), "1").unwrap();
            assert_eq!(classify_sysfs_device(&dev), StorageClass::Hdd);
        }

        #[test]
        fn rotational_zero_is_ssd() {
            let dir = tempfile::tempdir().unwrap();
            let dev = sysfs(dir.path(), "259:0");
            std::fs::write(dev.join("queue/rotational"), "0").unwrap();
            assert_eq!(classify_sysfs_device(&dev), StorageClass::Ssd);
        }

        #[test]
        fn unrecognized_rotational_is_unknown() {
            // An empty or unparseable value must not be misclassified as SSD.
            let dir = tempfile::tempdir().unwrap();
            let dev = sysfs(dir.path(), "8:0");
            std::fs::write(dev.join("queue/rotational"), "").unwrap();
            assert_eq!(classify_sysfs_device(&dev), StorageClass::Unknown);
        }

        #[test]
        fn rotational_trailing_newline_is_hdd() {
            let dir = tempfile::tempdir().unwrap();
            let dev = sysfs(dir.path(), "8:16");
            std::fs::write(dev.join("queue/rotational"), "1\n").unwrap();
            assert_eq!(classify_sysfs_device(&dev), StorageClass::Hdd);
        }

        #[test]
        fn missing_device_is_unknown() {
            let dir = tempfile::tempdir().unwrap();
            let dev = sysfs(dir.path(), "8:0");
            // No queue/rotational, no slaves.
            assert_eq!(classify_sysfs_device(&dev), StorageClass::Unknown);
        }

        #[test]
        fn empty_slaves_is_unknown() {
            let dir = tempfile::tempdir().unwrap();
            let dev = sysfs(dir.path(), "252:0");
            std::fs::create_dir_all(dev.join("slaves")).unwrap();
            assert_eq!(classify_sysfs_device(&dev), StorageClass::Unknown);
        }

        #[test]
        fn slaves_aggregate_rotational() {
            let dir = tempfile::tempdir().unwrap();
            let dev = sysfs(dir.path(), "252:0");
            mark_rotational(&add_slave(&dev, "sda"), "1");
            assert_eq!(classify_sysfs_device(&dev), StorageClass::Hdd);
        }

        #[test]
        fn slaves_aggregate_ssd() {
            let dir = tempfile::tempdir().unwrap();
            let dev = sysfs(dir.path(), "252:0");
            mark_rotational(&add_slave(&dev, "nvme0n1"), "0");
            assert_eq!(classify_sysfs_device(&dev), StorageClass::Ssd);
        }

        #[test]
        fn nested_slaves_aggregate() {
            // dm → loop → rotational backing: recursion must find the HDD.
            let dir = tempfile::tempdir().unwrap();
            let dev = sysfs(dir.path(), "252:1");
            mark_rotational(&add_slave(&add_slave(&dev, "loop0"), "sdb"), "1");
            assert_eq!(classify_sysfs_device(&dev), StorageClass::Hdd);
        }

        #[test]
        fn mixed_slaves_prefer_hdd() {
            let dir = tempfile::tempdir().unwrap();
            let dev = sysfs(dir.path(), "9:0");
            mark_rotational(&add_slave(&dev, "sdc"), "1");
            mark_rotational(&add_slave(&dev, "nvme1n1"), "0");
            assert_eq!(classify_sysfs_device(&dev), StorageClass::Hdd);
        }

        #[test]
        fn dev_number_decode_matches_stat() {
            use std::os::unix::fs::MetadataExt;
            // Spot-check the new_encode_dev decode against the sysfs entry the
            // kernel's own st_dev maps to. Guarded: /tmp may be on a virtual
            // filesystem (no sysfs entry), which is the Unknown case.
            let meta = std::fs::metadata("/tmp").unwrap();
            let dev = meta.dev();
            let major = dev_major(dev);
            let minor = dev_minor(dev);
            let sys_dev = Path::new("/sys/dev/block").join(format!("{major}:{minor}"));
            if sys_dev.exists() {
                let class = classify_sysfs_device(&sys_dev);
                // Whatever it is, it must be a real classification.
                assert!(class == StorageClass::Hdd || class == StorageClass::Ssd);
            }
        }
    }
}
