//! Independent Music Assistant compatible SOLO engine.
//! Source: music-assistant/airplay-cli @ 431c5c582eef9307c4e39c50a0ea65e970bc1128
//! Legacy SAirplay Solo/MultiRoom remain frozen.

pub mod native_timeline;
pub mod native_media;
pub mod native_sync;
pub mod native_rtx;
pub mod native_runtime;
pub mod native_codec;
pub mod native_io;
pub mod native_media_owner;
pub mod native_control;
pub mod native_solo;
pub mod pcm_chunker;
#[cfg(windows)]
pub mod wasapi_loopback;
#[cfg(windows)]
pub mod windows_audio_worker;
#[cfg(windows)]
pub mod windows_raop_session;
pub mod native_rtx_worker;
pub mod feedback;
pub mod native_commands;
pub mod native_parameters;
pub mod teardown;
pub mod native_metadata;
pub mod volume;
pub mod mrp;
pub mod mrp_event;
pub mod mrp_datastream;
pub mod native_timing_owner;
pub mod ptp_engine;
pub mod ntp_timing;
pub mod event_channel;
pub mod setpeers;
pub mod stream_setup;
pub mod record;
pub mod ptp_session_setup;
pub mod ntp_session_setup;
pub mod native_preflight;
pub mod preflight;
pub mod ap2_info;
pub mod hap_pairing;
pub mod hap_rtsp;
pub mod hap_crypto;
pub mod hap_srp;
pub mod hap_tlv8;
pub mod rtsp;
pub mod native_connect;
pub mod clock;
pub mod owned_session;
pub mod persistent_input;
pub mod ap2;
pub mod raop;

pub mod route;
pub mod timing;
pub mod audio_format;
pub mod pcm_ring;
use pcm_ring::PcmRing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState { Idle, Playing, Standby, Ended }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route { Raop, AirPlay2Compat, AirPlay2Native }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartAck { pub requested_unix_ms: u64, pub at_unix_ms: u64 }

pub trait SoloTransport {
    type Error;
    fn quiesce(&mut self);
    fn flush(&mut self) -> Result<(), Self::Error>;
    fn commit_start(&mut self, requested_unix_ms: u64) -> Result<u64, Self::Error>;
    fn resume(&mut self);
    fn standby(&mut self) -> Result<(), Self::Error>;
    fn disconnect(&mut self);
}

pub struct SoloSession<T: SoloTransport> {
    transport: T,
    state: SessionState,
    epoch: u64,
    pcm: PcmRing,
    ready_bytes: usize,
    audio_seen: bool,
}

impl<T: SoloTransport> SoloSession<T> {
    pub fn new(transport: T) -> Self {
        Self::with_byte_rate(transport, 176_400)
    }

    pub fn with_byte_rate(transport: T, byte_rate: usize) -> Self {
        Self::with_ready_bytes(transport, byte_rate, 1)
    }
    pub fn with_ready_bytes(transport: T, byte_rate: usize, ready_bytes: usize) -> Self {
        let pcm = PcmRing::for_byte_rate(byte_rate);
        assert!(ready_bytes > 0 && ready_bytes <= pcm.capacity());
        Self { transport, state: SessionState::Idle, epoch: 0, pcm, ready_bytes, audio_seen: false }
    }
    pub fn state(&self) -> SessionState { self.state }
    pub fn epoch(&self) -> u64 { self.epoch }
    pub fn buffered_bytes(&self) -> usize { self.pcm.fill() }

    pub fn buffer_pcm(&mut self, input: &[u8]) -> usize {
        if self.state == SessionState::Ended { return 0; }
        let n = self.pcm.push(input);
        if !self.audio_seen && self.pcm.fill() >= self.ready_bytes { self.audio_seen = true; }
        n
    }

    pub fn audio_ready(&self) -> bool { self.audio_seen }

    pub fn poll_idle_timeout(&mut self, elapsed_idle_ms: u64, idle_timeout_ms: u64) -> bool {
        if idle_timeout_ms == 0 { return false; }
        let idle = self.state == SessionState::Idle
            || self.state == SessionState::Standby
            || (self.state == SessionState::Playing && self.pcm.eof());
        if idle && elapsed_idle_ms >= idle_timeout_ms {
            self.state = SessionState::Ended;
            return true;
        }
        false
    }

    pub fn mark_input_eof(&mut self) { self.pcm.mark_eof(); }

    /// Mirrors ap2_session_read: media is exposed only while PLAYING.
    /// A final short buffer is returned at EOF; empty EOF is terminal.
    pub fn read_pcm(&mut self, output: &mut [u8]) -> Option<usize> {
        if self.state != SessionState::Playing { return Some(0); }
        if self.pcm.fill() >= output.len() || (self.pcm.eof() && self.pcm.fill() > 0) {
            return Some(self.pcm.pop(output));
        }
        if self.pcm.eof() { return None; }
        Some(0)
    }

