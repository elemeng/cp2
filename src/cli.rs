use clap::Parser;
use std::path::PathBuf;

use crate::platform::storage::StoragePreference;
use crate::protocol::VERSION_BANNER;

/// cp2 — rsync-style file synchronization over SSH.
///
/// `cp2 SRC DST` mirrors rsync: the direction is inferred from which side
/// is remote (`user@host:path`). If SRC is local → push; if SRC is remote →
/// pull. The transfer rides over an `ssh` connection; the remote side runs
/// `cp2 --server` and sshd handles authentication and permissions.
#[derive(Parser)]
#[command(name = "cp2")]
#[command(
    version = VERSION_BANNER,
    about = "rsync-style file synchronization over SSH for Linux, Windows and Mac ",
    long_about = None,
    help_template = "{about-with-newline}\n{before-help}{usage-heading} {usage}\n\nBare runs preserve recursion, links, perms, and times; -a adds owner/group, specials, literal links\n\n{all-args}{after-help}",
    before_help = "EXAMPLES:\n  \
        cp2 /path/to/dir user@host:backup              push (home-relative)\n  \
        cp2 user@host:backup /path/to/restore          pull\n  \
        cp2 -a /path/to/dir user@host:/home/user/dst   archive push (absolute)\n  \
        cp2 ./src ./dst                                local copy\n  \
        cp2 /path/to/dir user@host --dry-run           preview changes"
)]
// Each boolean is an independent rsync-style CLI flag (clap generates the
// flag parsing); grouping them would obscure the one-to-one flag mapping.
#[expect(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Source path: local path, user@host:path, or a quoted glob (`'./*.rs'`).
    /// A trailing slash means "sync the contents" (`./dir/` → DST/*); without
    /// one the source directory itself is recreated (`./dir` → DST/dir).
    pub source: Option<String>,

    /// Destination path: local path or user@host:path.
    pub destination: Option<String>,

    /// Server mode: invoked by sshd, reads the protocol from stdin and
    /// writes to stdout. Not for direct use.
    #[arg(long, hide = true, help_heading = "Server")]
    pub server: bool,

    /// Sync the absolute paths listed in FILE (one per line) into DST; SRC is unused
    #[arg(long, help_heading = "Selection")]
    pub files_from: Option<PathBuf>,

    /// Archive mode (rsync -a): adds owner/group and specials on Unix; implies --literal-links
    #[arg(short = 'a', long, help_heading = "Transfer")]
    pub archive: bool,

    /// Print the transfer direction and exit (local preview, no connect)
    #[arg(short = 'n', long, help_heading = "Transfer")]
    pub dry_run: bool,

    /// Increase verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, help_heading = "Transfer")]
    pub verbose: u8,

    /// Suppress the per-file listing, summary, and deploy/watch lines
    #[arg(short = 'q', long, help_heading = "Transfer")]
    pub quiet: bool,

    /// List the source files without transferring (rsync --list-only).
    /// Works for local and remote sources; SRC is listed, DST is unused.
    #[arg(long, help_heading = "Transfer")]
    pub list_only: bool,

    /// Print a detailed transfer statistics block after the sync (rsync --stats)
    #[arg(long, help_heading = "Transfer")]
    pub stats: bool,

    /// Print a per-file change line (rsync -i): `>f` new, `*deleting`,
    /// `.f` in-sync
    #[arg(short = 'i', long, help_heading = "Transfer")]
    pub itemize_changes: bool,

    /// Skip the per-file listing/progress, keep the summary
    #[arg(long, help_heading = "Transfer")]
    pub no_progress: bool,

    /// Remove destination files not present in the source
    #[arg(long, help_heading = "Selection")]
    pub delete: bool,

    /// Refuse to delete more than N files when --delete is set
    #[arg(long, help_heading = "Selection")]
    pub max_delete: Option<u64>,

    /// Exclude paths matching GLOB (repeatable)
    #[arg(long, help_heading = "Selection")]
    pub exclude: Vec<String>,

    /// Include paths matching GLOB, overriding excludes (repeatable)
    #[arg(long, help_heading = "Selection")]
    pub include: Vec<String>,

    /// Read additional exclude patterns from FILE (one per line)
    #[arg(long, value_name = "FILE", help_heading = "Selection")]
    pub exclude_from: Option<PathBuf>,

    /// Read additional include patterns from FILE (one per line)
    #[arg(long, value_name = "FILE", help_heading = "Selection")]
    pub include_from: Option<PathBuf>,

    /// Skip files where the destination is newer (rsync -u)
    #[arg(short = 'u', long, help_heading = "Selection")]
    pub update: bool,

    /// Compare BLAKE3 hashes instead of size+mtime
    #[arg(short = 'c', long, help_heading = "Selection")]
    pub checksum: bool,

    /// Skip files that already exist in the destination
    #[arg(long, help_heading = "Selection")]
    pub ignore_existing: bool,

    /// Only update files already present; create no new files
    #[arg(long, help_heading = "Selection")]
    pub existing: bool,

    /// Transfer all files, ignoring the size+mtime quick check
    #[arg(long, help_heading = "Selection")]
    pub ignore_times: bool,

    /// Delete source files only after the destination is verified & fsynced
    #[arg(long, help_heading = "Transfer")]
    pub remove_source_files: bool,

    /// Verify destination bytes match the source after transfer
    #[arg(long, help_heading = "Transfer")]
    pub verify: bool,

    /// Skip every symlink/shortcut entirely (highest priority)
    #[arg(long, help_heading = "Links")]
    pub skip_links: bool,

    /// Dereference every symlink (rsync -L)
    #[arg(long, help_heading = "Links")]
    pub follow_links: bool,

    /// Recreate every link with its literal target (implied by -a)
    #[arg(long, help_heading = "Links")]
    pub literal_links: bool,

    /// Keep internal symlinks with literal targets
    #[arg(long, help_heading = "Links")]
    pub literal_internal_links: bool,

    /// Keep external-file symlinks as links (high risk)
    #[arg(long, help_heading = "Links")]
    pub literal_external_file_links: bool,

    /// Keep external-dir symlinks as links (high risk)
    #[arg(long, help_heading = "Links")]
    pub literal_external_dir_links: bool,

    /// Don't preserve permission bits (0644/0755 defaults)
    #[arg(long, help_heading = "Metadata")]
    pub no_perms: bool,

    /// Write files sparsely (0-runs become holes)
    #[arg(short = 'S', long, help_heading = "Metadata")]
    pub sparse: bool,

    /// Copy extended attributes (rsync -X)
    #[arg(short = 'X', long, help_heading = "Metadata")]
    pub xattrs: bool,

    /// Restore the source's last-access time (rsync -U)
    #[arg(short = 'U', long, help_heading = "Metadata")]
    pub atimes: bool,

    /// Don't preserve mtimes; size-only quick check
    #[arg(long, help_heading = "Metadata")]
    pub no_times: bool,

    /// Don't recurse into subdirectories
    #[arg(long, help_heading = "Metadata")]
    pub no_recursive: bool,

    /// Use the rsync-style rollsum delta engine (experimental)
    #[arg(long, help_heading = "Engine")]
    pub rollsum: bool,

    /// Keep the replaced destination file as <name>~
    #[arg(long, help_heading = "Transfer")]
    pub backup: bool,

    /// fsync every received file before rename (slower)
    #[arg(long, help_heading = "Transfer")]
    pub fsync: bool,

    /// Compress the data stream (lz4)
    #[arg(short = 'z', long, help_heading = "Engine")]
    pub compress: bool,

    /// Limit bandwidth (K/M/G suffixes; 0 = unlimited)
    #[arg(long, value_parser = parse_bwlimit, help_heading = "Engine")]
    pub bwlimit: Option<Option<u64>>,

    /// Parallel workers (default: tuned from the storage class)
    #[arg(short = 'j', long, help_heading = "Engine")]
    pub jobs: Option<usize>,

    /// Target storage class: auto (default), hdd, ssd
    #[arg(long, default_value = "auto", help_heading = "Engine")]
    pub storage: StoragePreference,

    /// Watch SRC and sync continuously (optional -W=1h30m; default 24h)
    #[arg(short = 'W', long, num_args = 0..=1, require_equals = true, default_missing_value = "24h", value_parser = parse_duration, help_heading = "Watch")]
    pub watch: Option<std::time::Duration>,

    /// Debounce quiet window for --watch (ms)
    #[arg(long, default_value_t = 1000, help_heading = "Watch")]
    pub watch_delay: u64,

    /// SSH port
    #[arg(short = 'p', long, default_value = "22", help_heading = "SSH & remote")]
    pub port: Option<u16>,

    /// Connect via jump host user@host[:port] (`ProxyJump`)
    #[arg(long, value_name = "JUMP_HOST", help_heading = "SSH & remote")]
    pub jump_host: Option<String>,

    /// SSH password (visible in the process list; prefer keys/prompt)
    #[arg(long, value_name = "PASSWORD", help_heading = "SSH & remote")]
    pub password: Option<String>,

    /// Password for the jump host (default: reuse --password)
    #[arg(long, value_name = "PASSWORD", help_heading = "SSH & remote")]
    pub jump_password: Option<String>,

    /// Run the remote server under sudo (needed for root -a)
    #[arg(long, help_heading = "SSH & remote")]
    pub remote_sudo: bool,

    /// Password for --remote-sudo (default: --password)
    #[arg(long, value_name = "PASSWORD", help_heading = "SSH & remote")]
    pub sudo_password: Option<String>,

    /// Read the password from FILE (first line)
    #[arg(long, value_name = "FILE", help_heading = "SSH & remote")]
    pub password_file: Option<PathBuf>,

    /// Path of cp2 on the remote (rsync --rsync-path)
    #[arg(long, help_heading = "SSH & remote")]
    pub remote_path: Option<String>,

    /// Don't auto-deploy the remote binary
    #[arg(long, help_heading = "SSH & remote")]
    pub no_auto_install: bool,

    /// Directory holding prebuilt cp2-<triple> sidecars
    #[arg(long, help_heading = "SSH & remote")]
    pub binaries_dir: Option<PathBuf>,
}

