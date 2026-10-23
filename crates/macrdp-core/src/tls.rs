//! TLS certificate generation and management

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::config_dir;

/// Validity of a generated certificate.
const VALID_DAYS: i64 = 365;
/// Regenerate when the certificate expires within this many days.
const RENEW_BEFORE_DAYS: i64 = 30;

/// Generate a self-signed certificate and write PEM files.
///
/// SANs cover localhost, the host name, `<host>.local` and every LAN or WireGuard IP.
/// The key file is created with mode 0600.
pub fn generate_self_signed_cert(cert_path: &Path, key_path: &Path) -> Result<()> {
    generate_with_validity(cert_path, key_path, VALID_DAYS)
}

fn generate_with_validity(cert_path: &Path, key_path: &Path, days: i64) -> Result<()> {
    let mut params = CertificateParams::new(subject_names())?;
    let mut dn = DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "macrdp");
    dn.push(rcgen::DnType::OrganizationName, "macrdp");
    params.distinguished_name = dn;

    use chrono::Datelike;
    let ymd = |d: chrono::NaiveDate| rcgen::date_time_ymd(d.year(), d.month() as u8, d.day() as u8);
    let today = chrono::Utc::now().date_naive();
    params.not_before = ymd(today - chrono::Days::new(1));
    params.not_after = ymd(today + chrono::Days::new(days.unsigned_abs()));

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_private(key_path, key_pair.serialize_pem().as_bytes())?;
    fs::write(cert_path, cert.pem())?;

    tracing::info!(?cert_path, ?key_path, "Generated self-signed TLS certificate");
    Ok(())
}

/// Writes `data` to a file that is 0600 from the moment it exists.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("Cannot write {}", path.display()))?;
    // An older key file may predate the 0600 rule.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(data)?;
    Ok(())
}

/// DNS names and IP addresses a client may use to reach this Mac.
fn subject_names() -> Vec<String> {
    let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    for host in crate::auth::host_names() {
        let short = host.split('.').next().unwrap_or_default().to_string();
        names.push(host);
        names.push(format!("{short}.local"));
    }
    names.extend(interface_addresses().iter().map(ToString::to_string));
    let mut seen = std::collections::HashSet::new();
    names.retain(|n| !n.is_empty() && seen.insert(n.to_lowercase()));
    names
}

/// Non-loopback, non-link-local addresses of all interfaces (LAN, WireGuard utun).
fn interface_addresses() -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let mut addrs = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a list that is freed below with freeifaddrs.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return addrs;
    }
    let mut cur = list;
    while !cur.is_null() {
        // SAFETY: `cur` walks the list returned by getifaddrs; each sockaddr is sized for
        // its family.
        unsafe {
            let sa = (*cur).ifa_addr;
            // Down interfaces (a stopped WireGuard tunnel) are unreachable.
            let up = (*cur).ifa_flags & libc::IFF_UP as u32 != 0;
            if up && !sa.is_null() {
                match i32::from((*sa).sa_family) {
                    libc::AF_INET => {
                        let sin = &*(sa as *const libc::sockaddr_in);
                        addrs.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))));
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*(sa as *const libc::sockaddr_in6);
                        addrs.push(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            cur = (*cur).ifa_next;
        }
    }
    // SAFETY: `list` came from getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(list) };
    addrs.retain(|ip| match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified() && (v6.segments()[0] & 0xffc0) != 0xfe80,
    });
    addrs.dedup();
    addrs
}

/// IPv4 addresses of up, non-loopback interfaces (LAN, WireGuard utun), for display.
pub fn ipv4_addresses() -> Vec<String> {
    interface_addresses().into_iter().filter(std::net::IpAddr::is_ipv4).map(|ip| ip.to_string()).collect()
}

/// Ensure TLS cert and key files exist. Generate if missing or expiring.
pub fn ensure_tls_files(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
) -> Result<(PathBuf, PathBuf)> {
    let tls_dir = config_dir().join("tls");
    let cert = cert_path
        .map(PathBuf::from)
        .unwrap_or_else(|| tls_dir.join("cert.pem"));
    let key = key_path
        .map(PathBuf::from)
        .unwrap_or_else(|| tls_dir.join("key.pem"));

    let renew = if !cert.exists() || !key.exists() {
        true
    } else {
        match read_cert(&cert) {
            Ok(parsed) if days_left(&parsed) > RENEW_BEFORE_DAYS => false,
            Ok(_) => {
                tracing::info!("TLS certificate expires within {RENEW_BEFORE_DAYS} days, regenerating");
                true
            }
            Err(e) => {
                tracing::warn!("Unreadable TLS certificate ({e:#}), regenerating");
                true
            }
        }
    };
    if renew {
        generate_self_signed_cert(&cert, &key).context("Failed to generate self-signed certificate")?;
    }

    Ok((cert, key))
}

fn read_cert(path: &Path) -> Result<x509_cert::Certificate> {
    use x509_cert::der::DecodePem;
    let pem = fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?;
    x509_cert::Certificate::from_pem(pem).context("Invalid certificate PEM")
}

