//! Windows mixed-session PCM ownership shell.
//!
//! This module owns one persistent Windows PCM producer and one non-cloneable
//! coordinator source. It intentionally does not connect native or RAOP
//! transports yet; it only makes the future mixed-session ownership shape
//! explicit and testable by construction.

use crate::{
    Ap2AudioFormat, GroupPcmCoordinator, GroupPcmCoordinatorCycle,
    GroupPcmParticipant, OwnedNativePcmSink, WasapiLoopbackError,
    WindowsPcmCoordinatorOwner, WindowsPcmCoordinatorSource,
};
use crate::group_start_orchestrator::{
    run_concurrent_group_start_round, run_group_start_convergence,
    GroupStartConvergence, GroupStartIoError, GroupStartParticipant,
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

    /// Admit a native external-feed sink only after common START has armed it.
    ///
    /// This makes the Phase B ordering explicit:
    /// connect transports -> common START convergence -> PCM coordinator attach.
    pub fn add_armed_native_member(
        &mut self,
        member: OwnedNativePcmSink,
    ) -> Result<(), String> {
        if !member.is_armed() {
            return Err(format!(
                "{}: native mixed PCM sink must complete common START before attachment",
                GroupPcmParticipant::name(&member),
            ));
        }
        self.coordinator.add_member(Box::new(member))
    }

    /// Commit one common audible START across transport-neutral members using
    /// the existing MSA-aligned concurrent round + convergence contract.
    ///
    /// PCM ownership remains separate: this method only arms transport timing.
    /// Callers must not pump mixed PCM until START has succeeded.
    pub fn arm_common_start(
        &mut self,
        initial_target_unix_ms: u64,
        participants: &mut [&mut dyn GroupStartParticipant],
    ) -> Result<GroupStartConvergence, GroupStartIoError> {
        run_group_start_convergence(initial_target_unix_ms, |target_unix_ms| {
            let mut round_members =
                Vec::<&mut dyn GroupStartParticipant>::with_capacity(participants.len());
            for participant in participants.iter_mut() {
                round_members.push(&mut **participant);
            }
            run_concurrent_group_start_round(round_members, target_unix_ms)
        })
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
