//! Server configuration

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Audio forwarding configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub enabled: bool,
    pub sample_rate: u32,
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
        Ok(toml::from_str(&content)?)
    }
}

/// True when an old config still carries plaintext credentials.
fn has_legacy_credentials(content: &str) -> bool {
    toml::from_str::<toml::Table>(content)
        .is_ok_and(|t| t.contains_key("password") || t.contains_key("username"))
}

/// Returns the macrdp config directory.
/// Search order: ./ -> ~/.config/macrdp -> ~/Library/Application Support/macrdp
pub fn config_dir() -> PathBuf {
    let candidates = config_dir_candidates();
    for dir in &candidates {
        if dir.join("config.toml").exists() {
            return dir.clone();
        }
    }
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_dir_candidates() -> Vec<PathBuf> {
    let mut dirs_list = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        dirs_list.push(cwd);
    } else {
        dirs_list.push(PathBuf::from("."));
    }
    if let Some(home) = dirs::home_dir() {
        dirs_list.push(home.join(".config").join("macrdp"));
    }
    if let Some(native) = dirs::config_dir() {
        dirs_list.push(native.join("macrdp"));
    }
    dirs_list
}

#[cfg(test)]
mod tests {
    use super::*;

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
