//! Common types for cp2

use std::path::PathBuf;

/// A remote sync target: `user@host:port/path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTarget {
    /// Remote user (authenticated by sshd).
    pub user: String,
    /// Host name or IP literal (passed to `ssh` as-is, so ssh config,
    /// `known_hosts`, and `ProxyJump` all apply).
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// Remote path: empty means the serve root (the account home), a leading
    /// `/` is an absolute server path, anything else is relative to the serve
    /// root (rsync semantics).
    pub path: String,
}

impl std::fmt::Display for RemoteTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}:{}", self.user, self.host, self.path)
    }
}

/// A sync source or destination: either a local path or a remote target.
///
/// Mirrors rsync's URI syntax: `user@host:path` denotes a remote location,
/// anything else is a local path. The direction of a sync is inferred from
/// which side is remote — `cp2 SRC DST` pushes when `SRC` is local and
/// pulls when `SRC` is remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A local filesystem path.
    Local(PathBuf),
    /// A remote `user@host:port[/path]` target.
    Remote(RemoteTarget),
}

impl Location {
    /// Parse a source/destination string.
    ///
    /// Supports:
    /// - `/local/path`
    /// - `user@host:/remote/path` (absolute on the server)
    /// - `user@host:relative/path` (relative to the serve root, rsync-style;
    ///   a numeric suffix is a path, never a port)
    /// - `user@host` / `user@host:` (the serve root itself)
    ///
    /// The port always defaults to 22; set it with `--port` (`-p`), never in
    /// the target string (rsync semantics). A local path containing `@`
    /// (`./foo@bar`) stays local — the remote form requires a user and host
    /// free of path separators.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        if let Some((user, rest)) = s.split_once('@') {
            // A path separator in the candidate user or host means `@` is
            // part of a local path (`./foo@bar`, `C:\Users\alice@corp\file`),
            // not a remote target — remote users and hosts never contain one.
            if user.contains('/') || user.contains('\\') {
                return Location::Local(PathBuf::from(s));
            }
            let (host, path) = parse_remote(rest);
            if host.contains('/') || host.contains('\\') {
                return Location::Local(PathBuf::from(s));
            }
            Location::Remote(RemoteTarget {
                user: user.to_string(),
                host,
                port: DEFAULT_PORT,
                path,
            })
        } else {
            Location::Local(PathBuf::from(s))
        }
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(path) => write!(f, "{}", path.display()),
            Self::Remote(r) => write!(f, "{r}"),
        }
    }
}

/// Default port for remote targets when none is given (SSH's default).
const DEFAULT_PORT: u16 = 22;

/// Parse the part of a remote target after `user@` into host, port, and path.
///
/// rsync semantics: the first `:` (or the first `/` for the no-colon
/// `host/path` form) separates the host from the path, and the path is taken
/// **verbatim** — a leading `/` is an absolute server path, anything else is
/// relative to the serve root (the account home), and an absent path means
/// the serve root. A numeric suffix is always a path, never a port: cp2 has
/// no port-in-target syntax — set the port with `--port` (so `host:2024/reports`
/// is the relative path `2024/reports`, not "port 2024").
///
/// Handles bracketed IPv6 literals: `[::1]`, `[::1]:path`, `[::1]:/abs`,
/// `[::1]/path`.
fn parse_remote(rest: &str) -> (String, String) {
    // The first `/` splits off a path suffix. The path is *absolute* only
    // when the colon is immediately followed by the slash (`host:/abs`) — the
    // empty host-port tail is the marker. Otherwise the `/` is a separator
    // (`host/path`) or part of a port-less path (`host:2024/reports` →
    // relative `2024/reports`).
    let (host_port, slash_path) = match rest.split_once('/') {
        Some((hp, p)) => (hp, Some(p)),
        None => (rest, None),
    };
    let absolute = host_port.ends_with(':') && slash_path.is_some();
    let path = slash_path.map_or(String::new(), |p| {
        if absolute {
            format!("/{p}")
        } else {
            p.to_string()
        }
    });

    let (host, path) = if let Some(rest_brackets) = host_port.strip_prefix('[') {
        // Bracketed IPv6 literal: `[::1]`, `[::1]:path`, `[::1]:/abs`,
        // `[::1]/path`. An unbalanced '[' is not a valid literal — treat the
        // whole string verbatim as the host.
        match rest_brackets.split_once(']') {
            Some((host, tail)) => {
                let path = match tail.strip_prefix(':') {
                    // `[::1]:path` — the suffix after the colon is the path.
                    Some(suffix) => join_suffix(&path, suffix),
                    None => path,
                };
                (host, path)
            }
            None => (host_port, path),
        }
    } else if host_port.starts_with("::") {
        // Legacy unbracketed IPv6 host (`::1/data`): the colons are part of
        // the address, not separators.
        (host_port, path)
    } else {
        // Unbracketed host: rsync semantics — the FIRST `:` separates host
        // from path, and the path is taken verbatim (`host:a:b` → host
        // `host`, path `a:b`). IPv6 literals must be bracketed.
        match host_port.split_once(':') {
            Some((host, suffix)) => (host, join_suffix(&path, suffix)),
            None => (host_port, path),
        }
    };

    // An empty host is preserved as-is, never silently rewritten (e.g. to
    // `localhost`), so a malformed `user@:path` fails loudly instead of
    // routing to the wrong machine.
    (host.to_string(), path)
}

