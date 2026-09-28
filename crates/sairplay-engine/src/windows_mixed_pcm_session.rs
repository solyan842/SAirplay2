//! Windows mixed-session PCM ownership shell.
//!
//! This module owns one persistent Windows PCM producer and one non-cloneable
//! coordinator source. It intentionally does not connect native or RAOP
//! transports yet; it only makes the future mixed-session ownership shape
//! explicit and testable by construction.

use crate::{
    Ap2AudioFormat, GroupPcmCoordinator, GroupPcmCoordinatorCycle,
    GroupPcmParticipant, LegacyGroupSession, LegacyPcmSink, NativeGroupSession,
    OwnedNativePcmSink, WasapiLoopbackError, WindowsPcmCoordinatorOwner,
    WindowsPcmCoordinatorSource,
};
use crate::group_start_orchestrator::{
    run_concurrent_group_start_round, run_group_start_convergence,
    GroupStartConvergence, GroupStartIoError, GroupStartParticipant,
};
use std::time::Duration;

pub struct WindowsMixedPcmSession {
    owner: WindowsPcmCoordinatorOwner,
    coordinator: GroupPcmCoordinator<WindowsPcmCoordinatorSource>,
    native_groups: Vec<NativeGroupSession>,
    legacy_groups: Vec<LegacyGroupSession>,
}

impl WindowsMixedPcmSession {
    pub fn start(audio_format: Ap2AudioFormat) -> Result<Self, WasapiLoopbackError> {
        let mut owner = WindowsPcmCoordinatorOwner::start(audio_format)?;
        let source = owner
            .take_source()
            .map_err(WasapiLoopbackError::Windows)?;
        let coordinator = GroupPcmCoordinator::new(source);

        Ok(Self {
            owner,
            coordinator,
            native_groups: Vec::new(),
            legacy_groups: Vec::new(),
        })
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

    /// Admit an external RAOP sink only after its feed gate has been derived
    /// from the TRUE common START anchor.
    pub fn add_armed_legacy_member(
        &mut self,
        member: LegacyPcmSink,
    ) -> Result<(), String> {
        if !member.is_feed_armed() {
            return Err(format!(
                "{}: legacy mixed PCM sink must be armed from common START before attachment",
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

    /// Compose one already-connected native external-feed group and one
    /// already-connected RAOP external-feed group into the single-reader mixed
    /// PCM session. Phase B deliberately limits this path to 16-bit/44.1 kHz.
    ///
    /// Ordering mirrors MSA:
    /// connect transports -> common START convergence -> arm RAOP feed from the
    /// committed anchor -> attach every sink -> retain transport owners.
    pub fn attach_external_44100_16_groups(
        &mut self,
        mut native_group: NativeGroupSession,
        mut legacy_group: LegacyGroupSession,
        initial_target_unix_ms: u64,
    ) -> Result<GroupStartConvergence, String> {
        let mut native_sinks = native_group.take_external_pcm_sinks();
        let mut legacy_sinks = legacy_group.take_external_pcm_sinks();

        if native_sinks.is_empty() || legacy_sinks.is_empty() {
            return Err(
                "mixed Phase B composition requires at least one native and one RAOP sink"
                    .to_owned(),
            );
        }
        if native_sinks.iter().any(|sink| {
            sink.source_format() != Ap2AudioFormat::ALAC_44100_16_STEREO
                || sink.target_format().sample_rate != 44_100
                || sink.target_format().bit_depth != 16
        }) {
            return Err(
                "mixed Phase B native path is limited to 16-bit/44.1 kHz".to_owned(),
            );
        }

        let existing = self.member_names();
        let incoming = native_sinks
            .iter()
            .map(|sink| GroupPcmParticipant::name(sink).to_owned())
            .chain(
                legacy_sinks
                    .iter()
                    .map(|sink| GroupPcmParticipant::name(sink).to_owned()),
            )
            .collect::<Vec<_>>();
        for (index, name) in incoming.iter().enumerate() {
            if existing.iter().any(|current| current == name)
                || incoming[..index].iter().any(|current| current == name)
            {
                return Err(format!("duplicate mixed PCM member: {name}"));
            }
        }

        let convergence = {
            let mut legacy_start = legacy_group.external_start_participants();
            let mut participants =
                Vec::<&mut dyn GroupStartParticipant>::with_capacity(
                    native_sinks.len() + legacy_start.len(),
                );
            for sink in &mut native_sinks {
                participants.push(sink as &mut dyn GroupStartParticipant);
            }
            for participant in legacy_start.iter_mut() {
                participants.push(&mut **participant);
            }

            run_group_start_convergence(initial_target_unix_ms, |target_unix_ms| {
                let mut round_members =
                    Vec::<&mut dyn GroupStartParticipant>::with_capacity(participants.len());
                for participant in participants.iter_mut() {
                    round_members.push(&mut **participant);
                }
                run_concurrent_group_start_round(round_members, target_unix_ms)
            })
            .map_err(|error| format!("{error}"))?
        };

        for sink in &mut legacy_sinks {
            sink.arm_feed_from_start_unix_ms(convergence.anchor_unix_ms);
        }

        let mut attached = Vec::<String>::new();
        for sink in native_sinks {
            let name = GroupPcmParticipant::name(&sink).to_owned();
            if let Err(error) = self.add_armed_native_member(sink) {
                for member in attached.iter().rev() {
                    self.remove_member(member);
                }
                return Err(error);
            }
            attached.push(name);
        }
        for sink in legacy_sinks {
            let name = GroupPcmParticipant::name(&sink).to_owned();
            if let Err(error) = self.add_armed_legacy_member(sink) {
                for member in attached.iter().rev() {
                    self.remove_member(member);
                }
                return Err(error);
            }
            attached.push(name);
        }

        self.native_groups.push(native_group);
        self.legacy_groups.push(legacy_group);
        Ok(convergence)
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
        // Drop coordinator-held transport senders before joining RAOP helper
        // writers or tearing down native sessions. This preserves one clear
        // ownership boundary and avoids transport owners waiting on live sinks.
        let members = self.member_names();
        for name in members {
            self.coordinator.remove_member(&name);
        }
        for group in &mut self.legacy_groups {
            group.stop();
        }
        self.legacy_groups.clear();
        self.native_groups.clear();
        self.owner.stop();
    }
}
