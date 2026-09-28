//! Native realtime lifecycle ownership for the SAirplay 2.0 MSA clone.
//!
//! This deliberately implements only the source-verified warm FLUSH boundary.
//! START/STANDBY are not claimed here until their physical AP2 primitives are
//! wired. The caller must quiesce the Windows consumer before calling flush().

use crate::{
    msa_resolve_ap2_start, send_native_realtime_flush, MsaAp2ClockReadiness,
    MsaAp2StartResolution, MsaRealtimeFlushPoint, SharedCseq,
    SharedRealtimeMediaSender, SharedRtspControl,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaNativeWarmMode {
    /// Pinned MSA splice path: never send an RTSP FLUSH. Keep the receiver
    /// queue and frozen anchor line alive, and return the delivery head only.
    Splice,
    /// Stock native realtime fallback: discard receiver queued audio using
    /// classic RTSP FLUSH with the current RTP-Info.
    StockRealtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaNativeFlushAction {
    KeepSpliceQueue,
    SendStockRealtime {
        sequence: u16,
        rtptime: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsaNativeFlushResult {
    pub point: MsaRealtimeFlushPoint,
    pub action: MsaNativeFlushAction,
}

pub fn msa_native_flush_action(
    mode: MsaNativeWarmMode,
    point: MsaRealtimeFlushPoint,
) -> MsaNativeFlushAction {
    match mode {
        MsaNativeWarmMode::Splice => MsaNativeFlushAction::KeepSpliceQueue,
        MsaNativeWarmMode::StockRealtime => MsaNativeFlushAction::SendStockRealtime {
            sequence: point.sequence,
            rtptime: point.rtptime,
        },
    }
}

pub struct MsaNativeRealtimeOwner {
    sender: SharedRealtimeMediaSender,
    control: SharedRtspControl,
    next_cseq: SharedCseq,
    session_uri: String,
    dacp_id: String,
    active_remote: String,
    mode: MsaNativeWarmMode,
    latency_max: Option<u32>,
    lead_frames: u32,
    rtp_offset: u32,
}

impl MsaNativeRealtimeOwner {
    pub fn new(
        sender: SharedRealtimeMediaSender,
        control: SharedRtspControl,
        next_cseq: SharedCseq,
        session_uri: String,
        dacp_id: String,
        active_remote: String,
        mode: MsaNativeWarmMode,
        latency_max: Option<u32>,
        lead_frames: u32,
        rtp_offset: u32,
    ) -> Self {
        Self {
            sender,
            control,
            next_cseq,
            session_uri,
            dacp_id,
            active_remote,
            mode,
            latency_max,
            lead_frames,
            rtp_offset,
        }
    }

    pub fn sender(&self) -> &SharedRealtimeMediaSender {
        &self.sender
    }

    pub fn mode(&self) -> MsaNativeWarmMode {
        self.mode
    }

    /// Resolve feasibility exactly like pinned ap2cl_start(), then physically
    /// commit that acknowledged instant onto the one shared realtime sender.
    /// The returned ACK is therefore not published until the sender timeline
    /// has accepted the same audible instant.
    pub fn start_quiesced(
        &mut self,
        now_unix_ms: u64,
        requested_start_unix_ms: u64,
        clock: MsaAp2ClockReadiness,
    ) -> Result<MsaAp2StartResolution, String> {
        let resolution = msa_resolve_ap2_start(
            now_unix_ms,
            requested_start_unix_ms,
            clock,
        );

        self.sender
            .lock()
            .map_err(|_| "MSA realtime sender lock poisoned".to_owned())?
            .msa_commit_cold_start_at(
                resolution.acknowledged_unix_ms,
                self.latency_max,
                self.lead_frames,
                self.rtp_offset,
            )
            .map_err(|error| format!("MSA physical AP2 START failed: {error:?}"))?;

        Ok(resolution)
    }

    /// Apply the post-commit cold-clock correction before real audio has gone
    /// out. This is the physical half of ap2_rebase_pending_anchor(); it keeps
    /// RTP sequence/nonces continuous and moves only the pending anchor line.
    pub fn rebase_pending_start_quiesced(
        &mut self,
        corrected_unix_ms: u64,
    ) -> Result<(), String> {
        self.sender
            .lock()
            .map_err(|_| "MSA realtime sender lock poisoned".to_owned())?
            .msa_rebase_pending_anchor_at(
                corrected_unix_ms,
                self.lead_frames,
                self.rtp_offset,
            )
            .map_err(|error| format!("MSA physical AP2 rebase failed: {error:?}"))
    }

    /// Execute a source-accurate warm boundary after the send loop is
    /// quiesced. One sender lock captures seq/rtptime/head atomically.
    pub fn flush_quiesced(&mut self) -> Result<MsaNativeFlushResult, String> {
        let point = self
            .sender
            .lock()
            .map_err(|_| "MSA realtime sender lock poisoned".to_owned())?
            .msa_flush_point();

        let action = msa_native_flush_action(self.mode, point);
        if let MsaNativeFlushAction::SendStockRealtime { sequence, rtptime } = action {
            send_native_realtime_flush(
                &self.control,
                &self.next_cseq,
                &self.session_uri,
                &self.dacp_id,
                &self.active_remote,
                sequence,
                rtptime,
            )
            .map_err(|error| format!("{error}"))?;
        }

        Ok(MsaNativeFlushResult { point, action })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_flush_never_requests_rtsp_discard() {
        let point = MsaRealtimeFlushPoint {
            sequence: 9,
            rtptime: 1234,
            warm_head_unix_ms: Some(55_000),
        };

        assert_eq!(
            msa_native_flush_action(MsaNativeWarmMode::Splice, point),
            MsaNativeFlushAction::KeepSpliceQueue
        );
    }

    #[test]
    fn stock_flush_uses_wire_coordinates_not_warm_head() {
        let point = MsaRealtimeFlushPoint {
            sequence: 41,
            rtptime: 0xAABB_CCDD,
            warm_head_unix_ms: Some(88_000),
        };

        assert_eq!(
            msa_native_flush_action(MsaNativeWarmMode::StockRealtime, point),
            MsaNativeFlushAction::SendStockRealtime {
                sequence: 41,
                rtptime: 0xAABB_CCDD,
            }
        );
    }

    #[test]
    fn start_resolution_is_the_same_instant_committed_to_sender() {
        use crate::{
            MediaTransport, MsaAp2ClockState, PtpExchange, RealtimeMediaSender,
            RtpState, StreamPorts,
        };
        use std::net::{IpAddr, Ipv4Addr, UdpSocket};
        use std::sync::{Arc, Mutex};

        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut transport = MediaTransport::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        transport.attach_remote(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            StreamPorts {
                data_port: data_rx.local_addr().unwrap().port(),
                control_port: ctrl_rx.local_addr().unwrap().port(),
            },
        );

        let sender = Arc::new(Mutex::new(RealtimeMediaSender::new(
            transport,
            RtpState::new(1, 2, 3),
            [0x66u8; 32],
        )));

        // Dummy control is intentionally not exercised by START.
        let tcp = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = tcp.local_addr().unwrap();
        let join = std::thread::spawn(move || tcp.accept().unwrap().0);
        let client = std::net::TcpStream::connect(addr).unwrap();
        let _server = join.join().unwrap();
        let channel = crate::EncryptedRtspChannel::for_test_plaintext(client);
        let control = Arc::new(Mutex::new(channel));
        let cseq = Arc::new(std::sync::atomic::AtomicU32::new(1));

        let mut owner = MsaNativeRealtimeOwner::new(
            Arc::clone(&sender),
            control,
            cseq,
            "rtsp://127.0.0.1/1".into(),
            "AABBCCDDEEFF0011".into(),
            "123".into(),
            MsaNativeWarmMode::Splice,
            Some(66_150),
            11_025,
            0x000A_AA00,
        );

        let clock = MsaAp2ClockReadiness {
            state: MsaAp2ClockState::Ready,
            exchanges: 3,
            ready_in_ms: 0,
            ready_at_unix_ms: 10_000,
        };
        let ack = owner.start_quiesced(10_000, 10_100, clock).unwrap();

        assert_eq!(ack.acknowledged_unix_ms, 10_500);
        assert_eq!(
            sender.lock().unwrap().msa_flush_point().warm_head_unix_ms,
            Some(10_500)
        );
    }

}
