//! macrdp command-line server: a thin wrapper over `macrdp-core`.

use std::path::PathBuf;

mod config_store;
mod control;

use anyhow::Result;
use clap::{Parser, Subcommand};
use macrdp_core::{ServerConfig, ServerEventHandler};

#[derive(Parser, Debug)]
#[command(name = "macrdp", about = "macOS RDP Server")]
struct Cli {
    /// TCP port to listen on (overrides the config file)
    #[arg(short, long)]
    port: Option<u16>,

    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Target frame rate (30, 60, or 120)
    #[arg(long)]
    frame_rate: Option<u32>,

    /// Log level: trace, debug, info, warn, error
    #[arg(long)]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug, PartialEq)]
enum Command {
    /// Enroll your Mac password for remote login (stores only its NT hash in the Keychain)
    Enroll,
    /// Run as the macOS app's helper: JSON Lines commands on stdin, events on stdout
    Control,
}

/// The CLI only needs the log output, which tracing already writes.
struct CliEvents;

impl ServerEventHandler for CliEvents {}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Enroll) => return enroll(),
        Some(Command::Control) => {
            run_control(cli.config).await?;
            // tokio's stdin reader blocks a pool thread that cannot be cancelled: exit now.
            std::process::exit(0);
        }
        None => {}
    }

    let mut config = ServerConfig::load_from_file(cli.config.as_deref())?;
    if let Some(port) = cli.port {
        config.port = port;
    }
    if let Some(fps) = cli.frame_rate {
        config.frame_rate = fps;
    }
    if let Some(level) = cli.log_level {
        config.log_level = Some(level);
    }

    let log_path = init_logging(config.log_level.as_deref().unwrap_or("info"))?;
    eprintln!("Log file: {}", log_path.display());
    tracing::info!(port = config.port, frame_rate = config.frame_rate, "macrdp server starting");

    macrdp_core::permissions::request_permissions();
    let handle = macrdp_core::start_server(config, CliEvents).await?;
    tracing::info!(port = handle.port(), "Connect using an RDP client (e.g., Windows mstsc or Microsoft Remote Desktop)");

    tokio::signal::ctrl_c().await?;
    tracing::info!("Received Ctrl-C — shutting down");
    handle.stop().await
}

/// The app's helper. Stdout carries protocol JSON only; tracing goes to the log file.
async fn run_control(config: Option<PathBuf>) -> Result<()> {
    let path = match config {
        Some(path) => path,
        None => config_store::default_path()?,
    };
    let level = config_store::load(&path).ok().and_then(|c| c.log_level);
    let (events, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let set_log_level = init_control_logging(level.as_deref(), events.clone())?;
    let backend = control::MacBackend { set_log_level };
    let mut controller = control::Controller::new(backend, std::io::stdout(), path, events);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let shutdown = async move {
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM: shutting down"),
            _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT: shutting down"),
        }
    };
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    control::run(&mut controller, stdin, events_rx, shutdown).await;
    Ok(())
}

/// Log to `~/Library/Logs/macrdp/macrdp.log` only, and forward warnings (or the configured
/// level) to the app. Returns the function that changes the level at run time.
fn init_control_logging(
    level: Option<&str>,
    events: tokio::sync::mpsc::UnboundedSender<control::Event>,
) -> Result<control::SetLogLevel> {
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Arc;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // sspi debug output can carry NTLM material: capped at warn.
    let filter = |level: &str| {
        tracing_subscriber::EnvFilter::new(level).add_directive("sspi=warn".parse().expect("static directive"))
    };
    let (filter_layer, reload) = tracing_subscriber::reload::Layer::new(filter(level.unwrap_or("info")));
    let threshold = Arc::new(AtomicU8::new(control::level_rank(level.unwrap_or("warn"))));

    let log_path = macrdp_core::log_bridge::default_log_dir()?.join("macrdp.log");
    let log_file = macrdp_core::log_bridge::open_private_log(&log_path)?;
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(log_file)),
        )
        .with(control::LogForwarder { events, threshold: Arc::clone(&threshold) })
        .init();
    Ok(Box::new(move |level: &str| {
        if let Err(e) = reload.modify(|f| *f = filter(level)) {
            tracing::warn!("Cannot change the log level: {e}");
        }
        threshold.store(control::level_rank(level), Ordering::Relaxed);
    }))
}

/// Verifies the Mac password with OpenDirectory and stores its NT hash in the Keychain.
fn enroll() -> Result<()> {
    use macrdp_core::auth;

    let user = auth::current_user()?;
    eprint!("Mac password for {user}: ");
    let password = zeroize::Zeroizing::new(read_password()?);
    if auth::enroll(&auth::SystemAccountStore, &user, &password)? {
        eprintln!("Enrolled. Remote login is enabled for {user}.");
        Ok(())
    } else {
        anyhow::bail!("wrong password for {user}; nothing was stored")
    }
}

/// Reads one line from stdin without echo when stdin is a terminal.
fn read_password() -> Result<String> {
    use std::io::BufRead;

    /// Restores the terminal mode, even on error.
    struct EchoGuard(Option<libc::termios>);
    impl Drop for EchoGuard {
        fn drop(&mut self) {
            if let Some(original) = &self.0 {
                // SAFETY: restores the attributes read from the same descriptor.
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original) };
                eprintln!();
            }
        }
    }

    // SAFETY: termios is plain data; tcgetattr fills it or fails without side effects.
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1
        && unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } == 0;
    let _guard = if is_tty {
        let original = term;
        term.c_lflag &= !libc::ECHO;
        // SAFETY: `term` is a valid termios read from this descriptor.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) } != 0 {
            anyhow::bail!("cannot disable terminal echo");
        }
        EchoGuard(Some(original))
    } else {
        EchoGuard(None)
    };

    let mut line = zeroize::Zeroizing::new(String::new());
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// Log to stderr and to `~/Library/Logs/macrdp/macrdp.log` (dir 0700, file 0600).
fn init_logging(level: &str) -> Result<PathBuf> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // sspi debug output can carry NTLM material: capped at warn even under RUST_LOG.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level))
        .add_directive("sspi=warn".parse()?);

    let log_path = macrdp_core::log_bridge::default_log_dir()?.join("macrdp.log");
    let log_file = macrdp_core::log_bridge::open_private_log(&log_path)?;

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(log_file)),
        )
        .init();
    Ok(log_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enroll_is_a_subcommand() {
        let cli = Cli::try_parse_from(["macrdp-server", "enroll"]).unwrap();
        assert_eq!(cli.command, Some(Command::Enroll));
    }

    #[test]
    fn control_is_a_subcommand() {
        let cli = Cli::try_parse_from(["macrdp-server", "control"]).unwrap();
        assert_eq!(cli.command, Some(Command::Control));
    }

    #[test]
    fn no_subcommand_runs_the_server() {
        let cli = Cli::try_parse_from(["macrdp-server", "--port", "3390"]).unwrap();
        assert_eq!(cli.command, None);
        assert_eq!(cli.port, Some(3390));
    }
}