    pub fn discard_pcm(&mut self, want: usize) -> Option<usize> {
        if self.state != SessionState::Playing { return Some(0); }
        if self.pcm.fill() > 0 { return Some(self.pcm.discard(want)); }
        if self.pcm.eof() { return None; }
        Some(0)
    }

    pub fn start(&mut self, requested_unix_ms: u64) -> Result<StartAck, T::Error> {
        self.transport.quiesce();
        let result = self.transport.commit_start(requested_unix_ms);
        self.transport.resume();
        let at_unix_ms = result?;
        self.state = SessionState::Playing;
        self.epoch = self.epoch.wrapping_add(1);
        Ok(StartAck { requested_unix_ms, at_unix_ms })
    }

    pub fn flush(&mut self) -> Result<(), T::Error> {
        self.transport.quiesce();
        let result = self.transport.flush();
        if result.is_err() {
            self.transport.resume();
        }
        result?;
        // MSA keeps sends quiesced while the reader is parked, then resets the
        // ring and drains pre-FLUSH input. This pure owner has no fd yet; the
        // Windows source adapter supplies that drain handshake before resume.
        self.pcm.reset();
        self.audio_seen = false;
        self.state = SessionState::Idle;
        self.transport.resume();
        Ok(())
    }

    pub fn standby(&mut self) -> Result<(), T::Error> {
        self.transport.quiesce();
        let result = self.transport.standby();
        self.transport.resume();
        result?;
        self.state = SessionState::Standby;
        Ok(())
    }

