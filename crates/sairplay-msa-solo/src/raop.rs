//! RAOP session lane, source-aligned with MSA raop_session.c.

use crate::timing::{resolve_raop_start, StartResolution};

const NTP_SCALE:u128=1u128<<32;
fn unix_ms_to_ntp(ms:u64)->u64{(((u128::from(ms/1000))<<32)+((u128::from(ms%1000)<<32)/1000)) as u64}
fn ntp_to_unix_ms(ntp:u64)->u64{((u128::from(ntp>>32)*1000)+((u128::from(ntp&0xffff_ffff)*1000)>>32)) as u64}
fn frames_to_ntp(frames:u32,sample_rate:u32)->u64{if sample_rate==0{0}else{((u128::from(frames)<<32)/u128::from(sample_rate)) as u64}}
fn add_frames_to_ntp(base:u64,frames:u32,sample_rate:u32)->u64{
    if sample_rate==0{return base;}
    // Project the complete NTP sum once. This mirrors MSA's playtime + TS2NTP
    // contract without introducing a second millisecond-domain truncation.
    base.saturating_add(frames_to_ntp(frames,sample_rate))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaopState { Streaming, Flushed, Other }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaopContractError<E> { InvalidState, Transport(E) }

pub trait RaopClient {
    type Error;
    fn state(&self) -> RaopState;
    fn now_unix_ms(&self) -> u64;
    fn latency_frames(&self) -> u32;
    fn sample_rate(&self) -> u32;
    fn stop(&mut self);
    fn flush(&mut self) -> Result<(), Self::Error>;
    fn pause(&mut self);
    /// Transport anchor instant, before receiver latency. The session ACK remains audible time.
    fn start_at_transport_ntp(&mut self, transport_ntp: u64) -> Result<(), Self::Error>;
}

fn transport_start_ntp<C:RaopClient>(client:&C,audible_ms:u64)->u64{
    unix_ms_to_ntp(audible_ms).saturating_sub(frames_to_ntp(client.latency_frames(),client.sample_rate()))
}

pub fn commit<C:RaopClient>(client:&mut C,requested_unix_ms:u64)->Result<StartResolution,RaopContractError<C::Error>>{
    let state=client.state();
    if !matches!(state,RaopState::Streaming|RaopState::Flushed){return Err(RaopContractError::InvalidState)}
    let start=resolve_raop_start(client.now_unix_ms(),requested_unix_ms);
    client.stop();
    if state==RaopState::Streaming{client.flush().map_err(RaopContractError::Transport)?;}
    client.start_at_transport_ntp(transport_start_ntp(client,start.at_unix_ms)).map_err(RaopContractError::Transport)?;
    Ok(start)
}

pub fn start_after_flush<C:RaopClient>(client:&mut C,requested_unix_ms:u64)->Result<StartResolution,RaopContractError<C::Error>>{
    if client.state()!=RaopState::Flushed{return Err(RaopContractError::InvalidState)}
    let start=resolve_raop_start(client.now_unix_ms(),requested_unix_ms);
    client.start_at_transport_ntp(transport_start_ntp(client,start.at_unix_ms)).map_err(RaopContractError::Transport)?;
    Ok(start)
}

pub fn flush<C:RaopClient>(client:&mut C)->Result<(),RaopContractError<C::Error>>{
    let state=client.state();
    if !matches!(state,RaopState::Streaming|RaopState::Flushed){return Err(RaopContractError::InvalidState)}
    client.stop();
    if state==RaopState::Streaming{client.flush().map_err(RaopContractError::Transport)?;}
    Ok(())
}
pub fn standby<C:RaopClient>(client:&mut C)->Result<(),RaopContractError<C::Error>>{flush(client)}
pub fn pause<C:RaopClient>(client:&mut C)->Result<(),RaopContractError<C::Error>>{
    match client.state(){
        RaopState::Flushed=>Ok(()),
        RaopState::Streaming=>{client.pause();client.flush().map_err(RaopContractError::Transport)},
        RaopState::Other=>Err(RaopContractError::InvalidState),
    }
}
pub fn resume<C:RaopClient>(client:&mut C)->Result<StartResolution,RaopContractError<C::Error>>{
    if !matches!(client.state(),RaopState::Flushed|RaopState::Streaming){return Err(RaopContractError::InvalidState)}
    let start=resolve_raop_start(client.now_unix_ms(),0);
    client.start_at_transport_ntp(transport_start_ntp(client,start.at_unix_ms)).map_err(RaopContractError::Transport)?;
    Ok(start)
}

/// MSA's source uses fixed-point NTP; retain sub-ms precision until the final
/// Unix-ms projection instead of truncating frame duration first.
pub fn next_head_unix_ms(playtime_ntp:u64,chunk_frames:u32,sample_rate:u32)->u64{
    if playtime_ntp==0 || sample_rate==0{return 0;}
    ntp_to_unix_ms(add_frames_to_ntp(playtime_ntp,chunk_frames,sample_rate))
}

#[cfg(test)]
mod tests {
 use super::*;
 #[derive(Default)] struct Fake{state:u8,now:u64,stops:u8,flushes:u8,starts:Vec<u64>}
 impl RaopClient for Fake{
  type Error=();
  fn state(&self)->RaopState{match self.state{1=>RaopState::Streaming,2=>RaopState::Flushed,_=>RaopState::Other}}
  fn now_unix_ms(&self)->u64{self.now} fn latency_frames(&self)->u32{4410} fn sample_rate(&self)->u32{44100}
  fn stop(&mut self){self.stops+=1} fn flush(&mut self)->Result<(),Self::Error>{self.flushes+=1;self.state=2;Ok(())}
  fn pause(&mut self){} fn start_at_transport_ntp(&mut self,t:u64)->Result<(),Self::Error>{self.starts.push(t);self.state=1;Ok(())}
 }
 #[test] fn live_commit_discards_backlog_and_schedules_true_start(){let mut f=Fake{state:1,now:1000,..Default::default()};let a=commit(&mut f,1100).unwrap();assert_eq!(a.at_unix_ms,1400);assert_eq!((f.stops,f.flushes),(1,1));assert_eq!(f.starts,vec![transport_start_ntp(&f,1400)]);}
 #[test] fn warm_start_requires_flushed_and_does_not_flush_again(){let mut f=Fake{state:2,now:1000,..Default::default()};let a=start_after_flush(&mut f,1500).unwrap();assert_eq!(a.at_unix_ms,1500);assert_eq!(f.flushes,0);assert_eq!(f.starts,vec![transport_start_ntp(&f,1500)]);}
 #[test] fn ack_is_audible_but_transport_anchor_subtracts_receiver_latency(){let mut f=Fake{state:2,now:1000,..Default::default()};let a=start_after_flush(&mut f,1600).unwrap();assert_eq!(a.at_unix_ms,1600);assert_eq!(f.starts,vec![transport_start_ntp(&f,1600)]);}
 #[test] fn head_projection_is_contiguous(){assert_eq!(next_head_unix_ms(unix_ms_to_ntp(1000),441,44100),1009);}
 #[test] fn ntp_frame_latency_keeps_fraction_until_projection(){let audible=unix_ms_to_ntp(1600);let wire=audible-frames_to_ntp(1,44100);assert_eq!(ntp_to_unix_ms(wire),1599);}
 #[test] fn invalid_state_is_failure_not_panic(){let mut f=Fake::default();assert_eq!(commit(&mut f,1000),Err(RaopContractError::InvalidState));}
 #[test] fn head_projection_keeps_fraction_until_final_ms(){assert_eq!(next_head_unix_ms(unix_ms_to_ntp(1000),1,44100),1000);assert_eq!(next_head_unix_ms(unix_ms_to_ntp(1000),45,44100),1001);}
}