/// Combine the pre-slash path part with a `host:path` colon suffix:
/// `host:` → empty (serve root); `host:/abs` → `/abs`; `host:path` → `path`;
/// `host:path/more` → `path/more`.
fn join_suffix(path: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        path.to_string()
    } else if path.is_empty() {
        suffix.to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{suffix}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_numeric_suffix_is_path_not_port() {
        // rsync semantics: a numeric suffix is always a path — ports come
        // only from `--port`.
        let loc = Location::parse("user@127.0.0.1:4433/backup");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.user, "user");
                assert_eq!(r.host, "127.0.0.1");
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "4433/backup");
            }
            Location::Local(_) => panic!("expected remote"),
        }
        let loc = Location::parse("user@127.0.0.1:4433");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "4433");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_host_slash_path_defaults_port() {
        let loc = Location::parse("user@127.0.0.1:/data");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.port, 22);
                // Colon + `/` = an absolute server path.
                assert_eq!(r.path, "/data");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_rsync_host_colon_path_keeps_path() {
        // rsync syntax `user@host:path`: relative to the serve root.
        let loc = Location::parse("user@127.0.0.1:backup");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "backup");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_rsync_host_colon_path_with_slashes_keeps_all_segments() {
        // Regression: the first `/` used to split off `host:backup`, losing
        // the leading segments of a multi-segment relative path.
        let loc = Location::parse("user@127.0.0.1:softwares/cp2");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "softwares/cp2");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_absolute_path_after_colon() {
        let loc = Location::parse("user@host:/home/user/softwares/cp2");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "/home/user/softwares/cp2");
            }
            Location::Local(_) => panic!("expected remote"),
        }
        // `host:/` is the filesystem root, `host:` (empty) is the serve root.
        let loc = Location::parse("user@host:/");
        assert_eq!(
            match loc {
                Location::Remote(r) => r.path,
                Location::Local(_) => panic!("expected remote"),
            },
            "/"
        );
        let loc = Location::parse("user@host:");
        assert_eq!(
            match loc {
                Location::Remote(r) => r.path,
                Location::Local(_) => panic!("expected remote"),
            },
            ""
        );
    }

    #[test]
    fn parse_hostname_kept_verbatim() {
        let loc = Location::parse("alice@backup.example.com:2222");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.host, "backup.example.com");
                // `2222` is a relative path, not a port.
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "2222");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_local() {
        assert!(matches!(
            Location::parse("/tmp/foo"),
            Location::Local(_)
        ));
    }

    #[test]
    fn parse_ipv6_bracketed_with_numeric_suffix() {
        // Regression: `[::1]:port` used to swallow the port into the host
        // string; now the suffix is a path.
        let loc = Location::parse("user@[::1]:2222/backup");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.user, "user");
                assert_eq!(r.host, "::1");
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "2222/backup");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_ipv6_bracketed_without_port() {
        let loc = Location::parse("user@[::1]/data");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.host, "::1");
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "data");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_ipv6_bracketed_numeric_suffix_only() {
        let loc = Location::parse("user@[fe80::1]:4433");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.host, "fe80::1");
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "4433");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_ipv6_bracketed_absolute_path() {
        let loc = Location::parse("user@[::1]:/data");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.host, "::1");
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "/data");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_ipv6_unbracketed_without_port() {
        let loc = Location::parse("user@::1/data");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.host, "::1");
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "data");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_numeric_overflow_suffix_is_plain_path() {
        // A huge number is a path, not an invalid port.
        let loc = Location::parse("user@[::1]:99999/x");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.port, 22);
                assert_eq!(r.path, "99999/x");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_first_colon_splits_verbatim_path() {
        // rsync semantics: the first `:` separates host from path, and the
        // path is taken verbatim — a `:` inside the path is not a separator.
        let loc = Location::parse("user@host:a:b");
        match loc {
            Location::Remote(r) => {
                assert_eq!(r.user, "user");
                assert_eq!(r.host, "host");
                assert_eq!(r.path, "a:b");
            }
            Location::Local(_) => panic!("expected remote"),
        }
    }

    #[test]
    fn parse_at_sign_in_local_path_stays_local() {
        // A path separator in the candidate user/host means `@` is part of a
        // local path, not a remote target.
        assert!(matches!(
            Location::parse("./foo@bar"),
            Location::Local(_)
        ));
        assert!(matches!(
            Location::parse("C:\\Users\\alice@corp\\file"),
            Location::Local(_)
        ));
        assert!(matches!(
            Location::parse("alice@host\\backup"),
            Location::Local(_)
        ));
    }

    #[test]
    fn parse_empty_host_not_localhost() {
        // `user@:path` used to silently target localhost; the empty host is
        // now preserved so the run fails loudly instead of misrouting.
        let loc = Location::parse("user@:path");
        match loc {
            Location::Remote(r) => assert_eq!(r.host, ""),
            Location::Local(_) => panic!("expected remote"),
        }
        let loc = Location::parse("user@:");
        match loc {
            Location::Remote(r) => assert_eq!(r.host, ""),
            Location::Local(_) => panic!("expected remote"),
        }
    }
}
