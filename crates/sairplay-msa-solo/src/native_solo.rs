//! End-to-end native AP2 SOLO engine owner.
//! This is the first concrete path that combines control, timing, media codec,
//! sockets and MSA command/timeline semantics without touching the legacy engine.

use crate::ap2::{self, Ap2CommandError, Ap2State, NativeAp2Transport, NativeLane, ResumePlan};
use crate::clock::{clock_floor, ClockFloor, AP2_CLOCK_STALL_MS};
use crate::native_commands::{
    buffered_anchor_start, send_flushbuffered, send_realtime_flush,
    send_setrateanchortime, NativeCommandError,
};
use crate::native_control::{open_native_control, NativeControlConfig, NativeControlError, NativeControlReady};
use crate::native_media::{
    drain_buffered_pending, pacing_window_frames, BufferedPending, MediaCounters, MediaHealth, MediaIo, NativeMediaState, SendResult,
};
use crate::native_runtime::NativeRuntime;
use crate::native_sync::{PtpAnchor, SyncCounters};
use crate::native_rtx::{RtxCounters, RtxRing};
use crate::feedback::FeedbackWorker;
use crate::native_rtx_worker::RtxWorker;
use crate::native_timeline::{
    frames_for_ms, ms_for_frames, ntp_to_frames, unix_ms_to_ntp, Timeline,
};
use crate::{
    send_native_artwork, send_native_metadata, send_native_progress, send_teardown,
    set_native_volume, write_farewell_teardown_locked, Ap2AudioFormat,
    EncryptedRtspError, MetadataError, MetadataSetResult, ParameterError,
    ParameterResult, VolumeError, VolumeSetResult, mrp_post_command, MrpError,
    MrpArtworkInfo, MrpArtworkResult, MrpController, MrpDataStream, MrpDataStreamWorker, MrpEventWorker,
    MrpPlaybackState, MrpPushResult, MrpRemoteCommand, MrpState,
};
use crate::ntp_timing::system_time_to_ntp;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::sync::{Arc, Mutex, atomic::Ordering};

pub const MSA_NATIVE_LEAD_MS: u64 = 2000;
pub const MSA_SPLICE_DEPTH_MS: u64 = 600;
pub const MSA_SPLICE_DEPTH_MAX_MS: u64 = 3000;
pub const AP2_CLOCK_VERIFY_POLL_MS: u64 = 250;

