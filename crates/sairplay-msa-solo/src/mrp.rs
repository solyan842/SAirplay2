use crate::{EncryptedRtspError, RtspRequest, SharedCseq, SharedRtspControl};
use plist::{Dictionary, Value};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex, TryLockError, atomic::Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub const ARTWORK_STAGING_MAX_BYTES: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_millis(5000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum MrpPlaybackState {
    Playing = 1,
    Paused = 2,
    Stopped = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MrpArtworkResult {
    NotApplicable,
    Accepted,
    Unchanged,
    InvalidArgument,
    UnsupportedType,
    StagingLimit,
    InvalidJpegEnvelope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MrpArtworkInfo {
    pub result: MrpArtworkResult,
    pub bytes: usize,
    pub width: u16,
    pub height: u16,
    pub precision: u8,
    pub components: u8,
    pub sof_marker: u8,
    pub progressive: bool,
}

impl MrpArtworkInfo {
    fn new(bytes: usize) -> Self {
        Self {
            result: MrpArtworkResult::InvalidArgument,
            bytes,
            width: 0,
            height: 0,
            precision: 0,
            components: 0,
            sof_marker: 0,
            progressive: false,
        }
    }
}

#[derive(Debug)]
pub enum MrpError {
    Plist(plist::Error),
    Lock,
    LockTimeout,
    Transport(EncryptedRtspError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MrpPostResult {
    pub status: u16,
}

#[derive(Debug, Clone)]
pub struct MrpState {
    pub dacp_id: String,
    pub name: String,
    pub session_uuid: String,
    pub group_uuid: String,
    pub device_uuid: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: i64,
    pub elapsed_ms: i64,
    pub playback_state: MrpPlaybackState,
    pub elapsed_set_at: SystemTime,
    pub artwork_mime: Option<String>,
    pub artwork: Vec<u8>,
    pub artwork_id: String,
    pub item_id: String,
    pub np_uid: u64,
    pub device_registered: bool,
    pub extended_registered: bool,
    pub last_playback_state: Option<MrpPlaybackState>,
    pub progress_push_full: bool,
}

impl MrpState {
    pub fn new(
        dacp_id: impl Into<String>,
        name: impl Into<String>,
        session_uuid: impl Into<String>,
        group_uuid: Option<String>,
    ) -> Self {
        let dacp_id = dacp_id.into();
        let name = {
            let v = name.into();
            if v.is_empty() { "Music Assistant".to_string() } else { v }
        };
        Self {
            device_uuid: identity_uuid(&dacp_id),
            dacp_id,
            name,
            session_uuid: session_uuid.into(),
            group_uuid: group_uuid.unwrap_or_default(),
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration_ms: 0,
            elapsed_ms: 0,
            playback_state: MrpPlaybackState::Paused,
            elapsed_set_at: SystemTime::now(),
            artwork_mime: None,
            artwork: Vec::new(),
            artwork_id: String::new(),
            item_id: String::new(),
            np_uid: 0,
            device_registered: false,
            extended_registered: false,
            last_playback_state: None,
            progress_push_full: false,
        }
    }

    pub fn set_track(
        &mut self,
        title: &str,
        artist: &str,
        album: &str,
        duration_ms: i64,
        item_id: &str,
        artwork: Option<(&str, &[u8])>,
    ) -> (bool, MrpArtworkInfo) {
        let have_ids = !self.item_id.is_empty() && !item_id.is_empty();
        let track_changed = if have_ids {
            self.item_id != item_id
        } else {
            self.title != title || self.artist != artist || self.album != album
        };

        self.title.clear();
        self.title.push_str(title);
        self.artist.clear();
        self.artist.push_str(artist);
        self.album.clear();
        self.album.push_str(album);
        self.item_id.clear();
        self.item_id.push_str(item_id);

        if duration_ms > 0 {
            self.duration_ms = duration_ms;
        } else if track_changed {
            self.duration_ms = 0;
        }

        if track_changed || self.np_uid == 0 {
            self.np_uid = random_u63();
            if track_changed {
                self.elapsed_ms = 0;
                self.elapsed_set_at = SystemTime::now();
            }
        }

        let art_info = match artwork {
            Some((mime, bytes)) => self.set_artwork(mime, bytes),
            None => {
                if track_changed {
                    self.clear_artwork();
                }
                MrpArtworkInfo {
                    result: MrpArtworkResult::NotApplicable,
                    ..MrpArtworkInfo::new(0)
                }
            }
        };
        (track_changed, art_info)
    }

    pub fn set_artwork(&mut self, mime: &str, data: &[u8]) -> MrpArtworkInfo {
        let mut info = probe_artwork(mime, data);
        if info.result != MrpArtworkResult::Accepted {
            self.clear_artwork();
            return info;
        }
        if self.artwork == data && self.artwork_mime.as_deref() == Some(mime) {
            info.result = MrpArtworkResult::Unchanged;
            return info;
        }
        self.artwork.clear();
        self.artwork.extend_from_slice(data);
        self.artwork_mime = Some(mime.to_string());
        let mut id = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut id);
        self.artwork_id = id.iter().map(|b| format!("{b:02x}")).collect();
        info
    }

    pub fn clear_artwork(&mut self) {
        self.artwork.clear();
        self.artwork_mime = None;
        self.artwork_id.clear();
    }

    pub fn set_progress(&mut self, elapsed_ms: i64, duration_ms: i64, playing: bool) {
        self.elapsed_ms = elapsed_ms.max(0);
        if duration_ms > 0 {
            self.duration_ms = duration_ms;
        }
        self.playback_state = if playing {
            MrpPlaybackState::Playing
        } else {
            MrpPlaybackState::Paused
        };
        self.elapsed_set_at = SystemTime::now();
    }

    pub fn set_playing(&mut self, playing: bool) {
        let now = SystemTime::now();
        if self.playback_state == MrpPlaybackState::Playing {
            if let Ok(delta) = now.duration_since(self.elapsed_set_at) {
                self.elapsed_ms = self
                    .elapsed_ms
                    .saturating_add(delta.as_millis().min(i64::MAX as u128) as i64);
            }
        }
        self.playback_state = if playing {
            MrpPlaybackState::Playing
        } else {
            MrpPlaybackState::Paused
        };
        self.elapsed_set_at = now;
    }

    pub fn set_stopped(&mut self) {
        self.playback_state = MrpPlaybackState::Stopped;
        self.elapsed_set_at = SystemTime::now();
    }

    pub fn build_deviceinfo_command(&self) -> Result<Vec<u8>, MrpError> {
        let mut inner = Vec::new();
        pb_string(&mut inner, 1, &self.device_uuid);
        pb_string(&mut inner, 2, &self.name);
        pb_string(&mut inner, 3, "iPhone");
        pb_string(&mut inner, 4, "21F90");
        pb_string(&mut inner, 5, "com.apple.Music");
        pb_varint_field(&mut inner, 7, 1);
        pb_varint_field(&mut inner, 8, 139);
        pb_varint_field(&mut inner, 9, 1);
        pb_varint_field(&mut inner, 10, 1);
        pb_string(&mut inner, 12, "com.apple.Music");
        pb_varint_field(&mut inner, 13, 1);
        pb_varint_field(&mut inner, 14, 1);
        pb_varint_field(&mut inner, 15, 1);
        pb_varint_field(&mut inner, 17, 2);
        pb_string(&mut inner, 19, &self.dacp_id);
        pb_varint_field(&mut inner, 21, 1);
        pb_varint_field(&mut inner, 22, 1);
        pb_string(&mut inner, 31, "com.apple.podcasts");
        pb_string(&mut inner, 39, "iPhone17,1");
        if !self.session_uuid.is_empty() {
            pb_string(&mut inner, 41, &self.session_uuid);
        }
        if !self.group_uuid.is_empty() {
            pb_string(&mut inner, 42, &self.group_uuid);
        }
        pb_string(&mut inner, 43, "com.apple.iBooks");

        let envelope = mrp_envelope(15, 20, &inner);
        let mut params = Dictionary::new();
        let mut blob = Vec::new();
        pb_varint(&mut blob, envelope.len() as u64);
        blob.extend_from_slice(&envelope);
        params.insert("data".into(), Value::Data(blob));
        let mut root = Dictionary::new();
        root.insert("params".into(), Value::Dictionary(params));
        binary_plist(Value::Dictionary(root))
    }

    pub(crate) fn build_type130_opening_messages(&self) -> Vec<Vec<u8>> {
        vec![
            self.build_device_info_proto(),
            build_connection_state_proto(2),
            build_client_updates_config_proto(),
            self.build_now_playing_client_proto(),
            self.build_set_state_proto(true),
        ]
    }

    fn build_device_info_proto(&self) -> Vec<u8> {
        let mut inner = Vec::new();
        pb_string(&mut inner, 1, &self.device_uuid);
        pb_string(&mut inner, 2, &self.name);
        pb_string(&mut inner, 3, "iPhone");
        pb_string(&mut inner, 4, "21F90");
        pb_string(&mut inner, 5, "com.apple.Music");
        pb_varint_field(&mut inner, 7, 1);
        pb_varint_field(&mut inner, 8, 139);
        pb_varint_field(&mut inner, 9, 1);
        pb_varint_field(&mut inner, 10, 1);
        pb_string(&mut inner, 12, "com.apple.Music");
        pb_varint_field(&mut inner, 13, 1);
        pb_varint_field(&mut inner, 14, 1);
        pb_varint_field(&mut inner, 15, 1);
        pb_varint_field(&mut inner, 17, 2);
        pb_string(&mut inner, 19, &self.dacp_id);
        pb_varint_field(&mut inner, 21, 1);
        pb_varint_field(&mut inner, 22, 1);
        pb_string(&mut inner, 31, "com.apple.podcasts");
        pb_string(&mut inner, 39, "iPhone17,1");
        if !self.session_uuid.is_empty() { pb_string(&mut inner, 41, &self.session_uuid); }
        if !self.group_uuid.is_empty() { pb_string(&mut inner, 42, &self.group_uuid); }
        pb_string(&mut inner, 43, "com.apple.iBooks");
        mrp_envelope(15, 20, &inner)
    }

    fn build_now_playing_client_proto(&self) -> Vec<u8> {
        let mut client = Vec::new();
        pb_varint_field(&mut client, 1, std::process::id() as u64);
        pb_string(&mut client, 2, "com.apple.Music");
        pb_string(&mut client, 7, &self.name);
        let mut inner = Vec::new();
        pb_bytes(&mut inner, 1, &client);
        mrp_envelope(46, 50, &inner)
    }

    fn build_set_state_proto(&self, include_artwork: bool) -> Vec<u8> {
        let now_cf = cf_absolute_time(SystemTime::now());

        let mut npi = Vec::new();
        pb_string(&mut npi, 1, &self.album);
        pb_string(&mut npi, 2, &self.artist);
        if self.duration_ms > 0 { pb_double_field(&mut npi, 3, self.duration_ms as f64 / 1000.0); }
        pb_double_field(&mut npi, 4, self.elapsed_ms as f64 / 1000.0);
        pb_float_field(&mut npi, 5, if self.playback_state == MrpPlaybackState::Playing { 1.0 } else { 0.0 });
        pb_double_field(&mut npi, 8, now_cf);
        pb_string(&mut npi, 9, &self.title);
        pb_varint_field(&mut npi, 12, 1);

        let mut meta = Vec::new();
        pb_string(&mut meta, 1, &self.title);
        pb_string(&mut meta, 6, &self.album);
        pb_string(&mut meta, 7, &self.artist);
        if self.duration_ms > 0 { pb_double_field(&mut meta, 14, self.duration_ms as f64 / 1000.0); }
        pb_varint_field(&mut meta, 19, (!self.artwork.is_empty()) as u64);
        pb_varint_field(&mut meta, 27, (self.playback_state != MrpPlaybackState::Stopped) as u64);
        if let Some(mime) = self.artwork_mime.as_deref().filter(|_| !self.artwork.is_empty()) {
            pb_string(&mut meta, 31, mime);
        }
        pb_double_field(&mut meta, 35, self.elapsed_ms as f64 / 1000.0);
        pb_float_field(&mut meta, 39, if self.playback_state == MrpPlaybackState::Playing { 1.0 } else { 0.0 });
        pb_varint_field(&mut meta, 64, 1);
        pb_varint_field(&mut meta, 65, 1);
        pb_double_field(&mut meta, 74, cf_absolute_time(self.elapsed_set_at));

        let mut item = Vec::new();
        pb_string(&mut item, 1, &self.dacp_id);
        pb_bytes(&mut item, 2, &meta);
        if include_artwork && !self.artwork.is_empty() { pb_bytes(&mut item, 3, &self.artwork); }

        let mut queue = Vec::new();
        pb_varint_field(&mut queue, 1, 0);
        pb_bytes(&mut queue, 2, &item);

        let mut supported = Vec::new();
        for cmd in [1u64, 2, 3, 5, 6] {
            let mut ci = Vec::new();
            pb_varint_field(&mut ci, 1, cmd);
            pb_varint_field(&mut ci, 2, 1);
            pb_bytes(&mut supported, 1, &ci);
        }

        let mut inner = Vec::new();
        pb_bytes(&mut inner, 1, &npi);
        pb_bytes(&mut inner, 2, &supported);
        pb_bytes(&mut inner, 3, &queue);
        pb_string(&mut inner, 5, &self.name);
        pb_varint_field(&mut inner, 6, self.playback_state as u64);
        pb_double_field(&mut inner, 11, now_cf);
        mrp_envelope(4, 9, &inner)
    }

    pub fn build_supportedcommands_command(&self) -> Result<Vec<u8>, MrpError> {
        let mut arr = Vec::new();
        arr.push(command_info_blob(26, true, Some(("kMRMediaRemoteCommandInfoShuffleMode", Value::Integer(1.into()))))?);
        arr.push(command_info_blob(25, true, Some(("kMRMediaRemoteCommandInfoRepeatMode", Value::Integer(1.into()))))?);

        let mut scrub = Dictionary::new();
        scrub.insert("kMRMediaRemoteCommandInfoCanBeControlledByScrubbingKey".into(), Value::Boolean(false));
        scrub.insert("kMRMediaRemoteCommandInfoSupportsReferencePosition".into(), Value::Boolean(false));
        arr.push(command_info_blob_with_options(24, true, scrub)?);

        for cmd in [18i64, 17] {
            let mut options = Dictionary::new();
            options.insert("kMRMediaRemoteCommandInfoPreferredIntervalsKey".into(), Value::Array(Vec::new()));
            arr.push(command_info_blob_with_options(cmd, false, options)?);
        }
        for cmd in [10i64, 11, 8, 9, 5, 4, 3, 2, 1, 0] {
            arr.push(command_info_blob(cmd, true, None)?);
        }

        let mut params = Dictionary::new();
        params.insert("mrSupportedCommandsFromSender".into(), Value::Array(arr));
        let mut root = Dictionary::new();
        root.insert("type".into(), Value::String("updateMRSupportedCommands".into()));
        root.insert("params".into(), Value::Dictionary(params));
        binary_plist(Value::Dictionary(root))
    }

    pub fn build_playbackstate_command(&self) -> Result<Vec<u8>, MrpError> {
        let mut params = Dictionary::new();
        params.insert(
            "mrPlaybackState".into(),
            Value::Integer((self.playback_state as i64).into()),
        );
        let mut root = Dictionary::new();
        root.insert("type".into(), Value::String("updateMRPlaybackState".into()));
        root.insert("params".into(), Value::Dictionary(params));
        binary_plist(Value::Dictionary(root))
    }

    pub fn build_nowplayingclient_command(&self) -> Result<Vec<u8>, MrpError> {
        let mut client = Vec::new();
        pb_varint_field(&mut client, 1, std::process::id() as u64);
        pb_string(&mut client, 2, "com.apple.Music");
        pb_string(&mut client, 7, &self.name);

        let mut params = Dictionary::new();
        params.insert("mrNowPlayingClient".into(), Value::Data(client));
        let mut root = Dictionary::new();
        root.insert("type".into(), Value::String("updateMRNowPlayingClient".into()));
        root.insert("params".into(), Value::Dictionary(params));
        binary_plist(Value::Dictionary(root))
    }

    pub fn build_nowplaying_command(&self) -> Result<Vec<u8>, MrpError> {
        self.build_nowplaying_with_policy("replace", false)
    }

    pub fn build_progress_command(&self) -> Result<Vec<u8>, MrpError> {
        self.build_nowplaying_with_policy("update", true)
    }

    fn build_nowplaying_with_policy(
        &self,
        policy: &str,
        progress_only: bool,
    ) -> Result<Vec<u8>, MrpError> {
        let mut info = Dictionary::new();

        if !progress_only {
            info.insert("kMRMediaRemoteNowPlayingInfoTitle".into(), Value::String(self.title.clone()));
            info.insert("kMRMediaRemoteNowPlayingInfoArtist".into(), Value::String(self.artist.clone()));
            info.insert("kMRMediaRemoteNowPlayingInfoAlbum".into(), Value::String(self.album.clone()));
            if self.duration_ms > 0 {
                info.insert(
                    "kMRMediaRemoteNowPlayingInfoDuration".into(),
                    Value::Real(self.duration_ms as f64 / 1000.0),
                );
            }
        }

        info.insert(
            "kMRMediaRemoteNowPlayingInfoElapsedTime".into(),
            Value::Real(self.elapsed_ms as f64 / 1000.0),
        );
        info.insert(
            "kMRMediaRemoteNowPlayingInfoPlaybackRate".into(),
            Value::Real(if self.playback_state == MrpPlaybackState::Playing { 1.0 } else { 0.0 }),
        );
        info.insert(
            "kMRMediaRemoteNowPlayingInfoDefaultPlaybackRate".into(),
            Value::Real(1.0),
        );
        info.insert(
            "kMRMediaRemoteNowPlayingInfoTimestamp".into(),
            Value::Date(plist::Date::from(self.elapsed_set_at)),
        );

        if !progress_only {
            info.insert(
                "kMRMediaRemoteNowPlayingInfoMediaType".into(),
                Value::String("MRMediaRemoteMediaTypeMusic".into()),
            );
            info.insert(
                "kMRMediaRemoteNowPlayingInfoUniqueIdentifier".into(),
                Value::Integer((self.np_uid as i64).into()),
            );
            if !self.artwork.is_empty() {
                info.insert(
                    "kMRMediaRemoteNowPlayingInfoArtworkIdentifier".into(),
                    Value::String(self.artwork_id.clone()),
                );
                info.insert(
                    "kMRMediaRemoteNowPlayingInfoArtworkMIMEType".into(),
                    Value::String(self.artwork_mime.clone().unwrap_or_else(|| "image/jpeg".into())),
                );
                info.insert(
                    "kMRMediaRemoteNowPlayingInfoArtworkData".into(),
                    Value::Data(self.artwork.clone()),
                );
            }
        }

        let mut nested = Dictionary::new();
        nested.insert("type".into(), Value::String("npi-text".into()));
        nested.insert("mergePolicy".into(), Value::String(policy.into()));
        nested.insert("params".into(), Value::Dictionary(info));

        let mut root = Dictionary::new();
        root.insert("type".into(), Value::String("updateMRNowPlayingInfo".into()));
        root.insert("params".into(), Value::Dictionary(nested));
        binary_plist(Value::Dictionary(root))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MrpPushResult {
    pub overall_status: i32,
    pub nowplaying_status: i32,
}

impl MrpPushResult {
    pub const fn empty() -> Self {
        Self { overall_status: -1, nowplaying_status: 0 }
    }
}

#[derive(Clone)]
pub struct MrpController {
    state: Arc<Mutex<MrpState>>,
    publish_lock: Arc<Mutex<()>>,
    control: SharedRtspControl,
    next_cseq: SharedCseq,
    dacp_id: String,
    active_remote: String,
}

impl MrpController {
    pub fn new(
        state: MrpState,
        control: SharedRtspControl,
        next_cseq: SharedCseq,
        dacp_id: impl Into<String>,
        active_remote: impl Into<String>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            publish_lock: Arc::new(Mutex::new(())),
            control,
            next_cseq,
            dacp_id: dacp_id.into(),
            active_remote: active_remote.into(),
        }
    }

    pub fn snapshot(&self) -> Result<MrpState, MrpError> {
        self.state.lock().map(|v| v.clone()).map_err(|_| MrpError::Lock)
    }

    pub fn stage_track(
        &self,
        title: &str,
        artist: &str,
        album: &str,
        duration_ms: i64,
        item_id: &str,
        artwork: Option<(&str, &[u8])>,
    ) -> Result<(bool, MrpArtworkInfo), MrpError> {
        let mut state = self.state.lock().map_err(|_| MrpError::Lock)?;
        Ok(state.set_track(title, artist, album, duration_ms, item_id, artwork))
    }

    pub fn stage_artwork(
        &self,
        mime: &str,
        data: &[u8],
    ) -> Result<MrpArtworkInfo, MrpError> {
        let mut state = self.state.lock().map_err(|_| MrpError::Lock)?;
        Ok(state.set_artwork(mime, data))
    }

    pub fn clear_artwork_and_push(&self) -> Result<MrpPushResult, MrpError> {
        let _publish = self.publish_lock.lock().map_err(|_| MrpError::Lock)?;
        {
            let mut state = self.state.lock().map_err(|_| MrpError::Lock)?;
            state.clear_artwork();
        }
        self.push_full_locked()
    }

    pub fn push_full(&self) -> Result<MrpPushResult, MrpError> {
        let _publish = self.publish_lock.lock().map_err(|_| MrpError::Lock)?;
        self.push_full_locked()
    }

    pub fn set_progress_and_push(
        &self,
        elapsed_ms: i64,
        duration_ms: i64,
        playing: bool,
    ) -> Result<MrpPushResult, MrpError> {
        let _publish = self.publish_lock.lock().map_err(|_| MrpError::Lock)?;
        {
            let mut state = self.state.lock().map_err(|_| MrpError::Lock)?;
            state.set_progress(elapsed_ms, duration_ms, playing);
        }
        self.push_progress_locked()
    }

    pub fn publish_playback_state(
        &self,
        state: MrpPlaybackState,
        force: bool,
    ) -> Result<i32, MrpError> {
        let _publish = self.publish_lock.lock().map_err(|_| MrpError::Lock)?;
        self.publish_playback_locked(state, force)
    }

    pub fn publish_playback_state_on_transition(
        &self,
        state: MrpPlaybackState,
    ) -> Result<i32, MrpError> {
        let changed = {
            let current = self.state.lock().map_err(|_| MrpError::Lock)?;
            current.last_playback_state != Some(state)
        };
        if !changed {
            return Ok(200);
        }
        self.publish_playback_state(state, true)
    }

    fn post_body(&self, body: Vec<u8>) -> Result<i32, MrpError> {
        Ok(post_command(
            &self.control,
            &self.next_cseq,
            &self.dacp_id,
            &self.active_remote,
            body,
        )?.status as i32)
    }

    fn register_locked(&self) -> Result<i32, MrpError> {
        let already = self.state.lock().map_err(|_| MrpError::Lock)?.device_registered;
        if already {
            return Ok(1);
        }
        let body = self.state.lock().map_err(|_| MrpError::Lock)?
            .build_deviceinfo_command()?;
        let status = self.post_body(body)?;
        let ok = status_ok(status);
        if ok {
            self.state.lock().map_err(|_| MrpError::Lock)?.device_registered = true;
        }
        Ok(if ok { 1 } else { 0 })
    }

    fn send_playback_state_locked(
        &self,
        state: MrpPlaybackState,
        force: bool,
    ) -> Result<i32, MrpError> {
        let (last, body) = {
            let current = self.state.lock().map_err(|_| MrpError::Lock)?;
            if !force && current.last_playback_state == Some(state) {
                return Ok(200);
            }
            (current.last_playback_state, current.build_playbackstate_command()?)
        };
        let _ = last;
        let status = self.post_body(body)?;
        if status_ok(status) {
            self.state.lock().map_err(|_| MrpError::Lock)?.last_playback_state = Some(state);
        }
        Ok(status)
    }

    fn send_extended_registration_locked(&self) -> Result<i32, MrpError> {
        let commands = self.state.lock().map_err(|_| MrpError::Lock)?
            .build_supportedcommands_command()?;
        let st_cmd = self.post_body(commands)?;

        let state_value = self.state.lock().map_err(|_| MrpError::Lock)?.playback_state;
        let st_state = self.send_playback_state_locked(state_value, true)?;

        let client = self.state.lock().map_err(|_| MrpError::Lock)?
            .build_nowplayingclient_command()?;
        let st_client = self.post_body(client)?;

        let ok = status_ok(st_cmd) && status_ok(st_state) && status_ok(st_client);
        self.state.lock().map_err(|_| MrpError::Lock)?.extended_registered = ok;

        if !status_ok(st_cmd) {
            Ok(st_cmd)
        } else if !status_ok(st_state) {
            Ok(st_state)
        } else {
            Ok(st_client)
        }
    }

    fn push_with_locked(&self, progress_only: bool) -> Result<MrpPushResult, MrpError> {
        let mut result = MrpPushResult::empty();
        if self.register_locked()? != 1 {
            result.overall_status = 0;
            return Ok(result);
        }

        let body = {
            let state = self.state.lock().map_err(|_| MrpError::Lock)?;
            if progress_only {
                state.build_progress_command()?
            } else {
                state.build_nowplaying_command()?
            }
        };
        let status = self.post_body(body)?;
        result.nowplaying_status = status;
        result.overall_status = status;
        if !status_ok(status) {
            return Ok(result);
        }

        let extended = self.state.lock().map_err(|_| MrpError::Lock)?.extended_registered;
        let ext_status = if !extended {
            self.send_extended_registration_locked()?
        } else {
            let playback = self.state.lock().map_err(|_| MrpError::Lock)?.playback_state;
            self.send_playback_state_locked(playback, false)?
        };
        if !status_ok(ext_status) {
            result.overall_status = ext_status;
        }
        Ok(result)
    }

    fn push_full_locked(&self) -> Result<MrpPushResult, MrpError> {
        self.push_with_locked(false)
    }

    fn push_progress_locked(&self) -> Result<MrpPushResult, MrpError> {
        let full = self.state.lock().map_err(|_| MrpError::Lock)?.progress_push_full;
        if !full {
            let result = self.push_with_locked(true)?;
            if result.nowplaying_status >= 300 {
                self.state.lock().map_err(|_| MrpError::Lock)?.progress_push_full = true;
            } else {
                return Ok(result);
            }
        }
        self.push_full_locked()
    }

    fn publish_playback_locked(
        &self,
        state: MrpPlaybackState,
        mut force: bool,
    ) -> Result<i32, MrpError> {
        let state_was_current = {
            let mut current = self.state.lock().map_err(|_| MrpError::Lock)?;
            let was = current.last_playback_state == Some(state);
            match state {
                MrpPlaybackState::Stopped => current.set_stopped(),
                MrpPlaybackState::Playing => current.set_playing(true),
                MrpPlaybackState::Paused => current.set_playing(false),
            }
            was
        };

        if state != MrpPlaybackState::Stopped {
            let pushed = self.push_progress_locked()?;
            if status_ok(pushed.overall_status) && !state_was_current {
                force = false;
            }
        }
        self.send_playback_state_locked(state, force)
    }
}

fn status_ok(status: i32) -> bool {
    (200..300).contains(&status)
}

pub fn post_command(
    control: &SharedRtspControl,
    next_cseq: &SharedCseq,
    dacp_id: &str,
    active_remote: &str,
    body: Vec<u8>,
) -> Result<MrpPostResult, MrpError> {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let mut channel = loop {
        match control.try_lock() {
            Ok(v) => break v,
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(MrpError::LockTimeout);
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::Poisoned(_)) => return Err(MrpError::Lock),
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(MrpError::LockTimeout);
    }
    let cseq = next_cseq.fetch_add(1, Ordering::SeqCst);
    let req = RtspRequest {
        method: "POST".into(),
        uri: "/command".into(),
        cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: dacp_id.into(),
        active_remote: active_remote.into(),
        client_instance: None,
        content_type: Some("application/x-apple-binary-plist".into()),
        body,
    };
    let response = channel
        .exchange_with_timeout(&req.encode(), cseq, remaining)
        .map_err(MrpError::Transport)?;
    Ok(MrpPostResult { status: response.status })
}

pub fn probe_artwork(mime: &str, data: &[u8]) -> MrpArtworkInfo {
    let mut info = MrpArtworkInfo::new(data.len());
    if mime.is_empty() || data.is_empty() {
        return info;
    }
    if mime != "image/jpeg" {
        info.result = MrpArtworkResult::UnsupportedType;
        return info;
    }
    if data.len() > ARTWORK_STAGING_MAX_BYTES {
        info.result = MrpArtworkResult::StagingLimit;
        return info;
    }
    if data.len() < 4
        || data[0] != 0xff
        || data[1] != 0xd8
        || data[data.len() - 2] != 0xff
        || data[data.len() - 1] != 0xd9
    {
        info.result = MrpArtworkResult::InvalidJpegEnvelope;
        return info;
    }
    probe_jpeg_metadata(data, &mut info);
    info.result = MrpArtworkResult::Accepted;
    info
}

fn probe_jpeg_metadata(data: &[u8], info: &mut MrpArtworkInfo) {
    let mut pos = 2usize;
    while pos + 3 < data.len().saturating_sub(2) {
        if data[pos] != 0xff {
            return;
        }
        pos += 1;
        while pos < data.len().saturating_sub(2) && data[pos] == 0xff {
            pos += 1;
        }
        if pos >= data.len().saturating_sub(2) {
            return;
        }
        let marker = data[pos];
        pos += 1;
        if marker == 0xd9 || marker == 0xda {
            return;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if marker == 0x00 || marker == 0xd8 || pos + 2 > data.len() {
            return;
        }
        let seg_len = ((data[pos] as usize) << 8) | data[pos + 1] as usize;
        if seg_len < 2 || seg_len > data.len() - pos {
            return;
        }
        if jpeg_sof(marker) && seg_len >= 8 {
            let sof = &data[pos + 2..];
            let components = sof[5];
            if seg_len < 8 + 3 * components as usize {
                return;
            }
            info.precision = sof[0];
            info.height = u16::from_be_bytes([sof[1], sof[2]]);
            info.width = u16::from_be_bytes([sof[3], sof[4]]);
            info.components = components;
            info.sof_marker = marker;
            info.progressive = matches!(marker, 0xc2 | 0xc6 | 0xca | 0xce);
            return;
        }
        pos += seg_len;
    }
}

fn jpeg_sof(marker: u8) -> bool {
    matches!(
        marker,
        0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7
            | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
    )
}

fn identity_uuid(identity: &str) -> String {
    let digest = Sha256::digest(identity.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    uuid_from_bytes(&b)
}

fn random_uuid() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    uuid_from_bytes(&b)
}

fn uuid_from_bytes(b: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn random_u63() -> u64 {
    rand::thread_rng().next_u64() & 0x7fff_ffff_ffff_ffff
}


fn build_connection_state_proto(state: u64) -> Vec<u8> {
    let mut inner = Vec::new();
    pb_varint_field(&mut inner, 1, state);
    mrp_envelope(38, 42, &inner)
}

fn build_client_updates_config_proto() -> Vec<u8> {
    let mut inner = Vec::new();
    for field in 1..=5 { pb_varint_field(&mut inner, field, 0); }
    mrp_envelope(16, 21, &inner)
}

fn cf_absolute_time(t: SystemTime) -> f64 {
    const APPLE_EPOCH_OFFSET: f64 = 978_307_200.0;
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() - APPLE_EPOCH_OFFSET)
        .unwrap_or(-APPLE_EPOCH_OFFSET)
}

fn pb_double_field(out: &mut Vec<u8>, field: u32, value: f64) {
    pb_key(out, field, 1);
    out.extend_from_slice(&value.to_bits().to_le_bytes());
}

fn pb_float_field(out: &mut Vec<u8>, field: u32, value: f32) {
    pb_key(out, field, 5);
    out.extend_from_slice(&value.to_bits().to_le_bytes());
}

fn binary_plist(value: Value) -> Result<Vec<u8>, MrpError> {
    let mut out = Vec::new();
    value.to_writer_binary(&mut out).map_err(MrpError::Plist)?;
    Ok(out)
}

fn command_info_blob(
    command: i64,
    enabled: bool,
    option: Option<(&str, Value)>,
) -> Result<Value, MrpError> {
    let mut options = Dictionary::new();
    if let Some((k, v)) = option {
        options.insert(k.into(), v);
    }
    command_info_blob_with_options(command, enabled, options)
}

fn command_info_blob_with_options(
    command: i64,
    enabled: bool,
    options: Dictionary,
) -> Result<Value, MrpError> {
    let mut d = Dictionary::new();
    d.insert("kCommandInfoCommandKey".into(), Value::Integer(command.into()));
    d.insert("kCommandInfoEnabledKey".into(), Value::Boolean(enabled));
    if !options.is_empty() {
        d.insert("kCommandInfoOptionsKey".into(), Value::Dictionary(options));
    }
    Ok(Value::Data(binary_plist(Value::Dictionary(d))?))
}

fn pb_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn pb_key(out: &mut Vec<u8>, field: u32, wire: u32) {
    pb_varint(out, ((field as u64) << 3) | wire as u64);
}

fn pb_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    pb_key(out, field, 0);
    pb_varint(out, value);
}

fn pb_bytes(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    pb_key(out, field, 2);
    pb_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

fn pb_string(out: &mut Vec<u8>, field: u32, value: &str) {
    pb_bytes(out, field, value.as_bytes());
}

fn mrp_envelope(msg_type: u64, ext_field: u32, inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    pb_varint_field(&mut out, 1, msg_type);
    pb_bytes(&mut out, ext_field, inner);
    pb_string(&mut out, 85, &random_uuid());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_uuid_is_stable_v5_shape() {
        let a = identity_uuid("AABBCCDDEEFF0011");
        let b = identity_uuid("AABBCCDDEEFF0011");
        assert_eq!(a, b);
        assert_eq!(a.as_bytes()[14], b'5');
    }

    #[test]
    fn artwork_rejects_non_jpeg_and_large_payloads() {
        assert_eq!(
            probe_artwork("image/png", b"abc").result,
            MrpArtworkResult::UnsupportedType
        );
        let huge = vec![0u8; ARTWORK_STAGING_MAX_BYTES + 1];
        assert_eq!(
            probe_artwork("image/jpeg", &huge).result,
            MrpArtworkResult::StagingLimit
        );
    }

    #[test]
    fn stable_item_id_keeps_nowplaying_uid_across_tag_refinement() {
        let mut m = MrpState::new("AABB", "SAirplay2", "S", Some("G".into()));
        let _ = m.set_track("one", "a", "x", 1000, "item-1", None);
        let uid = m.np_uid;
        let (changed, _) = m.set_track("refined", "a", "x", 1000, "item-1", None);
        assert!(!changed);
        assert_eq!(uid, m.np_uid);
    }

    #[test]
    fn replace_and_progress_commands_are_binary_plists() {
        let mut m = MrpState::new("AABB", "SAirplay2", "S", Some("G".into()));
        let _ = m.set_track("title", "artist", "album", 123000, "id", None);
        let full = m.build_nowplaying_command().unwrap();
        let progress = m.build_progress_command().unwrap();
        assert!(full.starts_with(b"bplist00"));
        assert!(progress.starts_with(b"bplist00"));
    }

    #[test]
    fn device_info_wrapper_begins_with_binary_plist() {
        let m = MrpState::new("AABB", "SAirplay2", "S", Some("G".into()));
        assert!(m.build_deviceinfo_command().unwrap().starts_with(b"bplist00"));
    }
}
