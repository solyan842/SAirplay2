//! End-to-end native AP2 SOLO engine owner.
//! This is the first concrete path that combines control, timing, media codec,
//! sockets and MSA command/timeline semantics without touching the legacy engine.

use crate::ap2::{self, Ap2CommandError, Ap2State, NativeAp2Transport, NativeLane, ResumePlan};
use crate::clock::{clock_floor, ClockFloor};
use crate::native_commands::{
    buffered_anchor_start, send_flushbuffered, send_realtime_flush,
    send_setrateanchortime, NativeCommandError,
};
use crate::native_control::{open_native_control, NativeControlConfig, NativeControlError, NativeControlReady};
use crate::native_media::{
    drain_buffered_pending, BufferedPending, MediaCounters, MediaHealth, NativeMediaState, SendResult,
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
    set_native_volume, Ap2AudioFormat, MetadataSetResult, ParameterResult,
    TeardownError, VolumeSetResult,
};
use crate::ntp_timing::system_time_to_ntp;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::sync::{Arc, Mutex, atomic::Ordering};

pub const MSA_NATIVE_LEAD_MS: u64 = 2000;
pub const MSA_SPLICE_DEPTH_MS: u64 = 600;

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
    config: NativeSoloConfig,
    timeline_initialized: bool,
    anchored_buffered: bool,
    rtsp_dead: bool,
    clock_verify_armed: bool,
    clock_verify_requested_unix_ms: u64,
    clock_verify_anchor_unix_ms: u64,
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
        let ready = open_native_control(&config.control)?;
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
            splice_depth_ms: config.splice_depth_ms.max(1),
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

        let feedback = FeedbackWorker::start(
            Arc::clone(&ready.control),
            Arc::clone(&ready.next_cseq),
            config.control.dacp_id.clone(),
            config.control.active_remote.clone(),
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
            config,
            timeline_initialized: false,
            anchored_buffered: false,
            rtsp_dead: false,
            clock_verify_armed: false,
            clock_verify_requested_unix_ms: 0,
            clock_verify_anchor_unix_ms: 0,
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
        ap2::start(self, requested_unix_ms)
    }

    pub fn resume(&mut self, requested_unix_ms: u64) -> Result<ResumePlan, Ap2CommandError<NativeSoloError>> {
        ap2::resume(self, requested_unix_ms)
    }

    pub fn flush(&mut self) -> Result<(), Ap2CommandError<NativeSoloError>> {
        ap2::flush(self)
    }

    pub fn standby(&mut self) -> Result<(), Ap2CommandError<NativeSoloError>> {
        ap2::standby(self)
    }

    pub fn state(&self) -> Ap2State { self.runtime.state }

    pub fn is_connected(&mut self) -> bool {
        self.runtime.state != Ap2State::Down && self.control_healthy()
    }

    pub fn is_playing(&mut self) -> bool {
        self.runtime.state == Ap2State::Streaming && !self.content_paused && self.control_healthy()
    }

    pub fn content_paused(&self) -> bool { self.content_paused }
    pub fn content_stopped(&self) -> bool { self.content_stopped }

    pub fn format_capability(&self) -> (Ap2AudioFormat, crate::AudioFormatCapability, crate::AudioFormatCapability) {
        (self.config.control.audio_format, self.ready.info.realtime, self.ready.info.buffered)
    }

    pub fn latency_info(&self) -> (u64, Option<u32>, Option<u32>) {
        (self.runtime.lead_ms, self.ready.latency_min, self.ready.latency_max)
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
        self.runtime.splice_timeline
            && self.runtime.state == Ap2State::Streaming
            && self.runtime.ptp_anchor.valid
    }

    pub fn set_volume(&mut self, percent: u8) -> Result<VolumeSetResult, NativeSoloError> {
        let result = set_native_volume(
            &self.ready.control,
            &self.ready.next_cseq,
            &self.ready.session_uri,
            &self.config.control.dacp_id,
            &self.config.control.active_remote,
            percent,
        ).map_err(|e| NativeSoloError::Command(format!("volume: {e:?}")))?;
        if !(200..300).contains(&result.status) {
            return Err(NativeSoloError::Command(format!("volume status {}", result.status)));
        }
        Ok(result)
    }

    pub fn set_metadata(
        &mut self,
        title: &str,
        artist: &str,
        album: &str,
        duration_s: u32,
        item_id: &str,
    ) -> Result<MetadataSetResult, NativeSoloError> {
        if self.meta_delivered
            && self.meta_duration_s == duration_s
            && self.meta_title == title
            && self.meta_artist == artist
            && self.meta_album == album
            && self.meta_item_id == item_id
        {
            return Ok(MetadataSetResult { status: 200, bytes: 0 });
        }
        let result = send_native_metadata(
            &self.ready.control,
            &self.ready.next_cseq,
            &self.ready.session_uri,
            &self.config.control.dacp_id,
            &self.config.control.active_remote,
            title,
            artist,
            album,
            self.runtime.media.timeline.wire_rtp,
        ).map_err(|e| NativeSoloError::Command(format!("metadata: {e:?}")))?;
        self.meta_delivered = (200..300).contains(&result.status);
        if self.meta_delivered {
            self.meta_title = title.to_owned();
            self.meta_artist = artist.to_owned();
            self.meta_album = album.to_owned();
            self.meta_duration_s = duration_s;
            self.meta_item_id = item_id.to_owned();
        }
        Ok(result)
    }

    pub fn set_artwork(
        &mut self,
        content_type: &str,
        data: &[u8],
    ) -> Result<ParameterResult, NativeSoloError> {
        send_native_artwork(
            &self.ready.control,
            &self.ready.next_cseq,
            &self.ready.session_uri,
            &self.config.control.dacp_id,
            &self.config.control.active_remote,
            content_type,
            data,
            self.runtime.media.timeline.wire_rtp,
        ).map_err(|e| NativeSoloError::Command(format!("artwork: {e:?}")))
    }

    pub fn set_progress(
        &mut self,
        elapsed_s: u32,
        duration_s: u32,
    ) -> Result<ParameterResult, NativeSoloError> {
        let now_ntp = system_time_to_ntp(SystemTime::now())
            .map_err(|e| NativeSoloError::Timing(format!("{e:?}")))?;
        let wall = ntp_to_frames(now_ntp, self.runtime.media.timeline.sample_rate) as u32;
        let now_wire_rtp = wall.wrapping_add(self.runtime.media.timeline.rtp_offset);
        send_native_progress(
            &self.ready.control,
            &self.ready.next_cseq,
            &self.ready.session_uri,
            &self.config.control.dacp_id,
            &self.config.control.active_remote,
            now_wire_rtp,
            self.runtime.media.timeline.sample_rate,
            elapsed_s,
            duration_s,
        ).map_err(|e| NativeSoloError::Command(format!("progress: {e:?}")))
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
        let now_frame = ntp_to_frames(now_ntp, self.runtime.media.timeline.sample_rate);
        let warm = frames_for_ms(crate::timing::AP2_MIN_WARM_LEAD_MS, self.runtime.media.timeline.sample_rate);

        if self.runtime.splice_timeline && self.runtime.ptp_anchor.valid {
            let target = now_frame.saturating_add(warm);
            if self.runtime.media.timeline.head_frame > now_frame {
                if target > self.runtime.media.timeline.head_frame {
                    self.runtime.splice_pad_frames =
                        target - self.runtime.media.timeline.head_frame;
                }
            } else {
                self.runtime.splice_pad_frames = 0;
                let at = ms_for_frames(target, self.runtime.media.timeline.sample_rate);
                self.reanchor_stock_timeline(at, false);
                self.anchor_start(at)?;
            }
            self.runtime.state = Ap2State::Streaming;
            self.runtime.health.healthy = true;
            return Ok(());
        }

        if self.runtime.lane == NativeLane::Realtime && self.runtime.state == Ap2State::Paused {
            if self.runtime.media.timeline.head_frame <= now_frame {
                let target = now_frame.saturating_add(warm);
                let at = ms_for_frames(target, self.runtime.media.timeline.sample_rate);
                self.reanchor_stock_timeline(at, false);
                self.anchor_start(at)?;
            }
            self.runtime.state = Ap2State::Streaming;
            self.runtime.health.healthy = true;
            return Ok(());
        }

        if self.runtime.lane == NativeLane::Buffered {
            let target = now_frame.saturating_add(warm);
            let at = ms_for_frames(target, self.runtime.media.timeline.sample_rate);
            self.reanchor_stock_timeline(at, true);
            self.anchor_start(at)?;
            self.runtime.state = Ap2State::Streaming;
            self.runtime.health.healthy = true;
            self.anchor_buffered_start(at)?;
            return Ok(());
        }

        self.runtime.state = Ap2State::Streaming;
        self.runtime.health.healthy = true;
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

    /// SOLO clock verification is observation-only. A fresh probe streak
    /// satisfies the verification without moving the committed anchor.
    pub fn poll_clock_verify(&mut self) -> bool {
        if !self.clock_verify_armed { return false; }
        if self.ready.timing_owner.probe_streak().is_some() {
            self.clock_verify_armed = false;
            return true;
        }
        false
    }

    pub fn effective_lead_ms(&self) -> u64 { self.runtime.lead_ms }
    pub fn clock_verify_armed(&self) -> bool { self.clock_verify_armed }

    pub fn control_healthy(&mut self) -> bool {
        self.refresh_control_health();
        !self.rtsp_dead && self.runtime.health.healthy
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

    fn note_command_error(&mut self, err: &NativeCommandError) {
        if matches!(err, NativeCommandError::Transport(_)) {
            self.rtsp_dead = true;
            self.runtime.rtsp_dead = true;
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
        let Some(clock) = self.ready.timing_owner.ptp_clock().cloned() else {
            return Err(NativeSoloError::Command("buffered anchor without PTP".into()));
        };
        let session_uri = self.ready.session_uri.clone();
        let dacp = self.config.control.dacp_id.clone();
        let active = self.config.control.active_remote.clone();
        let rtp = self.runtime.media.timeline.wire_rtp;
        let ntp = self.runtime.start_ntp;
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
    }
}

impl Drop for NativeSoloEngine {
    fn drop(&mut self) {
        self.feedback.stop();
        if let Some(worker) = self.rtx_worker.as_mut() {
            worker.stop();
        }
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
    }
}
