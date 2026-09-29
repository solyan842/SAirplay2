//! Receiver clock readiness for native AP2 SOLO, ported from pinned MSA.
//! Group/join enforcement is deliberately excluded by the architecture lock.

use crate::timing::{AP2_CLOCK_LOCK_MS,AP2_CLOCK_SETTLE_MS,AP2_CLOCK_SEAT_EXCHANGES,AP2_MIN_WARM_LEAD_MS,StartResolution};

pub const AP2_CLOCK_STALL_MS:u64=5000;

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct ProbeStreak{pub first_age_ms:u64,pub third_age_ms:u64,pub exchanges:u32}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct ClockFloor{pub floor_ntp:u64,pub cold:bool}

const NTP_SCALE:u128=1u128<<32;
fn unix_ms_to_ntp(ms:u64)->u64{(((u128::from(ms/1000))<<32)+((u128::from(ms%1000)<<32)/1000)) as u64}
fn ntp_to_unix_ms(ntp:u64)->u64{((u128::from(ntp>>32)*1000)+((u128::from(ntp&0xffff_ffff)*1000)>>32)) as u64}
fn ms_to_ntp(ms:u64)->u64{((u128::from(ms)*NTP_SCALE)/1000) as u64}

pub fn ready_from(now:u64,apple_model:bool,ex:ProbeStreak)->u64{
    // MSA ap2_ptp_exchange reports AGES before now, not absolute timestamps.
    // Source formula: now - min(age, now) + lock/settle.
    let first_age=ex.first_age_ms.min(now);
    let mut ready=now.saturating_sub(first_age).saturating_add(AP2_CLOCK_LOCK_MS);
    if apple_model && ex.exchanges>=AP2_CLOCK_SEAT_EXCHANGES{
        let third_age=ex.third_age_ms.min(now);
        let fast=now.saturating_sub(third_age).saturating_add(AP2_CLOCK_SETTLE_MS);
        ready=ready.min(fast);
    }
    ready
}

/// MSA ap2_clock_floor: compare readiness and feasibility in NTP domain.
pub fn clock_floor(now_ntp:u64,native:bool,use_ptp:bool,apple_model:bool,exchange:Option<ProbeStreak>)->ClockFloor{
    let base=now_ntp.saturating_add(ms_to_ntp(AP2_MIN_WARM_LEAD_MS));
    if !native || !use_ptp{return ClockFloor{floor_ntp:base,cold:false}}
    let Some(ex)=exchange else{return ClockFloor{floor_ntp:base,cold:true}};
    let now_ms=ntp_to_unix_ms(now_ntp);
    let ready_ntp=unix_ms_to_ntp(ready_from(now_ms,apple_model,ex));
    ClockFloor{floor_ntp:base.max(ready_ntp),cold:false}
}

/// Exact MSA resolve shape: zero picks floor; a stale nonzero request gets one
/// extra warm-lead beyond the moving floor so a corrective retry converges.
pub fn resolve_at_floor(requested:u64,floor_ntp:u64)->StartResolution{
    let requested_ntp=if requested==0{0}else{unix_ms_to_ntp(requested)};
    if requested_ntp>=floor_ntp{return StartResolution{requested_unix_ms:requested,at_unix_ms:requested,corrected_forward:false}}
    if requested==0{return StartResolution{requested_unix_ms:0,at_unix_ms:ntp_to_unix_ms(floor_ntp),corrected_forward:false}}
    let at=floor_ntp.saturating_add(ms_to_ntp(AP2_MIN_WARM_LEAD_MS));
    StartResolution{requested_unix_ms:requested,at_unix_ms:ntp_to_unix_ms(at),corrected_forward:true}
}

#[cfg(test)]
mod tests{
 use super::*;
 #[test] fn third_party_waits_full_lock_from_streak_start(){
  let ex=ProbeStreak{first_age_ms:1000,third_age_ms:1300,exchanges:4};
  assert_eq!(ready_from(1500,false,ex),2800);
 }
 #[test] fn apple_after_three_exchanges_uses_fast_settle(){
  let ex=ProbeStreak{first_age_ms:1000,third_age_ms:1400,exchanges:3};
  assert_eq!(ready_from(1500,true,ex),350);
 }
 #[test] fn cold_ptp_keeps_warm_floor_and_marks_cold(){
  assert_eq!(clock_floor(unix_ms_to_ntp(1000),true,true,false,None),ClockFloor{floor_ntp:unix_ms_to_ntp(1250),cold:true});
 }
 #[test] fn stale_request_gets_retry_slack_beyond_clock_floor(){
  let s=resolve_at_floor(1500,unix_ms_to_ntp(2000));assert_eq!(s.at_unix_ms,2250);assert!(s.corrected_forward);
 }
}
