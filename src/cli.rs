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
    about = "rsync-style file synchronization over SSH",
    long_about = None,
    after_help = "EXAMPLES:\n  \
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
    pub source: Option<String>,

    /// Destination path: local path or user@host:path.
    pub destination: Option<String>,

    /// Server mode: invoked by sshd, reads the protocol from stdin and
    /// writes to stdout. Not for direct use.
    #[arg(long, hide = true)]
    pub server: bool,

    /// Read the list of absolute paths to sync from FILE (rsync
    /// `--files-from`): one absolute path per line (Unix or Windows line
    /// endings); relative entries are rejected. Each entry syncs to the
    /// destination mirroring its root-relative structure (`/data/a.txt` →
    /// `DST/data/a.txt`). Blank lines are skipped. Entries may be files or
    /// directories. SRC is not used — pass only the destination:
    /// `cp2 --files-from FILE DST`.
    #[arg(long)]
    pub files_from: Option<PathBuf>,

    /// Archive mode (full rsync `-a`): recursive with mode/mtime
    /// preservation (always on) plus, on Unix-like systems only, owner,
    /// group, and special files (fifos, sockets, devices). Also implies
    /// `--literal-links`: every symlink is recreated with its literal target
    /// (no DEST-relative rewriting) and Windows `.lnk` shortcuts are copied
    /// as opaque files — links stay byte-identical. On Windows the Unix
    /// ownership/device model does not exist, so those parts are silently
    /// skipped.
    #[arg(short = 'a', long)]
    pub archive: bool,

    /// Print the transfer direction and exit without connecting (a local
    /// preview — rsync's -n runs a full remote dry scan with a per-file
    /// change list; cp2's -n is intentionally lighter).
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Verbosity level (repeat for more: -v, -vv, -vvv).
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress the non-error output (rsync -q): no per-file listing, no
    /// transfer summary, no deploy or watch lines. Errors and the skipped
    /// file report still print; the exit code is unchanged.
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Remove destination files not present in the source.
    #[arg(long)]
    pub delete: bool,

    /// Refuse to delete more than N files when --delete is set
    /// (rsync `--max-delete`).
    #[arg(long)]
    pub max_delete: Option<u64>,

    /// Exclude paths matching GLOB (repeatable; applies to the source tree).
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Include paths matching GLOB, overriding excludes (repeatable).
    #[arg(long)]
    pub include: Vec<String>,

    /// Skip files where the destination is newer (rsync --update).
    #[arg(short = 'u', long)]
    pub update: bool,

    /// Compare BLAKE3 hashes instead of size+mtime.
    #[arg(short = 'c', long)]
    pub checksum: bool,

    /// Skip files that already exist in the destination.
    #[arg(long)]
    pub ignore_existing: bool,

    /// Only update files that already exist in the destination; do not create
    /// new ones (rsync `--existing`; directories are still created).
    #[arg(long)]
    pub existing: bool,

    /// Transfer all files, ignoring the size+mtime quick check (rsync -I).
    #[arg(long)]
    pub ignore_times: bool,

    /// Remove source files only after the destination is hash-verified and
    /// fsynced (move-off workflows; directories and symlinks are kept).
    #[arg(long)]
    pub remove_source_files: bool,

    /// Verify destination bytes match the source after transfer (BLAKE3,
    /// computed on the fly; deletes nothing).
    #[arg(long)]
    pub verify: bool,

    /// Skip every symlink and shortcut entirely: not synced, not followed
    /// (rsync `--no-l`). The link's target content is never transferred.
    /// Highest priority: overrides every other link flag.
    #[arg(long)]
    pub skip_links: bool,

    /// Dereference every symlink (rsync `-L`): the link's target content is
    /// copied in its place — file targets as regular files, directory
    /// targets recursed (with loop detection), Windows-source `.lnk`
    /// shortcuts followed to their target. Overrides the `--literal-*`
    /// family; overridden by `--skip-links`.
    #[arg(long)]
    pub follow_links: bool,

    /// Keep links and shortcuts exactly as they are (rsync `-l`): every
    /// symlink is recreated with its literal target string — no DEST-relative
    /// rewriting, no dereferencing, no skipping — and Windows-source `.lnk`
    /// shortcuts are copied as opaque files (their original bytes). On a
    /// Windows target a POSIX symlink still materializes as a `.lnk`
    /// (Windows cannot represent it), but the target string stays literal.
    /// Implied by `-a`. The fine-grained `--literal-internal-links` /
    /// `--literal-external-file-links` / `--literal-external-dir-links`
    /// switches turn on the same preservation per link class.
    #[arg(long)]
    pub literal_links: bool,

    /// Keep *internal* symlinks (targets resolving inside the source root)
    /// with their literal target string instead of rewriting it to a
    /// DEST-relative path (self-contained mirrors). Only affects internal
    /// links; the external-link policy is unchanged.
    #[arg(long)]
    pub literal_internal_links: bool,

    /// Keep *external file-target* symlinks as links with their literal
    /// target instead of dereferencing and copying the content. High risk:
    /// the destination machine must have the same absolute path. Ignored
    /// when the destination is Windows (which cannot represent a POSIX
    /// absolute link).
    #[arg(long)]
    pub literal_external_file_links: bool,

    /// Keep *external directory-target* symlinks as links with their
    /// literal target instead of skipping them (the default). High risk:
    /// the destination machine must have the same absolute path. Dangling
    /// external links are still skipped unless `--literal-links`.
    #[arg(long)]
    pub literal_external_dir_links: bool,

    /// Don't preserve permission bits (rsync `--no-p`): the destination gets
    /// explicit 0644/0755 defaults instead of the source's bits (spec §2.2),
    /// and the Windows-source executable heuristic is disabled.
    #[arg(long)]
    pub no_perms: bool,

    /// Write files sparsely (rsync `-S`): runs of zeros of at least 4096
    /// bytes become holes at the destination instead of allocated blocks —
    /// VM images, database files, and other sparse sources stay sparse. The
    /// content bytes are unchanged; only the destination's disk usage
    /// shrinks.
    #[arg(short = 'S', long)]
    pub sparse: bool,

    /// Copy extended attributes (rsync `-X`): name/value pairs for files and
    /// directories, best-effort (a `security.*` attribute a non-root
    /// receiver cannot set warns and is skipped; symlinks are not covered).
    /// On Linux, POSIX ACLs ride along as `system.posix_acl_*` attributes.
    #[arg(short = 'X', long)]
    pub xattrs: bool,

    /// Restore the source's last-access time (rsync `-U`); without it the
    /// receiver's atime is left alone. The quick check still compares
    /// size+mtime only.
    #[arg(short = 'U', long)]
    pub atimes: bool,

    /// Don't preserve modification times (rsync `--no-t`): the destination
    /// gets the transfer time, and the quick check falls back to size-only.
    #[arg(long)]
    pub no_times: bool,

    /// Don't recurse (rsync `--no-r`): sync only the source root's direct
    /// files; subdirectories are skipped.
    #[arg(long)]
    pub no_recursive: bool,

    /// Use the rsync-style rollsum delta engine (fixed-size blocks +
    /// byte-sliding scan) instead of `FastCDC` content-defined chunks.
    /// Experimental: an rsync-parity reference engine for comparison.
    #[arg(long, hide = true)]
    pub rollsum: bool,

    /// Keep the replaced destination file as `<name>~` (rsync `--backup`).
    #[arg(long)]
    pub backup: bool,

    /// fsync every received file before it is renamed into place (durable but
    /// slower; off by default).
    #[arg(long)]
    pub fsync: bool,

    /// Compress the data stream (lz4).
    #[arg(short = 'z', long)]
    pub compress: bool,

    /// Limit transfer bandwidth, e.g. `--bwlimit 10M` (bytes/s, K/M/G suffixes;
    /// `0` = unlimited).
    #[arg(long, value_parser = parse_bwlimit)]
    pub bwlimit: Option<Option<u64>>,

    /// Number of parallel transfer + hash workers. When omitted, tuned
    /// automatically from the target storage class (`--storage auto`).
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Storage class of the target filesystem: `auto` (default) detects
    /// HDD vs SSD and tunes concurrency; `hdd`/`ssd` force the class.
    #[arg(long, default_value = "auto")]
    pub storage: StoragePreference,

    /// Watch SRC and sync changes continuously (push/local: event-driven;
    /// pull: server-driven). Optional duration in d/h/m/s after `=`
    /// (`-W=1h30m`); plain `--watch` runs 24h.
    #[arg(short = 'W', long, num_args = 0..=1, require_equals = true, default_missing_value = "24h", value_parser = parse_duration)]
    pub watch: Option<std::time::Duration>,

    /// Debounce quiet window in milliseconds for --watch.
    #[arg(long, default_value_t = 1000)]
    pub watch_delay: u64,

    /// SSH port (default 22). Ports are never parsed from the target string
    /// (rsync semantics — a numeric suffix like `host:2222` is a path); use
    /// this flag for a non-default port.
    #[arg(short = 'p', long)]
    pub port: Option<u16>,

    /// Connect through a jump host `user@host[:port]` (OpenSSH `ProxyJump`).
    /// Used by the russh transport (the Windows default), which does not read
    /// ssh config; system ssh honors `ProxyJump` from `~/.ssh/config` instead.
    /// A numeric suffix is a port here — the one exception to the "no port in
    /// the target string" rule, which applies to the sync targets.
    #[arg(long, value_name = "JUMP_HOST")]
    pub jump_host: Option<String>,

    /// Password for ssh authentication, supplied directly instead of
    /// prompting. On Unix the system-ssh transport pty-injects it into the
    /// ssh password prompt (ssh cannot take a password on its argv); on
    /// Windows it requires the russh transport (the Windows default). The
    /// host key is never auto-accepted: a first connection to an unknown
    /// host fails — accept the key with ssh (or ssh-keyscan) first.
    /// **Warning**: the value is visible in the process list and shell history
    /// while the run is active; prefer keys or the prompt. Scrubbed from
    /// memory as soon as it has been sent to the server.
    #[arg(long, value_name = "PASSWORD")]
    pub password: Option<String>,

    /// Password for the jump host (`--jump-host`), when it differs from
    /// `--password`. Without this flag the jump host reuses `--password` (or
    /// the prompted value). Same visibility and scrubbing caveats as
    /// `--password`.
    #[arg(long, value_name = "PASSWORD")]
    pub jump_password: Option<String>,

    /// Run the remote `cp2 --server` under sudo, so `-a` can fully restore
    /// owner/group and device nodes (byte-identical Unix mirrors need root on
    /// the receiving side). The destination files are then owned by root —
    /// keep using this flag on every run. Needs either a NOPASSWD sudoers
    /// rule covering the remote path (e.g. `user ALL=(root) NOPASSWD:
    /// /home/user/.cargo/bin/cp2 *`), or `--sudo-password`/`--password`
    /// (injected into `sudo -S` as the first stdin line — ssh and sudo share
    /// the login password in practice). Ignored on a Windows remote.
    #[arg(long)]
    pub remote_sudo: bool,

    /// The password for `--remote-sudo` (injected into `sudo -S`). Defaults
    /// to the `--password` value — the same login password in practice.
    /// Ignored without `--remote-sudo`.
    #[arg(long, value_name = "PASSWORD")]
    pub sudo_password: Option<String>,

    /// Read the password from FILE (first line) instead of `--password` —
    /// keeps the secret out of the shell history and the process list (only
    /// the file path rides the command line). Mutually exclusive with
    /// `--password`. The file should be `chmod 600`.
    #[arg(long, value_name = "FILE")]
    pub password_file: Option<PathBuf>,

    /// Path of `cp2` on the remote, used as the remote command (rsync's
    /// `--rsync-path`). Defaults per remote OS (`~/.cargo/bin/cp2` on Unix,
    /// `%USERPROFILE%\.cargo\bin\cp2.exe` on Windows). On Unix remotes the
    /// value is shell-quoted, so paths with spaces or shell metacharacters
    /// work; on Windows remotes it is interpolated into a cmd/PowerShell
    /// command and must not contain spaces, quotes, or metacharacters.
    #[arg(long)]
    pub remote_path: Option<String>,

    /// Don't check/deploy the server-side `cp2` binary before syncing.
    #[arg(long)]
    pub no_auto_install: bool,

    /// Directory holding prebuilt `cp2-<triple>` sidecar binaries for
    /// remote platforms (checked before the directory next to this binary).
    #[arg(long)]
    pub binaries_dir: Option<PathBuf>,
}

/// Parse a `--storage` argument into a [`StoragePreference`].
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
