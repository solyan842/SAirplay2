//! MSA route resolution, ported from ap2_resolve_route() at the pinned source.

const FEAT_BUFFERED: u8 = 40;
const FEAT_PTP: u8 = 41;
const FEAT_UNIFIED_MEDIA: u8 = 38;
const FEAT_HK_PAIRING: u8 = 46;
const FEAT_COREUTILS: u8 = 48;
const SF_PIN_REQUIRED: u64 = 0x8;
const SF_LEGACY_PAIRING: u64 = 0x200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolPreference { Auto, Raop, AirPlay2, AirPlay2Compat }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow { Raop, AirPlay2Compat, AirPlay2Native }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing { Ntp, Ptp }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteDecision {
    pub flow: Flow,
    pub timing: Timing,
    pub transient_pairing: bool,
    pub features: u64,
}

fn feature(features: u64, bit: u8) -> bool { features & (1u64 << bit) != 0 }

pub fn resolve_route(
    pref: ProtocolPreference,
    features: u64,
    flags: u64,
    receiver_requires_password: bool,
    have_credentials: bool,
    have_password: bool,
    force_native: bool,
    ptp_override: Option<bool>,
) -> RouteDecision {
    let mut is_ap2 = match pref {
        ProtocolPreference::Raop => false,
        ProtocolPreference::AirPlay2 | ProtocolPreference::AirPlay2Compat => true,
        ProtocolPreference::Auto => feature(features, FEAT_UNIFIED_MEDIA) || feature(features, FEAT_COREUTILS),
    };
    if force_native { is_ap2 = true; }
    if !is_ap2 {
        return RouteDecision { flow: Flow::Raop, timing: Timing::Ntp, transient_pairing: false, features };
    }

    let pairable = feature(features, FEAT_HK_PAIRING)
        || feature(features, FEAT_COREUTILS)
        || (pref == ProtocolPreference::AirPlay2 && features == 0);
    let pairing_blocked = flags & (SF_PIN_REQUIRED | SF_LEGACY_PAIRING) != 0
        || (receiver_requires_password && !have_password);
    let native = have_credentials || force_native || (pairable && !pairing_blocked);

    if pref == ProtocolPreference::AirPlay2Compat || !native {
        return RouteDecision { flow: Flow::AirPlay2Compat, timing: Timing::Ntp, transient_pairing: false, features };
    }

    let ptp = ptp_override.unwrap_or_else(|| feature(features, FEAT_PTP));
    RouteDecision {
        flow: Flow::AirPlay2Native,
        timing: if ptp { Timing::Ptp } else { Timing::Ntp },
        transient_pairing: !have_credentials,
        features,
    }
}

pub fn supports_buffered_audio(features: u64) -> bool { feature(features, FEAT_BUFFERED) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn legacy_auto_is_raop() {
        assert_eq!(resolve_route(ProtocolPreference::Auto,0,0,false,false,false,false,None).flow,Flow::Raop);
    }
    #[test] fn coreutils_auto_is_native_ptp_when_advertised() {
        let f=(1u64<<FEAT_COREUTILS)|(1u64<<FEAT_PTP);
        let r=resolve_route(ProtocolPreference::Auto,f,0,false,false,false,false,None);
        assert_eq!(r.flow,Flow::AirPlay2Native); assert_eq!(r.timing,Timing::Ptp); assert!(r.transient_pairing);
    }
    #[test] fn explicit_compat_wins() {
        let f=(1u64<<FEAT_COREUTILS)|(1u64<<FEAT_PTP);
        assert_eq!(resolve_route(ProtocolPreference::AirPlay2Compat,f,0,false,true,false,true,None).flow,Flow::AirPlay2Compat);
    }
    #[test] fn password_block_without_password_falls_back_compat() {
        let f=1u64<<FEAT_COREUTILS;
        assert_eq!(resolve_route(ProtocolPreference::Auto,f,0,true,false,false,false,None).flow,Flow::AirPlay2Compat);
    }
}
