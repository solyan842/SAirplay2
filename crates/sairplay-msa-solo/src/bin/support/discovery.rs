//! Hardware harness input adapter. No legacy engine dependency or route overrides.
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::{collections::BTreeMap, net::IpAddr, time::{Duration, Instant}};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind { AirPlay, Raop }

impl ServiceKind {
    fn service(self) -> &'static str {
        match self { Self::AirPlay => "_airplay._tcp.local.", Self::Raop => "_raop._tcp.local." }
    }
}

#[derive(Debug, Clone)]
pub struct ObservedReceiver {
    pub address: String,
    pub port: u16,
    pub txt: String,
    fields: BTreeMap<String, String>,
}

impl ObservedReceiver {
    pub fn field(&self, key: &str) -> Option<String> { self.fields.get(key).cloned() }
}

fn matches_target(target: &str, hostname: &str, addresses: &[String]) -> bool {
    if target.trim_end_matches('.').eq_ignore_ascii_case(hostname.trim_end_matches('.')) {
        return true;
    }
    target.parse::<IpAddr>().ok().is_some_and(|ip| {
        addresses.iter().any(|v| v.parse::<IpAddr>().ok() == Some(ip))
    })
}

pub fn resolve(target: &str, port: u16, kind: ServiceKind, timeout: Duration) -> Result<ObservedReceiver, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("DISCOVERY daemon: {e}"))?;
    let result = (|| {
        let rx = daemon.browse(kind.service()).map_err(|e| format!("DISCOVERY browse: {e}"))?;
        let deadline = Instant::now() + timeout;
        let mut found = BTreeMap::new();
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(event) = rx.recv_timeout(remaining) else { break; };
            if let ServiceEvent::ServiceResolved(info) = event {
                let mut addresses: Vec<String> = info.get_addresses().iter().map(ToString::to_string).collect();
                addresses.sort_by_key(|v| (v.parse::<IpAddr>().is_ok_and(|ip| !ip.is_ipv4()), v.clone()));
                if info.get_port() != port || !matches_target(target, info.get_hostname(), &addresses) { continue; }
                let Some(address) = addresses.first().cloned() else { continue; };
                let fields: BTreeMap<String, String> = info.get_properties().iter()
                    .map(|p| (p.key().to_string(), p.val_str().to_string())).collect();
                // The pinned route API consumes whitespace-separated key=value.
                // Reject fields this representation cannot preserve losslessly.
                let route_keys = ["features", "ft", "flags", "sf", "model", "am", "igl", "pgid", "tsid", "osvers", "ov", "srcvers", "vs", "cn", "pk", "pw", "et"];
                if fields.iter().any(|(k, v)| route_keys.contains(&k.as_str()) && v.chars().any(char::is_whitespace)) {
                    return Err("DISCOVERY TXT contains whitespace; use explicitly observed MSA_TEST_TXT for the route fields".into());
                }
                let txt = fields.iter().filter(|(k, _)| route_keys.contains(&k.as_str()))
                    .map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ");
                found.insert(info.get_fullname().to_string(), ObservedReceiver { address, port: info.get_port(), txt, fields });
            }
        }
        if found.len() > 1 {
            return Err(format!("DISCOVERY ambiguous target {target}:{port}: {} resolved services; select the exact receiver endpoint", found.len()));
        }
        found.into_values().next().ok_or_else(|| format!("DISCOVERY no matching {} receiver at {target}:{port}; check the actual hostname/port, LAN and mDNS access. No empty-TXT connection attempted", kind.service()))
    })();
    let _ = daemon.stop_browse(kind.service());
    let _ = daemon.shutdown();
    result
}

pub fn redact_error(detail: &str) -> String {
    let mut safe = detail.to_string();
    for key in ["MSA_TEST_CREDENTIALS", "MSA_TEST_PASSWORD", "MSA_TEST_RAOP_SECRET", "MSA_TEST_PK"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() { safe = safe.replace(&value, "<redacted>"); }
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_host_or_address_only() {
        let ips = vec!["192.168.1.5".into()];
        assert!(matches_target("White.local", "white.local.", &ips));
        assert!(matches_target("192.168.1.5", "White.local.", &ips));
        assert!(!matches_target("Black.local", "White.local.", &ips));
        assert!(!matches_target("White", "White.local.", &ips));
        assert!(!matches_target("192.168.1.6", "White.local.", &ips));
    }
}
