use crate::{
    DiscoveredService, ReceiverCapabilities, Route, RouteResolver, ServiceKind,
};

#[derive(Debug, Clone)]
pub struct DeviceRecord {
    pub display_name: String,
    pub airplay: Option<DiscoveredService>,
    pub raop: Option<DiscoveredService>,
}

impl DeviceRecord {
    pub fn addresses(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(service) = &self.airplay {
            out.extend(service.addresses.iter().cloned());
        }
        if let Some(service) = &self.raop {
            out.extend(service.addresses.iter().cloned());
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn capabilities(
        &self,
        has_stored_credentials: bool,
        password_supplied: bool,
    ) -> ReceiverCapabilities {
        match &self.airplay {
            Some(service) => ReceiverCapabilities::from_txt(
                &service.txt,
                has_stored_credentials,
                password_supplied,
            ),
            None => ReceiverCapabilities::default(),
        }
    }

    pub fn route(
        &self,
        has_stored_credentials: bool,
        password_supplied: bool,
    ) -> Route {
        RouteResolver::resolve(self.capabilities(
            has_stored_credentials,
            password_supplied,
        ))
    }

    pub fn endpoint_for_route(&self, route: Route) -> Option<&DiscoveredService> {
        match route {
            Route::Raop => self.raop.as_ref().or(self.airplay.as_ref()),
            Route::AirPlay2Compat | Route::AirPlay2Native => {
                self.airplay.as_ref().or(self.raop.as_ref())
            }
        }
    }
}

#[derive(Default)]
pub struct DeviceCatalog {
    devices: Vec<DeviceRecord>,
}

impl DeviceCatalog {
    pub fn devices(&self) -> &[DeviceRecord] {
        &self.devices
    }

    pub fn upsert(&mut self, service: DiscoveredService) {
        let index = self
            .devices
            .iter()
            .position(|device| same_device(device, &service));

        let device = if let Some(index) = index {
            &mut self.devices[index]
        } else {
            self.devices.push(DeviceRecord {
                display_name: service.display_name.clone(),
                airplay: None,
                raop: None,
            });
            self.devices.last_mut().unwrap()
        };

        if service.kind == ServiceKind::AirPlay {
            device.display_name = service.display_name.clone();
            device.airplay = Some(service);
        } else {
            if device.airplay.is_none() {
                device.display_name = service.display_name.clone();
            }
            device.raop = Some(service);
        }
    }

    pub fn remove(&mut self, kind: ServiceKind, fullname: &str) {
        for device in &mut self.devices {
            match kind {
                ServiceKind::AirPlay
                    if device.airplay.as_ref().is_some_and(|s| s.fullname == fullname) =>
                {
                    device.airplay = None;
                }
                ServiceKind::Raop
                    if device.raop.as_ref().is_some_and(|s| s.fullname == fullname) =>
                {
                    device.raop = None;
                }
                _ => {}
            }
        }

        self.devices
            .retain(|device| device.airplay.is_some() || device.raop.is_some());
    }
}

fn same_device(device: &DeviceRecord, service: &DiscoveredService) -> bool {
    // An mDNS refresh of the exact same service must always update its own
    // record, even if a receiver changed its display name.
    let same_fullname = match service.kind {
        ServiceKind::AirPlay => device
            .airplay
            .as_ref()
            .is_some_and(|existing| existing.fullname == service.fullname),
        ServiceKind::Raop => device
            .raop
            .as_ref()
            .is_some_and(|existing| existing.fullname == service.fullname),
    };
    if same_fullname {
        return true;
    }

    // Follow Music Assistant's AirPlay discovery identity:
    // - RAOP: raw id before '@'
    // - AirPlay: TXT deviceid
    //
    // A shared IP is NOT device identity. TVs / meeting boxes can advertise
    // multiple independent AirPlay/RAOP endpoints on one address.
    let incoming_id = stable_service_id(service);
    let existing_ids = [device.airplay.as_ref(), device.raop.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(stable_service_id)
        .collect::<Vec<_>>();

    if let Some(incoming_id) = incoming_id.as_deref() {
        if !existing_ids.is_empty() {
            return existing_ids.iter().any(|id| id == incoming_id);
        }
    }

    // Some third-party receivers omit deviceid. MSA pairs related AirPlay/RAOP
    // discovery by display name in that case; keep the same conservative
    // fallback rather than merging unrelated services by network address.
    device
        .display_name
        .eq_ignore_ascii_case(&service.display_name)
}

fn stable_service_id(service: &DiscoveredService) -> Option<String> {
    let raw = match service.kind {
        ServiceKind::AirPlay => service.txt.fields.get("deviceid")?.as_str(),
        ServiceKind::Raop => {
            let instance = service.fullname.split('.').next().unwrap_or(&service.fullname);
            instance.split_once('@')?.0
        }
    };

    let normalized = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    (!normalized.is_empty()).then_some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AirPlayTxt;

    fn service(
        kind: ServiceKind,
        fullname: &str,
        name: &str,
        address: &str,
        port: u16,
        txt: AirPlayTxt,
    ) -> DiscoveredService {
        DiscoveredService {
            kind,
            fullname: fullname.into(),
            display_name: name.into(),
            host: "speaker.local.".into(),
            port,
            addresses: vec![address.into()],
            txt,
        }
    }

    #[test]
    fn airplay_and_raop_records_merge_into_one_device() {
        let mut catalog = DeviceCatalog::default();
        catalog.upsert(service(
            ServiceKind::Raop,
            "AABBCCDDEEFF@Kitchen._raop._tcp.local.",
            "Kitchen",
            "192.168.1.20",
            5000,
            AirPlayTxt::default(),
        ));
        catalog.upsert(service(
            ServiceKind::AirPlay,
            "Kitchen._airplay._tcp.local.",
            "Kitchen",
            "192.168.1.20",
            7000,
            AirPlayTxt::parse([("features", (1u64 << 38).to_string())]).unwrap(),
        ));

        assert_eq!(catalog.devices().len(), 1);
        let device = &catalog.devices()[0];
        assert!(device.airplay.is_some());
        assert!(device.raop.is_some());
    }

    #[test]
    fn shared_ip_does_not_merge_distinct_airplay_receivers() {
        let mut catalog = DeviceCatalog::default();
        catalog.upsert(service(
            ServiceKind::AirPlay,
            "EShare-1965._airplay._tcp.local.",
            "EShare-1965",
            "192.168.31.139",
            51010,
            AirPlayTxt::parse([
                ("deviceid", "AA:BB:CC:DD:EE:01"),
                ("model", "AppleTV3,2"),
            ])
            .unwrap(),
        ));
        catalog.upsert(service(
            ServiceKind::Raop,
            "AABBCCDDEE01@EShare-1965._raop._tcp.local.",
            "EShare-1965",
            "192.168.31.139",
            51040,
            AirPlayTxt::default(),
        ));
        catalog.upsert(service(
            ServiceKind::AirPlay,
            "LiOA_Meeting._airplay._tcp.local.",
            "LiOA_Meeting",
            "192.168.31.139",
            7000,
            AirPlayTxt::parse([
                ("deviceid", "AA:BB:CC:DD:EE:02"),
                ("model", "AppleTV3,2"),
            ])
            .unwrap(),
        ));
        catalog.upsert(service(
            ServiceKind::Raop,
            "AABBCCDDEE02@LiOA_Meeting._raop._tcp.local.",
            "LiOA_Meeting",
            "192.168.31.139",
            7102,
            AirPlayTxt::default(),
        ));

        assert_eq!(catalog.devices().len(), 2);

        let eshare = catalog
            .devices()
            .iter()
            .find(|device| device.display_name == "EShare-1965")
            .unwrap();
        assert_eq!(eshare.airplay.as_ref().unwrap().port, 51010);
        assert_eq!(eshare.raop.as_ref().unwrap().port, 51040);

        let lioa = catalog
            .devices()
            .iter()
            .find(|device| device.display_name == "LiOA_Meeting")
            .unwrap();
        assert_eq!(lioa.airplay.as_ref().unwrap().port, 7000);
        assert_eq!(lioa.raop.as_ref().unwrap().port, 7102);
    }

    #[test]
    fn stable_device_id_wins_over_same_display_name() {
        let mut catalog = DeviceCatalog::default();
        catalog.upsert(service(
            ServiceKind::AirPlay,
            "Room._airplay._tcp.local.",
            "Room",
            "192.168.1.50",
            7000,
            AirPlayTxt::parse([("deviceid", "AA:BB:CC:DD:EE:01")]).unwrap(),
        ));
        catalog.upsert(service(
            ServiceKind::Raop,
            "AABBCCDDEE02@Room._raop._tcp.local.",
            "Room",
            "192.168.1.51",
            5000,
            AirPlayTxt::default(),
        ));

        assert_eq!(catalog.devices().len(), 2);
    }

    #[test]
    fn airplay_txt_is_authoritative_for_route() {
        let mut catalog = DeviceCatalog::default();
        let mask = (1u64 << 38) | (1u64 << 46);
        catalog.upsert(service(
            ServiceKind::AirPlay,
            "Living Room._airplay._tcp.local.",
            "Living Room",
            "192.168.1.30",
            7000,
            AirPlayTxt::parse([("features", mask.to_string())]).unwrap(),
        ));
        catalog.upsert(service(
            ServiceKind::Raop,
            "001122334455@Living Room._raop._tcp.local.",
            "Living Room",
            "192.168.1.30",
            5000,
            AirPlayTxt::default(),
        ));

        assert_eq!(
            catalog.devices()[0].route(false, false),
            Route::AirPlay2Native
        );
    }

    #[test]
    fn raop_only_device_stays_raop() {
        let mut catalog = DeviceCatalog::default();
        catalog.upsert(service(
            ServiceKind::Raop,
            "AABBCCDDEEFF@AirPort._raop._tcp.local.",
            "AirPort",
            "192.168.1.40",
            5000,
            AirPlayTxt::default(),
        ));

        assert_eq!(catalog.devices()[0].route(false, false), Route::Raop);
    }
}
