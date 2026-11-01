//! PKI and certificate checking utilities
//!
//! Parses X.509 certificates to check expiry dates and provide warnings.

use chrono::{DateTime, Utc};
use talos9s_core::constants::{CERT_CRITICAL_DAYS, CERT_WARN_DAYS};
use x509_parser::prelude::*;

/// Status of a certificate based on expiry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertStatus {
    /// Certificate is valid with plenty of time remaining
    Valid,
    /// Certificate expires within 30 days
    Warning,
    /// Certificate expires within 7 days
    Critical,
    /// Certificate has expired
    Expired,
}

impl CertStatus {
    /// Get display indicator and color
    pub fn indicator(&self) -> (&'static str, ratatui::style::Color) {
        use ratatui::style::Color;
        match self {
            CertStatus::Valid => ("●", Color::Green),
            CertStatus::Warning => ("⚠", Color::Yellow),
            CertStatus::Critical => ("!", Color::Red),
            CertStatus::Expired => ("✗", Color::Red),
        }
    }

    /// Get status label
    pub fn label(&self) -> &'static str {
        match self {
            CertStatus::Valid => "Valid",
            CertStatus::Warning => "Expiring Soon",
            CertStatus::Critical => "Expiring Very Soon",
            CertStatus::Expired => "Expired",
        }
    }
}

/// Information about a parsed certificate
#[derive(Debug, Clone)]
pub struct CertificateInfo {
    /// Human-readable name for this certificate
    pub name: String,
    /// Subject (CN or full subject)
    pub subject: String,
    /// Issuer (CN or full issuer)
    pub issuer: String,
    /// Not valid before
    pub not_before: DateTime<Utc>,
    /// Not valid after (expiry)
    pub not_after: DateTime<Utc>,
    /// Days remaining until expiry (negative if expired)
    pub days_remaining: i64,
    /// Formatted time remaining (e.g., "9y 11m" or "45 days")
    pub time_remaining: String,
    /// Certificate status based on expiry
    pub status: CertStatus,
    /// Whether this is a CA certificate
    pub is_ca: bool,
    /// Subject organizations (O=): Talos puts the client's role there
    /// (`os:admin`, `os:reader`, ...)
    pub organizations: Vec<String>,
}

impl CertificateInfo {
    /// Status at `now`: expired only once `not_after` has passed (a
    /// certificate with 23 hours left has 0 whole days but is valid)
    fn status_at(not_after: DateTime<Utc>, now: DateTime<Utc>) -> CertStatus {
        if not_after <= now {
            return CertStatus::Expired;
        }
        match not_after.signed_duration_since(now).num_days() {
            d if d <= CERT_CRITICAL_DAYS => CertStatus::Critical,
            d if d <= CERT_WARN_DAYS => CertStatus::Warning,
            _ => CertStatus::Valid,
        }
    }

    /// One line for the expiry: "EXPIRED 5 days ago" or "expires in 2m 3d"
    pub fn expiry_text(&self) -> String {
        if self.status == CertStatus::Expired {
            format!("EXPIRED {}", self.time_remaining)
        } else {
            format!("expires in {}", self.time_remaining)
        }
    }

    /// Format the time to `not_after` in human-readable form: "5 days",
    /// "under 1 day", or for an expired one "5 days ago", "today"
    fn format_time_remaining(not_after: DateTime<Utc>, now: DateTime<Utc>) -> String {
        if not_after <= now {
            return match now.signed_duration_since(not_after).num_days() {
                0 => "today".to_string(),
                1 => "1 day ago".to_string(),
                d => format!("{d} days ago"),
            };
        }
        let days = not_after.signed_duration_since(now).num_days();
        if days == 0 {
            "under 1 day".to_string()
        } else if days < 60 {
            if days == 1 {
                "1 day".to_string()
            } else {
                format!("{} days", days)
            }
        } else if days < 365 {
            let months = days / 30;
            let remaining_days = days % 30;
            if remaining_days > 0 {
                format!("{}m {}d", months, remaining_days)
            } else {
                format!("{}m", months)
            }
        } else {
            let years = days / 365;
            let remaining_months = (days % 365) / 30;
            if remaining_months > 0 {
                format!("{}y {}m", years, remaining_months)
            } else {
                format!("{}y", years)
            }
        }
    }
}

/// The base64 certificates of a kubeconfig: every cluster's CA and the
/// first user's client certificate. The client key is never read.
#[derive(Debug, Default, PartialEq)]
pub struct KubeconfigCerts {
    pub cas: Vec<String>,
    pub client: Option<String>,
}

pub fn kubeconfig_certs(yaml: &str) -> Result<KubeconfigCerts, String> {
    let kc = kube::config::Kubeconfig::from_yaml(yaml).map_err(|e| e.to_string())?;
    Ok(KubeconfigCerts {
        cas: kc
            .clusters
            .iter()
            .filter_map(|c| c.cluster.as_ref()?.certificate_authority_data.clone())
            .collect(),
        client: kc
            .auth_infos
            .iter()
            .find_map(|a| a.auth_info.as_ref()?.client_certificate_data.clone()),
    })
}

