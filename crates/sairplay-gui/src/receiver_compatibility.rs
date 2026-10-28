//! Protocol compatibility profiles from advertised capabilities, independent of identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverCompatibility { Standard, LegacyAp2Ntp }

pub fn classify(has_airplay: bool, has_raop: bool, features: u64, flags: u64) -> ReceiverCompatibility {
    // Measured legacy advertisement with working transient-HAP/native-NTP setup.
    // Never infer this from an Apple model, display name, address, host or port.
    if has_airplay && has_raop && features == 0x0000_001e_527f_fff7 && flags == 4 {
        ReceiverCompatibility::LegacyAp2Ntp
    } else { ReceiverCompatibility::Standard }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mitv_capability_profile_has_no_device_identity_inputs() {
        assert_eq!(classify(true,true,0x1e527ffff7,4),ReceiverCompatibility::LegacyAp2Ntp);
    }
    #[test]
    fn mitv_profile_does_not_reclassify_raop_only_or_other_capabilities() {
        for (ap,raop,features,flags) in [
            (false,true,0x1e527ffff7,4),
            (true,false,0x1e527ffff7,4),
            (true,true,0,4),
            (true,true,0x1e527ffff7,0x200),
            (true,true,0x5a7ffff7,4),
        ] {
            assert_eq!(classify(ap,raop,features,flags),ReceiverCompatibility::Standard);
        }
    }
}
