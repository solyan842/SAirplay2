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
    pub has_stored_credentials: bool,
    pub requires_pin: bool,
}

pub struct RouteResolver;

impl RouteResolver {
    pub fn resolve(caps: ReceiverCapabilities) -> Route {
        if !caps.supports_airplay2 {
            return Route::Raop;
        }
        if caps.has_stored_credentials {
            return Route::AirPlay2Native;
        }
        if caps.supports_pairing && !caps.requires_pin {
            return Route::AirPlay2Native;
        }
        Route::AirPlay2Compat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_is_raop() {
        assert_eq!(RouteResolver::resolve(ReceiverCapabilities::default()), Route::Raop);
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
}