/// Parse a `--watch` duration argument into a [`std::time::Duration`].
///
/// Accepts a combination of `d`/`h`/`m`/`s` units (`1h30m`, `2d12h`, `90s`)
/// and a plain number, which means seconds (`300`). The value is normalized
/// to whole seconds. Zero and malformed values are rejected.
///
/// # Errors
///
/// Returns an error if the value is empty, malformed, zero, or overflows.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err("empty value".to_string());
    }
    let mut total: u64 = 0;
    let mut num: Option<u64> = None;
    for ch in s.chars() {
        match ch {
            '0'..='9' => {
                // `ch` is a digit by the match arm, so the subtraction is total.
                let digit = u64::from(ch as u32 - '0' as u32);
                num = Some(num.unwrap_or(0) * 10 + digit);
            }
            'd' | 'h' | 'm' | 's' => {
                let n = num.ok_or_else(|| format!("invalid duration '{s}'"))?;
                let multiplier = match ch {
                    'd' => 86_400,
                    'h' => 3_600,
                    'm' => 60,
                    _ => 1,
                };
                total = total
                    .checked_add(
                        n.checked_mul(multiplier)
                            .ok_or_else(|| format!("duration '{s}' overflows"))?,
                    )
                    .ok_or_else(|| format!("duration '{s}' overflows"))?;
                num = None;
            }
            ' ' => {} // separators allowed: "1h 30m"
            _ => return Err(format!("invalid duration '{s}'")),
        }
    }
    // A trailing plain number is seconds.
    if let Some(n) = num {
        total = total
            .checked_add(n)
            .ok_or_else(|| format!("duration '{s}' overflows"))?;
    }
    if total == 0 {
        return Err(format!("duration '{s}' is zero"));
    }
    Ok(std::time::Duration::from_secs(total))
}

