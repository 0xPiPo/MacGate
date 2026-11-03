//! Server configuration

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Audio forwarding configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub enabled: bool,
    /// Kept for old config files; audio is always sent in the captured 48 kHz stereo format.
    pub sample_rate: u32,
    /// Kept for old config files; see `sample_rate`.
    pub channels: u16,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_rate: 48000,
            channels: 2,
        }
    }
}

/// Clipboard synchronization configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    pub enabled: bool,
    pub file_transfer: bool,
    pub max_file_size_mb: u32,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            file_transfer: true,
            max_file_size_mb: 100,
        }
    }
}

/// Server configuration (loaded from TOML, shared between CLI and UI)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub port: u16,
    pub frame_rate: u32,
    /// Resolution width (0 = auto-detect from display)
    pub width: u32,
    /// Resolution height (0 = auto-detect from display)
    pub height: u32,
    /// Address to listen on (default 0.0.0.0). Credentials never live in this file:
    /// the Mac account is enrolled into the Keychain.
    pub bind_address: String,
    pub cert_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
    pub idle_timeout_secs: u64,
    /// Lock the Mac when a session ends (default: false; the next client unlocks it remotely)
    #[serde(default)]
    pub lock_on_disconnect: bool,
    /// Type the session's password into the lock screen when a client connects to a locked Mac
    /// (default: false). The password is held in memory for at most 10 s and typed once.
    #[serde(default)]
    pub auto_unlock: bool,
    /// Log level: trace, debug, info, warn, error
    pub log_level: Option<String>,
    /// Video quality: low_latency, balanced, high_quality (default: high_quality)
    pub quality: Option<String>,
    /// H.264 encoder: software, hardware, auto (default: hardware)
    pub encoder: Option<String>,
    /// Chroma subsampling mode: "avc420" or "avc444" (default: "avc420")
    pub chroma_mode: Option<String>,
    /// RemoteFX quality for clients without H.264: "auto", "low", "medium", "high"
    /// (default: "auto", which follows the connection)
    pub rfx_quality: Option<String>,
    /// How the session display relates to the Mac's displays: "curtain", "mirror" or "extend"
    /// (default: "curtain"). Applies to the next connection.
    pub display_mode: Option<String>,
    /// Curtain: the external monitors honour DDC standby, as the app's "Test monitor standby"
    /// showed (`None`: not tested). Only `Some(true)` puts them to sleep; otherwise the curtain
    /// covers the displays with black and moves their windows into the session.
    pub curtain_standby_ok: Option<bool>,
    /// Curtain with standby: re-send standby every 5 s so the monitor's power button cannot
    /// wake it mid-session (default: false, no timer).
    #[serde(default)]
    pub curtain_standby_keepalive: bool,
    /// Resolution: "auto", or "WxH" like "3840x2160" (default: "auto")
    #[serde(alias = "hidpi_scale")]
    pub resolution: Option<String>,
    /// Show cursor in capture (default: true)
    pub show_cursor: Option<bool>,
    /// Target bitrate in Mbps (default: auto-calculated)
    pub bitrate_mbps: Option<u32>,
    /// Audio forwarding configuration
    #[serde(default)]
    pub audio: AudioConfig,
    /// Clipboard synchronization configuration
    #[serde(default)]
    pub clipboard: ClipboardConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 3389,
            frame_rate: 60,
            width: 0,
            height: 0,
            bind_address: "0.0.0.0".to_string(),
            cert_path: None,
            key_path: None,
            idle_timeout_secs: 1800,
            lock_on_disconnect: false,
            auto_unlock: false,
            log_level: None,
            quality: None,
            encoder: None,
            chroma_mode: None,
            rfx_quality: None,
            display_mode: None,
            curtain_standby_ok: None,
            curtain_standby_keepalive: false,
            resolution: None,
            show_cursor: None,
            bitrate_mbps: None,
            audio: AudioConfig::default(),
            clipboard: ClipboardConfig::default(),
        }
    }
}

impl ServerConfig {
    /// Load config from a TOML file path, or use defaults if None.
    pub fn load_from_file(path: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let path = match path {
            Some(path) => path.to_path_buf(),
            None => config_dir().join("config.toml"),
        };
        if !path.exists() {
            return Ok(ServerConfig::default());
        }
        let content = std::fs::read_to_string(&path)?;
        if has_legacy_credentials(&content) {
            tracing::warn!(
                path = %path.display(),
                "config.toml still holds username/password; they are ignored. Remove them: login uses the enrolled Mac account"
            );
        }
        toml::from_str(&content).map_err(|e| redacted_parse_error(&content, &e))
    }
}

