use sairplay_engine::{AirPlayTxt, ReceiverCapabilities, Route, RouteResolver};

#[test]
fn airport_style_txt_routes_raop() {
    let txt = AirPlayTxt::parse([
        ("am", "AirPort10,115".to_string()),
        ("cn", "0,1".to_string()),
        ("et", "0,1".to_string()),
    ]).unwrap();

    let caps = ReceiverCapabilities::from_txt(&txt, false, false);
    assert_eq!(RouteResolver::resolve(caps), Route::Raop);
}

#[test]
fn homepod_style_capabilities_route_native_when_pairing_is_available() {
    let mask = (1u64 << 38) | (1u64 << 41) | (1u64 << 46) | (1u64 << 48);
    let txt = AirPlayTxt::parse([
        ("features", mask.to_string()),
        ("model", "AudioAccessory".to_string()),
        ("pw", "false".to_string()),
    ]).unwrap();

    let caps = ReceiverCapabilities::from_txt(&txt, false, false);
    assert_eq!(RouteResolver::resolve(caps), Route::AirPlay2Native);
    assert!(caps.supports_ptp);
}

#[test]
fn stored_credentials_force_native_even_if_pin_flag_is_set() {
    let mask = (1u64 << 38) | (1u64 << 46);
    let txt = AirPlayTxt::parse([
        ("features", mask.to_string()),
        ("flags", "0x8".to_string()),
    ]).unwrap();

    let caps = ReceiverCapabilities::from_txt(&txt, true, false);
    assert_eq!(RouteResolver::resolve(caps), Route::AirPlay2Native);
}

#[test]
fn no_credentials_with_pin_required_falls_back_to_compat() {
    let mask = (1u64 << 38) | (1u64 << 46);
    let txt = AirPlayTxt::parse([
        ("features", mask.to_string()),
        ("flags", "0x8".to_string()),
    ]).unwrap();

    let caps = ReceiverCapabilities::from_txt(&txt, false, false);
    assert_eq!(RouteResolver::resolve(caps), Route::AirPlay2Compat);
}
