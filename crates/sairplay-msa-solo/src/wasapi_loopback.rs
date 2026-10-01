use crate::pcm_bridge::{InputPcmFormat, InputSampleKind, PcmBridge, PcmBridgeError};
use crate::{Ap2AudioFormat, Pcm352Chunker};
use std::fmt;
use std::ptr::null_mut;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

const BUFFER_DURATION_100NS: i64 = 1_000_000; // 100 ms
const WAVE_FORMAT_PCM_TAG: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xfffe;
const WAVEFORMATEX_BASE_BYTES: usize = 18;
const WAVEFORMATEXTENSIBLE_EXTRA_BYTES: usize = 22;

#[derive(Debug)]
pub enum WasapiLoopbackError {
    Windows(String),
    InvalidBuffer,
    UnsupportedFormat(String),
    Conversion(String),
}

impl fmt::Display for WasapiLoopbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Windows(message) => write!(f, "{message}"),
            Self::InvalidBuffer => write!(f, "WASAPI returned an invalid capture buffer"),
            Self::UnsupportedFormat(message) => write!(f, "{message}"),
            Self::Conversion(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for WasapiLoopbackError {}

impl From<PcmBridgeError> for WasapiLoopbackError {
    fn from(value: PcmBridgeError) -> Self {
        Self::Conversion(value.to_string())
    }
}

struct ComGuard {
    initialized: bool,
}

impl ComGuard {
    fn enter() -> Result<Self, WasapiLoopbackError> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| WasapiLoopbackError::Windows(format!("CoInitializeEx failed: {e}")))?;
        }
        Ok(Self { initialized: true })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

/// Full-system WASAPI loopback capture from the default Windows render endpoint.
///
/// Architecture lock: Music Assistant converts source audio to cliairplay's raw
/// PCM stdin format *before* the AirPlay transport. This Windows adapter mirrors
/// that contract. It opens loopback in the endpoint's real shared-engine mix
/// format (GetMixFormat), never asks WASAPI to change/auto-convert the endpoint
/// to an AirPlay format, then converts captured PCM privately into the exact
/// 44.1/48 kHz s16le/s32le carrier required by the MSA SOLO transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WasapiDrainReport {
    /// Source frames drained from the Windows audio engine.
    pub frames: usize,
    /// Target PCM frames produced after private conversion/resampling.
    pub output_frames: usize,
    pub silent_frames: usize,
    pub discontinuities: u64,
    pub discontinuity_frame_offset: Option<u64>,
}

pub struct WasapiLoopbackCapture {
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    input_bytes_per_frame: usize,
    bridge: PcmBridge,
    summary: String,
    // Must be dropped after COM interfaces so CoUninitialize runs last.
    _com: ComGuard,
}

impl WasapiLoopbackCapture {
    /// Return the Windows shared-mode render-engine mix sample rate.
    pub fn default_render_mix_sample_rate() -> Result<u32, WasapiLoopbackError> {
        let _com = ComGuard::enter()?;

        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| WasapiLoopbackError::Windows(format!(
                        "MMDeviceEnumerator failed: {e}"
                    )))?;

