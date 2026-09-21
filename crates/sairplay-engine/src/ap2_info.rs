use plist::{Dictionary, Value};
use std::io::Cursor;

pub const ALAC_44100_16_2: u64 = 1u64 << 18;
pub const ALAC_44100_24_2: u64 = 1u64 << 19;
pub const ALAC_48000_16_2: u64 = 1u64 << 20;
pub const ALAC_48000_24_2: u64 = 1u64 << 21;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioFormatCapability {
    pub mask: u64,
    pub known: bool,
    pub extended: bool,
}

impl AudioFormatCapability {
    /// The reference implementation treats /info tables as advisory.
    /// Missing tables must not reject a format, and a present bit is not
    /// proof that hardware will render it correctly.
    pub fn advertises(&self, format: u64) -> bool {
        self.known && (self.mask & format) != 0
    }

    pub fn allows_probe(&self, format: u64) -> bool {
        !self.known || self.advertises(format)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ap2Info {
    pub realtime: AudioFormatCapability,
    pub buffered: AudioFormatCapability,
}

#[derive(Debug)]
pub enum Ap2InfoError {
    InvalidPlist(plist::Error),
    RootNotDictionary,
    InvalidFormatTable(&'static str),
}

impl From<plist::Error> for Ap2InfoError {
    fn from(value: plist::Error) -> Self {
        Self::InvalidPlist(value)
    }
}

impl Ap2Info {
    pub fn parse(body: &[u8]) -> Result<Self, Ap2InfoError> {
        let value = Value::from_reader(Cursor::new(body))?;
        let root = value
            .as_dictionary()
            .ok_or(Ap2InfoError::RootNotDictionary)?;

        Ok(Self {
            realtime: parse_stream_capability(root, "audioStream")?,
            buffered: parse_stream_capability(root, "bufferStream")?,
        })
    }

    pub fn requested_first_stable_format() -> u64 {
        ALAC_44100_16_2
    }
}

fn parse_stream_capability(
    root: &Dictionary,
    stream_key: &'static str,
) -> Result<AudioFormatCapability, Ap2InfoError> {
    if let Some(extended_root) = root.get("supportedAudioFormatsExtended") {
        let dict = extended_root
            .as_dictionary()
            .ok_or(Ap2InfoError::InvalidFormatTable("supportedAudioFormatsExtended"))?;

        if let Some(value) = dict.get(stream_key) {
            let array = value
                .as_array()
                .ok_or(Ap2InfoError::InvalidFormatTable(stream_key))?;

            let mut mask = 0u64;
            for bit in array {
                let bit = bit
                    .as_unsigned_integer()
                    .ok_or(Ap2InfoError::InvalidFormatTable(stream_key))?;
                if bit < 64 {
                    mask |= 1u64 << bit;
                }
            }

            return Ok(AudioFormatCapability {
                mask,
                known: true,
                extended: true,
            });
        }
    }

    if let Some(legacy_root) = root.get("supportedFormats") {
        let dict = legacy_root
            .as_dictionary()
            .ok_or(Ap2InfoError::InvalidFormatTable("supportedFormats"))?;

        if let Some(value) = dict.get(stream_key) {
            let mask = value
                .as_unsigned_integer()
                .ok_or(Ap2InfoError::InvalidFormatTable(stream_key))?;

            return Ok(AudioFormatCapability {
                mask,
                known: true,
                extended: false,
            });
        }
    }

    Ok(AudioFormatCapability::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_plist(root: Dictionary) -> Vec<u8> {
        let value = Value::Dictionary(root);
        let mut out = Vec::new();
        value.to_writer_binary(&mut out).unwrap();
        out
    }

    #[test]
    fn extended_table_has_priority_over_legacy_mask() {
        let mut ext = Dictionary::new();
        ext.insert(
            "audioStream".into(),
            Value::Array(vec![Value::Integer(18u64.into()), Value::Integer(20u64.into())]),
        );

        let mut legacy = Dictionary::new();
        legacy.insert(
            "audioStream".into(),
            Value::Integer(ALAC_44100_24_2.into()),
        );

        let mut root = Dictionary::new();
        root.insert("supportedAudioFormatsExtended".into(), Value::Dictionary(ext));
        root.insert("supportedFormats".into(), Value::Dictionary(legacy));

        let info = Ap2Info::parse(&binary_plist(root)).unwrap();
        assert!(info.realtime.known);
        assert!(info.realtime.extended);
        assert_eq!(
            info.realtime.mask,
            ALAC_44100_16_2 | ALAC_48000_16_2
        );
        assert!(!info.realtime.advertises(ALAC_44100_24_2));
    }

    #[test]
    fn legacy_mask_is_used_when_extended_table_is_absent() {
        let mut formats = Dictionary::new();
        formats.insert(
            "audioStream".into(),
            Value::Integer((ALAC_44100_16_2 | ALAC_44100_24_2).into()),
        );
        formats.insert(
            "bufferStream".into(),
            Value::Integer(ALAC_44100_16_2.into()),
        );

        let mut root = Dictionary::new();
        root.insert("supportedFormats".into(), Value::Dictionary(formats));

        let info = Ap2Info::parse(&binary_plist(root)).unwrap();
        assert_eq!(info.realtime.mask, ALAC_44100_16_2 | ALAC_44100_24_2);
        assert!(!info.realtime.extended);
        assert_eq!(info.buffered.mask, ALAC_44100_16_2);
    }

    #[test]
    fn absent_table_is_unknown_not_unsupported() {
        let info = Ap2Info::parse(&binary_plist(Dictionary::new())).unwrap();
        assert!(!info.realtime.known);
        assert_eq!(info.realtime.mask, 0);
        assert!(info.realtime.allows_probe(ALAC_44100_16_2));
    }

    #[test]
    fn first_stable_target_is_44100_16_stereo_alac() {
        assert_eq!(Ap2Info::requested_first_stable_format(), 0x40000);
    }
}
