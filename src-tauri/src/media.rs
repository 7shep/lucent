//! Windows media session monitor (GlobalSystemMediaTransportControls).
//!
//! WinRT objects never leave the monitor thread. WinRT event handlers only
//! send a message over a channel; the monitor re-reads state, re-attaches
//! when the current session changes, and emits `media://update` on change.
//! The thread blocks on the channel, so there is no polling.

use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use windows::core::{Interface, Result as WinResult, RuntimeType};
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows_future::{AsyncOperationCompletedHandler, AsyncStatus, IAsyncInfo, IAsyncOperation};

/// Event name the frontend listens for.
pub const EVENT: &str = "media://update";

/// Larger thumbnails are ignored rather than base64-encoded.
const MAX_ARTWORK_BYTES: u64 = 2 * 1024 * 1024;

/// Maximum characters kept per metadata string (title, artist, album, app).
const MAX_TEXT_CHARS: usize = 300;

/// Timeouts for WinRT async calls. The monitor thread must never block forever.
const PROPS_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct MediaState {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub playing: bool,
    pub app: Option<String>,
    /// `data:<mime>;base64,...` URL, or None.
    pub artwork: Option<String>,
    /// Playback position in ms, as of `updated_at_ms`. None when unknown.
    pub position_ms: Option<u64>,
    /// Track length in ms. None when unknown or not positive.
    pub duration_ms: Option<u64>,
    /// Unix epoch ms at which `position_ms` was sampled. None when unknown.
    pub updated_at_ms: Option<i64>,
}

