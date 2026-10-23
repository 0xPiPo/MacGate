//! The control helper's config file: load, validate a single key, and save atomically (0600).

use std::path::{Path, PathBuf};

use macrdp_core::{ConfigUpdate, ServerConfig};
use serde_json::Value;

/// `~/Library/Application Support/macrdp/config.toml`.
pub fn default_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no Application Support directory"))?;
    Ok(dir.join("macrdp").join("config.toml"))
}

/// The config at `path`; defaults when the file does not exist.
pub fn load(path: &Path) -> anyhow::Result<ServerConfig> {
    ServerConfig::load_from_file(Some(path))
}

/// Writes `config` to `path` atomically: a 0600 temp file in the same directory, then rename.
pub fn save(path: &Path, config: &ServerConfig) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let dir = path.parent().ok_or_else(|| std::io::Error::other("config path has no directory"))?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let text = toml::to_string_pretty(config).map_err(std::io::Error::other)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    // A stale temp file from a crashed helper with the same pid.
    let _ = std::fs::remove_file(&tmp);
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// How a valid change takes effect.
#[derive(Debug)]
pub enum Effect {
    /// Applied to the running server now (or from its next connection).
    Hot(ConfigUpdate),
    /// Takes effect when the server restarts.
    Restart,
}

/// Validates `value` for `key` and writes it into `config`.
pub fn apply(config: &mut ServerConfig, key: &str, value: &Value) -> Result<Effect, String> {
    let boolean = || value.as_bool().ok_or_else(|| format!("{key} must be true or false"));
    let one_of = |allowed: &[&str]| {
        value
            .as_str()
            .filter(|v| allowed.contains(v))
            .map(str::to_string)
            .ok_or_else(|| format!("{key} must be one of {}", allowed.join(", ")))
    };
    Ok(match key {
        "port" => {
            config.port = value
                .as_u64()
                .filter(|p| (1024..=65535).contains(p))
                .ok_or("port must be an integer from 1024 to 65535")? as u16;
            Effect::Restart
        }
        "bind_address" => {
            let ip: std::net::IpAddr = value
                .as_str()
                .and_then(|v| v.parse().ok())
                .ok_or("bind_address must be an IP address")?;
            config.bind_address = ip.to_string();
            Effect::Restart
        }
        "frame_rate" => {
            let fps = value
                .as_u64()
                .filter(|f| [15, 30, 60].contains(f))
                .ok_or("frame_rate must be 15, 30 or 60")? as u32;
            config.frame_rate = fps;
            Effect::Hot(ConfigUpdate::FrameRate(fps))
        }
        "rfx_quality" => {
            let q = one_of(&["auto", "low", "medium", "high"])?;
            config.rfx_quality = Some(q.clone());
            Effect::Hot(ConfigUpdate::RfxQuality(q))
        }
        "display_mode" => {
            let mode = one_of(&["curtain", "mirror", "extend"])?;
            config.display_mode = Some(mode.clone());
            Effect::Hot(ConfigUpdate::DisplayMode(mode))
        }
        "curtain_standby_ok" => {
            let ok = boolean()?;
            config.curtain_standby_ok = Some(ok);
            Effect::Hot(ConfigUpdate::CurtainStandbyOk(ok))
        }
        "curtain_standby_keepalive" => {
            let on = boolean()?;
            config.curtain_standby_keepalive = on;
            Effect::Hot(ConfigUpdate::CurtainStandbyKeepalive(on))
        }
        "show_cursor" => {
            let show = boolean()?;
            config.show_cursor = Some(show);
            Effect::Hot(ConfigUpdate::ShowCursor(show))
        }
        "log_level" => {
            let level = one_of(&["trace", "debug", "info", "warn", "error"])?;
            config.log_level = Some(level.clone());
            Effect::Hot(ConfigUpdate::LogLevel(level))
        }
        "idle_timeout_secs" => {
            config.idle_timeout_secs = value
                .as_u64()
                .filter(|s| *s <= MAX_IDLE_TIMEOUT_SECS)
                .ok_or("idle_timeout_secs must be an integer from 0 (off) to 604800")?;
            Effect::Restart
        }
        "lock_on_disconnect" => {
            config.lock_on_disconnect = boolean()?;
            Effect::Restart
        }
        "auto_unlock" => {
            config.auto_unlock = boolean()?;
            Effect::Restart
        }
        "audio.enabled" => {
            config.audio.enabled = boolean()?;
            Effect::Restart
        }
        "clipboard.enabled" => {
            config.clipboard.enabled = boolean()?;
            Effect::Restart
        }
        _ => return Err("unknown config key".to_string()),
    })
}

