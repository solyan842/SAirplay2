//! Native AP2 SOLO command semantics ported from pinned MSA ap2_client.c.
//! Late-join/group correction is intentionally excluded until SOLO parity is complete.

use crate::timing::{resolve_ap2_warm_start, StartResolution, AP2_MIN_WARM_LEAD_MS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ap2State { Down, Connected, Streaming }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeLane { Realtime, Buffered }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumePlan {
    pub start: StartResolution,
    pub silence_pad_ms: u64,
    pub preserve_anchor_line: bool,
}

pub trait NativeAp2Transport {
    type Error;
    fn state(&self) -> Ap2State;
    fn now_unix_ms(&self) -> u64;
    fn splice_timeline(&self) -> bool;
    fn anchor_valid(&self) -> bool;
    fn audible_head_unix_ms(&self) -> u64;
    fn lane(&self) -> NativeLane;
    fn rtsp_alive(&self) -> bool;
    fn keep_splice_queue(&mut self);
    fn flush_realtime(&mut self) -> Result<(), Self::Error>;
    fn flush_buffered(&mut self) -> Result<(), Self::Error>;
    fn park_buffered(&mut self) -> Result<(), Self::Error>;
    fn set_connected(&mut self);
    fn set_streaming(&mut self);
    fn clear_anchor(&mut self);
    fn anchor_start(&mut self, at_unix_ms: u64) -> Result<(), Self::Error>;
    fn announce_ptp_timeline(&mut self) -> Result<(), Self::Error>;
}

pub fn start<T: NativeAp2Transport>(t:&mut T, requested:u64)->Result<StartResolution,T::Error>{
    assert!(t.state()!=Ap2State::Down,"MSA AP2 START requires a live session");
    let s=resolve_ap2_warm_start(t.now_unix_ms(),requested);
    t.anchor_start(s.at_unix_ms)?;
    t.set_streaming();
    t.announce_ptp_timeline()?;
    Ok(s)
}

pub fn flush<T:NativeAp2Transport>(t:&mut T)->Result<(),T::Error>{
    assert!(t.state()!=Ap2State::Down,"MSA AP2 FLUSH requires a live session");
    assert!(t.rtsp_alive(),"MSA AP2 FLUSH requires live RTSP");
    if t.splice_timeline() {
        t.keep_splice_queue();
        return Ok(());
    }
    match t.lane(){NativeLane::Realtime=>t.flush_realtime()?,NativeLane::Buffered=>t.flush_buffered()?}
    t.clear_anchor();
    Ok(())
}

pub fn standby<T:NativeAp2Transport>(t:&mut T)->Result<(),T::Error>{
    assert!(t.state()!=Ap2State::Down,"MSA AP2 STANDBY requires a live session");
    if t.splice_timeline() {
        t.keep_splice_queue();
        return Ok(());
    }
    if t.rtsp_alive() {
        match t.lane(){NativeLane::Realtime=>t.flush_realtime()?,NativeLane::Buffered=>{t.park_buffered()?;t.flush_buffered()?;}}
    }
    t.clear_anchor();
    t.set_connected();
    Ok(())
}

/// MSA hot-splice rule: a valid future head is the feasibility floor.
/// Feasible requests pad silence to the exact instant; stale requests are
/// corrected to head + 250ms so a corrective command does not chase the head.
pub fn resolve_hot_splice(head_unix_ms:u64, requested:u64)->ResumePlan{
    if requested==0 {
        return ResumePlan{start:StartResolution{requested_unix_ms:0,at_unix_ms:head_unix_ms,corrected_forward:false},silence_pad_ms:0,preserve_anchor_line:true};
    }
    if requested>=head_unix_ms {
        return ResumePlan{start:StartResolution{requested_unix_ms:requested,at_unix_ms:requested,corrected_forward:false},silence_pad_ms:requested-head_unix_ms,preserve_anchor_line:true};
    }
    let at=head_unix_ms.saturating_add(AP2_MIN_WARM_LEAD_MS);
    ResumePlan{start:StartResolution{requested_unix_ms:requested,at_unix_ms:at,corrected_forward:true},silence_pad_ms:AP2_MIN_WARM_LEAD_MS,preserve_anchor_line:true}
}

pub fn resume<T:NativeAp2Transport>(t:&mut T,requested:u64)->Result<ResumePlan,T::Error>{
    assert!(t.state()!=Ap2State::Down && t.rtsp_alive(),"MSA AP2 RESUME requires live session");
    let now=t.now_unix_ms();
    if t.splice_timeline() && t.anchor_valid() && t.audible_head_unix_ms()>now {
        let plan=resolve_hot_splice(t.audible_head_unix_ms(),requested);
        t.set_streaming();
        return Ok(plan);
    }
    let s=resolve_ap2_warm_start(now,requested);
    t.anchor_start(s.at_unix_ms)?;
    t.set_streaming();
    t.announce_ptp_timeline()?;
    Ok(ResumePlan{start:s,silence_pad_ms:0,preserve_anchor_line:false})
}

#[cfg(test)]
mod tests {
 use super::*;
 #[test] fn hot_splice_exact_request_pads_to_command(){let p=resolve_hot_splice(2000,2300);assert_eq!(p.start.at_unix_ms,2300);assert_eq!(p.silence_pad_ms,300);assert!(p.preserve_anchor_line);}
 #[test] fn hot_splice_stale_request_corrects_beyond_head(){let p=resolve_hot_splice(2000,1900);assert_eq!(p.start.at_unix_ms,2250);assert!(p.start.corrected_forward);}
 #[test] fn hot_splice_zero_means_head(){let p=resolve_hot_splice(2000,0);assert_eq!(p.start.at_unix_ms,2000);assert_eq!(p.silence_pad_ms,0);}
}
