use plist::{Dictionary, Value};
use std::io::Cursor;

pub const ALAC_44100_16_2: u64 = 1u64 << 18;
pub const ALAC_44100_24_2: u64 = 1u64 << 19;
pub const ALAC_48000_16_2: u64 = 1u64 << 20;
pub const ALAC_48000_24_2: u64 = 1u64 << 21;

pub const AIRPLAY_HIRES_AUDIO_FORMATS: u64 = ALAC_44100_24_2 | ALAC_48000_24_2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ap2AudioFormat {
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
}

impl Ap2AudioFormat {
    pub const ALAC_44100_16_STEREO: Self = Self {
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
    };
    pub const ALAC_44100_24_STEREO: Self = Self {
        sample_rate: 44_100,
        bit_depth: 24,
        channels: 2,
    };
    pub const ALAC_48000_16_STEREO: Self = Self {
        sample_rate: 48_000,
        bit_depth: 16,
        channels: 2,
    };
    pub const ALAC_48000_24_STEREO: Self = Self {
        sample_rate: 48_000,
        bit_depth: 24,
        channels: 2,
    };

    pub const fn audio_format_code(self) -> u64 {
        if self.bit_depth > 16 && self.sample_rate >= 48_000 {
            ALAC_48000_24_2
        } else if self.bit_depth > 16 {
            ALAC_44100_24_2
        } else if self.sample_rate >= 48_000 {
            ALAC_48000_16_2
        } else {
            ALAC_44100_16_2
        }
    }

    pub const fn input_bytes_per_sample(self) -> usize {
        if self.bit_depth > 16 { 4 } else { 2 }
    }

    pub const fn alac_bytes_per_sample(self) -> usize {
        if self.bit_depth > 16 { 3 } else { 2 }
    }

    pub const fn input_bytes_per_frame(self) -> usize {
        self.input_bytes_per_sample() * self.channels as usize
    }

    pub const fn alac_bytes_per_frame(self) -> usize {
        self.alac_bytes_per_sample() * self.channels as usize
    }
}

impl Default for Ap2AudioFormat {
    fn default() -> Self {
        Self::ALAC_44100_16_STEREO
    }
}

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
        Ap2AudioFormat::ALAC_44100_16_STEREO.audio_format_code()
    }

    /// Music Assistant treats the two /info stream tables as advisory evidence
    /// and unions the formats from tables the receiver actually published.
    pub fn advertised_formats(&self) -> u64 {
        let realtime = if self.realtime.known { self.realtime.mask } else { 0 };
        let buffered = if self.buffered.known { self.buffered.mask } else { 0 };
        realtime | buffered
    }

    pub fn advertises_hires(&self) -> bool {
        self.advertised_formats() & AIRPLAY_HIRES_AUDIO_FORMATS != 0
    }
}


pub fn select_native_stream_format(
    info: &Ap2Info,
    hires_enabled: bool,
    session_sample_rate: u32,
) -> Ap2AudioFormat {
    if !hires_enabled || !info.advertises_hires() {
        return Ap2AudioFormat::ALAC_44100_16_STEREO;
    }

    match session_sample_rate {
        48_000 => Ap2AudioFormat::ALAC_48000_24_STEREO,
        44_100 => Ap2AudioFormat::ALAC_44100_24_STEREO,
        _ => Ap2AudioFormat::ALAC_44100_24_STEREO,
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

    #[test]
    fn source_audio_format_codes_cover_all_four_native_alac_formats() {
        assert_eq!(Ap2AudioFormat::ALAC_44100_16_STEREO.audio_format_code(), ALAC_44100_16_2);
        assert_eq!(Ap2AudioFormat::ALAC_44100_24_STEREO.audio_format_code(), ALAC_44100_24_2);
        assert_eq!(Ap2AudioFormat::ALAC_48000_16_STEREO.audio_format_code(), ALAC_48000_16_2);
        assert_eq!(Ap2AudioFormat::ALAC_48000_24_STEREO.audio_format_code(), ALAC_48000_24_2);
        assert_eq!(Ap2AudioFormat::ALAC_48000_24_STEREO.input_bytes_per_frame(), 8);
        assert_eq!(Ap2AudioFormat::ALAC_48000_24_STEREO.alac_bytes_per_frame(), 6);
    }


    #[test]
    fn source_policy_selects_hires_only_when_enabled_and_advertised() {
        let info = Ap2Info {
            realtime: AudioFormatCapability {
                mask: ALAC_44100_24_2,
                known: true,
                extended: false,
            },
            buffered: AudioFormatCapability::default(),
        };

        assert_eq!(
            select_native_stream_format(&info, false, 48_000),
            Ap2AudioFormat::ALAC_44100_16_STEREO
        );
        assert_eq!(
            select_native_stream_format(&info, true, 48_000),
            Ap2AudioFormat::ALAC_48000_24_STEREO
        );
        assert_eq!(
            select_native_stream_format(&info, true, 96_000),
            Ap2AudioFormat::ALAC_44100_24_STEREO
        );
    }

    #[test]
    fn source_policy_keeps_unknown_or_16bit_only_receivers_on_baseline() {
        let unknown = Ap2Info::default();
        assert_eq!(
            select_native_stream_format(&unknown, true, 48_000),
            Ap2AudioFormat::ALAC_44100_16_STEREO
        );

        let only_16 = Ap2Info {
            realtime: AudioFormatCapability {
                mask: ALAC_44100_16_2 | ALAC_48000_16_2,
                known: true,
                extended: false,
            },
            buffered: AudioFormatCapability::default(),
        };
        assert_eq!(
            select_native_stream_format(&only_16, true, 48_000),
            Ap2AudioFormat::ALAC_44100_16_STEREO
        );
    }

    #[test]
    fn advertised_formats_union_only_published_tables() {
        let info = Ap2Info {
            realtime: AudioFormatCapability {
                mask: ALAC_44100_16_2,
                known: true,
                extended: false,
            },
            buffered: AudioFormatCapability {
                mask: ALAC_48000_24_2,
                known: true,
                extended: true,
            },
        };
        assert_eq!(
            info.advertised_formats(),
            ALAC_44100_16_2 | ALAC_48000_24_2
        );
        assert!(info.advertises_hires());
    }
}
