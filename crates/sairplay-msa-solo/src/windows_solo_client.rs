#![cfg(windows)]

//! Unified Windows owner for the independent MSA SOLO implementation.
//! Mirrors the pinned ap2cl_s route decision: one discovery/config input
//! resolves to native AP2 or the exact-pin RAOP-compatible transport.

use crate::{
    MsaRaopConfig, NativeControlErrorClass, NativeFormatCapabilities,
    NativeLatencyInfo, NativeSoloConfig, NativeSoloEngine, NativeSoloError,
    WindowsSoloAudioWorker,
};
use crate::route::{
    apple_model, buffered_route, follow_receiver_clock, resolve_route_from_txt,
    Flow, ProtocolPreference, RouteDecision, Timing as RouteTiming,
};
use crate::timing::StartResolution;
use crate::windows_raop_worker::WindowsRaopAudioWorker;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct WindowsMsaSoloConfig {
    pub protocol: ProtocolPreference,
    pub txt: Option<String>,
    pub am: Option<String>,
    pub raop_cn: Option<String>,
    pub raop_pk: Option<String>,
    pub pw_txt: Option<String>,
    pub force_native: bool,
    pub ptp_override: Option<bool>,
    pub buffered_forced: bool,
    pub native: NativeSoloConfig,
    pub raop: MsaRaopConfig,
}

