//! PKI and certificate checking utilities
//!
//! Parses X.509 certificates to check expiry dates and provide warnings.

use chrono::{DateTime, Utc};
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
}

impl CertificateInfo {
    /// Calculate status based on days remaining
    fn status_from_days(days: i64) -> CertStatus {
        match days {
            d if d <= 0 => CertStatus::Expired,
            d if d <= 7 => CertStatus::Critical,
            d if d <= 30 => CertStatus::Warning,
            _ => CertStatus::Valid,
        }
    }

    /// Format time remaining in human-readable form
    fn format_time_remaining(days: i64) -> String {
        if days <= 0 {
            let abs_days = days.abs();
            if abs_days == 1 {
                "expired 1 day ago".to_string()
            } else {
                format!("expired {} days ago", abs_days)
            }
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

/// Parse a PEM-encoded certificate and extract info
pub fn parse_certificate(name: &str, pem_data: &[u8]) -> Result<CertificateInfo, String> {
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

    // Calculate days remaining
    let now = Utc::now();
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
        time_remaining: CertificateInfo::format_time_remaining(days_remaining),
        status: CertificateInfo::status_from_days(days_remaining),
        is_ca,
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
    /// Get the most urgent certificate (closest to expiry or already expired)
    pub fn most_urgent(&self) -> Option<&CertificateInfo> {
        let all_certs: Vec<&CertificateInfo> =
            self.cas.iter().chain(self.client_certs.iter()).collect();

        all_certs.into_iter().min_by_key(|c| c.days_remaining)
    }

    /// Check if any certificate is in warning or worse status
    pub fn has_warnings(&self) -> bool {
        self.cas.iter().chain(self.client_certs.iter()).any(|c| {
            matches!(
                c.status,
                CertStatus::Warning | CertStatus::Critical | CertStatus::Expired
            )
        })
    }

    /// Check if any certificate has expired
    pub fn has_expired(&self) -> bool {
        self.cas
            .iter()
            .chain(self.client_certs.iter())
            .any(|c| c.status == CertStatus::Expired)
    }

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
    /// Get encryption for STATE partition
    pub fn state_encryption(&self) -> Option<&VolumeEncryption> {
        self.volumes.iter().find(|v| v.name == "STATE")
    }

    /// Get encryption for EPHEMERAL partition
    pub fn ephemeral_encryption(&self) -> Option<&VolumeEncryption> {
        self.volumes.iter().find(|v| v.name == "EPHEMERAL")
    }

    /// Check if any volume is encrypted with strong encryption
    pub fn has_strong_encryption(&self) -> bool {
        self.volumes.iter().any(|v| {
            matches!(
                v.provider,
                EncryptionProvider::Tpm | EncryptionProvider::Kms
            )
        })
    }

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

    #[test]
    fn test_format_time_remaining() {
        assert_eq!(CertificateInfo::format_time_remaining(1), "1 day");
        assert_eq!(CertificateInfo::format_time_remaining(30), "30 days");
        assert_eq!(CertificateInfo::format_time_remaining(45), "45 days");
        assert_eq!(CertificateInfo::format_time_remaining(60), "2m");
        assert_eq!(CertificateInfo::format_time_remaining(90), "3m");
        assert_eq!(CertificateInfo::format_time_remaining(365), "1y");
        assert_eq!(CertificateInfo::format_time_remaining(400), "1y 1m");
        assert_eq!(CertificateInfo::format_time_remaining(3650), "10y");
        assert_eq!(
            CertificateInfo::format_time_remaining(-1),
            "expired 1 day ago"
        );
        assert_eq!(
            CertificateInfo::format_time_remaining(-30),
            "expired 30 days ago"
        );
    }

    #[test]
    fn test_status_from_days() {
        assert_eq!(CertificateInfo::status_from_days(365), CertStatus::Valid);
        assert_eq!(CertificateInfo::status_from_days(31), CertStatus::Valid);
        assert_eq!(CertificateInfo::status_from_days(30), CertStatus::Warning);
        assert_eq!(CertificateInfo::status_from_days(15), CertStatus::Warning);
        assert_eq!(CertificateInfo::status_from_days(7), CertStatus::Critical);
        assert_eq!(CertificateInfo::status_from_days(1), CertStatus::Critical);
        assert_eq!(CertificateInfo::status_from_days(0), CertStatus::Expired);
        assert_eq!(CertificateInfo::status_from_days(-1), CertStatus::Expired);
    }
}
