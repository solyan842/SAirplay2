//! Native AP2 SOLO timeline math from pinned MSA ap2_client.c.
//! Frame-domain decisions are retained until the final wall-clock projection.

pub const BUFFERED_RTP_GAP_MS:u64=100;
pub const MIN_WARM_LEAD_MS:u64=250;

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct Timeline{
 pub sample_rate:u32,
 pub head_frame:u64,
 pub wire_rtp:u32,
 pub rtp_offset:u32,
 pub seq:u16,
 pub first_packet:bool,
}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct SplicePlan{pub accepted_unix_ms:u64,pub pad_frames:u64,pub corrected:bool}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct RecoveryPlan{pub head_frame:u64,pub rtp_offset:u32,pub shifted_frames:u64}

/// MSA stock realtime starvation recovery. Re-anchor only when the head is
/// inside the floor and the RTP/head invariant is still intact. The wire RTP
/// remains continuous; the head moves forward and the offset folds backward.
pub fn plan_stock_recovery(head_frame:u64,wire_rtp:u32,rtp_offset:u32,now_frame:u64,floor_frames:u64,recovery_lead_frames:u64)->Option<RecoveryPlan>{
 if head_frame>now_frame.saturating_add(floor_frames){return None;}
 if (head_frame as u32).wrapping_add(rtp_offset)!=wire_rtp{return None;}
 let target=now_frame.saturating_add(recovery_lead_frames);
 if target<=head_frame{return None;}
 let shifted=target-head_frame;
 Some(RecoveryPlan{head_frame:target,rtp_offset:rtp_offset.wrapping_sub(shifted as u32),shifted_frames:shifted})
}

pub fn frames_for_ms(ms:u64,sample_rate:u32)->u64{
 if sample_rate==0{0}else{ms.saturating_mul(u64::from(sample_rate))/1000}
}
pub fn ms_for_frames(frames:u64,sample_rate:u32)->u64{
 if sample_rate==0{0}else{frames.saturating_mul(1000)/u64::from(sample_rate)}
}

/// Hot splice keeps the immutable anchor line and advances it with encoded
/// silence. A stale request is corrected to head + 250 ms.
pub fn hot_splice(head_frame:u64,head_unix_ms:u64,requested_unix_ms:u64,sample_rate:u32)->SplicePlan{
 if requested_unix_ms==0{return SplicePlan{accepted_unix_ms:head_unix_ms,pad_frames:0,corrected:false}}
 if requested_unix_ms>=head_unix_ms{
  // MSA converts the requested instant into the sample/frame domain first,
  // then subtracts head_ts. Do not derive padding from truncated ms delta.
  let target=head_frame.saturating_add(frames_for_ms(requested_unix_ms-head_unix_ms,sample_rate));
  return SplicePlan{accepted_unix_ms:requested_unix_ms,pad_frames:target.saturating_sub(head_frame),corrected:false}
 }
 let accepted=head_unix_ms.saturating_add(MIN_WARM_LEAD_MS);
 let target=head_frame.saturating_add(frames_for_ms(MIN_WARM_LEAD_MS,sample_rate));
 SplicePlan{accepted_unix_ms:accepted,pad_frames:target.saturating_sub(head_frame),corrected:true}
}

/// Buffered RTP never moves backwards across FLUSHBUFFERED/re-anchor.
/// Fold any required continuity shift into rtp_offset, exactly like MSA.
pub fn buffered_reanchor(prev_rtp:u32,new_head_frame:u64,old_offset:u32,sample_rate:u32,audio_packets_sent:u64)->(u32,u32){
 let mut offset=old_offset;
 if audio_packets_sent>0{
  let wall=(new_head_frame as u32).wrapping_add(offset);
  let cont=prev_rtp.wrapping_add(frames_for_ms(BUFFERED_RTP_GAP_MS,sample_rate) as u32);
  let ahead=cont.wrapping_sub(wall) as i32;
  if ahead>0{offset=offset.wrapping_add(ahead as u32);}
 }
 let rtp=(new_head_frame as u32).wrapping_add(offset);
 (offset,rtp)
}

impl Timeline{
 pub fn reanchor_stock(&mut self,new_head_frame:u64,buffered:bool,audio_packets_sent:u64){
  let prev=self.wire_rtp;self.head_frame=new_head_frame;
  if buffered{
   let (off,rtp)=buffered_reanchor(prev,new_head_frame,self.rtp_offset,self.sample_rate,audio_packets_sent);
   self.rtp_offset=off;self.wire_rtp=rtp;
  }else{self.wire_rtp=(new_head_frame as u32).wrapping_add(self.rtp_offset);}
  self.first_packet=true;
 }
 pub fn recover_stock(&mut self,now_frame:u64,floor_frames:u64,recovery_lead_frames:u64)->Option<u64>{
  let p=plan_stock_recovery(self.head_frame,self.wire_rtp,self.rtp_offset,now_frame,floor_frames,recovery_lead_frames)?;
  self.head_frame=p.head_frame;self.rtp_offset=p.rtp_offset;Some(p.shifted_frames)
 }
 pub fn advance(&mut self,frames:u32){
  self.head_frame=self.head_frame.saturating_add(u64::from(frames));
  self.wire_rtp=self.wire_rtp.wrapping_add(frames);
  self.seq=self.seq.wrapping_add(1);
  self.first_packet=false;
 }
}

#[cfg(test)]
mod tests{
 use super::*;
 #[test]fn stock_recovery_preserves_wire_rtp_and_moves_head(){
  let mut t=Timeline{sample_rate:44100,head_frame:100000,wire_rtp:105000,rtp_offset:5000,seq:7,first_packet:false};
  let shifted=t.recover_stock(120000,11025,77175).unwrap();
  assert_eq!(shifted,97175);assert_eq!(t.head_frame,197175);assert_eq!(t.wire_rtp,105000);
  assert_eq!((t.head_frame as u32).wrapping_add(t.rtp_offset),t.wire_rtp);
 }
 #[test]fn stock_recovery_refuses_broken_rtp_head_invariant(){
  assert_eq!(plan_stock_recovery(100000,105001,5000,120000,11025,77175),None);
 }
 #[test]fn stock_recovery_is_idle_when_head_is_outside_floor(){
  assert_eq!(plan_stock_recovery(200000,205000,5000,100000,11025,77175),None);
 }
 #[test]fn splice_padding_is_frame_exact(){let p=hot_splice(123_456,1000,1125,48000);assert_eq!(p.pad_frames,6000);assert_eq!(p.accepted_unix_ms,1125);}
 #[test]fn splice_padding_preserves_nonzero_head_domain(){let p=hot_splice(9_000_000,2000,2250,44100);assert_eq!(p.pad_frames,11025);assert_eq!(p.accepted_unix_ms,2250);}
 #[test]fn stale_splice_moves_one_lead_beyond_head(){let p=hot_splice(0,2000,1900,44100);assert_eq!(p.accepted_unix_ms,2250);assert_eq!(p.pad_frames,11025);assert!(p.corrected);}
 #[test]fn buffered_reanchor_stays_beyond_previous_wire_head(){
  let (off,rtp)=buffered_reanchor(500_000,100_000,7,48000,1);
  assert_eq!(rtp,504_800);assert_eq!(off,404_800);
 }
 #[test]fn first_buffered_anchor_needs_no_continuity_shift(){let (o,r)=buffered_reanchor(0,100_000,7,48000,0);assert_eq!((o,r),(7,100_007));}
}