fn days_left(cert: &x509_cert::Certificate) -> i64 {
    let not_after = cert.tbs_certificate.validity.not_after.to_unix_duration().as_secs();
    let now = chrono::Utc::now().timestamp();
    (i64::try_from(not_after).unwrap_or(i64::MAX) - now) / 86_400
}

/// SHA-256 fingerprint of the certificate, as `AB:CD:...`, to compare with the client prompt.
pub fn cert_fingerprint(cert_path: &Path) -> Result<String> {
    use x509_cert::der::Encode;
    let der = read_cert(cert_path)?.to_der()?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &der);
    Ok(digest.as_ref().iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn ipv4_addresses_are_routable_ipv4_only() {
        for addr in ipv4_addresses() {
            let ip: std::net::Ipv4Addr = addr.parse().unwrap();
            assert!(!ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified(), "{ip}");
        }
    }

    fn paths(dir: &TempDir) -> (PathBuf, PathBuf) {
        (dir.path().join("cert.pem"), dir.path().join("key.pem"))
    }

    fn subject_alt_names(cert: &x509_cert::Certificate) -> Vec<String> {
        use x509_cert::der::Decode;
        use x509_cert::ext::pkix::name::GeneralName;
        use x509_cert::ext::pkix::SubjectAltName;
        let ext = cert
            .tbs_certificate
            .extensions
            .iter()
            .flatten()
            .find(|e| e.extn_id == const_oid::db::rfc5280::ID_CE_SUBJECT_ALT_NAME)
            .unwrap();
        SubjectAltName::from_der(ext.extn_value.as_bytes())
            .unwrap()
            .0
            .into_iter()
            .map(|n| match n {
                GeneralName::DnsName(d) => d.to_string(),
                GeneralName::IpAddress(ip) => {
                    let b = ip.as_bytes();
                    if b.len() == 4 {
                        std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string()
                    } else {
                        format!("{b:?}")
                    }
                }
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn test_generate_self_signed_cert() {
        let dir = TempDir::new().unwrap();
        let (cert_path, key_path) = paths(&dir);

        generate_self_signed_cert(&cert_path, &key_path).unwrap();

        let cert_content = fs::read_to_string(&cert_path).unwrap();
        assert!(cert_content.contains("BEGIN CERTIFICATE"));
        let key_content = fs::read_to_string(&key_path).unwrap();
        assert!(key_content.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn key_file_is_owner_only() {
        let dir = TempDir::new().unwrap();
        let (cert_path, key_path) = paths(&dir);
        generate_self_signed_cert(&cert_path, &key_path).unwrap();
        let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn cert_names_this_mac() {
        let dir = TempDir::new().unwrap();
        let (cert_path, key_path) = paths(&dir);
        generate_self_signed_cert(&cert_path, &key_path).unwrap();
        let names = subject_alt_names(&read_cert(&cert_path).unwrap());
        assert!(names.contains(&"localhost".to_string()));
        assert!(names.contains(&"127.0.0.1".to_string()));
        let host = crate::auth::host_names().into_iter().next().unwrap();
        let short = host.split('.').next().unwrap().to_string();
        assert!(names.contains(&format!("{short}.local")), "{names:?}");
    }

    #[test]
    fn cert_is_valid_for_about_a_year() {
        let dir = TempDir::new().unwrap();
        let (cert_path, key_path) = paths(&dir);
        generate_self_signed_cert(&cert_path, &key_path).unwrap();
        let days = days_left(&read_cert(&cert_path).unwrap());
        assert!((360..=366).contains(&days), "{days}");
    }

    #[test]
    fn valid_cert_is_kept() {
        let dir = TempDir::new().unwrap();
        let (cert_path, key_path) = paths(&dir);
        ensure_tls_files(Some(&cert_path), Some(&key_path)).unwrap();
        let first = cert_fingerprint(&cert_path).unwrap();
        ensure_tls_files(Some(&cert_path), Some(&key_path)).unwrap();
        assert_eq!(cert_fingerprint(&cert_path).unwrap(), first);
    }

    #[test]
    fn expiring_cert_is_regenerated() {
        let dir = TempDir::new().unwrap();
        let (cert_path, key_path) = paths(&dir);
        generate_with_validity(&cert_path, &key_path, 10).unwrap();
        let old = cert_fingerprint(&cert_path).unwrap();
        ensure_tls_files(Some(&cert_path), Some(&key_path)).unwrap();
        assert_ne!(cert_fingerprint(&cert_path).unwrap(), old);
        assert!(days_left(&read_cert(&cert_path).unwrap()) > RENEW_BEFORE_DAYS);
    }

    #[test]
    fn fingerprint_is_colon_separated_sha256() {
        let dir = TempDir::new().unwrap();
        let (cert_path, key_path) = paths(&dir);
        generate_self_signed_cert(&cert_path, &key_path).unwrap();
        let fp = cert_fingerprint(&cert_path).unwrap();
        assert_eq!(fp.len(), 32 * 3 - 1);
        assert!(fp.split(':').all(|b| b.len() == 2 && b.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())));
    }
}