/// A parse error without the quoted source line: toml's own Display echoes the offending
/// line, which in an old config can be a plaintext `password = ...`.
fn redacted_parse_error(content: &str, e: &toml::de::Error) -> anyhow::Error {
    match e.span() {
        Some(span) => {
            let line = content[..span.start.min(content.len())].matches('\n').count() + 1;
            anyhow::anyhow!("TOML error on line {line}: {}", e.message())
        }
        None => anyhow::anyhow!("TOML error: {}", e.message()),
    }
}

/// True when an old config still carries plaintext credentials.
fn has_legacy_credentials(content: &str) -> bool {
    toml::from_str::<toml::Table>(content)
        .is_ok_and(|t| t.contains_key("password") || t.contains_key("username"))
}

/// The macrdp config directory, shared by the app's helper and the command line (and the
/// TLS files under it): `~/Library/Application Support/macrdp`. An older
/// `~/.config/macrdp/config.toml` is still used while Application Support has none. The
/// current directory is never searched, so where macrdp is started from cannot change its
/// config, certificate or listen address.
pub fn config_dir() -> PathBuf {
    let native = dirs::config_dir().map(|d| d.join("macrdp"));
    let legacy = dirs::home_dir().map(|h| h.join(".config").join("macrdp"));
    let chosen = match (&native, &legacy) {
        (Some(native), Some(legacy)) if !native.join("config.toml").exists() && legacy.join("config.toml").exists() => {
            legacy.clone()
        }
        _ => native.clone().or(legacy.clone()).unwrap_or_else(|| PathBuf::from(".")),
    };
    warn_ignored_configs(&chosen, [legacy, std::env::current_dir().ok()]);
    chosen
}

/// Says so, once, when a config.toml that earlier versions read is now ignored.
fn warn_ignored_configs(chosen: &std::path::Path, others: [Option<PathBuf>; 2]) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        for dir in others.into_iter().flatten().filter(|d| d != chosen) {
            let ignored = dir.join("config.toml");
            if ignored.exists() {
                tracing::warn!(
                    ignored = %ignored.display(),
                    used = %chosen.join("config.toml").display(),
                    "This config file is not read; move it, or pass --config to use it"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_does_not_echo_the_source_line() {
        let dir = std::env::temp_dir().join(format!("macrdp-config-redact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "port = 3389\npassword = hunter2\n").unwrap();
        let err = format!("{:#}", ServerConfig::load_from_file(Some(&path)).unwrap_err());
        assert!(!err.contains("hunter2"), "{err}");
        assert!(err.contains("line 2"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_default_config() {
        let config = ServerConfig::default();
        assert_eq!(config.port, 3389);
        assert_eq!(config.frame_rate, 60);
        assert_eq!(config.bind_address, "0.0.0.0");
    }

    #[test]
    fn legacy_credentials_are_detected_and_ignored() {
        let old = "port = 3389\nusername = \"admin\"\npassword = \"123456\"\n";
        assert!(has_legacy_credentials(old));
        assert!(!has_legacy_credentials("port = 3389\n"));
        let config: ServerConfig = toml::from_str(old).unwrap();
        assert_eq!(config.port, 3389);
    }

    #[test]
    fn example_config_parses() {
        let config: ServerConfig = toml::from_str(include_str!("../../../config.example.toml")).unwrap();
        assert!(config.audio.enabled && config.clipboard.enabled);
    }

    #[test]
    fn option_keys_parse_and_default() {
        let config: ServerConfig = toml::from_str("").unwrap();
        assert_eq!(config.rfx_quality, None);
        assert_eq!(config.display_mode, None);
        assert!(config.audio.enabled && config.clipboard.enabled && !config.lock_on_disconnect);
        assert_eq!(config.curtain_standby_ok, None);
        assert!(!config.curtain_standby_keepalive);
        let toml_str = r#"
            curtain_standby_ok = false
            curtain_standby_keepalive = true
            rfx_quality = "high"
            display_mode = "extend"
            frame_rate = 30
            lock_on_disconnect = true
            show_cursor = false
            idle_timeout_secs = 0
            [audio]
            enabled = false
            [clipboard]
            enabled = false
        "#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.rfx_quality.as_deref(), Some("high"));
        assert_eq!(config.display_mode.as_deref(), Some("extend"));
        assert_eq!(config.frame_rate, 30);
        assert!(config.lock_on_disconnect);
        assert_eq!(config.show_cursor, Some(false));
        assert_eq!(config.idle_timeout_secs, 0);
        assert_eq!(config.curtain_standby_ok, Some(false));
        assert!(config.curtain_standby_keepalive);
        assert!(!config.audio.enabled && !config.clipboard.enabled);
    }

    #[test]
    fn test_parse_toml_config() {
        let toml_str = r#"
            port = 13389
            frame_rate = 120
            bind_address = "10.0.0.2"
        "#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.port, 13389);
        assert_eq!(config.frame_rate, 120);
        assert_eq!(config.bind_address, "10.0.0.2");
    }
}
