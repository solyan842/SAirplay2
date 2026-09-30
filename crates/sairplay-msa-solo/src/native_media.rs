//! Source-faithful native AP2 SOLO media-wire state.
//! Pinned to music-assistant/airplay-cli 431c5c582eef9307c4e39c50a0ea65e970bc1128.

use crate::native_timeline::Timeline;

pub const REALTIME_PAYLOAD_TYPE:u8=96;
pub const BUFFERED_PAYLOAD_TYPE:u8=103;
pub const CHACHA_TAG_SIZE:usize=16;
pub const TRAILING_NONCE_SIZE:usize=8;
pub const BUFFERED_PREFIX_SIZE:usize=2;
pub const PERIODIC_SYNC_CHUNKS:u16=100;
pub const FILL_MIN_PACKET_GAP_US:u64=1000;
pub const PACING_MARGIN_MS:u64=250;
pub const PACING_DEFAULT_BUFFER_MS:u64=2000;

pub fn pacing_window_frames(sample_rate:u32,dev_latency_max:u64,buffered:bool,splice:bool,splice_depth_ms:u64,depth_explicit:bool)->u64{
 let margin=crate::native_timeline::frames_for_ms(PACING_MARGIN_MS,sample_rate);
 let reported=dev_latency_max>margin;
 let mut window=if reported{dev_latency_max-margin}else{crate::native_timeline::frames_for_ms(PACING_DEFAULT_BUFFER_MS-PACING_MARGIN_MS,sample_rate)};
 let depth=crate::native_timeline::frames_for_ms(splice_depth_ms,sample_rate);
 if buffered{return window.max(depth)}
 if splice{if !reported&&depth_explicit{return depth}window=window.min(depth);}
 window

}

pub fn pacing_accept(now_frame:u64,head_frame:u64,window_frames:u64,last_release_us:u64,now_us:u64)->bool{
 if now_frame.saturating_add(window_frames)<head_frame{return false}
 if last_release_us!=0&&now_us.saturating_sub(last_release_us)<FILL_MIN_PACKET_GAP_US{return false}
 true
}

pub fn recovery_lead_frames(lead_ms:u64,window_frames:u64,sample_rate:u32)->u64{
 crate::native_timeline::frames_for_ms(lead_ms,sample_rate).min(window_frames)
}

pub fn splice_recovery_pad(effective_head:u64,now_frame:u64,lapse_frame:u64,recovery_lead:u64)->Option<u64>{
 if effective_head>lapse_frame{return None}let target=now_frame.saturating_add(recovery_lead);if target<=effective_head{None}else{Some(target-effective_head)}
}

