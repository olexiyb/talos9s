//! Network analysis utilities
//!
//! Provides port-to-service mapping and network connection analysis
//! for Talos Linux and Kubernetes clusters.

/// Well-known service information
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServicePort {
    /// Port number
    pub port: u16,
    /// Service name
    pub name: &'static str,
    /// Short description
    pub description: &'static str,
    /// Whether this is a Talos-specific service
    pub is_talos: bool,
    /// Whether this is a Kubernetes control plane service
    pub is_controlplane: bool,
}

impl ServicePort {
    const fn new(
        port: u16,
        name: &'static str,
        description: &'static str,
        is_talos: bool,
        is_controlplane: bool,
    ) -> Self {
        Self {
            port,
            name,
            description,
            is_talos,
            is_controlplane,
        }
    }
}

/// Well-known Talos and Kubernetes service ports
pub const KNOWN_PORTS: &[ServicePort] = &[
    // Talos services
    ServicePort::new(50000, "apid", "Talos API daemon", true, false),
    ServicePort::new(50001, "trustd", "Talos trust daemon", true, false),
    ServicePort::new(51821, "kubernetesd", "Kubernetes daemon", true, true),
    // etcd
    ServicePort::new(2379, "etcd-client", "etcd client API", false, true),
    ServicePort::new(2380, "etcd-peer", "etcd peer communication", false, true),
    // Kubernetes control plane
    ServicePort::new(6443, "kube-apiserver", "Kubernetes API server", false, true),
    ServicePort::new(10250, "kubelet", "Kubelet API", false, false),
    ServicePort::new(10259, "kube-scheduler", "Kubernetes scheduler", false, true),
    ServicePort::new(
        10257,
        "kube-controller-manager",
        "Controller manager",
        false,
        true,
    ),
    // Kubernetes networking
    ServicePort::new(10256, "kube-proxy", "Kubernetes proxy", false, false),
    ServicePort::new(8472, "flannel-vxlan", "Flannel VXLAN overlay", false, false),
    ServicePort::new(4240, "cilium-health", "Cilium health check", false, false),
    ServicePort::new(4244, "cilium-hubble", "Cilium Hubble relay", false, false),
    // Common services
    ServicePort::new(53, "dns", "DNS", false, false),
    ServicePort::new(443, "https", "HTTPS", false, false),
    ServicePort::new(80, "http", "HTTP", false, false),
];

/// Get service name for a port
///
/// Returns the service name if the port is a well-known Talos/K8s port.
///
/// # Examples
///
/// ```
/// use talos9s_core::network::port_to_service;
///
/// assert_eq!(port_to_service(6443), Some("kube-apiserver"));
/// assert_eq!(port_to_service(50000), Some("apid"));
/// assert_eq!(port_to_service(12345), None);
/// ```
pub fn port_to_service(port: u16) -> Option<&'static str> {
    KNOWN_PORTS
        .iter()
        .find(|sp| sp.port == port)
        .map(|sp| sp.name)
}

/// Get service name for a u32 port (for compatibility)
pub fn port_to_service_u32(port: u32) -> Option<&'static str> {
    if port > u16::MAX as u32 {
        return None;
    }
    port_to_service(port as u16)
}

/// `/etc/resolv.conf` for display: nameservers, search domains and options,
/// then the raw file
pub fn resolv_conf_lines(content: &str) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push("─ DNS Configuration ─".to_string());
    lines.push(String::new());

    let mut nameservers = Vec::new();
    let mut search_domains = Vec::new();
    let mut options = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        if let Some(ns) = line.strip_prefix("nameserver ") {
            nameservers.push(ns.trim().to_string());
        } else if let Some(search) = line.strip_prefix("search ") {
            search_domains.extend(search.split_whitespace().map(String::from));
        } else if let Some(opt) = line.strip_prefix("options ") {
            options.extend(opt.split_whitespace().map(String::from));
        }
    }

    if !nameservers.is_empty() {
        lines.push("Nameservers:".to_string());
        for ns in &nameservers {
            lines.push(format!("  • {ns}"));
        }
        lines.push(String::new());
    }

    if !search_domains.is_empty() {
        lines.push("Search Domains:".to_string());
        for domain in &search_domains {
            lines.push(format!("  • {domain}"));
        }
        lines.push(String::new());
    }

    if !options.is_empty() {
        lines.push("Options:".to_string());
        for opt in &options {
            lines.push(format!("  • {opt}"));
        }
        lines.push(String::new());
    }

    lines.push("─ Raw Content ─".to_string());
    for line in content.lines() {
        lines.push(format!("  {line}"));
    }

    lines
}

/// `/proc/net/route` for display, one row per route, sorted
pub fn route_table_lines(content: &str) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push("─ Routing Table ─".to_string());
    lines.push(String::new());

    // Header
    lines.push(format!(
        "{:<18} {:<18} {:<18} {:>6} {:>6} {:>6} {:<10}",
        "Destination", "Gateway", "Genmask", "Flags", "Metric", "Ref", "Iface"
    ));
    lines.push("─".repeat(90));

    let mut route_lines: Vec<&str> = content.lines().skip(1).collect(); // Skip header
    route_lines.sort(); // Sort for consistent display

    for line in route_lines {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let [
            iface,
            destination,
            gateway,
            flags,
            _refcnt,
            _use,
            metric,
            mask,
            ..,
        ] = parts[..]
        {
            let destination = route_hex_to_ip(destination);
            let gateway = route_hex_to_ip(gateway);
            let mask = route_hex_to_ip(mask);

            // Decode flags
            let flag_val = u32::from_str_radix(flags, 16).unwrap_or(0);
            let flag_str = route_flags(flag_val);

            lines.push(format!(
                "{:<18} {:<18} {:<18} {:>6} {:>6} {:>6} {:<10}",
                destination, gateway, mask, flag_str, metric, "0", iface
            ));
        }
    }

    lines.push(String::new());
    lines.push("Flags: U=Up, G=Gateway, H=Host".to_string());

    lines
}

