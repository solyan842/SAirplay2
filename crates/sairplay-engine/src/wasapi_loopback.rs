use crate::Pcm352Chunker;
use std::fmt;
use std::ptr::null_mut;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
    WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;
const BLOCK_ALIGN: u16 = CHANNELS * (BITS_PER_SAMPLE / 8);
const AVG_BYTES_PER_SEC: u32 = SAMPLE_RATE * BLOCK_ALIGN as u32;
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
/// convert the endpoint mix to the exact SAirplay2 baseline:
/// PCM signed 16-bit, stereo, 44.1 kHz.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WasapiDrainReport {
    pub frames: usize,
    pub discontinuities: u64,
}

pub struct WasapiLoopbackCapture {
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    // Must be dropped after COM interfaces so CoUninitialize runs last.
    _com: ComGuard,
}

impl WasapiLoopbackCapture {
    pub fn open_default() -> Result<Self, WasapiLoopbackError> {
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

            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: CHANNELS,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: AVG_BYTES_PER_SEC,
                nBlockAlign: BLOCK_ALIGN,
                wBitsPerSample: BITS_PER_SAMPLE,
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
                    // Diagnostic only: Microsoft defines this flag as a capture
                    // glitch indicator. Do not alter PCM/timing here until a
                    // hardware click is correlated with the counter.
                    report.discontinuities = report.discontinuities.saturating_add(1);
                }

                let byte_len = frames as usize * BLOCK_ALIGN as usize;
                if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                    chunker.push(&vec![0u8; byte_len]);
                } else {
                    if data.is_null() && byte_len != 0 {
                        let _ = self.capture_client.ReleaseBuffer(frames);
                        return Err(WasapiLoopbackError::InvalidBuffer);
                    }
                    let bytes = std::slice::from_raw_parts(data as *const u8, byte_len);
                    chunker.push(bytes);
                }

                self.capture_client
                    .ReleaseBuffer(frames)
                    .map_err(|e| WasapiLoopbackError::Windows(format!(
                        "ReleaseBuffer failed: {e}"
                    )))?;

                report.frames = report.frames.saturating_add(frames as usize);
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