            let endpoint = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "default render endpoint failed: {e}"
                )))?;

            let audio_client: IAudioClient = endpoint
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "IAudioClient activation failed: {e}"
                )))?;

            let mix_format = audio_client
                .GetMixFormat()
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "IAudioClient GetMixFormat failed: {e}"
                )))?;

            let sample_rate = (*mix_format).nSamplesPerSec;
            CoTaskMemFree(Some(mix_format.cast()));
            Ok(sample_rate)
        }
    }

    pub fn open_default() -> Result<Self, WasapiLoopbackError> {
        Self::open_default_for_format(Ap2AudioFormat::ALAC_44100_16_STEREO)
    }

    pub fn open_default_for_format(
        audio_format: Ap2AudioFormat,
    ) -> Result<Self, WasapiLoopbackError> {
        let com = ComGuard::enter()?;

        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| WasapiLoopbackError::Windows(format!(
                        "MMDeviceEnumerator failed: {e}"
                    )))?;

            let endpoint = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "default render endpoint failed: {e}"
                )))?;

            let audio_client: IAudioClient = endpoint
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "IAudioClient activation failed: {e}"
                )))?;

            let mix_format = audio_client
                .GetMixFormat()
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "IAudioClient GetMixFormat failed: {e}"
                )))?;

            let input_format = match parse_mix_format(mix_format) {
                Ok(v) => v,
                Err(error) => {
                    CoTaskMemFree(Some(mix_format.cast()));
                    return Err(error);
                }
            };

            // Do not use AUTOCONVERTPCM/SRC_DEFAULT_QUALITY here. Those flags
            // make the WASAPI stream format part of the conversion contract,
            // while MSA's conversion belongs upstream of cliairplay. Opening
            // loopback with GetMixFormat keeps the Windows shared engine in its
            // own native mix format and makes this capture read-only in format.
            let initialize = audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_DURATION_100NS,
                0,
                &*mix_format,
                None,
            );
            CoTaskMemFree(Some(mix_format.cast()));
            initialize.map_err(|e| WasapiLoopbackError::Windows(format!(
                "WASAPI loopback Initialize(GetMixFormat) failed: {e}"
            )))?;

            let capture_client = audio_client
                .GetService::<IAudioCaptureClient>()
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "IAudioCaptureClient failed: {e}"
                )))?;

            let bridge = PcmBridge::new(input_format, audio_format)?;
            let target_carrier = if audio_format.bit_depth <= 16 { "s16le" } else { "s32le-carrier" };
            let summary = format!(
                "WASAPI mix {} -> MSA PCM {} Hz / {} / {}ch",
                input_format.describe(),
                audio_format.sample_rate,
                target_carrier,
                audio_format.channels
            );

            audio_client
                .Start()
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "WASAPI loopback Start failed: {e}"
                )))?;

            Ok(Self {
                audio_client,
                capture_client,
                input_bytes_per_frame: input_format.bytes_per_frame,
                bridge,
                summary,
                _com: com,
            })
        }
    }

    pub fn format_summary(&self) -> &str {
        &self.summary
    }

    /// Flush only conversion history. The endpoint capture stays alive, exactly
    /// as the persistent cliairplay stdin stays alive while MSA replaces its
    /// per-seek FFmpeg converter.
    pub fn reset_conversion(&mut self) {
        self.bridge.reset();
    }

    /// Drain every currently available WASAPI packet, privately normalize it
    /// to the MSA transport PCM format, and append that PCM to the 352-frame
    /// packetizer.
    pub fn drain_into(
        &mut self,
        chunker: &mut Pcm352Chunker,
    ) -> Result<WasapiDrainReport, WasapiLoopbackError> {
        let mut report = WasapiDrainReport::default();
        let mut drained_before = 0u64;

        unsafe {
            loop {
                let next = self.capture_client
                    .GetNextPacketSize()
                    .map_err(|e| WasapiLoopbackError::Windows(format!(
                        "GetNextPacketSize failed: {e}"
                    )))?;

                if next == 0 {
                    break;
                }

                let mut data: *mut u8 = null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;

                self.capture_client
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(|e| WasapiLoopbackError::Windows(format!(
                        "GetBuffer failed: {e}"
                    )))?;

                if (flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0 {
                    // Diagnostic only. Timeline recovery remains owned by the
                    // MSA media loop; the capture adapter never invents a new
                    // AirPlay anchor from a Windows flag.
                    report.discontinuities = report.discontinuities.saturating_add(1);
                    if report.discontinuity_frame_offset.is_none() {
                        report.discontinuity_frame_offset = Some(drained_before);
                    }
                }

                let source_frames = frames as usize;
                let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                let byte_len = source_frames.saturating_mul(self.input_bytes_per_frame);

                let produced = if silent {
                    report.silent_frames = report.silent_frames.saturating_add(source_frames);
                    self.bridge.push_packet(&[], source_frames, true, chunker)
                } else {
                    if data.is_null() && byte_len != 0 {
                        let _ = self.capture_client.ReleaseBuffer(frames);
                        return Err(WasapiLoopbackError::InvalidBuffer);
                    }
                    let bytes = std::slice::from_raw_parts(data as *const u8, byte_len);
                    self.bridge.push_packet(bytes, source_frames, false, chunker)
                };

                let produced = match produced {
                    Ok(v) => v,
                    Err(error) => {
                        let _ = self.capture_client.ReleaseBuffer(frames);
                        return Err(error.into());
                    }
                };

                self.capture_client
                    .ReleaseBuffer(frames)
                    .map_err(|e| WasapiLoopbackError::Windows(format!(
                        "ReleaseBuffer failed: {e}"
                    )))?;

                report.frames = report.frames.saturating_add(source_frames);
                report.output_frames = report.output_frames.saturating_add(produced);
                drained_before = drained_before.saturating_add(source_frames as u64);
            }
        }

        Ok(report)
    }
}

