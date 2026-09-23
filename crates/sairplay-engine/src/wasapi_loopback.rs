use crate::{Ap2AudioFormat, Pcm352Chunker};
use std::fmt;
use std::ptr::null_mut;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
    WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

const CHANNELS: u16 = 2;
const BUFFER_DURATION_100NS: i64 = 1_000_000; // 100 ms

#[derive(Debug)]
pub enum WasapiLoopbackError {
    Windows(String),
    InvalidBuffer,
}

impl fmt::Display for WasapiLoopbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Windows(message) => write!(f, "{message}"),
            Self::InvalidBuffer => write!(f, "WASAPI returned an invalid capture buffer"),
        }
    }
}

impl std::error::Error for WasapiLoopbackError {}

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
/// The stream is opened in shared mode and asks the Windows audio engine to
/// convert the endpoint mix to the exact transport format selected for the
/// native AirPlay 2 session. 16-bit uses s16le; 24-bit follows airplay-cli's
/// contract and uses an s32le carrier which is truncated to packed s24le
/// immediately before the ALAC encoder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WasapiDrainReport {
    pub frames: usize,
    pub discontinuities: u64,
    pub discontinuity_frame_offset: Option<u64>,
    pub first_non_silent_frame_offset: Option<u64>,
    pub first_nonzero_frame_offset: Option<u64>,
}

pub struct WasapiLoopbackCapture {
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    bytes_per_frame: usize,
    // Must be dropped after COM interfaces so CoUninitialize runs last.
    _com: ComGuard,
}

impl WasapiLoopbackCapture {
    /// Return the Windows shared-mode render-engine mix sample rate.
    ///
    /// This is the closest Windows equivalent of Music Assistant's shared
    /// session PCM rate: the source/session clock is chosen first, and an
    /// AirPlay 2 hi-res receiver follows it when that rate is one of the
    /// supported 44.1/48 kHz rates.
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

        let container_bits: u16 = if audio_format.bit_depth > 16 { 32 } else { 16 };
        let block_align: u16 = CHANNELS * (container_bits / 8);
        let avg_bytes_per_sec: u32 = audio_format.sample_rate * block_align as u32;

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

            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: CHANNELS,
                nSamplesPerSec: audio_format.sample_rate,
                nAvgBytesPerSec: avg_bytes_per_sec,
                nBlockAlign: block_align,
                wBitsPerSample: container_bits,
                cbSize: 0,
            };

            let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

            audio_client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    BUFFER_DURATION_100NS,
                    0,
                    &format,
                    None,
                )
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "WASAPI loopback Initialize failed: {e}"
                )))?;

            let capture_client = audio_client
                .GetService::<IAudioCaptureClient>()
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "IAudioCaptureClient failed: {e}"
                )))?;

            audio_client
                .Start()
                .map_err(|e| WasapiLoopbackError::Windows(format!(
                    "WASAPI loopback Start failed: {e}"
                )))?;

            Ok(Self {
                audio_client,
                capture_client,
                bytes_per_frame: block_align as usize,
                _com: com,
            })
        }
    }

    /// Drain every currently available WASAPI packet into the fixed 352-frame chunker.
    /// Returns the number of PCM frames copied (silent frames included).
    pub fn drain_into(
        &self,
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
                    // Diagnostic only: Microsoft defines this flag as either a
                    // stream-state transition or a timing glitch. Record where
                    // it occurred without modifying PCM or sender behavior.
                    report.discontinuities = report.discontinuities.saturating_add(1);
                    if report.discontinuity_frame_offset.is_none() {
                        report.discontinuity_frame_offset = Some(drained_before);
                    }
                }

                let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                if !silent && report.first_non_silent_frame_offset.is_none() {
                    report.first_non_silent_frame_offset = Some(drained_before);
                }

                let byte_len = frames as usize * self.bytes_per_frame;
                if silent {
                    chunker.push(&vec![0u8; byte_len]);
                } else {
                    if data.is_null() && byte_len != 0 {
                        let _ = self.capture_client.ReleaseBuffer(frames);
                        return Err(WasapiLoopbackError::InvalidBuffer);
                    }
                    let bytes = std::slice::from_raw_parts(data as *const u8, byte_len);
                    if report.first_nonzero_frame_offset.is_none() {
                        for (frame_index, frame) in bytes.chunks_exact(self.bytes_per_frame).enumerate() {
                            if frame.iter().any(|byte| *byte != 0) {
                                report.first_nonzero_frame_offset =
                                    Some(drained_before.saturating_add(frame_index as u64));
                                break;
                            }
                        }
                    }
                    chunker.push(bytes);
                }

                self.capture_client
                    .ReleaseBuffer(frames)
                    .map_err(|e| WasapiLoopbackError::Windows(format!(
                        "ReleaseBuffer failed: {e}"
                    )))?;

                report.frames = report.frames.saturating_add(frames as usize);
                drained_before = drained_before.saturating_add(frames as u64);
            }
        }

        Ok(report)
    }
}

impl Drop for WasapiLoopbackCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.audio_client.Stop();
        }
    }
}
