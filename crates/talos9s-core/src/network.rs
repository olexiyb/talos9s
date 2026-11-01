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
}
