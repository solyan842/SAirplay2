pub mod ap2_info;
pub mod catalog;
pub mod discovery;
pub mod hap_crypto;
pub mod hap_tlv8;
pub mod hap_srp;
pub mod hap_pairing;
pub mod hap_rtsp;
pub mod mdns_browser;
pub mod native_preflight;
pub mod native_session;
pub mod native_timing;
pub mod pcm_ring;
pub mod preflight;
pub mod route;
pub mod rtsp;
pub mod session;
pub mod timeline;
pub mod timing_policy;
pub mod ntp_timing;
pub mod ptp_engine;
pub mod ptp_session_setup;
pub mod ntp_session_setup;
pub mod event_channel;
pub mod feedback;
pub mod setpeers;
pub mod record;
pub mod stream_setup;
pub mod media_transport;
pub mod media_handshake;
pub mod rtp_packets;
pub mod audio_packet;
pub mod media_sender;
pub mod retransmit;
pub mod teardown;
pub mod volume;
pub mod native_metadata;
pub mod pcm_chunker;
#[cfg(windows)]
pub mod wasapi_loopback;
#[cfg(windows)]
pub mod windows_audio_worker;
#[cfg(windows)]
pub mod windows_multiroom_worker;
#[cfg(windows)]
pub mod native_group;
#[cfg(windows)]
pub mod legacy_session;
pub mod alac_encoder;

pub use ap2_info::{
    select_native_buffered_stream_format, select_native_realtime_stream_format,
    Ap2AudioFormat, Ap2Info, Ap2InfoError, AudioFormatCapability,
    AIRPLAY_HIRES_AUDIO_FORMATS, ALAC_44100_16_2, ALAC_44100_24_2,
    ALAC_48000_16_2, ALAC_48000_24_2,
};
pub use catalog::{DeviceCatalog, DeviceRecord};
pub use discovery::{AirPlayTxt, DiscoveryError};
pub use hap_crypto::{derive_control_keys, HapControlCipher, HapCryptoError};
pub use hap_tlv8::{Tlv8, Tlv8Error, TlvTag, HAP_TRANSIENT_FLAG};
pub use hap_srp::{srp_client_compute, SrpClientResult, SrpError, SRP_TRANSIENT_PIN};
pub use hap_pairing::{PairingError, TransientPairingClient, TransientPairingResult, TransientPairingSession};
pub use hap_rtsp::{EncryptedRtspChannel, EncryptedRtspError};
pub use mdns_browser::{DiscoveredService, DiscoveryEvent, MdnsBrowser, ServiceKind};
pub use native_preflight::{NativeConnectError, NativeConnectFlow, NativePhase};
pub use pcm_ring::PcmRing;
pub use preflight::{Ap2PreflightClient, PreflightError, PreflightResult};
pub use route::{ReceiverCapabilities, Route, RouteResolver};
pub use rtsp::{RtspCodec, RtspError, RtspRequest, RtspResponse};
pub use session::{EngineCommand, EngineEvent, EngineState, SessionCore};
pub use timeline::{Boundary, SplicePlan, Timeline, TimelineError};

pub use timing_policy::{TimingDecision, TimingMode, TimingPreference, TimingReadiness, TimingStartResult};
pub use ntp_timing::{build_timing_response, system_time_to_ntp, NtpTimingError, NtpTimingResponder};
pub use native_timing::{start_ntp_timing_gate, NativeTimingGateError};
pub use ntp_session_setup::{build_ntp_session_plist, parse_event_port, setup_ntp_session, NtpSessionSetupConfig, NtpSessionSetupError, NtpSessionSetupResult};
pub use event_channel::{derive_event_keys, open_event_channel, EventChannel, EventChannelError};
pub use feedback::{FeedbackWorker, SharedCseq, SharedRtspControl};
pub use setpeers::{build_setpeers_plist, send_setpeers, SetPeersConfig, SetPeersError};
pub use record::{send_record, RecordConfig, RecordError};
pub use stream_setup::{build_realtime_stream_plist, parse_stream_ports, parse_stream_setup_response, setup_realtime_stream, RealtimeStreamSetupConfig, RealtimeStreamSetupResult, StreamPorts, StreamSetupError};
pub use media_transport::{DatagramSendOutcome, MediaTransport, MediaTransportError, MediaTransportPorts, RemoteMediaEndpoints};
pub use media_handshake::{prepare_realtime_media, MediaHandshakeConfig, MediaHandshakeError, MediaHandshakeResult};
pub use rtp_packets::{build_ntp_sync_packet, build_ptp_sync_packet, build_rtp_header, NtpSyncPacketArgs, PtpSyncPacketArgs, RtpState, FRAMES_PER_PACKET_44100};
pub use audio_packet::{build_audio_nonce, build_encrypted_realtime_packet, AudioPacketError};
pub use media_sender::{RealtimeMediaSender, MediaSendError, MediaSendResult};
pub use retransmit::{RetransmitRing, RetransmitStats, RetransmitWorker, RTX_RING_SLOTS};
pub use teardown::{send_teardown, TeardownError};
pub use volume::{set_native_volume, volume_percent_to_db, NativeVolumeControl, VolumeError, VolumeSetResult};
pub use native_metadata::{build_dmap_metadata, send_native_metadata, MetadataError, MetadataSetResult, NativeMetadataControl};
pub use alac_encoder::{
    encode_alac_16_stereo_352, truncate_s32le_to_s24le, AlacEncodeError,
    ALAC_PCM_BYTES_PER_FRAME, ALAC_PCM_PACKET_BYTES, ALAC_FRAMES_PER_PACKET,
};
#[cfg(windows)]
pub use alac_encoder::Alac24Encoder;
pub use pcm_chunker::{Pcm352Chunker, PCM352_PACKET_BYTES};
#[cfg(windows)]
pub use wasapi_loopback::{WasapiDrainReport, WasapiLoopbackCapture, WasapiLoopbackError};
#[cfg(windows)]
pub use windows_audio_worker::{WindowsAudioWorker, WindowsAudioWorkerError};
#[cfg(windows)]
pub use windows_multiroom_worker::{
    WindowsAudioTarget, WindowsGroupAudioKind, WindowsMultiroomAudioError,
    WindowsMultiroomAudioWorker, WindowsMultiroomJoinHandle,
    AIRPLAY_COLD_GROUP_START_LEAD_MS,
    AIRPLAY_LATE_JOIN_MIN_HEADROOM_MS, AIRPLAY_START_LEAD_MS,
};
#[cfg(windows)]
pub use native_group::{
    NativeGroupError, NativeGroupJoinHandle, NativeGroupKind, NativeGroupMemberConfig,
    NativeGroupSession,
};
#[cfg(windows)]
pub use legacy_session::{
    LegacyGroupError, LegacyGroupSession, LegacyMemberConfig, LegacyVolumeControl,
    LIBRAOP_PINNED_COMMIT,
};
pub use native_session::{NativeSession, NativeSessionConfig, NativeSessionError};
pub use ptp_engine::{PtpClock, PtpEngine, PtpEngineError, PtpExchange};
pub use ptp_session_setup::{build_ptp_session_plist, setup_ptp_session, PtpSessionSetupConfig, PtpSessionSetupError, PtpSessionSetupResult};
