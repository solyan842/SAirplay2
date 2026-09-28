//! Native realtime lifecycle ownership for the SAirplay 2.0 MSA clone.
//!
//! This deliberately implements only the source-verified warm FLUSH boundary.
//! START/STANDBY are not claimed here until their physical AP2 primitives are
//! wired. The caller must quiesce the Windows consumer before calling flush().

use crate::{
    send_native_realtime_flush, MsaRealtimeFlushPoint, SharedCseq,
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
    ) -> Self {
        Self {
            sender,
            control,
            next_cseq,
            session_uri,
            dacp_id,
            active_remote,
            mode,
        }
    }

    pub fn sender(&self) -> &SharedRealtimeMediaSender {
        &self.sender
    }

    pub fn mode(&self) -> MsaNativeWarmMode {
        self.mode
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
}