#[derive(Debug,Clone,Copy,PartialEq,Eq)] pub enum SendResult{Sent,Dropped,Fatal}
#[derive(Debug,Clone,Copy,PartialEq,Eq)] pub enum SyncKind{Initial,Periodic}
#[derive(Debug,Clone,Copy,PartialEq,Eq)] pub struct RtpHeader{pub bytes:[u8;12]}
impl RtpHeader{
 pub fn new(buffered:bool,first:bool,seq:u16,rtp:u32,ssrc:u32)->Self{
  let mut b=[0u8;12];b[0]=0x80;
  let pt=if buffered{BUFFERED_PAYLOAD_TYPE}else{REALTIME_PAYLOAD_TYPE};
  b[1]=pt|if first{0x80}else{0};b[2..4].copy_from_slice(&seq.to_be_bytes());
  b[4..8].copy_from_slice(&rtp.to_be_bytes());b[8..12].copy_from_slice(&ssrc.to_be_bytes());Self{bytes:b}
 }
 pub fn aad(&self)->&[u8]{&self.bytes[4..12]}
}
pub fn realtime_nonce(seq:u16)->[u8;12]{let mut n=[0u8;12];n[4..6].copy_from_slice(&seq.to_le_bytes());n}
pub fn buffered_nonce(counter:u64)->[u8;12]{let mut n=[0u8;12];n[4..12].copy_from_slice(&counter.to_le_bytes());n}
pub fn trailing_nonce(nonce:&[u8;12])->[u8;8]{let mut out=[0u8;8];out.copy_from_slice(&nonce[4..12]);out}
pub fn buffered_total_len(encoded_len:usize)->Option<u16>{
 let n=BUFFERED_PREFIX_SIZE.checked_add(12)?.checked_add(encoded_len)?.checked_add(CHACHA_TAG_SIZE)?.checked_add(TRAILING_NONCE_SIZE)?;
 u16::try_from(n).ok()
}
pub fn sync_due(first_packet:bool,seq:u16)->Option<SyncKind>{
 if first_packet{Some(SyncKind::Initial)}else if seq%PERIODIC_SYNC_CHUNKS==0{Some(SyncKind::Periodic)}else{None}
}
#[derive(Debug,Clone,Copy,PartialEq,Eq)] pub struct MediaCounters{pub sent:u64,pub dropped:u64,pub nonce_counter:u64}
#[derive(Debug,Clone,Copy,PartialEq,Eq)] pub struct NativeMediaState{pub timeline:Timeline,pub counters:MediaCounters}
impl NativeMediaState{
 pub fn commit_realtime(&mut self,frames:u32,audio:SendResult,sync:SendResult)->SendResult{
  if audio==SendResult::Fatal{return SendResult::Fatal}
  match audio{SendResult::Sent=>self.counters.sent+=1,SendResult::Dropped=>self.counters.dropped+=1,SendResult::Fatal=>unreachable!()}
  if audio==SendResult::Sent&&sync==SendResult::Sent{self.timeline.first_packet=false;}
  self.timeline.head_frame=self.timeline.head_frame.saturating_add(u64::from(frames));
  self.timeline.wire_rtp=self.timeline.wire_rtp.wrapping_add(frames);self.timeline.seq=self.timeline.seq.wrapping_add(1);audio
 }
 pub fn consume_buffered_nonce(&mut self){self.counters.nonce_counter=self.counters.nonce_counter.wrapping_add(1);}
 pub fn commit_buffered_frame(&mut self,frames:u32){
  self.counters.sent+=1;self.timeline.first_packet=false;
  self.timeline.head_frame=self.timeline.head_frame.saturating_add(u64::from(frames));
  self.timeline.wire_rtp=self.timeline.wire_rtp.wrapping_add(frames);self.timeline.seq=self.timeline.seq.wrapping_add(1);
 }
}

pub trait AlacEncoder{type Error;fn encode(&mut self,pcm:&[u8],frames:u32)->Result<Vec<u8>,Self::Error>;}
pub trait AudioCipher{type Error;fn seal(&mut self,nonce:&[u8;12],aad:&[u8],plain:&[u8])->Result<(Vec<u8>,[u8;CHACHA_TAG_SIZE]),Self::Error>;}
#[derive(Debug,PartialEq,Eq)] pub enum BuildError<EE,CE>{Encode(EE),Encrypt(CE),FrameTooLarge}
#[derive(Debug,Clone,PartialEq,Eq)] pub struct BuiltPacket{pub bytes:Vec<u8>,pub seq:u16,pub rtp:u32,pub frames:u32}

pub fn build_realtime_packet<E:AlacEncoder,C:AudioCipher>(state:&NativeMediaState,ssrc:u32,pcm:&[u8],frames:u32,enc:&mut E,cipher:&mut C)->Result<BuiltPacket,BuildError<E::Error,C::Error>>{
 let encoded=enc.encode(pcm,frames).map_err(BuildError::Encode)?;
 let hdr=RtpHeader::new(false,state.timeline.first_packet,state.timeline.seq,state.timeline.wire_rtp,ssrc);
 let nonce=realtime_nonce(state.timeline.seq);
 let (ct,tag)=cipher.seal(&nonce,hdr.aad(),&encoded).map_err(BuildError::Encrypt)?;
 let mut bytes=Vec::with_capacity(12+ct.len()+CHACHA_TAG_SIZE+TRAILING_NONCE_SIZE);
 bytes.extend_from_slice(&hdr.bytes);bytes.extend_from_slice(&ct);bytes.extend_from_slice(&tag);bytes.extend_from_slice(&trailing_nonce(&nonce));
 Ok(BuiltPacket{bytes,seq:state.timeline.seq,rtp:state.timeline.wire_rtp,frames})
}

