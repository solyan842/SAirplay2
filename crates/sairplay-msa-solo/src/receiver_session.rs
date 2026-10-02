//! Receiver-level facade for the MSA-derived Windows transport.
//!
//! This is intentionally a zero-policy wrapper around `WindowsMsaSoloClient`.
//! The SOLO implementation remains the hardware-validated transport kernel while
//! higher layers migrate to a receiver-centric API that can later be coordinated
//! by Single, Stereo Pair, and MultiRoom without duplicating protocol behavior.
//!
//! No RTP/PTP/ALAC/RTSP behavior belongs in this facade.

use crate::{SoloConnectError, WindowsMsaSoloClient, WindowsMsaSoloConfig};
use std::ops::{Deref, DerefMut};

/// One receiver transport session backed by the hardware-validated MSA kernel.
///
/// Phase 1 keeps the exact existing SOLO transport underneath. The neutral
/// receiver name is the architectural seam: future coordinators own grouping,
/// shared clocks and PCM fan-out, while this object continues to own only one
/// receiver's protocol lifecycle.
pub struct WindowsMsaReceiverSession {
    inner: WindowsMsaSoloClient,
}

impl WindowsMsaReceiverSession {
    pub fn connect(config: WindowsMsaSoloConfig) -> Result<Self, SoloConnectError> {
        WindowsMsaSoloClient::connect(config).map(|inner| Self { inner })
    }

    /// Escape hatch for migration code only. New orchestration should depend on
    /// the receiver facade rather than naming the SOLO transport directly.
    pub fn into_inner(self) -> WindowsMsaSoloClient {
        self.inner
    }
}

impl Deref for WindowsMsaReceiverSession {
    type Target = WindowsMsaSoloClient;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for WindowsMsaReceiverSession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
