#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    Pause,
    Resume,
    NextTrack,
    Seek,
    SourceChange,
    Starvation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineError {
    NotAnchored,
    AlreadyAnchored,
    Stopped,
    InvalidSampleRate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplicePlan {
    pub requested_wall_ns: u64,
    pub effective_wall_ns: u64,
    pub head_wall_ns: u64,
    pub pad_frames: u64,
    pub corrected_forward: bool,
}

#[derive(Debug, Clone)]
pub struct Timeline {
    anchor: Option<(u64, u64)>,
    head: u64,
    running: bool,
    stopped: bool,
    splice_pad_frames: u64,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            anchor: None,
            head: 0,
            running: false,
            stopped: false,
            splice_pad_frames: 0,
        }
    }
}

impl Timeline {
    pub fn anchor_once(&mut self, rtp: u64, wall_ns: u64) -> Result<(), TimelineError> {
        if self.stopped {
            return Err(TimelineError::Stopped);
        }
        if self.anchor.is_some() {
            return Err(TimelineError::AlreadyAnchored);
        }
        self.anchor = Some((rtp, wall_ns));
        self.head = rtp;
        Ok(())
    }

    pub fn start(&mut self) -> Result<(), TimelineError> {
        if self.stopped {
            return Err(TimelineError::Stopped);
        }
        if self.anchor.is_none() {
            return Err(TimelineError::NotAnchored);
        }
        self.running = true;
        Ok(())
    }

    pub fn advance(&mut self, frames: u32) -> Result<u64, TimelineError> {
        if !self.running || self.stopped {
            return Err(TimelineError::NotAnchored);
        }
        let at = self.head;
        self.head = self.head.wrapping_add(frames as u64);
        Ok(at)
    }

    pub fn warm_boundary(&self, _boundary: Boundary) -> Result<(), TimelineError> {
        if self.stopped {
            return Err(TimelineError::Stopped);
        }
        if self.anchor.is_none() {
            return Err(TimelineError::NotAnchored);
        }
        Ok(())
    }

    pub fn head_wall_ns(&self, sample_rate: u32) -> Result<u64, TimelineError> {
        if sample_rate == 0 {
            return Err(TimelineError::InvalidSampleRate);
        }
        let (anchor_rtp, anchor_wall_ns) = self.anchor.ok_or(TimelineError::NotAnchored)?;
        let delta_frames = self.head.wrapping_sub(anchor_rtp);
        let delta_ns = ((delta_frames as u128 * 1_000_000_000u128) / sample_rate as u128) as u64;
        Ok(anchor_wall_ns.saturating_add(delta_ns))
    }

    /// Plan a warm splice on the existing frozen RTP<->wall line.
    ///
    /// This deliberately does NOT change the anchor. If the requested instant is
    /// too close to (or behind) the current delivery head, it is corrected to
    /// head + min_warm_lead. The gap becomes encoded-silence debt.
    pub fn plan_splice(
        &mut self,
        requested_wall_ns: u64,
        sample_rate: u32,
        min_warm_lead_ms: u32,
    ) -> Result<SplicePlan, TimelineError> {
        if !self.running || self.stopped {
            return Err(TimelineError::NotAnchored);
        }
        if sample_rate == 0 {
            return Err(TimelineError::InvalidSampleRate);
        }

        let head_wall_ns = self.head_wall_ns(sample_rate)?;
        let min_effective = head_wall_ns
            .saturating_add(min_warm_lead_ms as u64 * 1_000_000);
        let effective_wall_ns = requested_wall_ns.max(min_effective);
        let corrected_forward = effective_wall_ns != requested_wall_ns;

        let gap_ns = effective_wall_ns.saturating_sub(head_wall_ns);
        let pad_frames = ((gap_ns as u128 * sample_rate as u128 + 999_999_999u128)
            / 1_000_000_000u128) as u64;

        self.splice_pad_frames = pad_frames;

        Ok(SplicePlan {
            requested_wall_ns,
            effective_wall_ns,
            head_wall_ns,
            pad_frames,
            corrected_forward,
        })
    }

    pub fn splice_pad_frames(&self) -> u64 {
        self.splice_pad_frames
    }

    pub fn consume_splice_pad(&mut self, frames: u32) -> u32 {
        let take = self.splice_pad_frames.min(frames as u64) as u32;
        self.splice_pad_frames -= take as u64;
        take
    }

    pub fn anchor(&self) -> Option<(u64, u64)> {
        self.anchor
    }

    pub fn head(&self) -> u64 {
        self.head
    }

    pub fn stop(&mut self) {
        self.running = false;
        self.stopped = true;
        self.splice_pad_frames = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_boundaries_never_rebase_native_splice_anchor() {
        let mut t = Timeline::default();
        t.anchor_once(1000, 9_000_000_000).unwrap();
        t.start().unwrap();
        let anchor = t.anchor();

        for b in [
            Boundary::Pause,
            Boundary::Resume,
            Boundary::NextTrack,
            Boundary::Seek,
            Boundary::SourceChange,
            Boundary::Starvation,
        ] {
            t.warm_boundary(b).unwrap();
            assert_eq!(t.anchor(), anchor);
        }
    }

    #[test]
    fn warm_start_becomes_silence_pad_not_anchor_reset() {
        let mut t = Timeline::default();
        t.anchor_once(0, 1_000_000_000).unwrap();
        t.start().unwrap();
        t.advance(44_100).unwrap(); // head is now 2.0 s wall time

        let anchor = t.anchor();
        let plan = t
            .plan_splice(3_000_000_000, 44_100, 250)
            .unwrap();

        assert_eq!(t.anchor(), anchor);
        assert_eq!(plan.head_wall_ns, 2_000_000_000);
        assert_eq!(plan.effective_wall_ns, 3_000_000_000);
        assert_eq!(plan.pad_frames, 44_100);
        assert!(!plan.corrected_forward);
    }

    #[test]
    fn requested_start_behind_head_is_corrected_to_head_plus_250ms() {
        let mut t = Timeline::default();
        t.anchor_once(0, 1_000_000_000).unwrap();
        t.start().unwrap();
        t.advance(44_100).unwrap(); // 2.0 s

        let plan = t
            .plan_splice(1_900_000_000, 44_100, 250)
            .unwrap();

        assert_eq!(plan.effective_wall_ns, 2_250_000_000);
        assert_eq!(plan.pad_frames, 11_025);
        assert!(plan.corrected_forward);
    }

    #[test]
    fn silence_and_music_advance_same_head() {
        let mut t = Timeline::default();
        t.anchor_once(0, 1).unwrap();
        t.start().unwrap();
        assert_eq!(t.advance(352).unwrap(), 0);
        assert_eq!(t.advance(352).unwrap(), 352);
        assert_eq!(t.head(), 704);
    }
}