pub fn build_buffered_frame<E:AlacEncoder,C:AudioCipher>(state:&NativeMediaState,ssrc:u32,pcm:&[u8],frames:u32,enc:&mut E,cipher:&mut C)->Result<BuiltPacket,BuildError<E::Error,C::Error>>{
 let encoded=enc.encode(pcm,frames).map_err(BuildError::Encode)?;
 let hdr=RtpHeader::new(true,state.timeline.first_packet,state.timeline.seq,state.timeline.wire_rtp,ssrc);
 let nonce=buffered_nonce(state.counters.nonce_counter);
 let (ct,tag)=cipher.seal(&nonce,hdr.aad(),&encoded).map_err(BuildError::Encrypt)?;
 let total=buffered_total_len(ct.len()).ok_or(BuildError::FrameTooLarge)?;
 let mut bytes=Vec::with_capacity(total as usize);bytes.extend_from_slice(&total.to_be_bytes());bytes.extend_from_slice(&hdr.bytes);bytes.extend_from_slice(&ct);bytes.extend_from_slice(&tag);bytes.extend_from_slice(&trailing_nonce(&nonce));
 Ok(BuiltPacket{bytes,seq:state.timeline.seq,rtp:state.timeline.wire_rtp,frames})
}


#[derive(Debug,Clone,Copy,PartialEq,Eq)] pub enum StreamWrite{Complete,Partial(usize),WouldBlock,Fatal}
pub trait MediaIo{
 fn send_realtime(&mut self,packet:&[u8])->SendResult;
 fn send_buffered(&mut self,bytes:&[u8])->StreamWrite;
 fn close_buffered(&mut self){}
 fn store_retransmit(&mut self,_seq:u16,_packet:&[u8]){}
}
#[derive(Debug,Clone,Copy,PartialEq,Eq)] pub struct MediaHealth{pub healthy:bool}
impl Default for MediaHealth{fn default()->Self{Self{healthy:true}}}

pub fn execute_realtime<I:MediaIo>(state:&mut NativeMediaState,health:&mut MediaHealth,io:&mut I,packet:&BuiltPacket,sync:SendResult)->SendResult{
 if sync==SendResult::Fatal{health.healthy=false;return SendResult::Fatal}
 let result=io.send_realtime(&packet.bytes);
 if result==SendResult::Sent{io.store_retransmit(packet.seq,&packet.bytes);}
 if result==SendResult::Fatal{health.healthy=false;return SendResult::Fatal}
 state.commit_realtime(packet.frames,result,sync)
}

pub fn execute_buffered<I:MediaIo>(state:&mut NativeMediaState,health:&mut MediaHealth,pending:&mut BufferedPending,io:&mut I,packet:BuiltPacket)->SendResult{
 if !pending.is_empty(){return SendResult::Dropped}
 state.consume_buffered_nonce();
 let frame_len=packet.bytes.len();
 match io.send_buffered(&packet.bytes){
  StreamWrite::Fatal=>{health.healthy=false;pending.clear();io.close_buffered();SendResult::Fatal}
  StreamWrite::Complete=>{state.commit_buffered_frame(packet.frames);SendResult::Sent}
  StreamWrite::Partial(n)=>{
   let n=n.min(frame_len);if n<frame_len{let mut tail=packet.bytes;tail.drain(..n);let _=pending.park(tail);}
   state.commit_buffered_frame(packet.frames);SendResult::Sent
  }
  StreamWrite::WouldBlock=>{let _=pending.park(packet.bytes);state.commit_buffered_frame(packet.frames);SendResult::Sent}
 }
}

pub fn drain_buffered_pending<I:MediaIo>(health:&mut MediaHealth,pending:&mut BufferedPending,io:&mut I)->bool{
 while !pending.is_empty(){
  let rem=pending.remaining();
  match io.send_buffered(rem){
   StreamWrite::Complete=>{let n=rem.len();pending.consume(n);}
   StreamWrite::Partial(n)=>{if n==0{return false}pending.consume(n);}
   StreamWrite::WouldBlock=>return false,
   StreamWrite::Fatal=>{health.healthy=false;pending.clear();io.close_buffered();return false}
  }
 }
 true
}

