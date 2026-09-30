//! Integrated native AP2 SOLO runtime state.
//! Pinned to music-assistant/airplay-cli @ 431c5c582eef9307c4e39c50a0ea65e970bc1128.

use crate::ap2::{Ap2State, NativeLane};
use crate::native_media::{
    build_buffered_frame, build_realtime_packet, drain_buffered_pending, execute_buffered,
    execute_realtime, pacing_accept, pacing_window_frames, recovery_lead_frames,
    splice_recovery_pad, sync_due, AlacEncoder, AudioCipher, BufferedPending, MediaHealth,
    MediaIo, NativeMediaState, SendResult, SyncKind,
};
use crate::native_rtx::{serve_request, RtxCounters, RtxIo, RtxRing};
use crate::native_sync::{
    build_ntp_sync, build_ptp_sync, execute_sync, PtpAnchor, SyncCounters, SyncIo,
};
use crate::native_timeline::{frames_for_ms, MIN_WARM_LEAD_MS};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncTiming {
    pub ntp: u64,
    pub master_now_ns: u64,
    pub local_ptp_now_ns: u64,
    pub master_clock_id: u64,
}

#[derive(Debug)]
pub struct NativeRuntime {
    pub state: Ap2State,
    pub lane: NativeLane,
    pub rtsp_dead: bool,
    pub use_ptp: bool,
    pub splice_timeline: bool,
    pub lead_ms: u64,
    pub dev_latency_max: u64,
    pub splice_depth_ms: u64,
    pub splice_depth_explicit: bool,
    pub ssrc: u32,
    pub start_ntp: u64,
    pub media: NativeMediaState,
    pub health: MediaHealth,
    pub pending: BufferedPending,
    pub sync_counters: SyncCounters,
    pub rtx_ring: Arc<Mutex<RtxRing>>,
    pub rtx_counters: Arc<Mutex<RtxCounters>>,
    pub ptp_anchor: PtpAnchor,
    pub pace_last_release_us: u64,
    pub splice_pad_frames: u64,
    pub timeline_reanchors: u64,
    pub reanchor_shifted_frames: u64,
}

impl NativeRuntime {
    pub fn pacing_window_frames(&self) -> u64 {
        pacing_window_frames(
            self.media.timeline.sample_rate,
            self.dev_latency_max,
            self.lane == NativeLane::Buffered,
            self.splice_timeline,
            self.splice_depth_ms,
            self.splice_depth_explicit,
        )
    }

    /// Mirrors MSA ap2cl_accept_frames for native AP2.
    pub fn accept_frames<I: MediaIo>(&mut self, now_frame: u64, now_us: u64, io: &mut I) -> bool {
        if self.state != Ap2State::Streaming || self.rtsp_dead {
            return false;
        }
        if self.lane == NativeLane::Buffered && !self.pending.is_empty() {
            if !drain_buffered_pending(&mut self.health, &mut self.pending, io) {
                return false;
            }
            if !self.pending.is_empty() {
                return false;
            }
        }
        let window = self.pacing_window_frames();
        if !pacing_accept(
            now_frame,
            self.media.timeline.head_frame,
            window,
            self.pace_last_release_us,
            now_us,
        ) {
            return false;
        }
        self.pace_last_release_us = now_us;
        true
    }

    fn send_due_sync<I: SyncIo>(&mut self, io: &mut I, timing: SyncTiming) -> SendResult {
        let kind = match sync_due(self.media.timeline.first_packet, self.media.timeline.seq) {
            Some(v) => v,
            None => return SendResult::Sent,
        };
        let first = kind == SyncKind::Initial;
        if self.use_ptp {
            self.ptp_anchor.freeze_if_needed(
                timing.master_now_ns,
                timing.local_ptp_now_ns,
                self.start_ntp,
                self.lead_ms,
                self.media.timeline.sample_rate,
                self.media.timeline.rtp_offset,
                self.media.timeline.wire_rtp,
            );
            let pkt = build_ptp_sync(
                first,
                self.media.timeline.wire_rtp,
                self.lead_ms,
                self.media.timeline.sample_rate,
                timing.master_now_ns,
                timing.master_clock_id,
                self.ptp_anchor,
            );
            execute_sync(io, &mut self.health, &mut self.sync_counters, &pkt)
        } else {
            let pkt = build_ntp_sync(
                first,
                self.media.timeline.wire_rtp,
                self.lead_ms,
                self.media.timeline.sample_rate,
                timing.ntp,
            );
            execute_sync(io, &mut self.health, &mut self.sync_counters, &pkt)
        }
    }

