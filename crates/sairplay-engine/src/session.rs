use crate::{Boundary, PcmRing, Route, Timeline};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    Idle,
    Preparing,
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCommand {
    Start,
    Stop,
    PauseBoundary,
    ResumeBoundary,
    NextTrack,
    SourceChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    StateChanged(EngineState),
    BoundaryAccepted(Boundary),
}

pub struct SessionCore {
    pub route: Route,
    pub state: EngineState,
    pub timeline: Timeline,
    pub pcm: PcmRing,
}

impl SessionCore {
    pub fn new(route: Route, pcm_capacity_samples: usize) -> Self {
        Self {
            route,
            state: EngineState::Idle,
            timeline: Timeline::default(),
            pcm: PcmRing::new(pcm_capacity_samples),
        }
    }

    pub fn arm(&mut self, rtp: u64, wall_ns: u64) -> Result<(), String> {
        self.state = EngineState::Preparing;
        self.timeline
            .anchor_once(rtp, wall_ns)
            .map_err(|e| format!("{e:?}"))?;
        self.timeline.start().map_err(|e| format!("{e:?}"))?;
        self.state = EngineState::Running;
        Ok(())
    }

    pub fn warm_boundary(&mut self, boundary: Boundary) -> Result<EngineEvent, String> {
        self.timeline
            .warm_boundary(boundary)
            .map_err(|e| format!("{e:?}"))?;
        Ok(EngineEvent::BoundaryAccepted(boundary))
    }

    pub fn packet_pcm(&mut self, stereo_samples: usize) -> Result<(u64, Vec<i16>), String> {
        let rtp = self
            .timeline
            .advance((stereo_samples / 2) as u32)
            .map_err(|e| format!("{e:?}"))?;
        Ok((rtp, self.pcm.pop_or_silence(stereo_samples)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixty_seconds_of_no_source_does_not_stop_session() {
        let mut s = SessionCore::new(Route::AirPlay2Native, 44_100 * 2);
        s.arm(0, 1).unwrap();

        // 352 frames/packet, 44.1kHz. Simulate >60 seconds with no input.
        let packets = (44_100usize * 61) / 352;
        for _ in 0..packets {
            let (_, pcm) = s.packet_pcm(352 * 2).unwrap();
            assert!(pcm.iter().all(|v| *v == 0));
        }

        assert_eq!(s.state, EngineState::Running);
        s.pcm.push(&vec![1000; 352 * 2]);
        let (_, pcm) = s.packet_pcm(352 * 2).unwrap();
        assert!(pcm.iter().any(|v| *v != 0));
    }

    #[test]
    fn repeated_track_and_source_changes_do_not_change_transport_state() {
        let mut s = SessionCore::new(Route::AirPlay2Native, 8192);
        s.arm(1234, 9999).unwrap();
        let anchor = s.timeline.anchor();

        for _ in 0..100 {
            s.warm_boundary(Boundary::NextTrack).unwrap();
            s.warm_boundary(Boundary::SourceChange).unwrap();
            assert_eq!(s.state, EngineState::Running);
            assert_eq!(s.timeline.anchor(), anchor);
        }
    }
}