/// One week.
const MAX_IDLE_TIMEOUT_SECS: u64 = 7 * 24 * 3600;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn save_creates_an_owner_only_file_that_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("macrdp").join("config.toml");
        let config = ServerConfig {
            port: 3390,
            display_mode: Some("extend".into()),
            audio: macrdp_core::AudioConfig { enabled: false, ..Default::default() },
            ..Default::default()
        };
        save(&path, &config).unwrap();
        assert_eq!(mode(&path), 0o600);
        let back = load(&path).unwrap();
        assert_eq!(back.port, 3390);
        assert_eq!(back.display_mode.as_deref(), Some("extend"));
        assert!(!back.audio.enabled);
    }

    #[test]
    fn save_replaces_atomically_and_tightens_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = 4000\npassword = \"old\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.port, 4000);
        save(&path, &config).unwrap();
        assert_eq!(mode(&path), 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("password"), "legacy credentials are not written back");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["config.toml"], "no temp file left behind");
    }

    #[test]
    fn missing_file_loads_defaults_and_invalid_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(load(&path).unwrap().port, 3389);
        std::fs::write(&path, "port = \"x\"").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn hot_keys_apply_and_validate() {
        let mut c = ServerConfig::default();
        assert!(matches!(apply(&mut c, "frame_rate", &json!(30)), Ok(Effect::Hot(ConfigUpdate::FrameRate(30)))));
        assert_eq!(c.frame_rate, 30);
        assert!(apply(&mut c, "frame_rate", &json!(45)).is_err());
        assert!(apply(&mut c, "frame_rate", &json!("30")).is_err());
        assert!(matches!(apply(&mut c, "rfx_quality", &json!("high")), Ok(Effect::Hot(ConfigUpdate::RfxQuality(_)))));
        assert!(apply(&mut c, "rfx_quality", &json!("ultra")).is_err());
        assert!(matches!(apply(&mut c, "rfx_quality", &json!("auto")), Ok(Effect::Hot(ConfigUpdate::RfxQuality(q))) if q == "auto"));
        assert_eq!(c.rfx_quality.as_deref(), Some("auto"));
        assert!(matches!(apply(&mut c, "display_mode", &json!("mirror")), Ok(Effect::Hot(ConfigUpdate::DisplayMode(_)))));
        assert_eq!(c.display_mode.as_deref(), Some("mirror"));
        assert!(apply(&mut c, "display_mode", &json!("tile")).is_err());
        assert!(matches!(apply(&mut c, "show_cursor", &json!(false)), Ok(Effect::Hot(ConfigUpdate::ShowCursor(false)))));
        assert!(apply(&mut c, "show_cursor", &json!(0)).is_err());
        assert!(matches!(apply(&mut c, "log_level", &json!("debug")), Ok(Effect::Hot(ConfigUpdate::LogLevel(_)))));
        assert!(apply(&mut c, "log_level", &json!("loud")).is_err());
    }

    #[test]
    fn curtain_standby_keys_are_booleans_applied_hot() {
        let mut c = ServerConfig::default();
        assert_eq!(c.curtain_standby_ok, None, "untested until the app's test says otherwise");
        assert!(!c.curtain_standby_keepalive);
        assert!(matches!(apply(&mut c, "curtain_standby_ok", &json!(true)), Ok(Effect::Hot(ConfigUpdate::CurtainStandbyOk(true)))));
        assert_eq!(c.curtain_standby_ok, Some(true));
        assert!(apply(&mut c, "curtain_standby_ok", &json!("yes")).is_err());
        assert!(apply(&mut c, "curtain_standby_ok", &json!(null)).is_err());
        assert!(matches!(
            apply(&mut c, "curtain_standby_keepalive", &json!(true)),
            Ok(Effect::Hot(ConfigUpdate::CurtainStandbyKeepalive(true)))
        ));
        assert!(c.curtain_standby_keepalive);
        assert!(apply(&mut c, "curtain_standby_keepalive", &json!(1)).is_err());
    }

    #[test]
    fn restart_keys_apply_and_validate() {
        let mut c = ServerConfig::default();
        assert!(matches!(apply(&mut c, "port", &json!(3390)), Ok(Effect::Restart)));
        assert_eq!(c.port, 3390);
        for bad in [json!(80), json!(65536), json!(-1), json!("3390")] {
            assert!(apply(&mut c, "port", &bad).is_err(), "{bad}");
        }
        assert!(matches!(apply(&mut c, "bind_address", &json!("10.0.0.2")), Ok(Effect::Restart)));
        assert!(apply(&mut c, "bind_address", &json!("example.com")).is_err());
        assert!(matches!(apply(&mut c, "idle_timeout_secs", &json!(0)), Ok(Effect::Restart)));
        assert!(apply(&mut c, "idle_timeout_secs", &json!(-5)).is_err());
        assert!(apply(&mut c, "idle_timeout_secs", &json!(1.5)).is_err());
        assert!(matches!(apply(&mut c, "lock_on_disconnect", &json!(true)), Ok(Effect::Restart)));
        assert!(c.lock_on_disconnect);
        assert!(!c.auto_unlock, "off unless asked for");
        assert!(matches!(apply(&mut c, "auto_unlock", &json!(true)), Ok(Effect::Restart)));
        assert!(c.auto_unlock);
        assert!(apply(&mut c, "auto_unlock", &json!("true")).is_err());
        assert!(matches!(apply(&mut c, "audio.enabled", &json!(false)), Ok(Effect::Restart)));
        assert!(!c.audio.enabled);
        assert!(matches!(apply(&mut c, "clipboard.enabled", &json!(false)), Ok(Effect::Restart)));
        assert!(!c.clipboard.enabled);
        assert!(apply(&mut c, "cert_path", &json!("/tmp/x")).is_err(), "unknown keys are refused");
        assert_eq!(c.port, 3390, "a failed apply leaves the config unchanged");
    }
}
