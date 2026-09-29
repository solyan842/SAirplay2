//! Receiver clock readiness for native AP2 SOLO, ported from pinned MSA.
//! Group/join enforcement is deliberately excluded by the architecture lock.

use crate::timing::{AP2_CLOCK_LOCK_MS,AP2_CLOCK_SETTLE_MS,AP2_CLOCK_SEAT_EXCHANGES,AP2_MIN_WARM_LEAD_MS,StartResolution};

pub const AP2_CLOCK_STALL_MS:u64=5000;

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct ProbeStreak{pub first_unix_ms:u64,pub third_unix_ms:u64,pub exchanges:u32}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct ClockFloor{pub floor_unix_ms:u64,pub cold:bool}

pub fn ready_from(now:u64,apple_model:bool,ex:ProbeStreak)->u64{
    let first=ex.first_unix_ms.min(now);
    let mut ready=now.saturating_sub(first).saturating_add(AP2_CLOCK_LOCK_MS);
    if apple_model && ex.exchanges>=AP2_CLOCK_SEAT_EXCHANGES{
        let third=ex.third_unix_ms.min(now);
        let fast=now.saturating_sub(third).saturating_add(AP2_CLOCK_SETTLE_MS);
        ready=ready.min(fast);
    }
    ready
}

/// Equivalent to MSA ap2_clock_floor in Unix-ms scheduling domain.
pub fn clock_floor(now:u64,native:bool,use_ptp:bool,apple_model:bool,exchange:Option<ProbeStreak>)->ClockFloor{
    let base=now.saturating_add(AP2_MIN_WARM_LEAD_MS);
    if !native || !use_ptp{return ClockFloor{floor_unix_ms:base,cold:false}}
    let Some(ex)=exchange else{return ClockFloor{floor_unix_ms:base,cold:true}};
    ClockFloor{floor_unix_ms:base.max(ready_from(now,apple_model,ex)),cold:false}
}

/// Exact MSA resolve shape: zero picks floor; a stale nonzero request gets one
/// extra warm-lead beyond the moving floor so a corrective retry converges.
pub fn resolve_at_floor(requested:u64,floor:u64)->StartResolution{
    if requested==0{return StartResolution{requested_unix_ms:0,at_unix_ms:floor,corrected_forward:false}}
    if requested>=floor{return StartResolution{requested_unix_ms:requested,at_unix_ms:requested,corrected_forward:false}}
    StartResolution{requested_unix_ms:requested,at_unix_ms:floor.saturating_add(AP2_MIN_WARM_LEAD_MS),corrected_forward:true}
}

#[cfg(test)]
mod tests{
 use super::*;
 #[test] fn third_party_waits_full_lock_from_streak_start(){
  let ex=ProbeStreak{first_unix_ms:1000,third_unix_ms:1300,exchanges:4};
  assert_eq!(ready_from(1500,false,ex),2800);
 }
 #[test] fn apple_after_three_exchanges_uses_fast_settle(){
  let ex=ProbeStreak{first_unix_ms:1000,third_unix_ms:1400,exchanges:3};
  assert_eq!(ready_from(1500,true,ex),350);
 }
 #[test] fn cold_ptp_keeps_warm_floor_and_marks_cold(){
  assert_eq!(clock_floor(1000,true,true,false,None),ClockFloor{floor_unix_ms:1250,cold:true});
 }
 #[test] fn stale_request_gets_retry_slack_beyond_clock_floor(){
  let s=resolve_at_floor(1500,2000);assert_eq!(s.at_unix_ms,2250);assert!(s.corrected_forward);
 }
}
