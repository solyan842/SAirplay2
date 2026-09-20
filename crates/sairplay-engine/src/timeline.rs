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
}

#[derive(Debug, Clone)]
pub struct Timeline {
    anchor: Option<(u64, u64)>,
    head: u64,
    running: bool,
    stopped: bool,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            anchor: None,
            head: 0,
            running: false,
            stopped: false,
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

    pub fn anchor(&self) -> Option<(u64, u64)> {
        self.anchor
    }

    pub fn head(&self) -> u64 {
        self.head
    }

    pub fn stop(&mut self) {
        self.running = false;
        self.stopped = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_boundaries_never_reanchor() {
        let mut t = Timeline::default();
        t.anchor_once(1000, 9000).unwrap();
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
    fn silence_and_music_advance_same_head() {
        let mut t = Timeline::default();
        t.anchor_once(0, 1).unwrap();
        t.start().unwrap();
        assert_eq!(t.advance(352).unwrap(), 0);
        assert_eq!(t.advance(352).unwrap(), 352);
        assert_eq!(t.head(), 704);
    }
}
