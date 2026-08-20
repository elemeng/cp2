use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

fn main() -> Result<()> {
    // Cap the glibc malloc arenas. The batch-heavy paths free and
    // re-allocate the batch's ~128 MiB per flush, and glibc's default
    // multi-arena heaps retain each freed chunk in its allocating arena —
    // the RSS climbs to ~3x the working set (measured 400-590 MB vs ~180 MB
    // with MALLOC_ARENA_MAX=1; the working set is the batch + the read
    // window, and the single arena reuses the freed chunks directly,
    // measuring no slower). The glibc reads the variable at its *first*
    // allocation — the std's early startup, before `main` — so setting it
    // in-process is too late: the first run re-executes itself with the
    // variable set (a ~10 ms process start), and the fresh process's first
    // malloc sees it. `--server` children inherit both the environment and
    // the stdio pipes, so the receiver gets the cap too.
    #[cfg(unix)]
    if std::env::var_os("CP2_ARENA_CAPPED").is_none() {
        // # Safety
        //
        // No other thread exists yet (the runtime starts below), so no
        // thread can observe the environment mid-mutation.
        unsafe {
            std::env::set_var("MALLOC_ARENA_MAX", "1");
            std::env::set_var("CP2_ARENA_CAPPED", "1");
        }
        // `exec` replaces this process — a spawned `--server` child (the
        // local-pipe path and the e2e tests) must keep its identity: a
        // fork-and-exit intermediate would leave the grandchild holding the
        // pipes after the intermediate is killed.
        let err = std::process::Command::new(std::env::current_exe()?)
            .args(std::env::args().skip(1))
            .exec();
        return Err(err.into());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run())
}

async fn run() -> Result<()> {
    let args = cp2::cli::Cli::parse();
    init_logging(args.verbose);

    if let Err(e) = cp2::commands::dispatch(args).await {
        // Grepable: no decorative characters — scripts and logs search for
        // the `error:` prefix (clig.dev output contract).
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    Ok(())
}

fn init_logging(verbose: u8) {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    let env_filter = EnvFilter::try_from_default_env().unwrap_or(EnvFilter::new(filter));
    // Diagnostics go to stderr: stdout is the protocol channel in `--server`
    // mode (and must stay clean for scripts in normal mode too).
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
}
