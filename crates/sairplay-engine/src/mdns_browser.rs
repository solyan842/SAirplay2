use crate::AirPlayTxt;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::sync::mpsc::{self, Receiver};
use std::thread;

pub const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";
pub const RAOP_SERVICE: &str = "_raop._tcp.local.";
pub const COMPANION_SERVICE: &str = "_companion-link._tcp.local.";
pub const MRP_SERVICE: &str = "_mediaremotetv._tcp.local.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    AirPlay,
    Raop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredService {
    pub kind: ServiceKind,
    pub fullname: String,
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub addresses: Vec<String>,
    pub txt: AirPlayTxt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    Upsert(DiscoveredService),
    Removed {
        kind: ServiceKind,
        fullname: String,
    },
    ControlPresence {
        kind: String,
        fullname: String,
        host: String,
        port: u16,
        addresses: Vec<String>,
    },
    Error(String),
}

pub struct MdnsBrowser {
    daemon: ServiceDaemon,
}

impl MdnsBrowser {
    pub fn start() -> Result<(Self, Receiver<DiscoveryEvent>), String> {
        let daemon = ServiceDaemon::new().map_err(|e| format!("mDNS daemon: {e}"))?;
        let airplay_rx = daemon
            .browse(AIRPLAY_SERVICE)
            .map_err(|e| format!("browse {AIRPLAY_SERVICE}: {e}"))?;
        let raop_rx = daemon
            .browse(RAOP_SERVICE)
            .map_err(|e| format!("browse {RAOP_SERVICE}: {e}"))?;
        let companion_rx = daemon
            .browse(COMPANION_SERVICE)
            .map_err(|e| format!("browse {COMPANION_SERVICE}: {e}"))?;
        let mrp_rx = daemon
            .browse(MRP_SERVICE)
            .map_err(|e| format!("browse {MRP_SERVICE}: {e}"))?;

        let (tx, rx) = mpsc::channel();

        spawn_forwarder(ServiceKind::AirPlay, airplay_rx, tx.clone());
        spawn_forwarder(ServiceKind::Raop, raop_rx, tx.clone());
        spawn_control_forwarder("Companion", companion_rx, tx.clone());
        spawn_control_forwarder("MRP", mrp_rx, tx);

        Ok((Self { daemon }, rx))
    }

    pub fn shutdown(&self) -> Result<(), String> {
        let _ = self.daemon.stop_browse(AIRPLAY_SERVICE);
        let _ = self.daemon.stop_browse(RAOP_SERVICE);
        let _ = self.daemon.stop_browse(COMPANION_SERVICE);
        let _ = self.daemon.stop_browse(MRP_SERVICE);
        self.daemon
            .shutdown()
            .map(|_| ())
            .map_err(|e| format!("mDNS shutdown: {e}"))
    }
}

impl Drop for MdnsBrowser {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(AIRPLAY_SERVICE);
        let _ = self.daemon.stop_browse(RAOP_SERVICE);
        let _ = self.daemon.stop_browse(COMPANION_SERVICE);
        let _ = self.daemon.stop_browse(MRP_SERVICE);
        let _ = self.daemon.shutdown();
    }
}

fn spawn_forwarder(
    kind: ServiceKind,
    rx: mdns_sd::Receiver<ServiceEvent>,
    tx: mpsc::Sender<DiscoveryEvent>,
) {
    thread::Builder::new()
        .name(match kind {
            ServiceKind::AirPlay => "mdns-airplay".into(),
            ServiceKind::Raop => "mdns-raop".into(),
        })
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let pairs = info
                            .get_properties()
                            .iter()
                            .map(|prop| (prop.key().to_string(), prop.val_str().to_string()));

                        let txt = match AirPlayTxt::parse(pairs) {
                            Ok(txt) => txt,
                            Err(err) => {
                                let _ = tx.send(DiscoveryEvent::Error(format!(
                                    "TXT parse failed for {}: {err:?}",
                                    info.get_fullname()
                                )));
                                continue;
                            }
                        };

                        let mut addresses: Vec<String> = info
                            .get_addresses()
                            .iter()
                            .map(ToString::to_string)
                            .collect();
                        addresses.sort();
                        addresses.dedup();

                        let service = DiscoveredService {
                            kind,
                            fullname: info.get_fullname().to_string(),
                            display_name: display_name(kind, info.get_fullname()),
                            host: info.get_hostname().to_string(),
                            port: info.get_port(),
                            addresses,
                            txt,
                        };

                        if tx.send(DiscoveryEvent::Upsert(service)).is_err() {
                            break;
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        if tx
                            .send(DiscoveryEvent::Removed { kind, fullname })
                            .is_err()
                        {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        })
        .expect("failed to spawn mDNS forwarder");
}

fn spawn_control_forwarder(
    label: &'static str,
    rx: mdns_sd::Receiver<ServiceEvent>,
    tx: mpsc::Sender<DiscoveryEvent>,
) {
    thread::Builder::new()
        .name(format!("mdns-{}", label.to_ascii_lowercase()))
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                if let ServiceEvent::ServiceResolved(info) = event {
                    let mut addresses: Vec<String> = info
                        .get_addresses()
                        .iter()
                        .map(ToString::to_string)
                        .collect();
                    addresses.sort();
                    addresses.dedup();
                    if tx
                        .send(DiscoveryEvent::ControlPresence {
                            kind: label.to_owned(),
                            fullname: info.get_fullname().to_string(),
                            host: info.get_hostname().to_string(),
                            port: info.get_port(),
                            addresses,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
        .expect("failed to spawn control-service mDNS forwarder");
}

fn display_name(kind: ServiceKind, fullname: &str) -> String {
    let suffix = match kind {
        ServiceKind::AirPlay => AIRPLAY_SERVICE,
        ServiceKind::Raop => RAOP_SERVICE,
    };

    let mut instance = fullname
        .strip_suffix(suffix)
        .unwrap_or(fullname)
        .trim_end_matches('.')
        .to_string();

    if kind == ServiceKind::Raop {
        if let Some((_, name)) = instance.split_once('@') {
            instance = name.to_string();
        }
    }

    instance
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_airplay_display_name() {
        assert_eq!(
            display_name(ServiceKind::AirPlay, "Living Room._airplay._tcp.local."),
            "Living Room"
        );
    }

    #[test]
    fn strips_raop_device_id_prefix() {
        assert_eq!(
            display_name(
                ServiceKind::Raop,
                "AABBCCDDEEFF@Living Room._raop._tcp.local."
            ),
            "Living Room"
        );
    }
}