    pub fn send_immediate_ptp_sync<I: SyncIo>(
        &mut self,
        io: &mut I,
        timing: SyncTiming,
    ) -> SendResult {
        if !self.use_ptp || self.lane == NativeLane::Buffered {
            return SendResult::Sent;
        }
        self.ptp_anchor.freeze_if_needed(
            timing.master_now_ns,
            timing.local_ptp_now_ns,
            self.start_ntp,
            self.lead_ms,
            self.media.timeline.sample_rate,
            self.media.timeline.rtp_offset,
            self.media.timeline.wire_rtp,
        );
        let pkt = build_ptp_sync(
            true,
            self.media.timeline.wire_rtp,
            self.lead_ms,
            self.media.timeline.sample_rate,
            timing.master_now_ns,
            timing.master_clock_id,
            self.ptp_anchor,
        );
        execute_sync(io, &mut self.health, &mut self.sync_counters, &pkt)
    }

    /// Mirrors native ap2cl_send_chunk / ap2_native_send_chunk.
    pub fn send_chunk<E, C, I>(
        &mut self,
        pcm: &[u8],
        frames: u32,
        encoder: &mut E,
        cipher: &mut C,
        io: &mut I,
        timing: SyncTiming,
    ) -> SendResult
    where
        E: AlacEncoder,
        C: AudioCipher,
        I: MediaIo + SyncIo,
    {
        if self.state != Ap2State::Streaming || self.rtsp_dead {
            self.health.healthy = false;
            return SendResult::Fatal;
        }

        if self.lane == NativeLane::Buffered {
            let packet = match build_buffered_frame(
                &self.media,
                self.ssrc,
                pcm,
                frames,
                encoder,
                cipher,
            ) {
                Ok(v) => v,
                Err(_) => {
                    self.health.healthy = false;
                    return SendResult::Fatal;
                }
            };
            return execute_buffered(
                &mut self.media,
                &mut self.health,
                &mut self.pending,
                io,
                packet,
            );
        }

        let sync_result = self.send_due_sync(io, timing);
        if sync_result == SendResult::Fatal {
            return SendResult::Fatal;
        }

        let packet = match build_realtime_packet(
            &self.media,
            self.ssrc,
            pcm,
            frames,
            encoder,
            cipher,
        ) {
            Ok(v) => v,
            Err(_) => {
                self.health.healthy = false;
                return SendResult::Fatal;
            }
        };

        struct RtxAdapter<'a, I> {
            io: &'a mut I,
            ring: &'a Arc<Mutex<RtxRing>>,
        }
        impl<I: MediaIo> MediaIo for RtxAdapter<'_, I> {
            fn send_realtime(&mut self, packet: &[u8]) -> SendResult {
                self.io.send_realtime(packet)
            }
            fn send_buffered(&mut self, bytes: &[u8]) -> crate::native_media::StreamWrite {
                self.io.send_buffered(bytes)
            }
            fn close_buffered(&mut self) {
                self.io.close_buffered()
            }
            fn store_retransmit(&mut self, seq: u16, packet: &[u8]) {
                if let Ok(mut ring) = self.ring.lock() { let _ = ring.store(seq, packet); }
            }
        }

        let mut adapter = RtxAdapter { io, ring: &self.rtx_ring };
        execute_realtime(
            &mut self.media,
            &mut self.health,
            &mut adapter,
            &packet,
            sync_result,
        )
    }

    pub fn heartbeat_due(&self) -> bool {
        self.media.timeline.seq % 500 == 0
    }

    /// MSA realtime starvation recovery. Buffered native AP2 deliberately has no
    /// starvation re-anchor; its TCP queue is preserved instead.
    pub fn recover_input_gap<I: SyncIo>(
        &mut self,
        now_frame: u64,
        io: &mut I,
        timing: SyncTiming,
    ) -> bool {
        if self.state != Ap2State::Streaming || self.lane == NativeLane::Buffered {
            return false;
        }
        let window = self.pacing_window_frames();
        let recovery_lead = recovery_lead_frames(
            self.lead_ms,
            window,
            self.media.timeline.sample_rate,
        );

        if self.splice_timeline {
            let floor = frames_for_ms(MIN_WARM_LEAD_MS, self.media.timeline.sample_rate);
            let effective_head = self
                .media
                .timeline
                .head_frame
                .saturating_add(self.splice_pad_frames);
            let lapse = now_frame.saturating_add(floor);
            let pad = match splice_recovery_pad(effective_head, now_frame, lapse, recovery_lead) {
                Some(v) => v,
                None => return false,
            };
            self.splice_pad_frames = self.splice_pad_frames.saturating_add(pad);
            self.timeline_reanchors = self.timeline_reanchors.saturating_add(1);
            self.reanchor_shifted_frames = self.reanchor_shifted_frames.saturating_add(pad);
            return true;
        }

        let floor = frames_for_ms(MIN_WARM_LEAD_MS, self.media.timeline.sample_rate);
        let effects = match self
            .media
            .timeline
            .recover_stock(now_frame, floor, recovery_lead)
        {
            Some(v) => v,
            None => return false,
        };
        if self.use_ptp && self.ptp_anchor.valid {
            self.ptp_anchor.wall0_ns = self.ptp_anchor.wall0_ns.saturating_add(effects.anchor_shift_ns);
        }
        self.timeline_reanchors = self.timeline_reanchors.saturating_add(1);
        self.reanchor_shifted_frames = self
            .reanchor_shifted_frames
            .saturating_add(effects.shifted_frames);

        if effects.immediate_sync {
            let first = true;
            let result = if self.use_ptp {
                self.ptp_anchor.freeze_if_needed(
                    timing.master_now_ns,
                    timing.local_ptp_now_ns,
                    self.start_ntp,
                    self.lead_ms,
                    self.media.timeline.sample_rate,
                    self.media.timeline.rtp_offset,
                    self.media.timeline.wire_rtp,
                );
                let pkt = build_ptp_sync(
                    first,
                    self.media.timeline.wire_rtp,
                    self.lead_ms,
                    self.media.timeline.sample_rate,
                    timing.master_now_ns,
                    timing.master_clock_id,
                    self.ptp_anchor,
                );
                execute_sync(io, &mut self.health, &mut self.sync_counters, &pkt)
            } else {
                let pkt = build_ntp_sync(
                    first,
                    self.media.timeline.wire_rtp,
                    self.lead_ms,
                    self.media.timeline.sample_rate,
                    timing.ntp,
                );
                execute_sync(io, &mut self.health, &mut self.sync_counters, &pkt)
            };
            if result == SendResult::Fatal {
                return false;
            }
        }
        true
    }

    /// MSA splice delivery-gap recovery is non-anticipatory: only pad when the
    /// effective head has already fallen behind the current clock.
    pub fn recover_delivery_gap(&mut self, now_frame: u64) -> bool {
        if self.state != Ap2State::Streaming
            || self.lane == NativeLane::Buffered
            || !self.splice_timeline
        {
            return false;
        }
        let window = self.pacing_window_frames();
        let recovery_lead = recovery_lead_frames(
            self.lead_ms,
            window,
            self.media.timeline.sample_rate,
        );
        let effective_head = self
            .media
            .timeline
            .head_frame
            .saturating_add(self.splice_pad_frames);
        let pad = match splice_recovery_pad(effective_head, now_frame, now_frame, recovery_lead) {
            Some(v) => v,
            None => return false,
        };
        self.splice_pad_frames = self.splice_pad_frames.saturating_add(pad);
        self.timeline_reanchors = self.timeline_reanchors.saturating_add(1);
        self.reanchor_shifted_frames = self.reanchor_shifted_frames.saturating_add(pad);
        true
    }

    pub fn take_splice_pad_frames(&mut self, max_frames: u32) -> u32 {
        let take = self.splice_pad_frames.min(u64::from(max_frames)) as u32;
        self.splice_pad_frames -= u64::from(take);
        take
    }

    pub fn serve_rtx<I: RtxIo>(
        &mut self,
        io: &mut I,
        peer: &I::Peer,
        datagram: &[u8],
    ) -> bool {
        let Ok(ring) = self.rtx_ring.lock() else { return false };
        let Ok(mut counters) = self.rtx_counters.lock() else { return false };
        serve_request(&ring, &mut counters, io, peer, datagram)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_media::{MediaCounters, StreamWrite};
    use crate::native_timeline::Timeline;

    fn runtime(lane: NativeLane) -> NativeRuntime {
        NativeRuntime {
            state: Ap2State::Streaming,
            lane,
            rtsp_dead: false,
            use_ptp: false,
            splice_timeline: false,
            lead_ms: 2_000,
            dev_latency_max: 0,
            splice_depth_ms: 600,
            splice_depth_explicit: false,
            ssrc: 0x0506_0708,
            start_ntp: 0,
            media: NativeMediaState {
                timeline: Timeline {
                    sample_rate: 48_000,
                    head_frame: 48_000,
                    wire_rtp: 50_000,
                    rtp_offset: 2_000,
                    seq: 1,
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
        }
    }

    struct FakeAlac;
    impl AlacEncoder for FakeAlac {
        type Error = ();
        fn encode(&mut self, pcm: &[u8], _frames: u32) -> Result<Vec<u8>, Self::Error> {
            Ok(pcm.to_vec())
        }
    }

    struct FakeCipher;
    impl AudioCipher for FakeCipher {
        type Error = ();
        fn seal(
            &mut self,
            _nonce: &[u8; 12],
            _aad: &[u8],
            plain: &[u8],
        ) -> Result<(Vec<u8>, [u8; 16]), Self::Error> {
            Ok((plain.to_vec(), [0xaa; 16]))
        }
    }

    struct FakeIo {
        realtime: SendResult,
        sync: SendResult,
        buffered: Vec<StreamWrite>,
        closed: bool,
    }
    impl Default for FakeIo {
        fn default() -> Self {
            Self {
                realtime: SendResult::Sent,
                sync: SendResult::Sent,
                buffered: Vec::new(),
                closed: false,
            }
        }
    }
    impl MediaIo for FakeIo {
        fn send_realtime(&mut self, _packet: &[u8]) -> SendResult {
            self.realtime
        }
        fn send_buffered(&mut self, _bytes: &[u8]) -> StreamWrite {
            if self.buffered.is_empty() { StreamWrite::Complete } else { self.buffered.remove(0) }
        }
        fn close_buffered(&mut self) { self.closed = true; }
    }
    impl SyncIo for FakeIo {
        fn send_sync(&mut self, _packet: &[u8]) -> SendResult { self.sync }
    }

    fn timing() -> SyncTiming {
        SyncTiming {
            ntp: crate::native_timeline::unix_ms_to_ntp(1_000),
            master_now_ns: 1_000_000_000,
            local_ptp_now_ns: 1_000_000_000,
            master_clock_id: 7,
        }
    }

    #[test]
    fn accept_gate_is_streaming_rtsp_paced_and_drains_buffered_tail() {
        let mut r = runtime(NativeLane::Buffered);
        let mut io = FakeIo { buffered: vec![StreamWrite::WouldBlock], ..Default::default() };
        r.pending.park(vec![1, 2, 3]).unwrap();
        assert!(!r.accept_frames(48_000, 10_000, &mut io));
        io.buffered = vec![StreamWrite::Complete];
        assert!(r.accept_frames(48_000, 10_000, &mut io));
        assert_eq!(r.pace_last_release_us, 10_000);
        assert!(!r.accept_frames(48_000, 10_500, &mut io));
        r.rtsp_dead = true;
        assert!(!r.accept_frames(48_000, 20_000, &mut io));
    }

    #[test]
    fn first_realtime_send_syncs_stores_rtx_and_advances_once() {
        let mut r = runtime(NativeLane::Realtime);
        let mut io = FakeIo::default();
        let result = r.send_chunk(
            &[1, 2, 3],
            352,
            &mut FakeAlac,
            &mut FakeCipher,
            &mut io,
            timing(),
        );
        assert_eq!(result, SendResult::Sent);
        assert_eq!(r.sync_counters.sent, 1);
        assert!(r.rtx_ring.lock().unwrap().get(1).is_some());
        assert_eq!(r.media.timeline.seq, 2);
        assert!(!r.media.timeline.first_packet);
    }

    #[test]
    fn fatal_sync_stops_audio_before_timeline_advance() {
        let mut r = runtime(NativeLane::Realtime);
        let mut io = FakeIo { sync: SendResult::Fatal, ..Default::default() };
        assert_eq!(
            r.send_chunk(
                &[1],
                352,
                &mut FakeAlac,
                &mut FakeCipher,
                &mut io,
                timing(),
            ),
            SendResult::Fatal
        );
        assert_eq!(r.media.timeline.seq, 1);
        assert!(!r.health.healthy);
    }

    #[test]
    fn buffered_send_has_no_sync_or_rtx_and_commits_nonce_once() {
        let mut r = runtime(NativeLane::Buffered);
        let mut io = FakeIo { buffered: vec![StreamWrite::WouldBlock], ..Default::default() };
        assert_eq!(
            r.send_chunk(
                &[1, 2],
                352,
                &mut FakeAlac,
                &mut FakeCipher,
                &mut io,
                timing(),
            ),
            SendResult::Sent
        );
        assert_eq!(r.sync_counters.sent, 0);
        assert_eq!(r.media.counters.nonce_counter, 1);
        assert_eq!(r.media.timeline.seq, 2);
        assert!(!r.pending.is_empty());
        assert!(r.rtx_ring.lock().unwrap().get(1).is_none());
    }

    #[test]
    fn stock_recovery_preserves_wire_rtp_and_immediately_syncs() {
        let mut r = runtime(NativeLane::Realtime);
        r.media.timeline.head_frame = 48_000;
        r.media.timeline.wire_rtp = 50_000;
        r.media.timeline.rtp_offset = 2_000;
        let before = r.media.timeline.wire_rtp;
        let mut io = FakeIo::default();
        assert!(r.recover_input_gap(60_000, &mut io, timing()));
        assert_eq!(r.media.timeline.wire_rtp, before);
        assert_eq!(r.timeline_reanchors, 1);
        assert_eq!(r.sync_counters.sent, 1);
    }

    #[test]
    fn splice_input_recovery_is_anticipatory_but_delivery_recovery_is_not() {
        let mut r = runtime(NativeLane::Realtime);
        r.splice_timeline = true;
        r.media.timeline.head_frame = 100_000;
        let mut io = FakeIo::default();
        assert!(r.recover_input_gap(100_000, &mut io, timing()));
        assert!(r.splice_pad_frames > 0);
        let pad = r.splice_pad_frames;
        assert!(!r.recover_delivery_gap(100_000));
        assert_eq!(r.splice_pad_frames, pad);
        let taken = r.take_splice_pad_frames(352);
        assert_eq!(taken, 352.min(pad as u32));
    }
}
