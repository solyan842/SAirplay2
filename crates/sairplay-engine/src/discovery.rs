use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    InvalidFeatureMask(String),
    InvalidFlags(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AirPlayTxt {
    pub fields: BTreeMap<String, String>,
    pub features: u64,
    pub flags: u64,
    pub password_required: bool,
    pub encryption_types: Vec<String>,
    pub model: Option<String>,
}

impl AirPlayTxt {
    pub fn parse<I, K, V>(pairs: I) -> Result<Self, DiscoveryError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut fields = BTreeMap::new();
        for (k, v) in pairs {
            fields.insert(k.into().to_ascii_lowercase(), v.into());
        }

        let features = fields
            .get("features")
            .or_else(|| fields.get("ft"))
            .map(|v| parse_feature_mask(v))
            .transpose()?
            .unwrap_or(0);

        let flags = fields
            .get("flags")
            .or_else(|| fields.get("sf"))
            .map(|v| parse_u64_auto(v).map_err(|_| DiscoveryError::InvalidFlags(v.clone())))
            .transpose()?
            .unwrap_or(0);

        let password_required = fields
            .get("pw")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let encryption_types = fields
            .get("et")
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();

        let model = fields
            .get("model")
            .or_else(|| fields.get("am"))
            .cloned();

        Ok(Self {
            fields,
            features,
            flags,
            password_required,
            encryption_types,
            model,
        })
    }

    pub fn feature(&self, bit: u8) -> bool {
        bit < 64 && (self.features & (1u64 << bit)) != 0
    }

    pub fn supports_airplay2(&self) -> bool {
        self.feature(38) || self.feature(48)
    }

    pub fn supports_pairing(&self) -> bool {
        self.feature(46) || self.feature(48)
    }

    pub fn supports_ptp(&self) -> bool {
        self.feature(41)
    }

    pub fn supports_buffered_audio(&self) -> bool {
        self.feature(40)
    }

    pub fn pin_required(&self) -> bool {
        self.flags & 0x8 != 0
    }

    pub fn legacy_pairing(&self) -> bool {
        self.flags & 0x200 != 0
    }

    pub fn supports_auth_setup(&self) -> bool {
        self.encryption_types.iter().any(|v| v == "4")
    }
}

fn parse_feature_mask(value: &str) -> Result<u64, DiscoveryError> {
    let trimmed = value.trim();

    if let Some((low, high)) = trimmed.split_once(',') {
        let low = parse_u64_auto(low)
            .map_err(|_| DiscoveryError::InvalidFeatureMask(value.to_string()))?;
        let high = parse_u64_auto(high)
            .map_err(|_| DiscoveryError::InvalidFeatureMask(value.to_string()))?;
        return Ok((high << 32) | (low & 0xffff_ffff));
    }

    parse_u64_auto(trimmed)
        .map_err(|_| DiscoveryError::InvalidFeatureMask(value.to_string()))
}

fn parse_u64_auto(value: &str) -> Result<u64, std::num::ParseIntError> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        value.parse::<u64>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_word_feature_mask() {
        // bit 38 lives in high word bit 6.
        let txt = AirPlayTxt::parse([("features", "0x0,0x40")]).unwrap();
        assert!(txt.feature(38));
        assert!(txt.supports_airplay2());
    }

    #[test]
    fn parses_reference_capability_bits_and_flags() {
        let mask = (1u64 << 38) | (1u64 << 40) | (1u64 << 41) | (1u64 << 46) | (1u64 << 48);
        let txt = AirPlayTxt::parse([
            ("features", mask.to_string()),
            ("flags", "0x208".to_string()),
            ("pw", "true".to_string()),
            ("et", "0,4".to_string()),
            ("model", "AudioAccessory".to_string()),
        ]).unwrap();

        assert!(txt.supports_airplay2());
        assert!(txt.supports_pairing());
        assert!(txt.supports_ptp());
        assert!(txt.supports_buffered_audio());
        assert!(txt.pin_required());
        assert!(txt.legacy_pairing());
        assert!(txt.password_required);
        assert!(txt.supports_auth_setup());
        assert_eq!(txt.model.as_deref(), Some("AudioAccessory"));
    }

    #[test]
    fn accepts_ft_and_sf_aliases() {
        let mask = 1u64 << 38;
        let txt = AirPlayTxt::parse([
            ("ft", mask.to_string()),
            ("sf", "8".to_string()),
        ]).unwrap();
        assert!(txt.supports_airplay2());
        assert!(txt.pin_required());
    }
}
