//! Windows mixed-session PCM ownership shell.
//!
//! This module owns one persistent Windows PCM producer and one non-cloneable
//! coordinator source. It intentionally does not connect native or RAOP
//! transports yet; it only makes the future mixed-session ownership shape
//! explicit and testable by construction.

use crate::{
    Ap2AudioFormat, GroupPcmCoordinator, GroupPcmCoordinatorCycle,
    GroupPcmParticipant, WasapiLoopbackError, WindowsPcmCoordinatorOwner,
    WindowsPcmCoordinatorSource,
};
use std::time::Duration;

pub struct WindowsMixedPcmSession {
    owner: WindowsPcmCoordinatorOwner,
    coordinator: GroupPcmCoordinator<WindowsPcmCoordinatorSource>,
}

impl WindowsMixedPcmSession {
    pub fn start(audio_format: Ap2AudioFormat) -> Result<Self, WasapiLoopbackError> {
        let mut owner = WindowsPcmCoordinatorOwner::start(audio_format)?;
        let source = owner
            .take_source()
            .map_err(WasapiLoopbackError::Windows)?;
        let coordinator = GroupPcmCoordinator::new(source);

        Ok(Self { owner, coordinator })
    }

    pub fn add_member(
        &mut self,
        member: Box<dyn GroupPcmParticipant>,
    ) -> Result<(), String> {
        self.coordinator.add_member(member)
    }

    pub fn remove_member(&mut self, name: &str) -> bool {
        self.coordinator.remove_member(name)
    }

    pub fn member_count(&self) -> usize {
        self.coordinator.member_count()
    }

    pub fn member_names(&self) -> Vec<String> {
        self.coordinator.member_names()
    }

    pub fn buffered_bytes(&self) -> usize {
        self.owner.session().buffered_bytes()
    }

    pub fn pump_once(
        &mut self,
        want_bytes: usize,
        timeout: Duration,
    ) -> Result<GroupPcmCoordinatorCycle, String> {
        self.coordinator.pump_once(want_bytes, timeout)
    }

    pub fn stop(&mut self) {
        self.owner.stop();
    }
}