unsafe fn parse_mix_format(
    format: *const WAVEFORMATEX,
) -> Result<InputPcmFormat, WasapiLoopbackError> {
    if format.is_null() {
        return Err(WasapiLoopbackError::UnsupportedFormat(
            "GetMixFormat returned null".into(),
        ));
    }
    let base = &*format;
    let channels = base.nChannels;
    let sample_rate = base.nSamplesPerSec;
    let block_align = base.nBlockAlign as usize;
    let bits = base.wBitsPerSample;
    let tag = base.wFormatTag;

    if channels == 0 || sample_rate == 0 || block_align == 0 || bits == 0 {
        return Err(WasapiLoopbackError::UnsupportedFormat(format!(
            "invalid Windows mix format tag={tag:#06x} rate={sample_rate} channels={channels} block_align={block_align} bits={bits}"
        )));
    }
    if block_align % channels as usize != 0 {
        return Err(WasapiLoopbackError::UnsupportedFormat(format!(
            "Windows mix block alignment {block_align} is not divisible by {channels} channels"
        )));
    }

    let (effective_tag, valid_bits) = if tag == WAVE_FORMAT_EXTENSIBLE_TAG {
        if base.cbSize as usize < WAVEFORMATEXTENSIBLE_EXTRA_BYTES {
            return Err(WasapiLoopbackError::UnsupportedFormat(format!(
                "WAVE_FORMAT_EXTENSIBLE cbSize={} is smaller than {}",
                base.cbSize, WAVEFORMATEXTENSIBLE_EXTRA_BYTES
            )));
        }
        let total = WAVEFORMATEX_BASE_BYTES + base.cbSize as usize;
        let raw = std::slice::from_raw_parts(format as *const u8, total);
        if raw.len() < WAVEFORMATEX_BASE_BYTES + WAVEFORMATEXTENSIBLE_EXTRA_BYTES {
            return Err(WasapiLoopbackError::UnsupportedFormat(
                "truncated WAVEFORMATEXTENSIBLE".into(),
            ));
        }
        let valid = u16::from_le_bytes([raw[18], raw[19]]);
        // SubFormat is a GUID at offset 24. Standard wave subformats embed the
        // original WAVE_FORMAT_* tag in GUID.Data1.
        let sub_tag = u32::from_le_bytes([raw[24], raw[25], raw[26], raw[27]]);
        if sub_tag > u16::MAX as u32 {
            return Err(WasapiLoopbackError::UnsupportedFormat(format!(
                "unsupported Windows mix subformat GUID data1={sub_tag:#010x}"
            )));
        }
        (sub_tag as u16, if valid == 0 { bits } else { valid })
    } else {
        (tag, bits)
    };

    let kind = match (effective_tag, bits) {
        (WAVE_FORMAT_PCM_TAG, 16) => InputSampleKind::I16,
        (WAVE_FORMAT_PCM_TAG, 24) => InputSampleKind::I24,
        (WAVE_FORMAT_PCM_TAG, 32) => InputSampleKind::I32,
        (WAVE_FORMAT_IEEE_FLOAT_TAG, 32) => InputSampleKind::F32,
        (WAVE_FORMAT_IEEE_FLOAT_TAG, 64) => InputSampleKind::F64,
        _ => {
            return Err(WasapiLoopbackError::UnsupportedFormat(format!(
                "unsupported Windows mix format tag={effective_tag:#06x} bits={bits} valid_bits={valid_bits}"
            )));
        }
    };

    let expected_sample_bytes = match kind {
        InputSampleKind::I16 => 2,
        InputSampleKind::I24 => 3,
        InputSampleKind::I32 | InputSampleKind::F32 => 4,
        InputSampleKind::F64 => 8,
    };
    let actual_sample_bytes = block_align / channels as usize;
    if actual_sample_bytes != expected_sample_bytes {
        return Err(WasapiLoopbackError::UnsupportedFormat(format!(
            "Windows mix sample container mismatch: tag={effective_tag:#06x} bits={bits} says {expected_sample_bytes} B/sample but block alignment says {actual_sample_bytes}"
        )));
    }

    Ok(InputPcmFormat {
        sample_rate,
        channels,
        bytes_per_frame: block_align,
        bits_per_sample: bits,
        valid_bits_per_sample: valid_bits.min(bits),
        kind,
    })
}

impl Drop for WasapiLoopbackCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.audio_client.Stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_pcm_mix_parser_is_explicit() {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM_TAG,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 192_000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let parsed = unsafe { parse_mix_format(&format) }.unwrap();
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.kind, InputSampleKind::I16);
    }

    #[test]
    fn unsupported_pcm_width_fails_closed() {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM_TAG,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 96_000,
            nBlockAlign: 2,
            wBitsPerSample: 8,
            cbSize: 0,
        };
        assert!(unsafe { parse_mix_format(&format) }.is_err());
    }
}