fn env_enabled(name: &str, unset_default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(v.as_str(), "0" | "false" | "off"),
        Err(_) => unset_default,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoloClockVerifyOutcome {
    Idle,
    Pending,
    Verified { margin_ms: i64 },
    Unverified { readiness_late_ms: Option<u64> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFormatCapabilities {
    pub requested: u64,
    pub realtime_formats: u64,
    pub buffered_formats: u64,
    pub realtime_known: bool,
    pub buffered_known: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeLatencyInfo {
    pub lead_ms: u64,
    pub device_min_frames: u32,
    pub device_max_frames: u32,
    pub render_latency_ms: u32,
}

#[derive(Debug, Clone)]
pub struct NativeMetadataBundleResult {
    pub metadata: MetadataSetResult,
    pub artwork: Option<ParameterResult>,
    pub track_changed: bool,
    pub mrp_artwork: Option<MrpArtworkInfo>,
    pub mrp_push: Option<MrpPushResult>,
    pub delivered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeDiagnostics {
    pub state: Ap2State,
    pub seq: u16,
    pub rtp: u32,
    pub head_frame: u64,
    pub pacing_ahead_frames: i64,
    pub audio_sent: u64,
    pub audio_dropped: u64,
    pub sync_sent: u64,
    pub sync_dropped: u64,
    pub reanchors: u64,
    pub splice_pad_frames: u64,
    pub uses_ptp: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoloClockReadinessState { Cold, Probing, Ready, Stalled }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoloClockReadiness {
    pub state: SoloClockReadinessState,
    pub streak_age_ms: u64,
    pub exchanges: u32,
    pub ready_at_unix_ms: u64,
    pub ready_in_ms: u64,
}

#[derive(Debug, Clone)]
pub struct NativeSoloConfig {
    pub control: NativeControlConfig,
    pub apple_model: bool,
    pub splice_depth_ms: u64,
    pub splice_depth_explicit: bool,
}

impl NativeSoloConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            control: NativeControlConfig::new(host, port),
            apple_model: false,
            splice_depth_ms: MSA_SPLICE_DEPTH_MS,
            splice_depth_explicit: false,
        }
    }
}

#[derive(Debug)]
pub enum NativeSoloError {
    Control(NativeControlError),
    Command(String),
    Timing(String),
    MediaFatal,
    Lifecycle(String),
}

impl From<NativeControlError> for NativeSoloError {
    fn from(v: NativeControlError) -> Self { Self::Control(v) }
}

pub struct NativeSoloEngine {
    pub ready: NativeControlReady,
    pub runtime: NativeRuntime,
    feedback: FeedbackWorker,
    rtx_worker: Option<RtxWorker>,
    mrp: Option<MrpController>,
    mrp_event: Option<MrpEventWorker>,
    mrp_data: Option<MrpDataStreamWorker>,
    config: NativeSoloConfig,
    timeline_initialized: bool,
    first_start_done: bool,
    anchored_buffered: bool,
    rtsp_dead: bool,
    clock_verify_armed: bool,
    clock_verify_requested_unix_ms: u64,
    clock_verify_anchor_unix_ms: u64,
    clock_verify_packets_at_arm: u64,
    clock_connected_unix_ms: u64,
    clock_last_streak_unix_ms: u64,
    monotonic_zero: Instant,
    content_paused: bool,
    content_stopped: bool,
    disconnected: bool,
    meta_delivered: bool,
    meta_title: String,
    meta_artist: String,
    meta_album: String,
    meta_duration_s: u32,
    meta_item_id: String,
}

impl NativeSoloEngine {
    pub fn connect(config: NativeSoloConfig) -> Result<Self, NativeSoloError> {
        let mut ready = open_native_control(&config.control)?;
        let sample_rate = config.control.audio_format.sample_rate;
        let requested_frames = (MSA_NATIVE_LEAD_MS * u64::from(sample_rate) / 1000) as u32;
        let min_frames = ready.latency_min.unwrap_or(0);
        let max_frames = ready.latency_max.unwrap_or(requested_frames);
        let effective_frames = if requested_frames < min_frames {
            min_frames
        } else if requested_frames > max_frames {
            max_frames
        } else {
            requested_frames
        };
        let lead_ms = u64::from(effective_frames) * 1000 / u64::from(sample_rate);

        // Pinned MSA: buffered timeline has protocol-native FLUSHBUFFERED
        // boundaries and therefore disables realtime splice machinery.
        let splice_timeline = !ready.buffered;
        let lane = if ready.buffered { NativeLane::Buffered } else { NativeLane::Realtime };
        let use_ptp = ready.timing_owner.use_ptp();
        let ssrc = ready.ssrc;

        let runtime = NativeRuntime {
            state: Ap2State::Connected,
            lane,
            rtsp_dead: false,
            use_ptp,
            splice_timeline,
            lead_ms,
            dev_latency_max: u64::from(ready.latency_max.unwrap_or(0)),
            splice_depth_ms: config.splice_depth_ms.clamp(1, MSA_SPLICE_DEPTH_MAX_MS),
            splice_depth_explicit: config.splice_depth_explicit,
            ssrc,
            start_ntp: 0,
            media: NativeMediaState {
                timeline: Timeline {
                    sample_rate,
                    head_frame: 0,
                    wire_rtp: 0,
                    rtp_offset: 0,
                    seq: 0,
                    first_packet: true,
                },
                counters: MediaCounters { sent: 0, dropped: 0, nonce_counter: 0 },
            },
            health: MediaHealth::default(),
            pending: BufferedPending::default(),
            sync_counters: SyncCounters::default(),
            rtx_ring: Arc::new(Mutex::new(RtxRing::default())),
            rtx_counters: Arc::new(Mutex::new(RtxCounters::default())),
            ptp_anchor: PtpAnchor::default(),
            pace_last_release_us: 0,
            splice_pad_frames: 0,
            timeline_reanchors: 0,
            reanchor_shifted_frames: 0,
        };

        // Pinned ap2_mrp_ready(): MediaRemote is default-on only for a
        // pair-verified native session with a live reverse event channel.
        // Transient-paired third-party receivers remain DMAP-only.
        let mrp = if env_enabled("CLIAIRPLAY_MRP", true)
            && ready.pair_verified
            && ready.event.is_some()
        {
            Some(MrpController::new(
                MrpState::new(
                    config.control.dacp_id.clone(),
                    config.control.receiver_name.clone(),
                    ready.session_uuid.clone(),
                    ready.group_uuid.clone(),
                ),
                Arc::clone(&ready.control),
                Arc::clone(&ready.next_cseq),
                config.control.dacp_id.clone(),
                config.control.active_remote.clone(),
            ))
        } else {
            None
        };
        let mrp_data = if env_enabled("CLIAIRPLAY_MRP_TYPE130", false) {
            if let Some(controller) = mrp.clone() {
                let setup = ready.control.lock().ok().and_then(|mut control| {
                    MrpDataStream::setup(
                        &mut control,
                        &ready.next_cseq,
                        &ready.session_uri,
                        &config.control.dacp_id,
                        &config.control.active_remote,
                        ready.receiver.ip(),
                        &ready.hap_shared_secret,
                        &controller,
                    ).ok()
                });
                setup.map(|stream| MrpDataStreamWorker::start(stream, controller))
            } else {
                None
            }
        } else {
            None
        };
        let mrp_event = if mrp.is_some() {
            ready.event.take().map(MrpEventWorker::start)
        } else {
            None
        };

        let feedback = FeedbackWorker::start(
            Arc::clone(&ready.control),
            Arc::clone(&ready.next_cseq),
            config.control.dacp_id.clone(),
            config.control.active_remote.clone(),
            ready.session_uri.clone(),
        ).map_err(|e| NativeSoloError::Lifecycle(format!("feedback worker: {e}")))?;

        // Pinned MSA: retransmit responder is realtime-only and non-fatal if
        // the worker cannot be started.
        let rtx_worker = if lane == NativeLane::Realtime {
            ready.media.io.clone_control_socket().ok().and_then(|socket| {
                RtxWorker::start(
                    socket,
                    Arc::clone(&runtime.rtx_ring),
                    Arc::clone(&runtime.rtx_counters),
                ).ok()
            })
        } else {
            None
        };

        Ok(Self {
            ready,
            runtime,
            feedback,
            rtx_worker,
            mrp,
            mrp_event,
            mrp_data,
            config,
            timeline_initialized: false,
            first_start_done: false,
            anchored_buffered: false,
            rtsp_dead: false,
            clock_verify_armed: false,
            clock_verify_requested_unix_ms: 0,
            clock_verify_anchor_unix_ms: 0,
            clock_verify_packets_at_arm: 0,
            clock_connected_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0),
            clock_last_streak_unix_ms: 0,
            monotonic_zero: Instant::now(),
            content_paused: false,
            content_stopped: false,
            disconnected: false,
            meta_delivered: false,
            meta_title: String::new(),
            meta_artist: String::new(),
            meta_album: String::new(),
            meta_duration_s: 0,
            meta_item_id: String::new(),
        })
    }

    pub fn start(&mut self, requested_unix_ms: u64) -> Result<crate::timing::StartResolution, Ap2CommandError<NativeSoloError>> {
        // Pinned cliairplay session_commit(): the first START uses ap2cl_start
        // (fresh seq/rtp seed); every START after FLUSH uses ap2cl_resume so
        // sequence/audio-nonce continuity is preserved.
        let result = if self.first_start_done {
            ap2::resume(self, requested_unix_ms).map(|plan| plan.start)
        } else {
            ap2::start(self, requested_unix_ms)
        };
        if result.is_ok() {
            // Pinned cliairplay session_commit: before the first audio can
            // leave the send gate, metadata-gated receivers get a placeholder
            // unless real metadata was already delivered pre-START.
            if !self.first_start_done && !self.meta_delivered {
                let dmap_ok = match send_native_metadata(
                    &self.ready.control,
                    &self.ready.next_cseq,
                    &self.ready.session_uri,
                    &self.config.control.dacp_id,
                    &self.config.control.active_remote,
                    "cliairplay",
                    "",
                    "",
                    self.runtime.media.timeline.wire_rtp,
                ) {
                    Ok(v) => (200..300).contains(&v.status),
                    Err(MetadataError::Transport(ref transport)) => {
                        self.mark_rtsp_transport_error(transport);
                        false
                    }
                    Err(_) => false,
                };
                if dmap_ok {
                    if let Some(mrp) = self.mrp.clone() {
                        if mrp.stage_track("cliairplay", "", "", 0, "", None).is_ok() {
                            if let Err(e) = mrp.push_full() {
                                self.note_mrp_error(&e);
                            }
                        }
                    }
                }
            }
            self.first_start_done = true;
            self.content_paused = false;
            self.content_stopped = false;
        }
        result
    }

    pub fn resume(&mut self, requested_unix_ms: u64) -> Result<ResumePlan, Ap2CommandError<NativeSoloError>> {
        let result = ap2::resume(self, requested_unix_ms);
        if result.is_ok() { self.first_start_done = true; }
        result
    }

    pub fn flush(&mut self) -> Result<(), Ap2CommandError<NativeSoloError>> {
        let result = ap2::flush(self);
        if result.is_ok() {
            // Mirrors cliairplay's session_flush_op: transport can remain hot
            // (splice), but content delivery is paused until the next START.
            self.content_paused = true;
            self.content_stopped = false;
        }
        result
    }

    pub fn standby(&mut self) -> Result<(), Ap2CommandError<NativeSoloError>> {
        let result = ap2::standby(self);
        if result.is_ok() {
            // The outer session status is paused for both stock and splice;
            // splice keeps the wire alive with silence.
            self.content_paused = false;
            self.content_stopped = true;
        }
        result
    }

    pub fn state(&self) -> Ap2State { self.runtime.state }

    pub fn is_connected(&mut self) -> bool {
        self.runtime.state != Ap2State::Down && self.control_healthy()
    }

    pub fn is_playing(&mut self) -> bool {
        // Exact ap2cl_is_playing: this reports the transport/wire state.
        // Splice pause keeps AP2_STREAMING while content_paused carries the
        // user-visible content state separately.
        self.runtime.state == Ap2State::Streaming && self.control_healthy()
    }

    pub fn content_paused(&self) -> bool { self.content_paused }
    pub fn content_stopped(&self) -> bool { self.content_stopped }

    pub fn format_capability(&self) -> (Ap2AudioFormat, crate::AudioFormatCapability, crate::AudioFormatCapability) {
        (self.config.control.audio_format, self.ready.info.realtime, self.ready.info.buffered)
    }

    pub fn latency_info(&self) -> NativeLatencyInfo {
        NativeLatencyInfo {
            lead_ms: self.runtime.lead_ms,
            device_min_frames: self.ready.latency_min.unwrap_or(0),
            device_max_frames: self.ready.latency_max.unwrap_or(0),
            render_latency_ms: self.ready.arrival_to_render_latency_ms.unwrap_or(0),
        }
    }

    pub fn render_latency_ms(&self) -> Option<u32> {
        self.ready.arrival_to_render_latency_ms
    }

    pub fn audible_lag_frames(&self) -> u64 {
        if self.runtime.splice_timeline || self.runtime.lane == NativeLane::Buffered {
            self.runtime.pacing_window_frames()
        } else {
            frames_for_ms(self.runtime.lead_ms, self.runtime.media.timeline.sample_rate)
        }
    }

    pub fn warm_lead_ms(&self) -> u64 {
        if self.runtime.splice_timeline {
            self.runtime.splice_depth_ms
        } else {
            0
        }
    }

    pub fn splice_head_unix_ms(&self) -> u64 {
        if self.runtime.splice_timeline && self.runtime.media.timeline.head_frame != 0 {
            ms_for_frames(
                self.runtime.media.timeline.head_frame,
                self.runtime.media.timeline.sample_rate,
            )
        } else {
            0
        }
    }

    pub fn head_audible_unix_ms(&self) -> u64 {
        if self.runtime.media.timeline.head_frame == 0 { 0 } else {
            ms_for_frames(
                self.runtime.media.timeline.head_frame,
                self.runtime.media.timeline.sample_rate,
            )
        }
    }

    pub fn splice_hot(&self) -> bool {
        // Exact ap2cl_splice_hot: wire-hot is a session/state property and
        // does not require a frozen PTP anchor. NTP splice sessions still
        // keep the realtime line fed with silence while STREAMING.
        self.runtime.splice_timeline
            && self.runtime.state == Ap2State::Streaming
            && !self.rtsp_dead
    }

    pub fn set_volume(&mut self, percent: u8) -> Result<VolumeSetResult, NativeSoloError> {
        let result = match set_native_volume(
            &self.ready.control,
            &self.ready.next_cseq,
            &self.ready.session_uri,
            &self.config.control.dacp_id,
            &self.config.control.active_remote,
            percent,
        ) {
            Ok(v) => v,
            Err(e) => {
                if let VolumeError::Transport(ref transport) = e {
                    self.mark_rtsp_transport_error(transport);
                }
                return Err(NativeSoloError::Command(format!("volume: {e:?}")));
            }
        };
        if !(200..300).contains(&result.status) {
            return Err(NativeSoloError::Command(format!("volume status {}", result.status)));
        }
        Ok(result)
    }

    pub fn set_metadata_bundle(
        &mut self,
        title: &str,
        artist: &str,
        album: &str,
        duration_s: u32,
        item_id: &str,
        artwork: Option<(&str, &[u8])>,
    ) -> Result<NativeMetadataBundleResult, NativeSoloError> {
        let mrp = self.mrp.clone();
        let mut track_changed = false;
        let mut mrp_artwork = None;
        let mut mrp_push = None;

        // Exact ap2cl_set_metadata_ex serialization: the MRP mutation,
        // DMAP metadata/artwork delivery and the ONE full MRP replace push
        // share one publication scope. This prevents tvOS rebuilding its
        // Now Playing view twice for one track.
        let _publication = if let Some(mrp) = mrp.as_ref() {
            Some(
                mrp.publication_guard()
                    .map_err(|e| NativeSoloError::Command(format!("MRP publication lock: {e:?}")))?,
            )
        } else {
            None
        };

        if let Some(mrp) = mrp.as_ref() {
            let (changed, info) = mrp
                .stage_track_locked(
                    title,
                    artist,
                    album,
                    i64::from(duration_s) * 1000,
                    item_id,
                    artwork,
                )
                .map_err(|e| NativeSoloError::Command(format!("MRP metadata bundle stage: {e:?}")))?;
            track_changed = changed;
            mrp_artwork = Some(info);
        }

        let metadata_identical = self.meta_delivered
            && self.meta_duration_s == duration_s
            && self.meta_title == title
            && self.meta_artist == artist
            && self.meta_album == album
            && self.meta_item_id == item_id;
        let artwork_identical = match artwork {
            None => true,
            Some(_) => mrp_artwork
                .as_ref()
                .is_some_and(|info| info.result == MrpArtworkResult::Unchanged),
        };

        if metadata_identical && artwork_identical {
            return Ok(NativeMetadataBundleResult {
                metadata: MetadataSetResult { status: 200, bytes: 0 },
                artwork: None,
                track_changed,
                mrp_artwork,
                mrp_push: None,
                delivered: true,
            });
        }

        let metadata = match send_native_metadata(
            &self.ready.control,
            &self.ready.next_cseq,
            &self.ready.session_uri,
            &self.config.control.dacp_id,
            &self.config.control.active_remote,
            title,
            artist,
            album,
            self.runtime.media.timeline.wire_rtp,
        ) {
            Ok(v) => v,
            Err(e) => {
                self.meta_delivered = false;
                if let MetadataError::Transport(ref transport) = e {
                    self.mark_rtsp_transport_error(transport);
                }
                return Err(NativeSoloError::Command(format!("metadata: {e:?}")));
            }
        };

        let mut artwork_result = None;
        if let Some((content_type, data)) = artwork {
            // MSA re-sends DMAP art for a new track even when the bytes are
            // identical; same-item identical art is the only no-op. MRP
            // rejected artwork still goes to the DMAP path for Sonos-class
            // receivers.
            let should_send = mrp.is_none()
                || track_changed
                || mrp_artwork
                    .as_ref()
                    .is_none_or(|info| info.result != MrpArtworkResult::Unchanged);
            if should_send {
                match send_native_artwork(
                    &self.ready.control,
                    &self.ready.next_cseq,
                    &self.ready.session_uri,
                    &self.config.control.dacp_id,
                    &self.config.control.active_remote,
                    content_type,
                    data,
                    self.runtime.media.timeline.wire_rtp,
                ) {
                    Ok(v) => artwork_result = Some(v),
                    Err(e) => {
                        // Source does not fold artwork SET_PARAMETER success
                        // into the metadata identity latch, but a transport
                        // failure still kills the shared RTSP channel.
                        if let ParameterError::Transport(ref transport) = e {
                            self.mark_rtsp_transport_error(transport);
                        }
                    }
                }
            }
        }

        let mut delivered = (200..300).contains(&metadata.status);
        if let Some(mrp) = mrp.as_ref() {
            match mrp.push_full_under_publication_lock() {
                Ok(push) => {
                    delivered &= (200..300).contains(&push.overall_status);
                    mrp_push = Some(push);
                }
                Err(e) => {
                    self.note_mrp_error(&e);
                    delivered = false;
                }
            }
        }

        self.meta_delivered = delivered;
        if delivered {
            self.meta_title = title.to_owned();
            self.meta_artist = artist.to_owned();
            self.meta_album = album.to_owned();
            self.meta_duration_s = duration_s;
            self.meta_item_id = item_id.to_owned();
        }

        Ok(NativeMetadataBundleResult {
            metadata,
            artwork: artwork_result,
            track_changed,
            mrp_artwork,
            mrp_push,
            delivered,
        })
    }

    pub fn set_metadata(
        &mut self,
        title: &str,
        artist: &str,
        album: &str,
        duration_s: u32,
        item_id: &str,
    ) -> Result<MetadataSetResult, NativeSoloError> {
        self.set_metadata_bundle(title, artist, album, duration_s, item_id, None)
            .map(|result| result.metadata)
    }

    pub fn set_artwork(
        &mut self,
        content_type: &str,
        data: &[u8],
    ) -> Result<ParameterResult, NativeSoloError> {
        let mrp = self.mrp.clone();
        if let Some(mrp) = mrp.as_ref() {
            let info = mrp.stage_artwork(content_type, data)
                .map_err(|e| NativeSoloError::Command(format!("MRP artwork stage: {e:?}")))?;
            if info.result == MrpArtworkResult::Unchanged {
                return Ok(ParameterResult { status: 200, bytes: 0 });
            }
        }

        let result = match send_native_artwork(
            &self.ready.control,
            &self.ready.next_cseq,
            &self.ready.session_uri,
            &self.config.control.dacp_id,
            &self.config.control.active_remote,
            content_type,
            data,
            self.runtime.media.timeline.wire_rtp,
        ) {
            Ok(v) => v,
            Err(e) => {
                if let ParameterError::Transport(ref transport) = e {
                    self.mark_rtsp_transport_error(transport);
                }
                return Err(NativeSoloError::Command(format!("artwork: {e:?}")));
            }
        };

        if let Some(mrp) = mrp {
            if let Err(e) = mrp.push_full() {
                // Source returns the DMAP result independently; MRP failure is
                // surfaced through control health/status, not by rewriting the
                // already completed SET_PARAMETER result.
                self.note_mrp_error(&e);
            }
        }
        Ok(result)
    }

    pub fn set_progress(
        &mut self,
        elapsed_s: u32,
        duration_s: u32,
    ) -> Result<ParameterResult, NativeSoloError> {
        if let Some(mrp) = self.mrp.clone() {
            let playing = self.runtime.state == Ap2State::Streaming
                && !self.content_paused
                && !self.content_stopped;
            return match mrp.set_progress_and_push(
                i64::from(elapsed_s) * 1000,
                i64::from(duration_s) * 1000,
                playing,
            ) {
                Ok(push) => Ok(ParameterResult {
                    status: if push.overall_status >= 0 {
                        push.overall_status.min(u16::MAX as i32) as u16
                    } else {
                        0
                    },
                    bytes: 0,
                }),
                Err(e) => {
                    self.note_mrp_error(&e);
                    Err(NativeSoloError::Command(format!("MRP progress: {e:?}")))
                }
            };
        }

        let now_ntp = system_time_to_ntp(SystemTime::now())
            .map_err(|e| NativeSoloError::Timing(format!("{e:?}")))?;
        let wall = ntp_to_frames(now_ntp, self.runtime.media.timeline.sample_rate) as u32;
        let now_wire_rtp = wall.wrapping_add(self.runtime.media.timeline.rtp_offset);
        match send_native_progress(
            &self.ready.control,
            &self.ready.next_cseq,
            &self.ready.session_uri,
            &self.config.control.dacp_id,
            &self.config.control.active_remote,
            now_wire_rtp,
            self.runtime.media.timeline.sample_rate,
            elapsed_s,
            duration_s,
        ) {
            Ok(v) => Ok(v),
            Err(e) => {
                if let ParameterError::Transport(ref transport) = e {
                    self.mark_rtsp_transport_error(transport);
                }
                Err(NativeSoloError::Command(format!("progress: {e:?}")))
            }
        }
    }

    pub fn set_progress_and_publish(
        &mut self,
        elapsed_s: u32,
        duration_s: u32,
    ) -> Result<ParameterResult, NativeSoloError> {
        // set_progress already mirrors pinned ap2cl_set_progress: MRP when
        // active, otherwise native RTSP SET_PARAMETER progress.
        self.set_progress(elapsed_s, duration_s)
    }

    pub fn pause_content(&mut self) -> Result<(), NativeSoloError> {
        self.content_stopped = false;
        if self.runtime.splice_timeline {
            self.content_paused = true;
            return Ok(());
        }

        if self.runtime.lane == NativeLane::Buffered && self.anchored_buffered {
            self.park_buffered()?;
        }
        self.runtime.ptp_anchor = PtpAnchor::default();
        self.runtime.state = Ap2State::Paused;
        self.content_paused = true;
        Ok(())
    }

    pub fn play_content(&mut self) -> Result<(), NativeSoloError> {
        self.content_paused = false;
        self.content_stopped = false;
        let now_ntp = system_time_to_ntp(SystemTime::now())
            .map_err(|e| NativeSoloError::Timing(format!("{e:?}")))?;
        let sample_rate = self.runtime.media.timeline.sample_rate;
        let now_frame = ntp_to_frames(now_ntp, sample_rate);
        let warm = frames_for_ms(crate::timing::AP2_MIN_WARM_LEAD_MS, sample_rate);
        let warm_ntp = unix_ms_to_ntp(crate::timing::AP2_MIN_WARM_LEAD_MS);

        if self.runtime.splice_timeline && self.runtime.ptp_anchor.valid {
            let target = now_frame.saturating_add(warm);
            if self.runtime.media.timeline.head_frame > now_frame {
                if target > self.runtime.media.timeline.head_frame {
                    self.runtime.splice_pad_frames =
                        target - self.runtime.media.timeline.head_frame;
                }
            } else {
                // Exact ap2_reanchor_after_drain: do not round through unix
                // milliseconds, do not reset sequence/reanchor diagnostics,
                // and do mark the first packet on the fresh realtime line.
                self.runtime.splice_pad_frames = 0;
                let start_ntp = now_ntp.saturating_add(warm_ntp);
                let head = ntp_to_frames(start_ntp, sample_rate);
                self.runtime.start_ntp = start_ntp;
                self.runtime.media.timeline.reanchor_after_drain(head);
                self.runtime.ptp_anchor = PtpAnchor::default();
            }
            self.runtime.state = Ap2State::Streaming;
            return Ok(());
        }

        if self.runtime.lane == NativeLane::Realtime && self.runtime.state == Ap2State::Paused {
            if self.runtime.media.timeline.head_frame <= now_frame {
                let start_ntp = now_ntp.saturating_add(warm_ntp);
                let head = ntp_to_frames(start_ntp, sample_rate);
                self.runtime.start_ntp = start_ntp;
                self.runtime.media.timeline.reanchor_after_drain(head);
                self.runtime.ptp_anchor = PtpAnchor::default();
            }
            self.runtime.state = Ap2State::Streaming;
            return Ok(());
        }

        if self.runtime.lane == NativeLane::Buffered {
            // Pinned ap2cl_play keeps seq and first_packet untouched on a
            // buffered un-pause. It only rebases head/RTP continuity and then
            // establishes rate=1; state becomes STREAMING only after success.
            let resume_ntp = now_ntp.saturating_add(warm_ntp);
            let head = ntp_to_frames(resume_ntp, sample_rate);
            let sent = self.runtime.media.counters.sent;
            self.runtime.media.timeline.rebase_buffered_play(head, sent);
            if let Err(err) = self.buffered_anchor_at_ntp(resume_ntp) {
                self.runtime.health.healthy = false;
                return Err(err);
            }
            self.runtime.state = Ap2State::Streaming;
            return Ok(());
        }

        self.runtime.state = Ap2State::Streaming;
        Ok(())
    }

    pub fn stop_content(&mut self) -> Result<(), NativeSoloError> {
        self.disarm_clock_verify();
        self.content_paused = false;
        self.content_stopped = false;
        if self.runtime.lane == NativeLane::Buffered && !self.rtsp_dead {
            let _ = self.flush_buffered();
        }
        self.clear_anchor();
        self.runtime.state = Ap2State::Down;
        Ok(())
    }

    pub fn disconnect(&mut self) -> Result<(), NativeSoloError> {
        if self.disconnected {
            return Ok(());
        }
        self.disarm_clock_verify();
        self.feedback.stop();
        if let Some(worker) = self.rtx_worker.as_mut() {
            worker.stop();
        }

        // Pinned ap2cl_disconnect: final STOPPED state is published while the
        // encrypted RTSP session is still alive, after feedback/RTX workers
        // stop but before MediaRemote event teardown and RTSP TEARDOWN.
        if let Some(mrp) = self.mrp.clone() {
            if let Err(e) = mrp.publish_playback_state(MrpPlaybackState::Stopped, true) {
                self.note_mrp_error(&e);
            }
        }
        if let Some(worker) = self.mrp_data.as_mut() {
            worker.stop();
        }
        if let Some(worker) = self.mrp_event.as_mut() {
            worker.stop();
        }

        self.ready.media.io.close_buffered();

        if !self.rtsp_dead {
            send_teardown(
                &self.ready.control,
                &self.ready.next_cseq,
                &self.ready.session_uri,
                &self.config.control.dacp_id,
                &self.config.control.active_remote,
            ).map_err(|e| NativeSoloError::Command(format!("TEARDOWN: {e:?}")))?;
        }
        self.meta_delivered = false;
        self.clear_anchor();
        self.runtime.state = Ap2State::Down;
        self.disconnected = true;
        Ok(())
    }

    pub fn accept_frames_now(&mut self) -> Result<bool, NativeSoloError> {
        self.refresh_control_health();
        if self.rtsp_dead { return Ok(false); }
        let ntp = system_time_to_ntp(SystemTime::now())
            .map_err(|e| NativeSoloError::Timing(format!("{e:?}")))?;
        let now_frame = ntp_to_frames(ntp, self.runtime.media.timeline.sample_rate);
        let now_us = self.monotonic_zero.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        Ok(self.runtime.accept_frames(now_frame, now_us, &mut self.ready.media.io))
    }

    pub fn send_pcm_352(&mut self, pcm: &[u8]) -> Result<SendResult, NativeSoloError> {
        self.refresh_control_health();
        if self.rtsp_dead { return Err(NativeSoloError::MediaFatal); }
        let timing = self.ready.timing_owner.sync_timing().map_err(NativeSoloError::Timing)?;
        let media = &mut self.ready.media;
        let result = self.runtime.send_chunk(
            pcm,
            352,
            &mut media.encoder,
            &mut media.cipher,
            &mut media.io,
            timing,
        );
        if result == SendResult::Fatal {
            return Err(NativeSoloError::MediaFatal);
        }
        Ok(result)
    }

    pub fn poll_rtx_once(&mut self) -> Result<bool, NativeSoloError> {
        if self.rtx_worker.is_some() { return Ok(false); }
        let mut buf = [0u8; 2048];
        let received = self.ready.media.io
            .recv_control_nonblocking(&mut buf)
            .map_err(|e| NativeSoloError::Command(format!("RTX recv: {e}")))?;
        let Some((n, peer)) = received else { return Ok(false) };
        Ok(self.runtime.serve_rtx(&mut self.ready.media.io, &peer, &buf[..n]))
    }

    /// Exact SOLO/origin branch of pinned ap2cl_clock_verify_poll:
    /// verification is observation-only (enforce=false), so the committed
    /// anchor never moves. We still distinguish ready-before-anchor from a
    /// late/no-probe close of the verification window.
    pub fn poll_clock_verify(&mut self) -> SoloClockVerifyOutcome {
        if !self.clock_verify_armed {
            return SoloClockVerifyOutcome::Idle;
        }
        let now_ms = self.now_unix_ms();
        let anchor_ms = self.clock_verify_anchor_unix_ms;
        let sent = self.runtime.media.counters.sent != self.clock_verify_packets_at_arm;

        if let Some(ex) = self.ready.timing_owner.probe_streak() {
            let ready_ms = crate::clock::ready_from(now_ms, self.config.apple_model, ex);
            self.clock_verify_armed = false;
            if ready_ms <= anchor_ms {
                return SoloClockVerifyOutcome::Verified {
                    margin_ms: (anchor_ms - ready_ms) as i64,
                };
            }
            return SoloClockVerifyOutcome::Unverified {
                readiness_late_ms: Some(ready_ms - anchor_ms),
            };
        }

        let window_close_ms = anchor_ms.saturating_sub(self.runtime.splice_depth_ms);
        if sent || now_ms.saturating_add(AP2_CLOCK_VERIFY_POLL_MS) >= window_close_ms {
            self.clock_verify_armed = false;
            return SoloClockVerifyOutcome::Unverified {
                readiness_late_ms: None,
            };
        }
        SoloClockVerifyOutcome::Pending
    }

    pub fn format_capabilities(&self) -> NativeFormatCapabilities {
        NativeFormatCapabilities {
            requested: self.config.control.audio_format.audio_format_code(),
            realtime_formats: self.ready.info.realtime_formats(),
            buffered_formats: self.ready.info.buffered_formats(),
            realtime_known: self.ready.info.realtime.known,
            buffered_known: self.ready.info.buffered.known,
        }
    }

    pub fn clock_watch_restart(&mut self) {
        self.clock_last_streak_unix_ms = self.now_unix_ms();
    }

    pub fn clock_readiness(&mut self) -> SoloClockReadiness {
        if !self.ready.timing_owner.use_ptp() || self.runtime.state == Ap2State::Down {
            return SoloClockReadiness {
                state: SoloClockReadinessState::Cold,
                streak_age_ms: 0,
                exchanges: 0,
                ready_at_unix_ms: 0,
                ready_in_ms: 0,
            };
        }
        let now = self.now_unix_ms();
        if let Some(ex) = self.ready.timing_owner.probe_streak() {
            self.clock_last_streak_unix_ms = now;
            let ready_at = crate::clock::ready_from(now, self.config.apple_model, ex);
            return SoloClockReadiness {
                state: if ready_at > now {
                    SoloClockReadinessState::Probing
                } else {
                    SoloClockReadinessState::Ready
                },
                streak_age_ms: ex.first_age_ms,
                exchanges: ex.exchanges,
                ready_at_unix_ms: ready_at,
                ready_in_ms: ready_at.saturating_sub(now),
            };
        }
        let stall_from = self.clock_last_streak_unix_ms.max(self.clock_connected_unix_ms);
        SoloClockReadiness {
            state: if stall_from != 0 && now >= stall_from.saturating_add(AP2_CLOCK_STALL_MS) {
                SoloClockReadinessState::Stalled
            } else {
                SoloClockReadinessState::Cold
            },
            streak_age_ms: 0,
            exchanges: 0,
            ready_at_unix_ms: 0,
            ready_in_ms: 0,
        }
    }

    pub fn clear_mrp_artwork(&mut self) -> Result<Option<crate::MrpPushResult>, NativeSoloError> {
        let Some(mrp) = self.mrp.clone() else { return Ok(None) };
        mrp.clear_artwork_and_push().map(Some).map_err(|e| {
            self.note_mrp_error(&e);
            NativeSoloError::Command(format!("MRP clear artwork: {e:?}"))
        })
    }

    pub fn mrp_register(&mut self) -> Result<i32, NativeSoloError> {
        let Some(mrp) = self.mrp.clone() else { return Ok(-1) };
        mrp.register().map_err(|e| {
            self.note_mrp_error(&e);
            NativeSoloError::Command(format!("MRP register: {e:?}"))
        })
    }

    pub fn mrp_push(&mut self) -> Result<i32, NativeSoloError> {
        let Some(mrp) = self.mrp.clone() else { return Ok(-1) };
        mrp.push_full().map(|v| v.overall_status).map_err(|e| {
            self.note_mrp_error(&e);
            NativeSoloError::Command(format!("MRP push: {e:?}"))
        })
    }

    pub fn mrp_push_progress(&mut self) -> Result<i32, NativeSoloError> {
        let Some(mrp) = self.mrp.clone() else { return Ok(-1) };
        mrp.push_progress().map(|v| v.overall_status).map_err(|e| {
            self.note_mrp_error(&e);
            NativeSoloError::Command(format!("MRP progress push: {e:?}"))
        })
    }

    pub fn mrp_channel_status(&self) -> i32 {
        if !self.ready.pair_verified || !env_enabled("CLIAIRPLAY_MRP_TYPE130", false) {
            -1
        } else if self.mrp_data.as_ref().is_some_and(MrpDataStreamWorker::healthy) {
            1
        } else {
            0
        }
    }

    pub fn mrp_controller(&self) -> Option<MrpController> {
        self.mrp.clone()
    }

    pub fn mrp_event_healthy(&self) -> Option<bool> {
        self.mrp_event.as_ref().map(MrpEventWorker::healthy)
    }

    pub fn mrp_data_healthy(&self) -> Option<bool> {
        self.mrp_data.as_ref().map(MrpDataStreamWorker::healthy)
    }

    pub fn pop_remote_command(&self) -> Option<MrpRemoteCommand> {
        self.mrp_event.as_ref()?.pop_command()
    }

    pub fn set_remote_command_callback(
        &self,
        callback: Option<crate::MrpRemoteCommandCallback>,
    ) {
        if let Some(worker) = self.mrp_event.as_ref() {
            worker.set_callback(callback);
        }
    }

    pub fn uses_ptp(&self) -> bool { self.runtime.use_ptp }

    pub fn splice_pad_frames(&self) -> u64 { self.runtime.splice_pad_frames }

    pub fn consume_splice_pad_frames(&mut self, frames: u32) -> u32 {
        self.runtime.take_splice_pad_frames(frames)
    }

    pub fn diagnostics(&self) -> NativeDiagnostics {
        let now_frame = system_time_to_ntp(SystemTime::now())
            .map(|ntp| ntp_to_frames(ntp, self.runtime.media.timeline.sample_rate))
            .unwrap_or(0);
        let head = self.runtime.media.timeline.head_frame;
        let pacing_ahead_frames = if head >= now_frame {
            (head - now_frame).min(i64::MAX as u64) as i64
        } else {
            -((now_frame - head).min(i64::MAX as u64) as i64)
        };
        NativeDiagnostics {
            state: self.runtime.state,
            seq: self.runtime.media.timeline.seq,
            rtp: self.runtime.media.timeline.wire_rtp,
            head_frame: head,
            pacing_ahead_frames,
            audio_sent: self.runtime.media.counters.sent,
            audio_dropped: self.runtime.media.counters.dropped,
            sync_sent: self.runtime.sync_counters.sent,
            sync_dropped: self.runtime.sync_counters.dropped,
            reanchors: self.runtime.timeline_reanchors,
            splice_pad_frames: self.runtime.splice_pad_frames,
            uses_ptp: self.runtime.use_ptp,
        }
    }

    pub fn effective_lead_ms(&self) -> u64 { self.runtime.lead_ms }
    pub fn clock_verify_armed(&self) -> bool { self.clock_verify_armed }

    pub fn control_healthy(&mut self) -> bool {
        self.refresh_control_health();
        if self.rtsp_dead || !self.runtime.health.healthy {
            return false;
        }
        // Pinned ap2cl_control_healthy: no MRP/event attachment is -1 and
        // therefore healthy; once MediaRemote owns the reverse event channel,
        // losing that channel makes the control plane unhealthy.
        match (&self.mrp, &self.mrp_event) {
            (None, _) => true,
            (Some(_), Some(worker)) => worker.healthy(),
            (Some(_), None) => false,
        }
    }

    fn refresh_control_health(&mut self) {
        if !self.feedback.healthy() {
            self.rtsp_dead = true;
            self.runtime.rtsp_dead = true;
        }
    }

    fn next_cseq(&self) -> u32 {
        self.ready.next_cseq.fetch_add(1, Ordering::SeqCst)
    }

    fn mark_rtsp_transport_error(&mut self, err: &EncryptedRtspError) {
        if matches!(err, EncryptedRtspError::Timeout) {
            // The request was fully written and the read side timed out. MSA
            // appends one final encrypted TEARDOWN before abandoning the
            // still-intact write direction.
            if let Ok(mut control) = self.ready.control.lock() {
                let _ = write_farewell_teardown_locked(
                    &mut control,
                    &self.ready.next_cseq,
                    &self.ready.session_uri,
                    &self.config.control.dacp_id,
                    &self.config.control.active_remote,
                );
            }
        }
        self.rtsp_dead = true;
        self.runtime.rtsp_dead = true;
    }

    fn note_mrp_error(&mut self, err: &MrpError) {
        if let MrpError::Transport(transport) = err {
            self.mark_rtsp_transport_error(transport);
        }
    }

    fn note_command_error(&mut self, err: &NativeCommandError) {
        if let NativeCommandError::Transport(transport) = err {
            self.mark_rtsp_transport_error(transport);
        }
    }

    fn quiesce_buffered_pending(&mut self) {
        if self.runtime.pending.is_empty() { return; }
        if !self.runtime.pending.started() {
            self.runtime.pending.clear();
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while !self.runtime.pending.is_empty() && Instant::now() < deadline {
            if drain_buffered_pending(
                &mut self.runtime.health,
                &mut self.runtime.pending,
                &mut self.ready.media.io,
            ) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn buffered_anchor_at_ntp(&mut self, ntp: u64) -> Result<(), NativeSoloError> {
        let Some(clock) = self.ready.timing_owner.ptp_clock().cloned() else {
            return Err(NativeSoloError::Command("buffered anchor without PTP".into()));
        };
        let session_uri = self.ready.session_uri.clone();
        let dacp = self.config.control.dacp_id.clone();
        let active = self.config.control.active_remote.clone();
        let rtp = self.runtime.media.timeline.wire_rtp;
        let result = {
            let mut control = self.ready.control.lock()
                .map_err(|_| NativeSoloError::Lifecycle("RTSP control mutex poisoned".into()))?;
            buffered_anchor_start(
                &mut control,
                self.ready.next_cseq.as_ref(),
                &session_uri,
                &dacp,
                &active,
                &clock,
                rtp,
                ntp,
            )
        };
        match result {
            Ok(_) => {
                self.anchored_buffered = true;
                Ok(())
            }
            Err(err) => {
                self.note_command_error(&err);
                Err(NativeSoloError::Command(format!("buffered anchor: {err:?}")))
            }
        }
    }

    fn process_seed() -> (u32, u16) {
        let pid = std::process::id();
        let offset = pid.wrapping_mul(2_654_435_761u32) & 0x0fff_ff00;
        let seq = pid.wrapping_mul(40_503u32) as u16;
        (offset, seq)
    }
}

impl NativeAp2Transport for NativeSoloEngine {
    type Error = NativeSoloError;

    fn state(&self) -> Ap2State { self.runtime.state }

    fn now_unix_ms(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    }

    fn start_floor(&self) -> ClockFloor {
        let now_ntp = system_time_to_ntp(SystemTime::now()).unwrap_or(0);
        clock_floor(
            now_ntp,
            true,
            self.ready.timing_owner.use_ptp(),
            self.config.apple_model,
            self.ready.timing_owner.probe_streak(),
        )
    }

    fn splice_timeline(&self) -> bool { self.runtime.splice_timeline }

    fn clock_verify_applicable(&self, at_unix_ms: u64) -> bool {
        self.runtime.splice_timeline
            && self.ready.timing_owner.use_ptp()
            && at_unix_ms >= self.now_unix_ms()
                .saturating_add(self.runtime.splice_depth_ms.saturating_add(500))
    }

    fn anchor_valid(&self) -> bool {
        if self.runtime.lane == NativeLane::Buffered {
            self.anchored_buffered
        } else {
            self.runtime.ptp_anchor.valid
        }
    }

    fn audible_head_unix_ms(&self) -> u64 {
        ms_for_frames(
            self.runtime.media.timeline.head_frame,
            self.runtime.media.timeline.sample_rate,
        )
    }

    fn lane(&self) -> NativeLane { self.runtime.lane }
    fn rtsp_alive(&self) -> bool { !self.rtsp_dead && self.feedback.healthy() }

    fn keep_splice_queue(&mut self) {
        self.runtime.splice_pad_frames = 0;
    }

    fn flush_realtime(&mut self) -> Result<(), Self::Error> {
        let cseq = self.next_cseq();
        let result = {
            let mut control = self.ready.control.lock()
                .map_err(|_| NativeSoloError::Lifecycle("RTSP control mutex poisoned".into()))?;
            send_realtime_flush(
                &mut control,
                cseq,
                &self.ready.session_uri,
                &self.config.control.dacp_id,
                &self.config.control.active_remote,
                self.runtime.media.timeline.seq,
                self.runtime.media.timeline.wire_rtp,
            )
        };
        if let Err(err) = result {
            self.note_command_error(&err);
            return Err(NativeSoloError::Command(format!("FLUSH: {err:?}")));
        }
        Ok(())
    }

    fn flush_buffered(&mut self) -> Result<(), Self::Error> {
        self.quiesce_buffered_pending();
        let cseq = self.next_cseq();
        let result = {
            let mut control = self.ready.control.lock()
                .map_err(|_| NativeSoloError::Lifecycle("RTSP control mutex poisoned".into()))?;
            send_flushbuffered(
                &mut control,
                cseq,
                &self.ready.session_uri,
                &self.config.control.dacp_id,
                &self.config.control.active_remote,
                self.runtime.media.timeline.seq,
                self.runtime.media.timeline.wire_rtp,
            )
        };
        self.anchored_buffered = false;
        if let Err(err) = result {
            self.note_command_error(&err);
            return Err(NativeSoloError::Command(format!("FLUSHBUFFERED: {err:?}")));
        }
        Ok(())
    }

    fn park_buffered(&mut self) -> Result<(), Self::Error> {
        if !self.anchored_buffered { return Ok(()) }
        let Some(clock) = self.ready.timing_owner.ptp_clock().cloned() else { return Ok(()) };
        let cseq = self.next_cseq();
        let result = {
            let mut control = self.ready.control.lock()
                .map_err(|_| NativeSoloError::Lifecycle("RTSP control mutex poisoned".into()))?;
            send_setrateanchortime(
                &mut control,
                cseq,
                &self.ready.session_uri,
                &self.config.control.dacp_id,
                &self.config.control.active_remote,
                &clock,
                self.runtime.media.timeline.wire_rtp,
                clock.master_now_ns(),
                0,
            )
        };
        // MSA standby treats the rate-0 park as best effort and continues to
        // FLUSHBUFFERED even when the anchor request is rejected.
        if let Err(err) = result {
            self.note_command_error(&err);
        }
        Ok(())
    }

    fn set_connected(&mut self) { self.runtime.state = Ap2State::Connected; }

    fn set_streaming(&mut self) {
        self.runtime.state = Ap2State::Streaming;
        self.runtime.health.healthy = true;
        self.runtime.reanchor_shifted_frames = 0;
    }

    fn clear_anchor(&mut self) {
        self.runtime.ptp_anchor = PtpAnchor::default();
        self.anchored_buffered = false;
    }

    fn reanchor_stock_timeline(&mut self, at_unix_ms: u64, buffered: bool) {
        let head = ntp_to_frames(
            unix_ms_to_ntp(at_unix_ms),
            self.runtime.media.timeline.sample_rate,
        );
        if !self.timeline_initialized {
            let (offset, seq) = Self::process_seed();
            self.runtime.media.timeline.rtp_offset = offset;
            self.runtime.media.timeline.seq = seq;
            self.timeline_initialized = true;
        }
        let sent = self.runtime.media.counters.sent;
        self.runtime.media.timeline.reanchor_stock(head, buffered, sent);
        self.runtime.ptp_anchor = PtpAnchor::default();
        self.anchored_buffered = false;
        self.runtime.reanchor_shifted_frames = 0;
    }

    fn anchor_start(&mut self, at_unix_ms: u64) -> Result<(), Self::Error> {
        self.runtime.start_ntp = unix_ms_to_ntp(at_unix_ms);
        Ok(())
    }

    fn anchor_buffered_start(&mut self, _at_unix_ms: u64) -> Result<(), Self::Error> {
        self.buffered_anchor_at_ntp(self.runtime.start_ntp)
    }

    fn sync_realtime_ptp_if_ready(&mut self) -> Result<(), Self::Error> {
        if !self.ready.timing_owner.use_ptp() || self.runtime.lane == NativeLane::Buffered {
            return Ok(());
        }
        let timing = self.ready.timing_owner.sync_timing().map_err(NativeSoloError::Timing)?;
        let result = self.runtime.send_immediate_ptp_sync(&mut self.ready.media.io, timing);
        if result == SendResult::Fatal {
            return Err(NativeSoloError::MediaFatal);
        }
        Ok(())
    }

    fn disarm_clock_verify(&mut self) {
        self.clock_verify_armed = false;
        self.clock_verify_requested_unix_ms = 0;
        self.clock_verify_anchor_unix_ms = 0;
        self.clock_verify_packets_at_arm = 0;
    }

    fn arm_clock_verify(
        &mut self,
        requested_unix_ms: u64,
        at_unix_ms: u64,
        enforce: bool,
    ) {
        debug_assert!(!enforce, "SOLO verification is observation-only");
        self.clock_verify_armed = true;
        self.clock_verify_requested_unix_ms = requested_unix_ms;
        self.clock_verify_anchor_unix_ms = at_unix_ms;
        self.clock_verify_packets_at_arm = self.runtime.media.counters.sent;
    }
}

impl Drop for NativeSoloEngine {
    fn drop(&mut self) {
        let _ = self.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_seed_matches_pinned_msa_formula() {
        let pid = std::process::id();
        let (offset, seq) = NativeSoloEngine::process_seed();
        assert_eq!(offset, pid.wrapping_mul(2_654_435_761u32) & 0x0fff_ff00);
        assert_eq!(seq, pid.wrapping_mul(40_503u32) as u16);
    }

    #[test]
    fn native_lead_default_is_exact_pinned_value() {
        assert_eq!(MSA_NATIVE_LEAD_MS, 2000);
        assert_eq!(MSA_SPLICE_DEPTH_MS, 600);
        assert_eq!(MSA_SPLICE_DEPTH_MAX_MS, 3000);
    }
}
