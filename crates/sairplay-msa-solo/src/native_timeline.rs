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
  let delta=requested_unix_ms-head_unix_ms;
  return SplicePlan{accepted_unix_ms:requested_unix_ms,pad_frames:frames_for_ms(delta,sample_rate),corrected:false}
 }
 let accepted=head_unix_ms.saturating_add(MIN_WARM_LEAD_MS);
 SplicePlan{accepted_unix_ms:accepted,pad_frames:frames_for_ms(MIN_WARM_LEAD_MS,sample_rate),corrected:true}
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
 #[test]fn splice_padding_is_frame_exact(){let p=hot_splice(0,1000,1125,48000);assert_eq!(p.pad_frames,6000);assert_eq!(p.accepted_unix_ms,1125);}
 #[test]fn stale_splice_moves_one_lead_beyond_head(){let p=hot_splice(0,2000,1900,44100);assert_eq!(p.accepted_unix_ms,2250);assert_eq!(p.pad_frames,11025);assert!(p.corrected);}
 #[test]fn buffered_reanchor_stays_beyond_previous_wire_head(){
  let (off,rtp)=buffered_reanchor(500_000,100_000,7,48000,1);
  assert_eq!(rtp,504_800);assert_eq!(off,404_800);
 }
 #[test]fn first_buffered_anchor_needs_no_continuity_shift(){let (o,r)=buffered_reanchor(0,100_000,7,48000,0);assert_eq!((o,r),(7,100_007));}
}