impl WindowsMsaSoloConfig {
    pub fn new(host: impl Into<String>, ap2_port: u16, raop_port: u16) -> Self {
        let host = host.into();
        Self {
            protocol: ProtocolPreference::Auto,
            txt: None,
            am: None,
            raop_cn: None,
            raop_pk: None,
            pw_txt: None,
            force_native: false,
            ptp_override: None,
            buffered_forced: false,
            native: NativeSoloConfig::new(host.clone(), ap2_port),
            raop: MsaRaopConfig::new(host, raop_port),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoloConnectErrorClass {
    Generic,
    AuthRequired,
    AuthFailed,
}

#[derive(Debug, Clone)]
pub struct SoloConnectError {
    pub class: SoloConnectErrorClass,
    pub http_status: u16,
    pub detail: String,
    pub route: RouteDecision,
}

impl SoloConnectError {
    fn native(route: RouteDecision, error: NativeSoloError) -> Self {
        if let NativeSoloError::Control(control) = &error {
            let class = match control.class() {
                NativeControlErrorClass::Generic => SoloConnectErrorClass::Generic,
                NativeControlErrorClass::AuthRequired => SoloConnectErrorClass::AuthRequired,
                NativeControlErrorClass::AuthFailed => SoloConnectErrorClass::AuthFailed,
            };
            return Self {
                class,
                http_status: control.http_status(),
                detail: control.detail(),
                route,
            };
        }
        Self {
            class: SoloConnectErrorClass::Generic,
            http_status: 0,
            detail: format!("{error:?}"),
            route,
        }
    }

    fn raop(route: RouteDecision, error: impl std::fmt::Display) -> Self {
        Self {
            class: SoloConnectErrorClass::Generic,
            http_status: 0,
            detail: error.to_string(),
            route,
        }
    }
}

#[derive(Debug)]
pub enum WindowsMsaSoloError {
    Native(String),
    Raop(String),
    InvalidState(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoloFlushAck {
    pub head_unix_ms: u64,
}

enum Transport {
    Native {
        engine: Arc<Mutex<NativeSoloEngine>>,
        worker: WindowsSoloAudioWorker,
    },
    Raop {
        worker: WindowsRaopAudioWorker,
    },
}

pub struct WindowsMsaSoloClient {
    route: RouteDecision,
    transport: Transport,
}

impl WindowsMsaSoloClient {
    pub fn connect(mut config: WindowsMsaSoloConfig) -> Result<Self, SoloConnectError> {
        let have_credentials = config.native.control.auth_credentials.is_some();
        let have_password = config.native.control.password.is_some()
            || config.raop.password.is_some();
        // Pinned ap2cl_s owns one requested format independent of the
        // route selected underneath it. RAOP-compatible must therefore use
        // the same sample-rate/bit-depth/channels requested for native AP2.
        config.raop.sample_rate = config.native.control.audio_format.sample_rate;
        config.raop.bit_depth = config.native.control.audio_format.bit_depth;
        config.raop.channels = config.native.control.audio_format.channels;
        config.raop.lead_ms = 2_000;
        config.raop.dacp_id = config.native.control.dacp_id.clone();
        config.raop.active_remote = config.native.control.active_remote.clone();
        config.raop.bind_ip = config.native.control.bind_ip;
        config.raop.mfi_auth = config.raop.mfi_auth
            || config.am.as_deref().is_some_and(|v| v.to_ascii_lowercase().contains("airport"));

        let route = resolve_route_from_txt(
            config.protocol,
            config.txt.as_deref(),
            config.pw_txt.as_deref(),
            have_credentials,
            have_password,
            config.force_native,
            config.ptp_override,
        );

        match route.flow {
            Flow::AirPlay2Native => {
                config.native.control.prefer_ptp = route.timing == RouteTiming::Ptp;
                config.native.control.follow_receiver_clock =
                    follow_receiver_clock(config.txt.as_deref(), config.am.as_deref());
                config.native.control.buffered_requested = buffered_route(
                    &route,
                    config.txt.as_deref(),
                    config.am.as_deref(),
                    config.buffered_forced,
                );
                config.native.apple_model =
                    apple_model(config.txt.as_deref(), config.am.as_deref());

                let engine = NativeSoloEngine::connect(config.native)
                    .map_err(|e| SoloConnectError::native(route, e))?;
                let engine = Arc::new(Mutex::new(engine));
                let worker = WindowsSoloAudioWorker::start(Arc::clone(&engine))
                    .map_err(|e| SoloConnectError::raop(route, e))?;
                Ok(Self {
                    route,
                    transport: Transport::Native { engine, worker },
                })
            }
            Flow::Raop | Flow::AirPlay2Compat => {
                if route.flow == Flow::Raop
                    && config.am.as_deref().is_some_and(|v| v.to_ascii_lowercase().contains("appletv"))
                    && config.raop_pk.as_deref().is_some_and(|v| !v.is_empty())
                    && config.raop.secret.as_deref().is_none_or(|v| v.is_empty())
                {
                    return Err(SoloConnectError {
                        class: SoloConnectErrorClass::Generic,
                        http_status: 0,
                        detail: "AppleTV requires authentication (need secret)".into(),
                        route,
                    });
                }
                if route.flow == Flow::AirPlay2Compat {
                    // ap2_client.c RAOP-compatible flow always uses compressed
                    // ALAC and clear transport; cn/raw policy belongs to the
                    // explicit legacy RAOP CLI route.
                    config.raop.compressed_alac = true;
                    config.raop.encrypt = false;
                } else {
                    if let Some(cn) = config.raop_cn.as_deref() {
                        if !cn.contains('1') {
                            config.raop.compressed_alac = false;
                        }
                    }
                    // Exact cliairplay RAOP rule: RSA is legal only when the
                    // caller requested encryption and the receiver advertises
                    // et type 1.
                    config.raop.encrypt = config.raop.encrypt && config.raop.et.contains('1');
                }
                let worker = WindowsRaopAudioWorker::connect(config.raop)
                    .map_err(|e| SoloConnectError::raop(route, e))?;
                Ok(Self {
                    route,
                    transport: Transport::Raop { worker },
                })
            }
        }
    }

    pub fn route(&self) -> RouteDecision { self.route }

    pub fn commit_start(&self, requested_unix_ms: u64) -> Result<StartResolution, WindowsMsaSoloError> {
        match &self.transport {
            Transport::Native { worker, .. } => worker.commit_start(requested_unix_ms)
                .map_err(|e| WindowsMsaSoloError::Native(e.to_string())),
            Transport::Raop { worker } => worker.commit_start(requested_unix_ms)
                .map_err(|e| WindowsMsaSoloError::Raop(e.to_string())),
        }
    }

    pub fn flush_content(&self) -> Result<SoloFlushAck, WindowsMsaSoloError> {
        match &self.transport {
            Transport::Native { worker, .. } => worker.flush_content()
                .map(|head| SoloFlushAck { head_unix_ms: head.unwrap_or(0) })
                .map_err(|e| WindowsMsaSoloError::Native(e.to_string())),
            Transport::Raop { worker } => {
                worker.flush_content().map_err(|e| WindowsMsaSoloError::Raop(e.to_string()))?;
                Ok(SoloFlushAck { head_unix_ms: 0 })
            }
        }
    }

    pub fn standby_content(&self) -> Result<(), WindowsMsaSoloError> {
        match &self.transport {
            Transport::Native { worker, .. } => worker.standby_content()
                .map_err(|e| WindowsMsaSoloError::Native(e.to_string())),
            Transport::Raop { worker } => worker.standby_content()
                .map_err(|e| WindowsMsaSoloError::Raop(e.to_string())),
        }
    }

    pub fn pause_content(&self) -> Result<(), WindowsMsaSoloError> {
        match &self.transport {
            Transport::Native { worker, .. } => worker.set_content_enabled(false)
                .map_err(|e| WindowsMsaSoloError::Native(e.to_string())),
            Transport::Raop { worker } => worker.pause_content()
                .map_err(|e| WindowsMsaSoloError::Raop(e.to_string())),
        }
    }

    pub fn play_content(&self) -> Result<(), WindowsMsaSoloError> {
        match &self.transport {
            Transport::Native { worker, .. } => worker.set_content_enabled(true)
                .map_err(|e| WindowsMsaSoloError::Native(e.to_string())),
            Transport::Raop { worker } => worker.play_content()
                .map_err(|e| WindowsMsaSoloError::Raop(e.to_string())),
        }
    }

    pub fn stop_content(&self) -> Result<(), WindowsMsaSoloError> {
        match &self.transport {
            Transport::Native { worker, .. } => worker.stop_content()
                .map_err(|e| WindowsMsaSoloError::Native(e.to_string())),
            Transport::Raop { worker } => worker.stop_content()
                .map_err(|e| WindowsMsaSoloError::Raop(e.to_string())),
        }
    }

    pub fn set_volume(&self, percent: u8) -> Result<(), WindowsMsaSoloError> {
        match &self.transport {
            Transport::Native { engine, .. } => {
                engine.lock()
                    .map_err(|_| WindowsMsaSoloError::Native("native engine mutex poisoned".into()))?
                    .set_volume(percent)
                    .map(|_| ())
                    .map_err(|e| WindowsMsaSoloError::Native(format!("{e:?}")))
            }
            Transport::Raop { worker } => {
                worker.session().lock()
                    .map_err(|_| WindowsMsaSoloError::Raop("RAOP session mutex poisoned".into()))?
                    .set_volume(percent)
                    .map_err(|e| WindowsMsaSoloError::Raop(e.to_string()))
            }
        }
    }

    pub fn set_metadata(
        &self,
        title: &str,
        artist: &str,
        album: &str,
        duration_s: u32,
        item_id: &str,
    ) -> Result<(), WindowsMsaSoloError> {
        match &self.transport {
            Transport::Native { engine, .. } => {
                engine.lock()
                    .map_err(|_| WindowsMsaSoloError::Native("native engine mutex poisoned".into()))?
                    .set_metadata(title, artist, album, duration_s, item_id)
                    .map(|_| ())
                    .map_err(|e| WindowsMsaSoloError::Native(format!("{e:?}")))
            }
            Transport::Raop { worker } => {
                worker.session().lock()
                    .map_err(|_| WindowsMsaSoloError::Raop("RAOP session mutex poisoned".into()))?
                    .set_metadata(title, artist, album, duration_s, item_id)
                    .map_err(|e| WindowsMsaSoloError::Raop(e.to_string()))
            }
        }
    }

    pub fn set_artwork(&self, content_type: &str, data: &[u8]) -> Result<(), WindowsMsaSoloError> {
        match &self.transport {
            Transport::Native { engine, .. } => {
                engine.lock()
                    .map_err(|_| WindowsMsaSoloError::Native("native engine mutex poisoned".into()))?
                    .set_artwork(content_type, data)
                    .map(|_| ())
                    .map_err(|e| WindowsMsaSoloError::Native(format!("{e:?}")))
            }
            Transport::Raop { worker } => {
                worker.session().lock()
                    .map_err(|_| WindowsMsaSoloError::Raop("RAOP session mutex poisoned".into()))?
                    .set_artwork(content_type, data)
                    .map_err(|e| WindowsMsaSoloError::Raop(e.to_string()))
            }
        }
    }

    pub fn set_progress(&self, elapsed_s: u32, duration_s: u32) -> Result<(), WindowsMsaSoloError> {
        match &self.transport {
            Transport::Native { engine, .. } => {
                engine.lock()
                    .map_err(|_| WindowsMsaSoloError::Native("native engine mutex poisoned".into()))?
                    .set_progress(elapsed_s, duration_s)
                    .map(|_| ())
                    .map_err(|e| WindowsMsaSoloError::Native(format!("{e:?}")))
            }
            Transport::Raop { worker } => {
                worker.session().lock()
                    .map_err(|_| WindowsMsaSoloError::Raop("RAOP session mutex poisoned".into()))?
                    .set_progress(elapsed_s, duration_s)
                    .map_err(|e| WindowsMsaSoloError::Raop(e.to_string()))
            }
        }
    }

    pub fn head_audible_unix_ms(&self) -> u64 {
        match &self.transport {
            Transport::Native { engine, .. } => engine.lock()
                .map(|v| v.head_audible_unix_ms()).unwrap_or(0),
            Transport::Raop { worker } => worker.session().lock()
                .map(|v| v.head_audible_unix_ms()).unwrap_or(0),
        }
    }

    pub fn is_connected(&self) -> bool {
        match &self.transport {
            Transport::Native { engine, worker } => worker.is_running()
                && engine.lock().map(|mut v| v.is_connected()).unwrap_or(false),
            Transport::Raop { worker } => worker.is_running()
                && worker.session().lock().map(|v| v.state() != crate::MsaRaopState::Down).unwrap_or(false),
        }
    }

    pub fn is_playing(&self) -> bool {
        match &self.transport {
            Transport::Native { engine, worker } => worker.is_running()
                && engine.lock().map(|mut v| v.is_playing()).unwrap_or(false),
            Transport::Raop { worker } => worker.is_running()
                && worker.session().lock().map(|v| v.state() == crate::MsaRaopState::Streaming).unwrap_or(false),
        }
    }

    pub fn format_capabilities(&self) -> NativeFormatCapabilities {
        match &self.transport {
            Transport::Native { engine, .. } => engine.lock().map(|v| v.format_capabilities())
                .unwrap_or(NativeFormatCapabilities {
                    requested: 0, realtime_formats: 0, buffered_formats: 0,
                    realtime_known: false, buffered_known: false,
                }),
            Transport::Raop { worker } => {
                let requested = worker.session().lock().ok().map(|v| {
                    let r = v.ready();
                    crate::Ap2AudioFormat {
                        sample_rate: r.sample_rate,
                        bit_depth: r.bit_depth,
                        channels: r.channels,
                    }.audio_format_code()
                }).unwrap_or(0);
                NativeFormatCapabilities {
                    requested,
                    realtime_formats: 0,
                    buffered_formats: 0,
                    realtime_known: false,
                    buffered_known: false,
                }
            },
        }
    }

    pub fn latency_info(&self) -> NativeLatencyInfo {
        match &self.transport {
            Transport::Native { engine, .. } => engine.lock().map(|v| v.latency_info())
                .unwrap_or(NativeLatencyInfo {
                    lead_ms: 0, device_min_frames: 0, device_max_frames: 0, render_latency_ms: 0,
                }),
            Transport::Raop { worker } => {
                let ready = worker.session().lock().map(|v| v.ready()).ok();
                if let Some(ready) = ready {
                    NativeLatencyInfo {
                        lead_ms: if ready.sample_rate == 0 { 0 }
                            else { u64::from(ready.latency_frames) * 1000 / u64::from(ready.sample_rate) },
                        device_min_frames: 0,
                        device_max_frames: 0,
                        render_latency_ms: 0,
                    }
                } else {
                    NativeLatencyInfo {
                        lead_ms: 0, device_min_frames: 0, device_max_frames: 0, render_latency_ms: 0,
                    }
                }
            }
        }
    }
}