/// Parse a `--bwlimit` value into bytes/second.
///
/// Accepts a plain number (bytes/s) or a number with a `K`/`M`/`G` suffix
/// (1024-based), e.g. `10M` → 10 MiB/s. A value of `0` means unlimited
/// (`None`).
///
/// # Errors
///
/// Returns an error if the value is empty, has an unknown suffix, does not
/// parse as a number, or overflows.
pub fn parse_bwlimit(s: &str) -> Result<Option<u64>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty value".to_string());
    }
    let (digits, multiplier) = match s.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            let (d, _) = s.split_at(s.len() - 1);
            let m = match c.to_ascii_lowercase() {
                'k' => 1024u64,
                'm' => 1024 * 1024,
                'g' => 1024 * 1024 * 1024,
                _ => return Err(format!("invalid suffix '{c}' (expected K, M, or G)")),
            };
            (d, m)
        }
        _ => (s, 1),
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid bandwidth value '{s}'"))?;
    let bytes = value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("bandwidth value '{s}' overflows"))?;
    Ok(if bytes == 0 { None } else { Some(bytes) })
}

#[cfg(test)]
mod tests {
    use super::parse_bwlimit;
    use super::parse_duration;
    use std::time::Duration;

    #[test]
    fn bwlimit_parses() {
        assert_eq!(parse_bwlimit("1024").unwrap(), Some(1024));
        assert_eq!(parse_bwlimit("10K").unwrap(), Some(10 * 1024));
        assert_eq!(parse_bwlimit("10M").unwrap(), Some(10 * 1024 * 1024));
        assert_eq!(parse_bwlimit("1g").unwrap(), Some(1024 * 1024 * 1024));
        assert_eq!(parse_bwlimit("0").unwrap(), None);
        assert!(parse_bwlimit("10X").is_err());
        assert!(parse_bwlimit("abc").is_err());
    }

    #[test]
    fn duration_parses() {
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_hours(1));
        assert_eq!(parse_duration("90m").unwrap(), Duration::from_mins(90));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_mins(90));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_hours(48));
        assert_eq!(parse_duration("2d12h").unwrap(), Duration::from_hours(60));
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("300").unwrap(), Duration::from_mins(5));
        assert_eq!(parse_duration("1h 30m").unwrap(), Duration::from_mins(90));
        assert_eq!(parse_duration("1H30M").unwrap(), Duration::from_mins(90));
    }

    #[test]
    fn duration_rejects_bad_input() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("1x").is_err());
        assert!(parse_duration("h30m").is_err());
        // A trailing plain number is seconds: "1h30" = 1h + 30s.
        assert_eq!(parse_duration("1h30").unwrap(), Duration::from_secs(3630));
    }
}
