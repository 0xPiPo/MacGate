use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

static LOG_FILE: OnceLock<std::sync::Mutex<File>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub struct LogBridgeLayer;

impl LogBridgeLayer {
    pub fn new() -> Self { Self }
}

/// Initialize the log file. Call once at startup.
pub fn init_log_file() -> PathBuf {
    let path = std::env::temp_dir().join("macrdp-logs.jsonl");
    let file = open_private_log(&path).expect("failed to create log file");
    let _ = LOG_FILE.set(std::sync::Mutex::new(file));
    let _ = LOG_PATH.set(path.clone());
    path
}

/// Appends to (or creates) a log file with mode 0600, fixing the mode of an older file.
/// shortcut: no rotation, the file grows across runs; add size-based rotation if it gets large.
/// A symlink at `path` is refused, so nobody can redirect the log onto another file.
pub fn open_private_log(path: &std::path::Path) -> std::io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// `~/Library/Logs/macrdp`, created owner-only (0700).
pub fn default_log_dir() -> std::io::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| std::io::Error::other("no home directory"))?;
    log_dir_in(&home)
}

fn log_dir_in(home: &std::path::Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = home.join("Library/Logs/macrdp");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// One JSON log line. Escaping control characters keeps client-supplied text
/// (user names, file names) from forging extra lines.
fn json_line(level: &str, message: &str, timestamp: &str) -> String {
    format!(
        r#"{{"level":"{}","message":"{}","timestamp":"{}"}}"#,
        json_escape(level),
        json_escape(message),
        json_escape(timestamp)
    )
}

fn json_escape(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Get the log file path (for UI to read).
pub fn log_file_path() -> Option<&'static PathBuf> {
    LOG_PATH.get()
}

impl<S: Subscriber> Layer<S> for LogBridgeLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let Some(file) = LOG_FILE.get() else { return };
        let Ok(mut file) = file.try_lock() else { return };

        let metadata = event.metadata();
        let level = metadata.level().as_str();

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let message = if visitor.message.is_empty() {
            metadata.name()
        } else {
            &visitor.message
        };

        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let _ = writeln!(file, "{}", json_line(&level.to_lowercase(), message, &ts));
        let _ = file.flush();
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else if self.message.is_empty() {
            self.message = format!("{} = {:?}", field.name(), value);
        } else {
            self.message.push_str(&format!(" {} = {:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else if self.message.is_empty() {
            self.message = format!("{} = {}", field.name(), value);
        } else {
            self.message.push_str(&format!(" {} = {}", field.name(), value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newlines_cannot_forge_log_lines() {
        let line = json_line("warn", "user = \"x\n{\"level\":\"info\",\"message\":\"forged\"}\"", "t");
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains(r"\n"));
    }

    #[test]
    fn quotes_backslashes_and_controls_are_escaped() {
        let line = json_line("info", "a\"b\\c\rd\te\u{1}f", "t");
        assert_eq!(line, r#"{"level":"info","message":"a\"b\\c\rd\te\u0001f","timestamp":"t"}"#);
    }

    #[test]
    fn symlinked_log_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("victim");
        std::fs::write(&target, "keep").unwrap();
        let link = dir.path().join("macrdp.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_private_log(&link).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    }

    #[test]
    fn log_dir_is_under_library_logs_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = log_dir_in(home.path()).unwrap();
        assert_eq!(dir, home.path().join("Library/Logs/macrdp"));
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn log_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        open_private_log(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Earlier runs stay readable for diagnosis.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }
}
