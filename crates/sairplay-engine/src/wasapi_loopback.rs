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
    pub packets: usize,
    pub discontinuities: u64,
    pub discontinuity_frame_offset: Option<u64>,
    pub discontinuity_discarded_bytes: usize,
    pub discontinuity_discarded_nonzero_bytes: usize,
    pub discontinuity_packet_frames: Option<u32>,
    pub discontinuity_packet_silent: Option<bool>,
    pub discontinuity_packet_nonzero: Option<bool>,
    pub first_non_silent_frame_offset: Option<u64>,
    pub first_nonzero_frame_offset: Option<u64>,
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

                let discontinuity =
                    (flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0;
                let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;

                if discontinuity {
                    // Microsoft defines DATA_DISCONTINUITY as a capture glitch
                    // or stream transition: this packet is not position-
                    // continuous with the preceding packet. Never concatenate a
                    // partial PCM packet from the old capture epoch with this
                    // new epoch, because that creates an artificial waveform
                    // edge before ALAC encoding. Cut only the local PCM queue;
                    // AirPlay RTP/anchor/crypto state lives above this layer and
                    // remains untouched.
                    report.discontinuities = report.discontinuities.saturating_add(1);
                    if report.discontinuity_frame_offset.is_none() {
                        report.discontinuity_frame_offset = Some(drained_before);
                    }
                    let pending = chunker.pending_bytes();
                    let pending_nonzero = chunker.pending_nonzero_bytes();
                    report.discontinuity_discarded_bytes =
                        report.discontinuity_discarded_bytes.saturating_add(pending);
                    report.discontinuity_discarded_nonzero_bytes =
                        report.discontinuity_discarded_nonzero_bytes.saturating_add(pending_nonzero);
                    chunker.clear();
                    report.discontinuity_packet_frames = Some(frames);
                    report.discontinuity_packet_silent = Some(silent);
                }

                if !silent && report.first_non_silent_frame_offset.is_none() {
                    report.first_non_silent_frame_offset = Some(drained_before);
                }

                let byte_len = frames as usize * BLOCK_ALIGN as usize;
                if silent {
                    if discontinuity {
                        report.discontinuity_packet_nonzero = Some(false);
                    }
                    chunker.push(&vec![0u8; byte_len]);
                } else {
                    if data.is_null() && byte_len != 0 {
                        let _ = self.capture_client.ReleaseBuffer(frames);
                        return Err(WasapiLoopbackError::InvalidBuffer);
                    }
                    let bytes = std::slice::from_raw_parts(data as *const u8, byte_len);
                    let packet_has_nonzero = bytes.iter().any(|byte| *byte != 0);
                    if discontinuity {
                        report.discontinuity_packet_nonzero = Some(packet_has_nonzero);
                    }
                    if report.first_nonzero_frame_offset.is_none() {
                        for (frame_index, frame) in bytes.chunks_exact(BLOCK_ALIGN as usize).enumerate() {
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
                report.packets = report.packets.saturating_add(1);
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
