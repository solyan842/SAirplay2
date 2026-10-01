//! Native AirPlay route policy ported from pinned MSA ap2_client.c.
//! Source: music-assistant/airplay-cli @ 431c5c582eef9307c4e39c50a0ea65e970bc1128.

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
    pub flags: u64,
    pub reason: &'static str,
}

fn feature(features: u64, bit: u8) -> bool { features & (1u64 << bit) != 0 }

pub fn env_enabled(name: &str, unset_default: bool) -> bool {
    let Ok(value) = std::env::var(name) else { return unset_default };
    !matches!(value.as_str(), "0" | "false" | "off")
}

fn txt_hex_field(txt: Option<&str>, key1: &str, key2: Option<&str>) -> u64 {
    let Some(txt) = txt else { return 0 };
    let find_value = |key: &str| -> Option<&str> {
        txt.find(key).map(|at| &txt[at + key.len()..])
    };
    let value = find_value(key1).or_else(|| key2.and_then(find_value));
    let Some(value) = value else { return 0 };
    // sscanf in pinned MSA only consumes this field. A comma in a later
    // model (AudioAccessory5,1) must never become this mask's high half.
    let first_token = value.split_whitespace().next().unwrap_or("");
    let value = if first_token.contains(',') { value } else { first_token };
    let mut parts = value.splitn(2, ',');
    let low = parts.next()
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0);
    let Some(high_text) = parts.next() else { return low };
    let high = high_text.split_whitespace().next()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0);
    (high << 32) | (low as u32 as u64)
}

pub fn txt_features(txt: Option<&str>) -> u64 {
    txt_hex_field(txt, "features=", Some("ft="))
}

pub fn txt_flags(txt: Option<&str>) -> u64 {
    txt_hex_field(txt, "flags=", Some("sf="))
}

fn txt_field<'a>(txt: Option<&'a str>, key: &str) -> Option<&'a str> {
    let txt = txt?;
    let needle = format!("{key}=");
    for (at, _) in txt.match_indices(&needle) {
        if at == 0 || txt.as_bytes().get(at.wrapping_sub(1)) == Some(&b' ') {
            let start = at + needle.len();
            return Some(txt[start..].split_whitespace().next().unwrap_or(""));
        }
    }
    None
}

fn model_prefix(txt: Option<&str>, am: Option<&str>, prefix: &str) -> bool {
    txt_field(txt, "model").is_some_and(|v| v.starts_with(prefix))
        || am.is_some_and(|v| v.starts_with(prefix))
}

fn txt_version_major(txt: Option<&str>, key: &str) -> Option<u32> {
    let value = txt_field(txt, key)?;
    let digits: String = value.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn receiver_os_ge_27(txt: Option<&str>) -> bool {
    if let Some(os) = txt_version_major(txt, "osvers").or_else(|| txt_version_major(txt, "ov")) {
        return os >= 27;
    }
    txt_version_major(txt, "srcvers")
        .or_else(|| txt_version_major(txt, "vs"))
        .is_some_and(|v| v >= 980)
}

pub fn apple_model(txt: Option<&str>, am: Option<&str>) -> bool {
    ["AppleTV", "AudioAccessory", "iPhone", "iPad", "iPod", "Mac"]
        .into_iter()
        .any(|prefix| model_prefix(txt, am, prefix))
}

/// Exact MSA standalone-HomePod OS27+ follow-clock policy, including the
/// CLIAIRPLAY_PTP_FOLLOW diagnostic override.
pub fn follow_receiver_clock(txt: Option<&str>, am: Option<&str>) -> bool {
    if std::env::var_os("CLIAIRPLAY_PTP_FOLLOW").is_some() {
        return env_enabled("CLIAIRPLAY_PTP_FOLLOW", false);
    }
    if !model_prefix(txt, am, "AudioAccessory") { return false; }
    if !txt_field(txt, "igl").is_some_and(|v| v.starts_with('1')) { return false; }
    if txt_field(txt, "pgid").is_some() || txt_field(txt, "tsid").is_some() { return false; }
    receiver_os_ge_27(txt)
}

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
        return RouteDecision {
            flow: Flow::Raop, timing: Timing::Ntp, transient_pairing: false,
            features, flags, reason: "legacy RAOP",
        };
    }

    let pairable = feature(features, FEAT_HK_PAIRING)
        || feature(features, FEAT_COREUTILS)
        || (pref == ProtocolPreference::AirPlay2 && features == 0);
    let pairing_blocked = flags & (SF_PIN_REQUIRED | SF_LEGACY_PAIRING) != 0
        || (receiver_requires_password && !have_password);
    let native = have_credentials || force_native || (pairable && !pairing_blocked);
    let transient = native && !have_credentials;

    if pref == ProtocolPreference::AirPlay2Compat {
        return RouteDecision {
            flow: Flow::AirPlay2Compat, timing: Timing::Ntp, transient_pairing: false,
            features, flags, reason: "AirPlay 2 (RAOP-compat, forced)",
        };
    }
    if !native {
        return RouteDecision {
            flow: Flow::AirPlay2Compat, timing: Timing::Ntp, transient_pairing: false,
            features, flags, reason: "AirPlay 2 (RAOP-compat)",
        };
    }

    let ptp = ptp_override.unwrap_or_else(|| feature(features, FEAT_PTP));
    let reason = if transient {
        if have_password { "native AP2, password, realtime" }
        else { "native AP2, transient, realtime" }
    } else {
        "native AP2, pair-verify, realtime"
    };
    RouteDecision {
        flow: Flow::AirPlay2Native,
        timing: if ptp { Timing::Ptp } else { Timing::Ntp },
        transient_pairing: transient,
        features,
        flags,
        reason,
    }
}