/// How long reading the kubeconfig from Talos may take
pub const KUBECONFIG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Parse a PEM-encoded certificate and extract info
pub fn parse_certificate(name: &str, pem_data: &[u8]) -> Result<CertificateInfo, String> {
    parse_certificate_at(name, pem_data, Utc::now())
}

/// [`parse_certificate`] with the time to judge expiry at
pub fn parse_certificate_at(
    name: &str,
    pem_data: &[u8],
    now: DateTime<Utc>,
) -> Result<CertificateInfo, String> {
    // Parse PEM to get DER
    let pem = parse_x509_pem(pem_data)
        .map_err(|e| format!("Failed to parse PEM: {}", e))?
        .1;

    // Parse X.509 certificate
    let cert = pem
        .parse_x509()
        .map_err(|e| format!("Failed to parse X.509: {}", e))?;

    // Extract subject CN or full subject
    let subject = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| cert.subject().to_string());

    let organizations = cert
        .subject()
        .iter_organization()
        .filter_map(|o| o.as_str().ok().map(str::to_string))
        .collect();

    // Extract issuer CN or full issuer
    let issuer = cert
        .issuer()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| cert.issuer().to_string());

    // Extract validity dates
    let not_before = cert.validity().not_before.to_datetime();
    let not_after = cert.validity().not_after.to_datetime();

    // Convert to chrono DateTime
    let not_before_chrono =
        DateTime::from_timestamp(not_before.unix_timestamp(), 0).unwrap_or_else(Utc::now);
    let not_after_chrono =
        DateTime::from_timestamp(not_after.unix_timestamp(), 0).unwrap_or_else(Utc::now);

    let duration = not_after_chrono.signed_duration_since(now);
    let days_remaining = duration.num_days();

    // Check if this is a CA certificate
    let is_ca = cert
        .basic_constraints()
        .ok()
        .flatten()
        .map(|bc| bc.value.ca)
        .unwrap_or(false);

    Ok(CertificateInfo {
        name: name.to_string(),
        subject,
        issuer,
        not_before: not_before_chrono,
        not_after: not_after_chrono,
        days_remaining,
        time_remaining: CertificateInfo::format_time_remaining(not_after_chrono, now),
        status: CertificateInfo::status_at(not_after_chrono, now),
        is_ca,
        organizations,
    })
}

/// Parse a base64-encoded PEM certificate (as stored in talosconfig)
pub fn parse_base64_certificate(name: &str, base64_data: &str) -> Result<CertificateInfo, String> {
    use base64::Engine;
    let pem_data = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .map_err(|e| format!("Failed to decode base64: {}", e))?;
    parse_certificate(name, &pem_data)
}

/// Information about all certificates in the system
#[derive(Debug, Clone, Default)]
pub struct PkiStatus {
    /// Certificate authorities
    pub cas: Vec<CertificateInfo>,
    /// Client certificates
    pub client_certs: Vec<CertificateInfo>,
    /// RBAC role from talosconfig (e.g., "os:admin")
    pub rbac_role: Option<String>,
    /// Whether RBAC is enabled
    pub rbac_enabled: bool,
    /// Error message if certificate parsing failed
    pub error: Option<String>,
}

impl PkiStatus {
    /// Get summary counts
    pub fn summary(&self) -> (usize, usize, usize, usize) {
        let all_certs: Vec<&CertificateInfo> =
            self.cas.iter().chain(self.client_certs.iter()).collect();

        let valid = all_certs
            .iter()
            .filter(|c| c.status == CertStatus::Valid)
            .count();
        let warning = all_certs
            .iter()
            .filter(|c| c.status == CertStatus::Warning)
            .count();
        let critical = all_certs
            .iter()
            .filter(|c| c.status == CertStatus::Critical)
            .count();
        let expired = all_certs
            .iter()
            .filter(|c| c.status == CertStatus::Expired)
            .count();

        (valid, warning, critical, expired)
    }
}

/// Disk encryption provider type
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncryptionProvider {
    /// No encryption
    None,
    /// Static passphrase (weak - stored in META or config)
    Static,
    /// Node ID based (weak - derived from UUID)
    NodeID,
    /// TPM-sealed (strong - requires SecureBoot)
    Tpm,
    /// KMS-sealed (strong - requires network)
    Kms,
    /// Unknown provider
    Unknown(String),
}

impl EncryptionProvider {
    /// Get display name
    pub fn name(&self) -> &str {
        match self {
            EncryptionProvider::None => "None",
            EncryptionProvider::Static => "static",
            EncryptionProvider::NodeID => "nodeID",
            EncryptionProvider::Tpm => "tpm",
            EncryptionProvider::Kms => "kms",
            EncryptionProvider::Unknown(s) => s,
        }
    }

