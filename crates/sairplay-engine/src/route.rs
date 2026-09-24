use crate::AirPlayTxt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Raop,
    AirPlay2Compat,
    AirPlay2Native,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiverCapabilities {
    pub supports_airplay2: bool,
    pub supports_pairing: bool,
    pub supports_ptp: bool,
    pub supports_buffered_audio: bool,
    pub is_apple_model: bool,
    pub has_stored_credentials: bool,
    pub requires_pin: bool,
    pub legacy_pairing: bool,
    pub password_required: bool,
    pub password_supplied: bool,
    pub txt_was_empty: bool,
}

impl ReceiverCapabilities {
    pub fn from_txt(
        txt: &AirPlayTxt,
        has_stored_credentials: bool,
        password_supplied: bool,
    ) -> Self {
        Self {
            supports_airplay2: txt.supports_airplay2(),
            supports_pairing: txt.supports_pairing(),
            supports_ptp: txt.supports_ptp(),
            supports_buffered_audio: txt.supports_buffered_audio(),
            is_apple_model: txt.is_apple_model(),
            has_stored_credentials,
            requires_pin: txt.pin_required(),
            legacy_pairing: txt.legacy_pairing(),
            password_required: txt.password_required,
            password_supplied,
            txt_was_empty: txt.fields.is_empty(),
        }
    }
}

pub struct RouteResolver;

impl RouteResolver {
    /// Port of pinned music-assistant/airplay-cli ap2_buffered_route() auto policy.
    ///
    /// Buffered type 103 is a transport decision layered on top of the base route:
    /// native AirPlay 2 + PTP + SupportsBufferedAudio, excluding Apple models.
    /// The upstream measured-hostile deny-list is currently empty, so there is
    /// no model-specific third-party exclusion to copy here yet.
    pub fn buffered_auto_eligible(route: Route, caps: ReceiverCapabilities) -> bool {
        route == Route::AirPlay2Native
            && caps.supports_ptp
            && caps.supports_buffered_audio
            && !caps.is_apple_model
    }


    pub fn resolve(caps: ReceiverCapabilities) -> Route {
        if !caps.supports_airplay2 {
            return Route::Raop;
        }

        if caps.has_stored_credentials {
            return Route::AirPlay2Native;
        }

        let password_ok = !caps.password_required || caps.password_supplied;
        let transient_native_ok =
            caps.supports_pairing &&
            !caps.requires_pin &&
            !caps.legacy_pairing &&
            password_ok;

        if transient_native_ok {
            Route::AirPlay2Native
        } else {
            Route::AirPlay2Compat
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AirPlayTxt;

    #[test]
    fn legacy_is_raop() {
        assert_eq!(
            RouteResolver::resolve(ReceiverCapabilities::default()),
            Route::Raop
        );
    }

    #[test]
    fn credentialed_ap2_is_native() {
        assert_eq!(
            RouteResolver::resolve(ReceiverCapabilities {
                supports_airplay2: true,
                has_stored_credentials: true,
                ..Default::default()
            }),
            Route::AirPlay2Native
        );
    }

    #[test]
    fn pairing_capable_without_pin_is_native() {
        assert_eq!(
            RouteResolver::resolve(ReceiverCapabilities {
                supports_airplay2: true,
                supports_pairing: true,
                ..Default::default()
            }),
            Route::AirPlay2Native
        );
    }

    #[test]
    fn pin_or_legacy_flag_falls_back_to_compat_without_credentials() {
        assert_eq!(
            RouteResolver::resolve(ReceiverCapabilities {
                supports_airplay2: true,
                supports_pairing: true,
                requires_pin: true,
                ..Default::default()
            }),
            Route::AirPlay2Compat
        );

        assert_eq!(
            RouteResolver::resolve(ReceiverCapabilities {
                supports_airplay2: true,
                supports_pairing: true,
                legacy_pairing: true,
                ..Default::default()
            }),
            Route::AirPlay2Compat
        );
    }

    #[test]
    fn buffered_auto_matches_pinned_msa_third_party_policy() {
        let txt = AirPlayTxt::parse([
            ("features", ((1u64 << 38) | (1u64 << 40) | (1u64 << 41) | (1u64 << 46)).to_string()),
            ("model", "Era 100".to_string()),
        ]).unwrap();
        let caps = ReceiverCapabilities::from_txt(&txt, false, false);
        let route = RouteResolver::resolve(caps);
        assert_eq!(route, Route::AirPlay2Native);
        assert!(RouteResolver::buffered_auto_eligible(route, caps));
    }

    #[test]
    fn buffered_auto_rejects_apple_models_even_when_bit40_and_ptp_are_set() {
        for model in ["AppleTV14,1", "AudioAccessory5,1", "Mac14,7"] {
            let txt = AirPlayTxt::parse([
                ("features", ((1u64 << 38) | (1u64 << 40) | (1u64 << 41) | (1u64 << 46)).to_string()),
                ("model", model.to_string()),
            ]).unwrap();
            let caps = ReceiverCapabilities::from_txt(&txt, false, false);
            let route = RouteResolver::resolve(caps);
            assert_eq!(route, Route::AirPlay2Native);
            assert!(!RouteResolver::buffered_auto_eligible(route, caps));
        }
    }

    #[test]
    fn buffered_auto_requires_ptp_and_native_route() {
        let no_ptp = ReceiverCapabilities {
            supports_airplay2: true,
            supports_pairing: true,
            supports_buffered_audio: true,
            supports_ptp: false,
            ..Default::default()
        };
        let route = RouteResolver::resolve(no_ptp);
        assert_eq!(route, Route::AirPlay2Native);
        assert!(!RouteResolver::buffered_auto_eligible(route, no_ptp));

        let compat = ReceiverCapabilities {
            supports_airplay2: true,
            supports_pairing: false,
            supports_ptp: true,
            supports_buffered_audio: true,
            ..Default::default()
        };
        let route = RouteResolver::resolve(compat);
        assert_eq!(route, Route::AirPlay2Compat);
        assert!(!RouteResolver::buffered_auto_eligible(route, compat));

        let raop = ReceiverCapabilities::default();
        assert_eq!(RouteResolver::resolve(raop), Route::Raop);
        assert!(!RouteResolver::buffered_auto_eligible(Route::Raop, raop));
    }

    #[test]
    fn password_advertisement_requires_password_for_transient_native() {
        let txt = AirPlayTxt::parse([
            ("features", ((1u64 << 38) | (1u64 << 46)).to_string()),
            ("pw", "true".to_string()),
        ]).unwrap();

        let no_password = ReceiverCapabilities::from_txt(&txt, false, false);
        assert_eq!(RouteResolver::resolve(no_password), Route::AirPlay2Compat);

        let with_password = ReceiverCapabilities::from_txt(&txt, false, true);
        assert_eq!(RouteResolver::resolve(with_password), Route::AirPlay2Native);
    }
}
