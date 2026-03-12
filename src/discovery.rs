//! mDNS discovery for local LAN Hanzo instances.
//!
//! Requires the `mdns` feature.

/// mDNS service type for Hanzo tunnel discovery.
pub const MDNS_SERVICE_TYPE: &str = "_hanzo._tcp.local.";

/// Advertise this instance on the local network via mDNS.
#[cfg(feature = "mdns")]
pub async fn advertise(
    instance_id: &str,
    port: u16,
    app_kind: &crate::protocol::AppKind,
) -> Result<(), crate::TunnelError> {
    use mdns_sd::{ServiceDaemon, ServiceInfo};

    let mdns = ServiceDaemon::new()
        .map_err(|e| crate::TunnelError::Discovery(e.to_string()))?;

    let service = ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        instance_id,
        &format!("{}.local.", instance_id),
        "",
        port,
        [("kind", app_kind.to_string().as_str())].as_slice(),
    )
    .map_err(|e| crate::TunnelError::Discovery(e.to_string()))?;

    mdns.register(service)
        .map_err(|e| crate::TunnelError::Discovery(e.to_string()))?;

    Ok(())
}

/// Placeholder when mdns feature is not enabled.
#[cfg(not(feature = "mdns"))]
pub async fn advertise(
    _instance_id: &str,
    _port: u16,
    _app_kind: &crate::protocol::AppKind,
) -> Result<(), crate::TunnelError> {
    Err(crate::TunnelError::Discovery(
        "mDNS feature not enabled".into(),
    ))
}