    pub fn end(&mut self) {
        self.transport.disconnect();
        self.state = SessionState::Ended;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTransport { scheduled: u64 }

    impl SoloTransport for FakeTransport {
        type Error = &'static str;
        fn quiesce(&mut self) {}
        fn flush(&mut self) -> Result<(), Self::Error> { Ok(()) }
        fn commit_start(&mut self, requested: u64) -> Result<u64, Self::Error> {
            Ok(if self.scheduled == 0 { requested } else { self.scheduled })
        }
        fn resume(&mut self) {}
        fn standby(&mut self) -> Result<(), Self::Error> { Ok(()) }
        fn disconnect(&mut self) {}
    }

    #[test]
    fn lifecycle_matches_msa_command_shape() {
        let mut session = SoloSession::new(FakeTransport { scheduled: 1234 });
        let ack = session.start(1000).unwrap();
        assert_eq!(ack, StartAck { requested_unix_ms: 1000, at_unix_ms: 1234 });
        assert_eq!(session.state(), SessionState::Playing);
        assert_eq!(session.epoch(), 1);
        session.flush().unwrap();
        assert_eq!(session.state(), SessionState::Idle);
        session.start(2000).unwrap();
        assert_eq!(session.epoch(), 2);
        session.standby().unwrap();
        assert_eq!(session.state(), SessionState::Standby);
        session.end();
        assert_eq!(session.state(), SessionState::Ended);
    }

    #[test]
    fn pcm_is_owned_persistently_and_flush_discards_old_track() {
        let mut session = SoloSession::new(FakeTransport::default());
        assert_eq!(session.buffer_pcm(&[1, 2, 3, 4]), 4);

        let mut out = [0u8; 4];
        assert_eq!(session.read_pcm(&mut out), Some(0));

        session.start(1000).unwrap();
        assert_eq!(session.read_pcm(&mut out), Some(4));
        assert_eq!(out, [1, 2, 3, 4]);

        session.buffer_pcm(&[5, 6, 7, 8]);
        session.flush().unwrap();
        assert_eq!(session.buffered_bytes(), 0);

        session.buffer_pcm(&[9, 10]);
        session.mark_input_eof();
        session.start(2000).unwrap();
        assert_eq!(session.read_pcm(&mut out), Some(2));
        assert_eq!(&out[..2], &[9, 10]);
        assert_eq!(session.read_pcm(&mut out), None);
    }
}

pub use rtsp::{RtspCodec, RtspError, RtspRequest, RtspResponse};
pub use hap_tlv8::{Tlv8, Tlv8Error, TlvTag, HAP_TRANSIENT_FLAG};
pub use hap_srp::{srp_client_compute, SrpClientResult, SrpError, SRP_TRANSIENT_PIN};
pub use hap_crypto::{derive_control_keys, HapControlCipher, HapCryptoError};
pub use hap_rtsp::{EncryptedRtspChannel, EncryptedRtspError};
pub use hap_pairing::{
    NativeHapPairingClient, PairingError, StoredHapCredentials,
    TransientPairingClient, TransientPairingResult, TransientPairingSession,
};


pub use ap2_info::{
    Ap2AudioFormat, Ap2Info, Ap2InfoError, AudioFormatCapability,
    AIRPLAY_HIRES_AUDIO_FORMATS, ALAC_44100_16_2, ALAC_44100_24_2,
    ALAC_48000_16_2, ALAC_48000_24_2,
};
pub use preflight::{Ap2PreflightClient, PreflightError, PreflightResult};
pub use native_preflight::{NativeConnectError, NativeConnectFlow, NativePhase};
pub use ntp_session_setup::{
    setup_ntp_session, NtpSessionSetupConfig, NtpSessionSetupError, NtpSessionSetupResult,
};
pub use ptp_session_setup::{
    setup_ptp_session, PtpSessionSetupConfig, PtpSessionSetupError, PtpSessionSetupResult,
};
pub use record::{send_record, RecordConfig, RecordError};
pub use stream_setup::{
    setup_buffered_stream, setup_realtime_stream, BufferedStreamSetupConfig,
    BufferedStreamSetupResult, RealtimeStreamSetupConfig, RealtimeStreamSetupResult,
    StreamPorts, StreamSetupError,
};
pub use setpeers::{send_setpeers, SetPeersConfig, SetPeersError};
pub use event_channel::{open_event_channel, open_event_channel_best_effort, EventChannel, EventChannelError};

pub use native_media_owner::{NativeMediaOwner, NativeMediaOwnerError};
pub use native_control::{
    open_native_control, LiveTiming, NativeControlConfig, NativeControlError, NativeControlReady,
};

pub use ntp_timing::{build_timing_response, system_time_to_ntp, NtpTimingError, NtpTimingResponder};
pub use ptp_engine::{PtpClock, PtpEngine, PtpEngineError, PtpExchange};
pub use native_timing_owner::NativeTimingOwner;

pub use native_commands::NativeCommandError;
pub use native_solo::{
    NativeSoloConfig, NativeSoloEngine, NativeSoloError, NativeFormatCapabilities,
    NativeLatencyInfo, SoloClockReadiness, SoloClockReadinessState,
    SoloClockVerifyOutcome,
    AP2_CLOCK_VERIFY_POLL_MS, MSA_NATIVE_LEAD_MS, MSA_SPLICE_DEPTH_MS,
    MSA_SPLICE_DEPTH_MAX_MS,
};

pub use feedback::{
    FeedbackWorker, SharedCseq, SharedRtspControl,
    FEEDBACK_INTERVAL, FEEDBACK_TIMEOUT, MAX_CONSECUTIVE_MISSES,
};
pub use native_rtx_worker::RtxWorker;

pub use pcm_chunker::{Pcm352Chunker, PCM352_PACKET_BYTES};
#[cfg(windows)]
pub use wasapi_loopback::{WasapiDrainReport, WasapiLoopbackCapture, WasapiLoopbackError};
#[cfg(windows)]
pub use windows_audio_worker::{
    SharedNativeSoloEngine, WindowsSoloAudioWorker, WindowsSoloAudioWorkerError,
    AIRPLAY_CLOCK_READY_TIMEOUT, FLUSH_DRAIN_TIMEOUT, STARVATION_RECOVERY_INTERVAL,
};

pub use volume::{set_native_volume, volume_percent_to_db, NativeVolumeControl, VolumeError, VolumeSetResult};
pub use native_metadata::{build_dmap_metadata, send_native_metadata, MetadataError, MetadataSetResult, NativeMetadataControl};
pub use native_parameters::{
    send_native_artwork, send_native_progress, ParameterError, ParameterResult,
};
pub use teardown::{send_teardown, write_farewell_teardown_locked, TeardownError};

pub use mrp::{
    post_command as mrp_post_command, probe_artwork as mrp_probe_artwork,
    MrpArtworkInfo, MrpArtworkResult, MrpController, MrpError, MrpPlaybackState,
    MrpPostResult, MrpPushResult, MrpState, ARTWORK_STAGING_MAX_BYTES,
};

pub use mrp_event::{MrpEventWorker, MrpRemoteCommand};

pub use mrp_datastream::{MrpDataStream, MrpDataStreamError, MrpDataStreamWorker, MRP_CLIENT_TYPE_UUID, MRP_STREAM_CONTROL_TYPE, MRP_STREAM_TYPE_REMOTE_CONTROL};

#[cfg(windows)]
pub use windows_raop_session::{MsaRaopConfig, MsaRaopError, MsaRaopReady, MsaRaopSession, MsaRaopState, MSA_LIBRAOP_PIN, RAOP_FRAMES_PER_PACKET, RAOP_PCM_PACKET_BYTES};