/// Convert hex IP to dotted decimal
fn route_hex_to_ip(hex: &str) -> String {
    // /proc/net/route holds addresses in host (little-endian) order
    match u32::from_str_radix(hex, 16) {
        Ok(val) => std::net::Ipv4Addr::from(val.to_le_bytes()).to_string(),
        Err(_) => hex.to_string(),
    }
}

/// Route flags as `U`/`G`/`H` letters, `-` for none
fn route_flags(flags: u32) -> String {
    let mut s = String::new();
    if flags & 0x0001 != 0 {
        s.push('U');
    } // RTF_UP
    if flags & 0x0002 != 0 {
        s.push('G');
    } // RTF_GATEWAY
    if flags & 0x0004 != 0 {
        s.push('H');
    } // RTF_HOST
    if s.is_empty() {
        s.push('-');
    }
    s
}

/// Talos' default pod network (`cluster.network.podSubnets`), for when the
/// node's config can't be read
pub const DEFAULT_POD_SUBNET: &str = "10.244.0.0/16";

/// Whether `ip` is inside `cidr` (`10.44.0.0/16`, `fd00::/64`); false for
/// anything that doesn't parse or mixes IPv4 and IPv6
pub fn cidr_contains(cidr: &str, ip: &str) -> bool {
    use std::net::IpAddr;
    let Some((net, len)) = cidr.split_once('/') else {
        return false;
    };
    let (Ok(net), Ok(len), Ok(ip)) = (
        net.parse::<IpAddr>(),
        len.parse::<u32>(),
        ip.parse::<IpAddr>(),
    ) else {
        return false;
    };
    let masked = |bits: u128, width: u32| {
        if len == 0 { 0 } else { bits >> (width - len) }
    };
    match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(i)) if len <= 32 => {
            masked(u32::from(n).into(), 32) == masked(u32::from(i).into(), 32)
        }
        (IpAddr::V6(n), IpAddr::V6(i)) if len <= 128 => {
            masked(u128::from(n), 128) == masked(u128::from(i), 128)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_to_service() {
        assert_eq!(port_to_service(6443), Some("kube-apiserver"));
        assert_eq!(port_to_service(50000), Some("apid"));
        assert_eq!(port_to_service(2379), Some("etcd-client"));
        assert_eq!(port_to_service(12345), None);
    }

    #[test]
    fn test_port_to_service_u32() {
        assert_eq!(port_to_service_u32(6443), Some("kube-apiserver"));
        assert_eq!(port_to_service_u32(70000), None); // Out of u16 range
    }

    #[test]
    fn route_addresses_are_little_endian() {
        assert_eq!(route_hex_to_ip("0100A8C0"), "192.168.0.1");
        assert_eq!(route_hex_to_ip("00000000"), "0.0.0.0");
        assert_eq!(route_hex_to_ip("zz"), "zz");
    }

    #[test]
    fn route_table_rows() {
        let table = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
            eth0\t00000000\t0100A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            eth0\t0000A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
            short line\n";
        let lines = route_table_lines(table);
        let rows: Vec<_> = lines
            .iter()
            .filter(|l| l.starts_with("0.0.0.0") || l.starts_with("192."))
            .collect();
        assert_eq!(rows.len(), 2, "{lines:#?}");
        assert!(
            rows[0].contains("192.168.0.1") && rows[0].contains("UG"),
            "{}",
            rows[0]
        );
        assert!(rows[1].starts_with("192.168.0.0") && rows[1].contains("255.255.255.0"));
        assert_eq!(route_flags(0), "-");
        assert_eq!(route_flags(0x5), "UH");
    }

    #[test]
    fn resolv_conf_sections() {
        let lines = resolv_conf_lines(
            "# generated\nnameserver 10.96.0.10\nsearch a.svc b.local\noptions ndots:5\n",
        );
        let pos = |s: &str| lines.iter().position(|l| l == s).unwrap();
        assert_eq!(lines[pos("Nameservers:") + 1], "  • 10.96.0.10");
        assert_eq!(lines[pos("Search Domains:") + 2], "  • b.local");
        assert_eq!(lines[pos("Options:") + 1], "  • ndots:5");
        // The raw file follows, comments included
        assert!(pos("  # generated") > pos("─ Raw Content ─"));
        // Nothing parsed: no empty sections
        assert!(!resolv_conf_lines("").iter().any(|l| l.ends_with(':')));
    }

    #[test]
    fn cidr_membership() {
        assert!(cidr_contains("10.44.0.0/16", "10.44.3.7"));
        assert!(!cidr_contains("10.44.0.0/16", "10.45.0.1"));
        assert!(!cidr_contains("10.244.0.0/16", "10.0.0.11"));
        assert!(cidr_contains("0.0.0.0/0", "8.8.8.8"));
        assert!(cidr_contains("10.0.0.5/32", "10.0.0.5"));
        assert!(!cidr_contains("10.0.0.5/32", "10.0.0.6"));
        assert!(cidr_contains("fd00:10:244::/56", "fd00:10:244:0:1::5"));
        assert!(!cidr_contains("fd00:10:244::/56", "fd00:10:245::5"));
        // Mixed families, bad input
        assert!(!cidr_contains("10.0.0.0/8", "::ffff:10.0.0.1"));
        assert!(!cidr_contains("10.0.0.0/33", "10.0.0.1"));
        assert!(!cidr_contains("10.0.0.0", "10.0.0.1"));
        assert!(!cidr_contains("10.0.0.0/8", "*"));
    }
}
