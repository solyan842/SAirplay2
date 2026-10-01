//! Concrete timing owner for the independent MSA SOLO engine.
//! Owns either the AirPlay NTP responder or the native PTP engine for the
//! complete lifetime of one SOLO session.

use crate::clock::ProbeStreak;
use crate::native_control::LiveTiming;
use crate::native_runtime::SyncTiming;
use crate::ntp_timing::NtpTimingResponder;
use crate::native_timeline::system_time_to_source_ntp;
use crate::ptp_engine::{PtpClock, PtpEngine};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub enum NativeTimingOwner {
    Ntp {
        responder: NtpTimingResponder,
    },
    Ptp {
        engine: PtpEngine,
        clock: PtpClock,
    },
}

impl NativeTimingOwner {
    pub fn start(
        receiver_ip: IpAddr,
        bind_ip: IpAddr,
        dacp_id: &str,
        prefer_ptp: bool,
        follow_receiver_clock: bool,
    ) -> Result<(Self, LiveTiming), String> {
        if prefer_ptp {
            if let Some(clock_id) = dacp_clock_id(dacp_id) {
                if let Ok(engine) = PtpEngine::start(
                    receiver_ip,
                    bind_ip,
                    clock_id,
                    follow_receiver_clock,
                ) {
                    engine.settle_receiver(receiver_ip, Duration::from_millis(400));
                    let clock = engine
                        .clock_handle_for(receiver_ip)
                        .map_err(|e| format!("PTP clock handle: {e}"))?;
                    let master_clock_id = clock.master_clock_id();
                    return Ok((
                        Self::Ptp { engine, clock },
                        LiveTiming::Ptp { master_clock_id },
                    ));
                }
            }
        }

        let mut responder = NtpTimingResponder::bind(SocketAddr::new(bind_ip, 0))
            .map_err(|e| format!("NTP bind: {e:?}"))?;
        responder.start().map_err(|e| format!("NTP start: {e:?}"))?;
        let port = responder.port().map_err(|e| format!("NTP local port: {e}"))?;
        Ok((
            Self::Ntp { responder },
            LiveTiming::Ntp { timing_port: port },
        ))
    }

    pub fn live_timing(&self) -> LiveTiming {
        match self {
            Self::Ntp { responder } => LiveTiming::Ntp {
                timing_port: responder.port().unwrap_or(0),
            },
            Self::Ptp { clock, .. } => LiveTiming::Ptp {
                master_clock_id: clock.master_clock_id(),
            },
        }
    }

    pub fn use_ptp(&self) -> bool {
        matches!(self, Self::Ptp { .. })
    }

    pub fn set_session_peers(&self, receiver_ip: IpAddr, local_ip: IpAddr) {
        if let Self::Ptp { engine, .. } = self {
            // Pinned MSA sends [receiver, us] in SETPEERS and immediately
            // hands the same peer set to the timing engine.
            engine.add_peers(&[receiver_ip, local_ip]);
        }
    }

    pub fn ptp_clock(&self) -> Option<&PtpClock> {
        match self {
            Self::Ptp { clock, .. } => Some(clock),
            Self::Ntp { .. } => None,
        }
    }

    pub fn probe_streak(&self) -> Option<ProbeStreak> {
        let Self::Ptp { clock, .. } = self else { return None };
        let exchange = clock.exchange()?;
        Some(ProbeStreak {
            first_age_ms: exchange.first_ms,
            third_age_ms: exchange.third_ms,
            exchanges: exchange.count,
        })
    }

    pub fn sync_timing(&self) -> Result<SyncTiming, String> {
        let now = SystemTime::now();
        // ap2_send_sync_packet_ptp in pinned MSA uses raopcl_get_ntp(NULL):
        // Unix-epoch fixed point. The NTP responder below remains RFC/NTP-epoch.
        let ntp = system_time_to_source_ntp(now)
            .ok_or_else(|| "system clock before UNIX epoch".to_string())?;
        let local_ns = now.duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock before UNIX epoch".to_string())?
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;

        match self {
            Self::Ntp { .. } => Ok(SyncTiming {
                ntp,
                master_now_ns: local_ns,
                local_ptp_now_ns: local_ns,
                master_clock_id: 0,
            }),
            Self::Ptp { clock, .. } => Ok(SyncTiming {
                ntp,
                master_now_ns: clock.master_now_ns(),
                local_ptp_now_ns: local_ns,
                master_clock_id: clock.master_clock_id(),
            }),
        }
    }
}

fn dacp_clock_id(dacp_id: &str) -> Option<u64> {
    let compact: String = dacp_id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if compact.len() != 16 {
        return None;
    }
    u64::from_str_radix(&compact, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, UdpSocket};

    #[test]
    fn malformed_dacp_forces_ntp_fallback() {
        let receiver = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (owner, timing) =
            NativeTimingOwner::start(receiver, bind, "bad", true, false).unwrap();
        assert!(matches!(owner, NativeTimingOwner::Ntp { .. }));
        assert!(matches!(timing, LiveTiming::Ntp { timing_port } if timing_port != 0));
    }

    #[test]
    fn ntp_owner_exposes_live_responder_and_snapshot() {
        let receiver = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (owner, timing) =
            NativeTimingOwner::start(receiver, bind, "A1B2C3D4E5F60708", false, false).unwrap();
        let port = match timing {
            LiveTiming::Ntp { timing_port } => timing_port,
            _ => panic!("expected NTP"),
        };
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut request = [0u8; 32];
        request[0] = 0x80;
        request[1] = 0xD2;
        request[2] = 1;
        client.send_to(&request, (Ipv4Addr::LOCALHOST, port)).unwrap();
        let mut response = [0u8; 32];
        let (n, _) = client.recv_from(&mut response).unwrap();
        assert_eq!(n, 32);
        let snapshot = owner.sync_timing().unwrap();
        assert!(snapshot.ntp != 0);
    }
}