pub fn resolve_route_from_txt(
    pref: ProtocolPreference,
    txt: Option<&str>,
    pw: Option<&str>,
    have_credentials: bool,
    have_password: bool,
    force_native: bool,
    ptp_override: Option<bool>,
) -> RouteDecision {
    resolve_route(
        pref,
        txt_features(txt),
        txt_flags(txt),
        pw.is_some_and(|v| v.eq_ignore_ascii_case("true")),
        have_credentials,
        have_password,
        force_native,
        ptp_override,
    )
}

pub fn supports_buffered_audio(features: u64) -> bool { feature(features, FEAT_BUFFERED) }

/// Pinned source currently has an empty realtime-splice deny-list.
pub fn splice_timeline_allowed(_txt: Option<&str>, _am: Option<&str>) -> bool { true }

/// Exact MSA type-103 selection policy. Buffered is eligible only for native
/// PTP, CLIAIRPLAY_BUFFERED outranks all other decisions, and Apple models are
/// excluded from automatic type-103 selection by measured behavior.
pub fn buffered_route(
    route: &RouteDecision,
    txt: Option<&str>,
    am: Option<&str>,
    forced: bool,
) -> bool {
    if route.flow != Flow::AirPlay2Native || route.timing != Timing::Ptp {
        return false;
    }
    if std::env::var_os("CLIAIRPLAY_BUFFERED").is_some() {
        return env_enabled("CLIAIRPLAY_BUFFERED", false);
    }
    if forced { return true; }
    supports_buffered_audio(txt_features(txt)) && !apple_model(txt, am)
    // Pinned buffered deny-list is intentionally empty.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn parses_split_feature_mask_exactly() {
        assert_eq!(txt_features(Some("features=0x1,0x2 sf=0x8")), 0x0000_0002_0000_0001);
        assert_eq!(txt_flags(Some("features=0x1,0x2 sf=0x200")), 0x200);
    }
    #[test] fn later_model_comma_does_not_change_feature_or_flag_mask() {
        assert_eq!(txt_features(Some("features=0x123 model=AudioAccessory5,1")), 0x123);
        assert_eq!(txt_flags(Some("sf=0x4 model=AudioAccessory5,1")), 0x4);
        assert_eq!(txt_flags(Some("flags=0x0 model=AudioAccessory5,1")), 0);
        assert_eq!(txt_features(Some("features=0x1, 0x2 model=AudioAccessory5,1")), 0x0000_0002_0000_0001);
    }
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
    #[test] fn auto_buffered_excludes_apple_but_forced_allows_it() {
        let route=RouteDecision{flow:Flow::AirPlay2Native,timing:Timing::Ptp,transient_pairing:false,features:1u64<<FEAT_BUFFERED,flags:0,reason:""};
        let txt=Some("features=0x0,0x100 model=AppleTV14,1");
        assert!(!buffered_route(&route,txt,None,false));
        assert!(buffered_route(&route,txt,None,true));
    }
    #[test] fn standalone_homepod_os27_follows_receiver_clock() {
        assert!(follow_receiver_clock(Some("model=AudioAccessory5,1 igl=1 osvers=27.0"),None));
        assert!(!follow_receiver_clock(Some("model=AudioAccessory5,1 igl=1 osvers=26.0"),None));
        assert!(!follow_receiver_clock(Some("model=AudioAccessory5,1 igl=1 osvers=27.0 tsid=x"),None));
    }
}