    /// Get strength assessment
    pub fn strength(&self) -> &'static str {
        match self {
            EncryptionProvider::None => "unencrypted",
            EncryptionProvider::Static | EncryptionProvider::NodeID => "weak",
            EncryptionProvider::Tpm | EncryptionProvider::Kms => "strong",
            EncryptionProvider::Unknown(_) => "unknown",
        }
    }

    /// Get indicator color
    pub fn indicator(&self) -> (&'static str, ratatui::style::Color) {
        use ratatui::style::Color;
        match self {
            EncryptionProvider::None => ("○", Color::DarkGray),
            EncryptionProvider::Static | EncryptionProvider::NodeID => ("◐", Color::Yellow),
            EncryptionProvider::Tpm | EncryptionProvider::Kms => ("●", Color::Green),
            EncryptionProvider::Unknown(_) => ("?", Color::DarkGray),
        }
    }
}

/// Encryption status for a volume
#[derive(Debug, Clone)]
pub struct VolumeEncryption {
    /// Volume name (e.g., "STATE", "EPHEMERAL")
    pub name: String,
    /// Encryption provider
    pub provider: EncryptionProvider,
}

/// Full encryption status for the node
#[derive(Debug, Clone, Default)]
pub struct EncryptionStatus {
    /// Per-volume encryption status
    pub volumes: Vec<VolumeEncryption>,
}

impl EncryptionStatus {
    /// Get summary string
    pub fn summary(&self) -> String {
        if self.volumes.is_empty() {
            return "Unknown".to_string();
        }

        let parts: Vec<String> = self
            .volumes
            .iter()
            .map(|v| format!("{}: {}", v.name, v.provider.name()))
            .collect();

        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn in_days(days: i64) -> DateTime<Utc> {
        now() + Duration::days(days)
    }

    #[test]
    fn kubeconfig_certs_without_the_key() {
        // Placeholders only: no real certificate or key
        let yaml = "apiVersion: v1
kind: Config
clusters:
- name: bc
  cluster:
    server: https://10.0.0.1:6443
    certificate-authority-data: Q0EtREFUQQ==
contexts:
- name: admin@bc
  context: {cluster: bc, user: admin@bc}
current-context: admin@bc
users:
- name: admin@bc
  user:
    client-certificate-data: Q0VSVA==
    client-key-data: S0VZ
";
        assert_eq!(
            kubeconfig_certs(yaml).unwrap(),
            KubeconfigCerts {
                cas: vec!["Q0EtREFUQQ==".to_string()],
                client: Some("Q0VSVA==".to_string()),
            }
        );
        assert!(kubeconfig_certs("not: [valid").is_err());
    }

    #[test]
    fn test_format_time_remaining() {
        let fmt = |t| CertificateInfo::format_time_remaining(t, now());
        assert_eq!(fmt(in_days(1)), "1 day");
        assert_eq!(fmt(in_days(30)), "30 days");
        assert_eq!(fmt(in_days(45)), "45 days");
        assert_eq!(fmt(in_days(60)), "2m");
        assert_eq!(fmt(in_days(90)), "3m");
        assert_eq!(fmt(in_days(365)), "1y");
        assert_eq!(fmt(in_days(400)), "1y 1m");
        assert_eq!(fmt(in_days(3650)), "10y");
        assert_eq!(fmt(now() + Duration::hours(23)), "under 1 day");
        assert_eq!(fmt(now() - Duration::hours(3)), "today");
        assert_eq!(fmt(in_days(-1)), "1 day ago");
        assert_eq!(fmt(in_days(-30)), "30 days ago");
    }

    #[test]
    fn test_status_at() {
        let status = |t| CertificateInfo::status_at(t, now());
        assert_eq!(status(in_days(365)), CertStatus::Valid);
        assert_eq!(status(in_days(31)), CertStatus::Valid);
        assert_eq!(status(in_days(30)), CertStatus::Warning);
        assert_eq!(status(in_days(15)), CertStatus::Warning);
        assert_eq!(status(in_days(7)), CertStatus::Critical);
        assert_eq!(status(in_days(1)), CertStatus::Critical);
        // 23 hours left: 0 whole days, but not expired
        assert_eq!(status(now() + Duration::hours(23)), CertStatus::Critical);
        assert_eq!(status(now()), CertStatus::Expired);
        assert_eq!(status(in_days(-5)), CertStatus::Expired);
    }

    #[test]
    fn expiry_text_says_ago_once() {
        let cert = |not_after: DateTime<Utc>| CertificateInfo {
            name: "talosconfig".into(),
            subject: String::new(),
            issuer: String::new(),
            not_before: now(),
            not_after,
            days_remaining: not_after.signed_duration_since(now()).num_days(),
            time_remaining: CertificateInfo::format_time_remaining(not_after, now()),
            status: CertificateInfo::status_at(not_after, now()),
            is_ca: false,
            organizations: Vec::new(),
        };
        assert_eq!(cert(in_days(-5)).expiry_text(), "EXPIRED 5 days ago");
        assert_eq!(cert(in_days(45)).expiry_text(), "expires in 45 days");
    }
}
