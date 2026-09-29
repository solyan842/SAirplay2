//! Native AP2 SOLO connect orchestration ported from pinned MSA ap2_native_connect.
//! This owns protocol ORDER only; socket/HAP/PTP/plist mechanics stay behind the
//! adapter trait. No START_JOIN, group coordinator or MultiRoom behavior lives here.

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum NativeConnectStage{
 Down,SocketOpen,InfoRead,Paired,TimingReady,SessionSetup,Recorded,StreamSetup,PeersSet,Ready
}
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum TimingMode{Ntp,Ptp}
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum StreamMode{Realtime,Buffered}

pub trait NativeConnectIo{
 type Error;
 fn open_rtsp(&mut self)->Result<(),Self::Error>;
 fn get_info(&mut self)->Result<(),Self::Error>;
 fn pair(&mut self)->Result<(),Self::Error>;
 /// Returns true only when the PTP engine/clock is actually active.
 fn start_ptp(&mut self)->Result<bool,Self::Error>;
 fn start_ntp(&mut self)->Result<(),Self::Error>;
 fn session_setup(&mut self,timing:TimingMode)->Result<(),Self::Error>;
 fn open_media_sockets(&mut self)->Result<(),Self::Error>;
 fn record(&mut self)->Result<(),Self::Error>;
 fn stream_setup(&mut self,mode:StreamMode)->Result<(),Self::Error>;
 fn set_peers(&mut self)->Result<(),Self::Error>;
 fn finish(&mut self)->Result<(),Self::Error>;
}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct NativeConnectResult{pub timing:TimingMode,pub stream:StreamMode,pub stage:NativeConnectStage}

pub fn connect_solo<I:NativeConnectIo>(io:&mut I,want_ptp:bool,buffered_requested:bool)->Result<NativeConnectResult,I::Error>{
 io.open_rtsp()?;
 io.get_info()?;
 io.pair()?;

 let timing=if want_ptp && io.start_ptp()?{TimingMode::Ptp}else{
   io.start_ntp()?;
   TimingMode::Ntp
 };
 // MSA: buffered type 103 is viable only with an active PTP timeline.
 let stream=if buffered_requested && timing==TimingMode::Ptp{StreamMode::Buffered}else{StreamMode::Realtime};

 io.session_setup(timing)?;
 io.open_media_sockets()?;
 // Source-critical ordering: RECORD BEFORE stream SETUP. Samsung receivers
 // may 200-ACK an inverted session but render silence.
 io.record()?;
 io.stream_setup(stream)?;
 if timing==TimingMode::Ptp{io.set_peers()?;}
 io.finish()?;
 Ok(NativeConnectResult{timing,stream,stage:NativeConnectStage::Ready})
}

#[cfg(test)]
mod tests{
 use super::*;
 #[derive(Default)]struct F{log:Vec<&'static str>,ptp_ok:bool}
 impl NativeConnectIo for F{
  type Error=();
  fn open_rtsp(&mut self)->Result<(),Self::Error>{self.log.push("socket");Ok(())}
  fn get_info(&mut self)->Result<(),Self::Error>{self.log.push("info");Ok(())}
  fn pair(&mut self)->Result<(),Self::Error>{self.log.push("pair");Ok(())}
  fn start_ptp(&mut self)->Result<bool,Self::Error>{self.log.push("ptp");Ok(self.ptp_ok)}
  fn start_ntp(&mut self)->Result<(),Self::Error>{self.log.push("ntp");Ok(())}
  fn session_setup(&mut self,_:TimingMode)->Result<(),Self::Error>{self.log.push("session_setup");Ok(())}
  fn open_media_sockets(&mut self)->Result<(),Self::Error>{self.log.push("media_sockets");Ok(())}
  fn record(&mut self)->Result<(),Self::Error>{self.log.push("record");Ok(())}
  fn stream_setup(&mut self,_:StreamMode)->Result<(),Self::Error>{self.log.push("stream_setup");Ok(())}
  fn set_peers(&mut self)->Result<(),Self::Error>{self.log.push("setpeers");Ok(())}
  fn finish(&mut self)->Result<(),Self::Error>{self.log.push("ready");Ok(())}
 }
 #[test]fn msa_native_order_record_precedes_stream_setup(){
  let mut f=F{ptp_ok:true,..Default::default()};let r=connect_solo(&mut f,true,false).unwrap();
  assert_eq!(r.timing,TimingMode::Ptp);assert_eq!(r.stream,StreamMode::Realtime);
  assert_eq!(f.log,vec!["socket","info","pair","ptp","session_setup","media_sockets","record","stream_setup","setpeers","ready"]);
 }
 #[test]fn failed_ptp_falls_back_ntp_and_forces_realtime(){
  let mut f=F::default();let r=connect_solo(&mut f,true,true).unwrap();
  assert_eq!(r.timing,TimingMode::Ntp);assert_eq!(r.stream,StreamMode::Realtime);
  assert_eq!(f.log,vec!["socket","info","pair","ptp","ntp","session_setup","media_sockets","record","stream_setup","ready"]);
 }
 #[test]fn buffered_requires_live_ptp(){
  let mut f=F{ptp_ok:true,..Default::default()};let r=connect_solo(&mut f,true,true).unwrap();
  assert_eq!(r.stream,StreamMode::Buffered);assert!(f.log.contains(&"setpeers"));
 }
}