impl MediaState {
    /// Trims text fields and turns empty strings into None.
    fn cleaned(self) -> Self {
        MediaState {
            title: clean_text(self.title),
            artist: clean_text(self.artist),
            album: clean_text(self.album),
            playing: self.playing,
            app: clean_text(self.app),
            artwork: clean(self.artwork),
            position_ms: self.position_ms,
            duration_ms: self.duration_ms,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

/// Windows timeline and clock units: 100 ns ticks.
const TICKS_PER_MS: i64 = 10_000;
/// Ticks from 1601-01-01 UTC (the WinRT DateTime epoch) to the Unix epoch.
const UNIX_EPOCH_UNIVERSAL_TICKS: i64 = 116_444_736_000_000_000;

/// Converts raw GSMTC timeline values (100 ns ticks) to milliseconds.
/// Returns (position_ms, duration_ms, updated_at_ms). A duration that is not
/// positive gives (None, None, None). The position is clamped to the track.
/// `updated_at_ms` is None when `last_updated_universal` is not positive.
pub fn timeline(
    start_ticks: i64,
    end_ticks: i64,
    position_ticks: i64,
    last_updated_universal: i64,
) -> (Option<u64>, Option<u64>, Option<i64>) {
    let duration = end_ticks.saturating_sub(start_ticks);
    if duration <= 0 {
        return (None, None, None);
    }
    let position = position_ticks.saturating_sub(start_ticks).clamp(0, duration);
    let updated_at_ms = if last_updated_universal <= 0 {
        None
    } else {
        Some((last_updated_universal - UNIX_EPOCH_UNIVERSAL_TICKS) / TICKS_PER_MS)
    };
    (
        Some((position / TICKS_PER_MS) as u64),
        Some((duration / TICKS_PER_MS) as u64),
        updated_at_ms,
    )
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Like `clean`, and keeps at most MAX_TEXT_CHARS characters (char-boundary safe).
fn clean_text(value: Option<String>) -> Option<String> {
    clean(value).map(|s| s.chars().take(MAX_TEXT_CHARS).collect::<String>())
}

/// Waits for a WinRT async operation for at most `timeout`. Timeouts cancel
/// the operation. Failed or cancelled operations give None.
fn wait<T: RuntimeType + 'static>(op: &IAsyncOperation<T>, timeout: Duration) -> Option<T> {
    if op.Status().ok()? != AsyncStatus::Started {
        return op.GetResults().ok();
    }
    let (tx, rx) = sync_channel::<()>(1);
    let handler = AsyncOperationCompletedHandler::new(move |_op, _status| {
        let _ = tx.try_send(());
        Ok(())
    });
    op.SetCompleted(&handler).ok()?;
    if rx.recv_timeout(timeout).is_err() {
        if let Ok(info) = op.cast::<IAsyncInfo>() {
            let _ = info.Cancel();
        }
        return None;
    }
    op.GetResults().ok()
}

/// Thumbnail MIME type: the stream's own type when it is an image type,
/// otherwise image/png.
fn pick_mime(content_type: &str) -> String {
    let t = content_type.trim().to_ascii_lowercase();
    if t.starts_with("image/") {
        t
    } else {
        "image/png".to_string()
    }
}

/// Thumbnail sizes: non-empty and within the cap.
fn accept_size(n: u64) -> bool {
    n > 0 && n <= MAX_ARTWORK_BYTES
}

/// Builds a data URL for artwork bytes. Empty input has no artwork.
pub fn artwork_data_url(mime: &str, bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Emit only when the state differs from the last emitted one.
pub fn should_emit(prev: &Option<MediaState>, next: &MediaState) -> bool {
    prev.as_ref() != Some(next)
}

/// Identity of the current track. Artwork is reused while this stays equal.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TrackKey {
    pub app: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

/// Last artwork read, keyed by track. Play/pause changes reuse it instead of
/// re-reading and re-encoding the thumbnail.
#[derive(Default)]
pub struct ArtworkCache {
    key: Option<TrackKey>,
    artwork: Option<String>,
}

impl ArtworkCache {
    /// Cached artwork for `key`, or None when a read is needed. A cached None
    /// is a miss, so artwork that was not ready yet is read again.
    pub fn lookup(&self, key: &TrackKey) -> Option<String> {
        if self.key.as_ref() == Some(key) {
            self.artwork.clone()
        } else {
            None
        }
    }

    pub fn store(&mut self, key: TrackKey, artwork: Option<String>) {
        self.key = Some(key);
        self.artwork = artwork;
    }

    pub fn clear(&mut self) {
        self.key = None;
        self.artwork = None;
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
    Toggle,
    Next,
    Prev,
}

enum Msg {
    Command(Command),
    SessionChanged,
    StateChanged,
}

/// Managed by Tauri: a channel into the monitor thread plus the last state.
pub struct MediaHandle {
    tx: Sender<Msg>,
    last: Arc<Mutex<MediaState>>,
}

impl MediaHandle {
    fn send(&self, command: Command) {
        let _ = self.tx.send(Msg::Command(command));
    }
}

/// Starts the monitor thread and returns the handle used by commands.
pub fn start(app: AppHandle) -> MediaHandle {
    let (tx, rx) = channel();
    let last = Arc::new(Mutex::new(MediaState::default()));
    let handle = MediaHandle {
        tx: tx.clone(),
        last: Arc::clone(&last),
    };
    let _ = std::thread::Builder::new()
        .name("lucent-media".into())
        .spawn(move || monitor_thread(app, last, tx, rx));
    handle
}

/// One-shot read for `lucent --print-media`. No window, no monitor thread.
pub fn probe() -> MediaState {
    init_apartment();
    let Some(manager) = request_manager_plain() else {
        return MediaState::default();
    };
    match current_session(&manager) {
        Some(session) => read_session(&session, None).unwrap_or_default(),
        None => MediaState::default(),
    }
}

#[tauri::command]
pub fn get_media_state(media: State<'_, MediaHandle>) -> MediaState {
    media
        .last
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default()
}

#[tauri::command]
pub fn media_toggle(media: State<'_, MediaHandle>) {
    media.send(Command::Toggle);
}

#[tauri::command]
pub fn media_next(media: State<'_, MediaHandle>) {
    media.send(Command::Next);
}

#[tauri::command]
pub fn media_prev(media: State<'_, MediaHandle>) {
    media.send(Command::Prev);
}

fn init_apartment() {
    // Already-initialised is reported as an error; that is harmless here.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
}

struct Subscription {
    session: Session,
    media: Option<i64>,
    playback: Option<i64>,
    timeline: Option<i64>,
}

struct Monitor {
    app: AppHandle,
    last: Arc<Mutex<MediaState>>,
    tx: Sender<Msg>,
    manager: Option<SessionManager>,
    sub: Option<Subscription>,
    emitted: Option<MediaState>,
    artwork: ArtworkCache,
}

fn monitor_thread(app: AppHandle, last: Arc<Mutex<MediaState>>, tx: Sender<Msg>, rx: Receiver<Msg>) {
    init_apartment();
    let manager = request_manager_subscribed(&tx);
    let mut monitor = Monitor {
        app,
        last,
        tx,
        manager,
        sub: None,
        emitted: None,
        artwork: ArtworkCache::default(),
    };
    monitor.attach_session();
    monitor.publish();

    // Blocks until a handler or a command sends something.
    for msg in rx {
        // A failed startup request is retried lazily, on the next message.
        if monitor.ensure_manager() {
            monitor.attach_session();
            monitor.publish();
        }
        match msg {
            Msg::Command(command) => monitor.command(command),
            Msg::SessionChanged => {
                monitor.attach_session();
                monitor.publish();
            }
            Msg::StateChanged => monitor.publish(),
        }
    }
}

impl Monitor {
    /// Retries the session manager request when it is missing.
    /// Returns true when the manager was newly acquired. The manager is only
    /// ever set from None, so no event token needs removing.
    fn ensure_manager(&mut self) -> bool {
        if self.manager.is_some() {
            return false;
        }
        self.manager = request_manager_subscribed(&self.tx);
        self.manager.is_some()
    }

    fn attach_session(&mut self) {
        self.detach_session();
        self.artwork.clear();
        let Some(manager) = &self.manager else {
            return;
        };
        let Some(session) = current_session(manager) else {
            return;
        };

        let tx_media = self.tx.clone();
        let media = session
            .MediaPropertiesChanged(&TypedEventHandler::new(move |_sender, _args| {
                let _ = tx_media.send(Msg::StateChanged);
                Ok(())
            }))
            .ok();
        let tx_playback = self.tx.clone();
        let playback = session
            .PlaybackInfoChanged(&TypedEventHandler::new(move |_sender, _args| {
                let _ = tx_playback.send(Msg::StateChanged);
                Ok(())
            }))
            .ok();
        let tx_timeline = self.tx.clone();
        let timeline = session
            .TimelinePropertiesChanged(&TypedEventHandler::new(move |_sender, _args| {
                let _ = tx_timeline.send(Msg::StateChanged);
                Ok(())
            }))
            .ok();

        self.sub = Some(Subscription {
            session,
            media,
            playback,
            timeline,
        });
    }

    fn detach_session(&mut self) {
        if let Some(sub) = self.sub.take() {
            if let Some(token) = sub.media {
                let _ = sub.session.RemoveMediaPropertiesChanged(token);
            }
            if let Some(token) = sub.playback {
                let _ = sub.session.RemovePlaybackInfoChanged(token);
            }
            if let Some(token) = sub.timeline {
                let _ = sub.session.RemoveTimelinePropertiesChanged(token);
            }
        }
    }

    fn read(&mut self) -> MediaState {
        match &self.sub {
            Some(sub) => read_session(&sub.session, Some(&mut self.artwork)).unwrap_or_default(),
            None => MediaState::default(),
        }
    }

    fn publish(&mut self) {
        let next = self.read();
        if !should_emit(&self.emitted, &next) {
            return;
        }
        if let Ok(mut guard) = self.last.lock() {
            *guard = next.clone();
        }
        let _ = self.app.emit(EVENT, next.clone());
        self.emitted = Some(next);
    }

    fn command(&self, command: Command) {
        let Some(sub) = &self.sub else {
            return;
        };
        let op = match command {
            Command::Toggle => sub.session.TryTogglePlayPauseAsync(),
            Command::Next => sub.session.TrySkipNextAsync(),
            Command::Prev => sub.session.TrySkipPreviousAsync(),
        };
        if let Ok(op) = op {
            let _ = wait(&op, COMMAND_TIMEOUT);
        }
    }
}

fn request_manager_plain() -> Option<SessionManager> {
    wait(&SessionManager::RequestAsync().ok()?, REQUEST_TIMEOUT)
}

fn request_manager_subscribed(tx: &Sender<Msg>) -> Option<SessionManager> {
    let manager = request_manager_plain()?;
    let tx_current = tx.clone();
    let _ = manager.CurrentSessionChanged(&TypedEventHandler::new(move |_sender, _args| {
        let _ = tx_current.send(Msg::SessionChanged);
        Ok(())
    }));
    let tx_sessions = tx.clone();
    let _ = manager.SessionsChanged(&TypedEventHandler::new(move |_sender, _args| {
        let _ = tx_sessions.send(Msg::SessionChanged);
        Ok(())
    }));
    Some(manager)
}

/// `GetCurrentSession` returns an error when nothing is playing, which means
/// "no session".
fn current_session(manager: &SessionManager) -> Option<Session> {
    manager.GetCurrentSession().ok()
}

/// With a cache, the thumbnail is only read when the track key misses it.
fn read_session(session: &Session, cache: Option<&mut ArtworkCache>) -> WinResult<MediaState> {
    let props = wait(&session.TryGetMediaPropertiesAsync()?, PROPS_TIMEOUT)
        .ok_or_else(windows::core::Error::empty)?;
    let playing = session.GetPlaybackInfo()?.PlaybackStatus()? == PlaybackStatus::Playing;
    let app = session
        .SourceAppUserModelId()
        .ok()
        .map(|s| s.to_string());
    let title = props.Title().ok().map(|s| s.to_string());
    let artist = props.Artist().ok().map(|s| s.to_string());
    let album = props.AlbumTitle().ok().map(|s| s.to_string());

    let read_artwork = || {
        props
            .Thumbnail()
            .ok()
            .and_then(|thumb| read_thumbnail(&thumb))
            .and_then(|(mime, bytes)| artwork_data_url(&mime, &bytes))
    };
    let artwork = match cache {
        Some(cache) => {
            let key = TrackKey {
                app: app.clone(),
                title: title.clone(),
                artist: artist.clone(),
                album: album.clone(),
            };
            match cache.lookup(&key) {
                Some(hit) => Some(hit),
                None => {
                    let fresh = read_artwork();
                    cache.store(key, fresh.clone());
                    fresh
                }
            }
        }
        None => read_artwork(),
    };

    let (position_ms, duration_ms, updated_at_ms) = read_timeline(session)
        .map(|(start, end, position, updated)| timeline(start, end, position, updated))
        .unwrap_or((None, None, None));

    Ok(MediaState {
        title,
        artist,
        album,
        playing,
        app,
        artwork,
        position_ms,
        duration_ms,
        updated_at_ms,
    }
    .cleaned())
}

/// Raw timeline values in ticks: (start, end, position, last updated
/// universal time). Any failure gives None.
fn read_timeline(session: &Session) -> Option<(i64, i64, i64, i64)> {
    let tl = session.GetTimelineProperties().ok()?;
    let start = tl.StartTime().ok()?.Duration;
    let end = tl.EndTime().ok()?.Duration;
    let position = tl.Position().ok()?.Duration;
    let updated = tl.LastUpdatedTime().ok()?.UniversalTime;
    Some((start, end, position, updated))
}

/// Reads thumbnail bytes and their MIME type. The stream's own content type
/// is used when it is an image type, otherwise image/png. Any failure, or a
/// size over the cap, gives None.
fn read_thumbnail(thumb: &IRandomAccessStreamReference) -> Option<(String, Vec<u8>)> {
    let stream = wait(&thumb.OpenReadAsync().ok()?, PROPS_TIMEOUT)?;
    let mime = pick_mime(
        &stream
            .ContentType()
            .map(|s| s.to_string())
            .unwrap_or_default(),
    );
    let size = stream.Size().ok()?;
    if !accept_size(size) {
        return None;
    }
    let input = stream.GetInputStreamAt(0).ok()?;
    let reader = DataReader::CreateDataReader(&input).ok()?;
    let loaded = wait(&reader.LoadAsync(size as u32).ok()?, PROPS_TIMEOUT)?;
    let mut buf = vec![0u8; loaded as usize];
    reader.ReadBytes(&mut buf).ok()?;
    Some((mime, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_state_has_exactly_the_nine_keys() {
        let value = serde_json::to_value(MediaState::default()).unwrap();
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "album",
                "app",
                "artist",
                "artwork",
                "duration_ms",
                "playing",
                "position_ms",
                "title",
                "updated_at_ms"
            ]
        );
    }

    #[test]
    fn empty_strings_normalise_to_none() {
        let state = MediaState {
            title: Some("   ".into()),
            artist: Some(String::new()),
            album: Some("Album".into()),
            playing: true,
            app: Some("".into()),
            artwork: Some("".into()),
            position_ms: Some(5_000),
            duration_ms: Some(180_000),
            updated_at_ms: Some(1_700_000_000_000),
        }
        .cleaned();
        assert_eq!(state.title, None);
        assert_eq!(state.artist, None);
        assert_eq!(state.album, Some("Album".to_string()));
        assert_eq!(state.app, None);
        assert_eq!(state.artwork, None);
        assert!(state.playing);
        assert_eq!(state.position_ms, Some(5_000));
        assert_eq!(state.duration_ms, Some(180_000));
        assert_eq!(state.updated_at_ms, Some(1_700_000_000_000));
    }

    // 1 s = 10_000_000 ticks. Universal time 116_444_736_000_000_000 is the Unix epoch.

    #[test]
    fn timeline_normal_case() {
        // Track starts at 1 s and ends at 214 s (213 s long). Position is at 61 s,
        // i.e. 60 s into the track. Sampled 1_234_567 ms after the Unix epoch.
        let got = timeline(
            10_000_000,
            2_140_000_000,
            610_000_000,
            116_444_736_000_000_000 + 12_345_670_000,
        );
        assert_eq!(got, (Some(60_000), Some(213_000), Some(1_234_567)));
    }

    #[test]
    fn timeline_zero_or_negative_duration_is_all_none() {
        let universal = 116_444_736_000_000_000 + 10_000_000;
        assert_eq!(timeline(5_000_000, 5_000_000, 0, universal), (None, None, None));
        assert_eq!(timeline(5_000_000, 1_000_000, 0, universal), (None, None, None));
    }

    #[test]
    fn timeline_position_before_start_clamps_to_zero() {
        let got = timeline(10_000_000, 20_000_000, 2_000_000, 116_444_736_000_000_000);
        assert_eq!(got.0, Some(0));
        assert_eq!(got.1, Some(1_000));
    }

    #[test]
    fn timeline_position_past_end_clamps_to_duration() {
        let got = timeline(0, 30_000_000, 90_000_000, 116_444_736_000_000_000);
        assert_eq!(got.0, Some(3_000));
        assert_eq!(got.1, Some(3_000));
    }

    #[test]
    fn timeline_epoch_converts_to_zero() {
        let got = timeline(0, 10_000_000, 0, 116_444_736_000_000_000);
        assert_eq!(got.2, Some(0));
    }

    #[test]
    fn timeline_universal_zero_gives_no_updated_at() {
        let got = timeline(0, 10_000_000, 5_000_000, 0);
        assert_eq!(got, (Some(500), Some(1_000), None));
        assert_eq!(timeline(0, 10_000_000, 5_000_000, -1).2, None);
    }

    #[test]
    fn artwork_data_url_encodes_and_handles_empty() {
        assert_eq!(
            artwork_data_url("image/png", &[1, 2, 3]),
            Some("data:image/png;base64,AQID".to_string())
        );
        assert_eq!(artwork_data_url("image/png", &[]), None);
    }

    #[test]
    fn metadata_strings_are_capped_at_300_chars() {
        let state = MediaState {
            title: Some("é".repeat(400)),
            ..MediaState::default()
        }
        .cleaned();
        assert_eq!(state.title.unwrap().chars().count(), 300);
    }

    #[test]
    fn pick_mime_cases() {
        assert_eq!(pick_mime("text/html"), "image/png");
        assert_eq!(pick_mime(" IMAGE/JPEG "), "image/jpeg");
        assert_eq!(pick_mime(""), "image/png");
    }

    #[test]
    fn accept_size_cases() {
        assert!(!accept_size(0));
        assert!(accept_size(MAX_ARTWORK_BYTES));
        assert!(!accept_size(MAX_ARTWORK_BYTES + 1));
    }

    #[test]
    fn wait_times_out_on_never_completing_op() {
        let op = IAsyncOperation::<bool>::spawn(|| {
            std::thread::sleep(Duration::from_secs(5));
            Ok(true)
        });
        let start = std::time::Instant::now();
        assert_eq!(wait(&op, Duration::from_millis(200)), None);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn wait_returns_ready_value() {
        let op = IAsyncOperation::<bool>::ready(Ok(true));
        assert_eq!(wait(&op, Duration::from_millis(200)), Some(true));
    }

    fn track(title: &str) -> TrackKey {
        TrackKey {
            app: Some("Spotify.exe".into()),
            title: Some(title.into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
        }
    }

    #[test]
    fn artwork_cache_same_key_hit_reuses() {
        let mut cache = ArtworkCache::default();
        cache.store(track("Song"), Some("data:image/png;base64,AQID".into()));
        assert_eq!(
            cache.lookup(&track("Song")),
            Some("data:image/png;base64,AQID".to_string())
        );
    }

    #[test]
    fn artwork_cache_different_title_misses() {
        let mut cache = ArtworkCache::default();
        cache.store(track("Song"), Some("data:image/png;base64,AQID".into()));
        assert_eq!(cache.lookup(&track("Other")), None);
    }

    #[test]
    fn artwork_cache_any_key_field_change_misses() {
        let mut cache = ArtworkCache::default();
        cache.store(track("Song"), Some("data:image/png;base64,AQID".into()));
        let other_app = TrackKey {
            app: Some("Other.exe".into()),
            ..track("Song")
        };
        let other_album = TrackKey {
            album: None,
            ..track("Song")
        };
        assert_eq!(cache.lookup(&other_app), None);
        assert_eq!(cache.lookup(&other_album), None);
    }

    #[test]
    fn artwork_cache_cached_none_re_reads() {
        let mut cache = ArtworkCache::default();
        cache.store(track("Song"), None);
        assert_eq!(cache.lookup(&track("Song")), None);
        cache.store(track("Song"), Some("data:image/png;base64,AQID".into()));
        assert_eq!(
            cache.lookup(&track("Song")),
            Some("data:image/png;base64,AQID".to_string())
        );
    }

    #[test]
    fn artwork_cache_cleared_on_session_change() {
        let mut cache = ArtworkCache::default();
        cache.store(track("Song"), Some("data:image/png;base64,AQID".into()));
        cache.clear();
        assert_eq!(cache.lookup(&track("Song")), None);
    }

    #[test]
    fn should_emit_only_on_change() {
        let base = MediaState::default();
        assert!(should_emit(&None, &base));
        assert!(!should_emit(&Some(base.clone()), &base));
        let playing = MediaState {
            playing: true,
            ..base.clone()
        };
        assert!(should_emit(&Some(base), &playing));
    }
}