#[derive(Debug,Clone,PartialEq,Eq,Default)] pub struct BufferedPending{bytes:Vec<u8>,off:usize}
impl BufferedPending{
 pub fn is_empty(&self)->bool{self.off>=self.bytes.len()}
 pub fn remaining(&self)->&[u8]{if self.is_empty(){&[]}else{&self.bytes[self.off..]}}
 pub fn park(&mut self,frame:Vec<u8>)->Result<(),Vec<u8>>{if !self.is_empty(){return Err(frame)}self.bytes=frame;self.off=0;Ok(())}
 pub fn consume(&mut self,n:usize){self.off=self.off.saturating_add(n).min(self.bytes.len());if self.is_empty(){self.bytes.clear();self.off=0;}}
 pub fn clear(&mut self){self.bytes.clear();self.off=0;}
}
#[cfg(test)] mod tests{
 use super::*;
 struct FakeIo{rt:SendResult,writes:Vec<StreamWrite>,rtx:usize}
 impl Default for FakeIo{fn default()->Self{Self{rt:SendResult::Sent,writes:Vec::new(),rtx:0}}}
 impl MediaIo for FakeIo{fn send_realtime(&mut self,_:&[u8])->SendResult{self.rt}fn send_buffered(&mut self,_:&[u8])->StreamWrite{if self.writes.is_empty(){StreamWrite::Complete}else{self.writes.remove(0)}}fn store_retransmit(&mut self,_:u16,_:&[u8]){self.rtx+=1}}
 #[test]fn realtime_executor_stores_rtx_only_on_sent(){let mut s=state(true);let mut h=MediaHealth::default();let mut io=FakeIo{rt:SendResult::Sent,..Default::default()};let p=BuiltPacket{bytes:vec![1],seq:s.timeline.seq,rtp:s.timeline.wire_rtp,frames:352};assert_eq!(execute_realtime(&mut s,&mut h,&mut io,&p,SendResult::Sent),SendResult::Sent);assert_eq!(io.rtx,1);assert!(h.healthy);let p2=BuiltPacket{bytes:vec![2],seq:s.timeline.seq,rtp:s.timeline.wire_rtp,frames:352};io.rt=SendResult::Dropped;execute_realtime(&mut s,&mut h,&mut io,&p2,SendResult::Sent);assert_eq!(io.rtx,1);assert_eq!(s.counters.dropped,1);}
 #[test]fn fatal_sync_never_sends_audio_and_marks_unhealthy(){let mut s=state(true);let mut h=MediaHealth::default();let mut io=FakeIo{rt:SendResult::Sent,..Default::default()};let p=BuiltPacket{bytes:vec![1],seq:s.timeline.seq,rtp:s.timeline.wire_rtp,frames:352};assert_eq!(execute_realtime(&mut s,&mut h,&mut io,&p,SendResult::Fatal),SendResult::Fatal);assert!(!h.healthy);assert_eq!(s.timeline.seq,0x1234);}
 #[test]fn buffered_partial_write_commits_frame_and_parks_tail(){let mut s=state(true);let mut h=MediaHealth::default();let mut p=BufferedPending::default();let mut io=FakeIo{writes:vec![StreamWrite::Partial(2)],rt:SendResult::Sent,rtx:0};let b=BuiltPacket{bytes:vec![1,2,3,4,5],seq:s.timeline.seq,rtp:s.timeline.wire_rtp,frames:352};assert_eq!(execute_buffered(&mut s,&mut h,&mut p,&mut io,b),SendResult::Sent);assert_eq!(p.remaining(),&[3,4,5]);assert_eq!(s.timeline.seq,0x1235);}
 #[test]fn buffered_wouldblock_parks_whole_committed_frame(){let mut s=state(true);let mut h=MediaHealth::default();let mut p=BufferedPending::default();let mut io=FakeIo{writes:vec![StreamWrite::WouldBlock],rt:SendResult::Sent,rtx:0};let b=BuiltPacket{bytes:vec![1,2,3],seq:s.timeline.seq,rtp:s.timeline.wire_rtp,frames:352};execute_buffered(&mut s,&mut h,&mut p,&mut io,b);assert_eq!(p.remaining(),&[1,2,3]);assert_eq!(s.counters.nonce_counter,1);}
 #[test]fn buffered_pending_drains_in_order_and_fatal_marks_unhealthy(){let mut h=MediaHealth::default();let mut p=BufferedPending::default();p.park(vec![1,2,3,4]).unwrap();let mut io=FakeIo{writes:vec![StreamWrite::Partial(2),StreamWrite::WouldBlock],rt:SendResult::Sent,rtx:0};assert!(!drain_buffered_pending(&mut h,&mut p,&mut io));assert_eq!(p.remaining(),&[3,4]);io.writes=vec![StreamWrite::Fatal];assert!(!drain_buffered_pending(&mut h,&mut p,&mut io));assert!(!h.healthy);assert!(p.is_empty());}
 fn state(first:bool)->NativeMediaState{NativeMediaState{timeline:Timeline{sample_rate:44100,head_frame:1000,wire_rtp:5000,rtp_offset:4000,seq:0x1234,first_packet:first},counters:MediaCounters{sent:0,dropped:0,nonce_counter:0}}}
 #[derive(Default)]struct FakeAlac;impl AlacEncoder for FakeAlac{type Error=();fn encode(&mut self,pcm:&[u8],_:u32)->Result<Vec<u8>,Self::Error>{Ok(pcm.to_vec())}}
 #[derive(Default)]struct FakeCipher;impl AudioCipher for FakeCipher{type Error=();fn seal(&mut self,_:&[u8;12],_:&[u8],plain:&[u8])->Result<(Vec<u8>,[u8;16]),Self::Error>{Ok((plain.to_vec(),[0xaa;16]))}}
 #[test]fn realtime_builder_matches_msa_wire_shape(){let s=state(true);let p=build_realtime_packet(&s,0x05060708,&[9,8,7],352,&mut FakeAlac,&mut FakeCipher).unwrap();assert_eq!(&p.bytes[..12],&[0x80,0xe0,0x12,0x34,0,0,0x13,0x88,5,6,7,8]);assert_eq!(&p.bytes[12..15],&[9,8,7]);assert_eq!(&p.bytes[p.bytes.len()-8..],&[0x34,0x12,0,0,0,0,0,0]);}
 #[test]fn buffered_builder_prefix_and_nonce_match_msa(){let mut s=state(true);s.counters.nonce_counter=0x0807060504030201;let p=build_buffered_frame(&s,0x05060708,&[9,8,7],352,&mut FakeAlac,&mut FakeCipher).unwrap();assert_eq!(u16::from_be_bytes([p.bytes[0],p.bytes[1]]) as usize,p.bytes.len());assert_eq!(p.bytes[3],0xe7);assert_eq!(&p.bytes[p.bytes.len()-8..],&[1,2,3,4,5,6,7,8]);}
 #[test]fn headers_match_msa(){let r=RtpHeader::new(false,true,0x1234,0x01020304,0x05060708);assert_eq!(r.bytes,[0x80,0xe0,0x12,0x34,1,2,3,4,5,6,7,8]);let b=RtpHeader::new(true,false,0x1234,0x01020304,0x05060708);assert_eq!(b.bytes[1],0x67);assert_eq!(b.aad(),&[1,2,3,4,5,6,7,8]);}
 #[test]fn nonce_contracts(){assert_eq!(realtime_nonce(0x1234),[0,0,0,0,0x34,0x12,0,0,0,0,0,0]);let n=buffered_nonce(0x0807060504030201);assert_eq!(n,[0,0,0,0,1,2,3,4,5,6,7,8]);assert_eq!(trailing_nonce(&n),[1,2,3,4,5,6,7,8]);}
 #[test]fn buffered_prefix_counts_itself(){assert_eq!(buffered_total_len(100),Some(138));}
 #[test]fn sync_contract(){assert_eq!(sync_due(true,7),Some(SyncKind::Initial));assert_eq!(sync_due(false,100),Some(SyncKind::Periodic));assert_eq!(sync_due(false,101),None);}
 #[test]fn realtime_drop_advances_without_retry(){let mut s=state(true);s.commit_realtime(352,SendResult::Dropped,SendResult::Sent);assert_eq!((s.timeline.seq,s.timeline.wire_rtp,s.timeline.head_frame),(0x1235,5352,1352));assert!(s.timeline.first_packet);assert_eq!(s.counters.dropped,1);}
 #[test]fn realtime_success_clears_marker_only_with_sync(){let mut s=state(true);s.commit_realtime(352,SendResult::Sent,SendResult::Dropped);assert!(s.timeline.first_packet);s.commit_realtime(352,SendResult::Sent,SendResult::Sent);assert!(!s.timeline.first_packet);}
 #[test]fn fatal_does_not_advance(){let mut s=state(true);s.commit_realtime(352,SendResult::Fatal,SendResult::Sent);assert_eq!(s.timeline.seq,0x1234);}
 #[test]fn buffered_commit_advances_line_after_nonce_was_consumed(){let mut s=state(true);s.consume_buffered_nonce();s.commit_buffered_frame(352);assert_eq!(s.counters.nonce_counter,1);assert_eq!(s.timeline.seq,0x1235);}
 #[test]fn buffered_hard_error_still_consumes_nonce_and_closes_channel(){struct HardIo{closed:bool}impl MediaIo for HardIo{fn send_realtime(&mut self,_:&[u8])->SendResult{SendResult::Fatal}fn send_buffered(&mut self,_:&[u8])->StreamWrite{StreamWrite::Fatal}fn close_buffered(&mut self){self.closed=true}}let mut s=state(true);let mut h=MediaHealth::default();let mut p=BufferedPending::default();let mut io=HardIo{closed:false};let b=BuiltPacket{bytes:vec![1,2,3],seq:s.timeline.seq,rtp:s.timeline.wire_rtp,frames:352};assert_eq!(execute_buffered(&mut s,&mut h,&mut p,&mut io,b),SendResult::Fatal);assert_eq!(s.counters.nonce_counter,1);assert_eq!(s.timeline.seq,0x1234);assert!(io.closed);assert!(!h.healthy);}
 #[test]fn pending_tail_blocks_new_frame(){let mut p=BufferedPending::default();assert!(p.park(vec![1,2,3,4]).is_ok());p.consume(2);assert_eq!(p.remaining(),&[3,4]);assert!(p.park(vec![9]).is_err());p.consume(2);assert!(p.park(vec![9]).is_ok());}
 #[test]fn pacing_matches_msa_window_and_release_floor(){let w=pacing_window_frames(48000,0,false,false,0,false);assert_eq!(w,84000);assert!(!pacing_accept(48000,132001,w,0,10000));assert!(pacing_accept(48000,132000,w,0,10000));assert!(!pacing_accept(48000,132000,w,9501,10000));}
 #[test]fn buffered_depth_can_expand_window(){assert_eq!(pacing_window_frames(48000,96000,true,false,3000,true),144000);}
 #[test]fn splice_reported_window_clamps_depth(){assert_eq!(pacing_window_frames(48000,144000,false,true,3000,true),132000);}
 #[test]fn explicit_splice_depth_outranks_default_assumption(){assert_eq!(pacing_window_frames(48000,0,false,true,3000,true),144000);}
 #[test]fn recovery_lead_is_min_latency_and_window(){assert_eq!(recovery_lead_frames(2000,72000,48000),72000);}
 #[test]fn splice_starvation_is_anticipatory_but_delivery_is_not(){let lead=48000;assert_eq!(splice_recovery_pad(110000,100000,112000,lead),Some(38000));assert_eq!(splice_recovery_pad(110000,100000,100000,lead),None);}
 #[test]fn queued_splice_pad_makes_recovery_idempotent(){assert_eq!(splice_recovery_pad(148000,100000,112000,48000),None);}
}