#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod player;
mod innertube;
mod remote_config;
mod updates;
mod jsc;
mod youtube;
mod ytlogin;

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel, Weak};
use symphonia::core::io::MediaSource;

use player::{Event, Player};
use youtube::TrackInfo;

slint::include_modules!();

pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;


#[derive(Default)]
struct State {
    playlist_ids: Vec<String>,
    /// Tracks of the playlist being viewed.
    view: Vec<TrackInfo>,
    view_playlist: Option<usize>,
    /// Tracks of the playlist being played (stays put while browsing others).
    queue: Vec<TrackInfo>,
    queue_playlist: Option<usize>,
    current: Option<usize>,
    playing: bool,
    /// In shuffle mode, the next track is picked early so it can be prefetched.
    planned_next: Option<usize>,
}

/// The next track's audio, downloaded while the current one plays (one track, ~4 MB).
#[derive(Default)]
struct Prefetch {
    video_id: Option<String>,
    stream: Option<youtube::AudioStream>,
    in_flight: bool,
}

struct App {
    ui: Weak<AppWindow>,
    state: Mutex<State>,
    player: Player,
    prefetch: Mutex<Prefetch>,
    shuffle: AtomicBool,
    /// Bumped on every track/playlist load so stale background results are dropped.
    load_gen: AtomicU64,
    view_gen: AtomicU64,
}

/// Run `f` on the UI thread.
fn on_ui(ui: &Weak<AppWindow>, f: impl FnOnce(&AppWindow) + Send + 'static) {
    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            f(&ui);
        }
    });
}

fn decode_image(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height()))
}

impl App {
    fn status(&self, msg: impl Into<String>) {
        let msg = msg.into();
        on_ui(&self.ui, move |ui| ui.set_status(msg.into()));
    }

    fn startup(self: &Arc<Self>) {
        let app = self.clone();
        std::thread::spawn(move || {
            auth::remove_legacy_login();
            // Have YouTube's current player ready before the first play.
            std::thread::spawn(|| {
                remote_config::refresh_if_stale();
                updates::check_if_due();
                innertube::prepare();
            });
            // REPRODUCTOR_PREVIEW_SIGNIN=1 shows the sign-in screen without signing out
            // (for working on its design).
            let preview_sign_in = std::env::var_os("REPRODUCTOR_PREVIEW_SIGNIN").is_some();
            if youtube::cookies_path().is_some() && !preview_sign_in {
                app.status("Loading your library…");
                app.load_library();
            }
        });
    }

    /// The only sign-in: YouTube Music in its own window (see `ytlogin`).
    fn sign_in(self: &Arc<Self>) {
        on_ui(&self.ui, |ui| {
            ui.set_busy(true);
            ui.set_status("Sign in to YouTube Music in the window that opened…".into());
        });
        let app = self.clone();
        std::thread::spawn(move || {
            if youtube::reconnect_session() {
                app.load_library();
            } else {
                on_ui(&app.ui, |ui| {
                    ui.set_busy(false);
                    ui.set_status("Sign-in was cancelled or didn't finish.".into());
                });
            }
        });
    }

    /// Load account and playlists with the current session. Runs on a worker thread.
    fn load_library(self: &Arc<Self>) {
        let loaded = youtube::user_info().and_then(|u| Ok((u, youtube::playlists()?)));
        let (user, playlists) = match loaded {
            Ok(v) => v,
            Err(e) => {
                let msg = if e.to_string().contains("LOGIN_REQUIRED") {
                    "Your YouTube Music session has ended. Sign in again.".to_string()
                } else {
                    format!("Couldn't load your library: {e}")
                };
                on_ui(&self.ui, move |ui| {
                    ui.set_busy(false);
                    ui.set_status(msg.into());
                });
                return;
            }
        };
        self.state.lock().unwrap().playlist_ids = playlists.iter().map(|p| p.id.clone()).collect();
        let items: Vec<PlaylistItem> =
            playlists.iter().map(|p| PlaylistItem { title: p.title.as_str().into(), count: p.count }).collect();
        let name = user.name.clone();
        let app = self.clone();
        on_ui(&self.ui, move |ui| {
            let has_playlists = !items.is_empty();
            ui.set_busy(false);
            ui.set_status("".into());
            ui.set_user_name(name.into());
            ui.set_playlists(ModelRc::new(VecModel::from(items)));
            ui.set_signed_in(true);
            if has_playlists {
                ui.set_selected_playlist(0);
                app.select_playlist(0);
            }
        });
        if let Some(px) = user.picture.and_then(|url| youtube::fetch_image(&url).ok()).and_then(|b| decode_image(&b)) {
            on_ui(&self.ui, move |ui| ui.set_avatar(Image::from_rgba8(px)));
        }
    }

    fn sign_out(self: &Arc<Self>) {
        self.player.stop();
        self.load_gen.fetch_add(1, Ordering::SeqCst);
        *self.state.lock().unwrap() = State::default();
        std::thread::spawn(youtube::clear_session);
        on_ui(&self.ui, |ui| {
            ui.set_signed_in(false);
            ui.set_playing(false);
            ui.set_current_track(-1);
            ui.set_selected_playlist(-1);
            ui.set_tracks(ModelRc::default());
            ui.set_playlists(ModelRc::default());
            ui.set_now_title("".into());
            ui.set_now_artist("".into());
            ui.set_cover(Image::default());
            ui.set_avatar(Image::default());
            ui.set_status("Signed out.".into());
        });
    }

    fn select_playlist(self: &Arc<Self>, index: usize) {
        let Some(id) = self.state.lock().unwrap().playlist_ids.get(index).cloned() else { return };
        let generation = self.view_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.status("Loading playlist…");
        let app = self.clone();
        std::thread::spawn(move || {
            let tracks = match youtube::playlist_tracks(&id) {
                Ok(t) => t,
                Err(e) => return app.status(format!("Couldn't load playlist: {e}")),
            };
            if app.view_gen.load(Ordering::SeqCst) != generation {
                return;
            }
            let (items, highlight) = {
                let mut st = app.state.lock().unwrap();
                st.view = tracks;
                st.view_playlist = Some(index);
                // Resolve the list in the background (one light request per track,
                // top first) so any click starts right away.
                let ids: Vec<&str> = st.view.iter().take(remote_config::get().warm_playlist_limit).map(|t| t.video_id.as_str()).collect();
                youtube::warm(&ids, true);
                let highlight = if st.queue_playlist == Some(index) { st.current } else { None };
                (st.view.iter().map(|t| (t.title.clone(), t.artist.clone())).collect::<Vec<_>>(), highlight)
            };
            on_ui(&app.ui, move |ui| {
                let items: Vec<TrackItem> =
                    items.into_iter().map(|(t, a)| TrackItem { title: t.into(), artist: a.into() }).collect();
                ui.set_tracks(ModelRc::new(VecModel::from(items)));
                ui.set_current_track(highlight.map_or(-1, |i| i as i32));
                ui.set_status("".into());
            });
            // REPRODUCTOR_PREVIEW_PLAYING=1 starts the first track muted (for
            // working on the design of the playing state).
            if std::env::var_os("REPRODUCTOR_PREVIEW_PLAYING").is_some() && app.state.lock().unwrap().current.is_none() {
                app.player.set_volume(0.0);
                on_ui(&app.ui, |ui| ui.set_volume(0.0));
                app.play_from_view(0);
            }
        });
    }

    /// Play `index` of the viewed playlist (replacing the queue with it).
    fn play_from_view(self: &Arc<Self>, index: usize) {
        {
            let mut st = self.state.lock().unwrap();
            st.queue = st.view.clone();
            st.queue_playlist = st.view_playlist;
        }
        self.play_queue(index);
    }

    fn play_queue(self: &Arc<Self>, index: usize) {
        let (track, highlight) = {
            let mut st = self.state.lock().unwrap();
            let Some(track) = st.queue.get(index).cloned() else { return };
            st.current = Some(index);
            st.playing = false;
            (track, st.view_playlist == st.queue_playlist)
        };
        let generation = self.load_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.player.stop();

        let (title, artist) = (track.title.clone(), track.artist.clone());
        on_ui(&self.ui, move |ui| {
            ui.set_current_track(if highlight { index as i32 } else { -1 });
            ui.set_now_title(title.into());
            ui.set_now_artist(artist.into());
            ui.set_loading(true);
            ui.set_playing(false);
            ui.set_position(0.0);
            ui.set_duration(0.0);
            ui.set_cover(Image::default());
        });

        let app = self.clone();
        let video_id = track.video_id.clone();
        std::thread::spawn(move || {
            let result: Res<Box<dyn MediaSource>> = match app.take_prefetched(&track.video_id) {
                Some(stream) => Ok(Box::new(Cursor::new(stream.bytes))),
                None => youtube::open_audio(&track.video_id, true),
            };
            if app.load_gen.load(Ordering::SeqCst) != generation {
                return;
            }
            match result {
                Ok(source) => {
                    app.player.load(source);
                    app.state.lock().unwrap().playing = true;
                    on_ui(&app.ui, move |ui| {
                        ui.set_loading(false);
                        ui.set_playing(true);
                        ui.set_status("".into());
                    });
                    app.prefetch_after(index);
                }
                Err(e) => {
                    let msg = format!("Couldn't play \"{}\": {e}", track.title);
                    on_ui(&app.ui, move |ui| {
                        ui.set_loading(false);
                        ui.set_status(msg.into());
                    });
                }
            }
        });

        let app = self.clone();
        std::thread::spawn(move || {
            let px = youtube::fetch_image(&youtube::thumbnail_url(&video_id)).ok().and_then(|b| decode_image(&b));
            if let Some(px) = px {
                if app.load_gen.load(Ordering::SeqCst) == generation {
                    on_ui(&app.ui, move |ui| ui.set_cover(Image::from_rgba8(px)));
                }
            }
        });
    }

    /// Takes the prefetched stream for `video_id`, waiting if its download is still running.
    fn take_prefetched(&self, video_id: &str) -> Option<youtube::AudioStream> {
        loop {
            {
                let mut pf = self.prefetch.lock().unwrap();
                if pf.video_id.as_deref() != Some(video_id) {
                    return None;
                }
                if let Some(stream) = pf.stream.take() {
                    pf.video_id = None;
                    return Some(stream);
                }
                if !pf.in_flight {
                    return None;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Start downloading whichever track will play after `index`.
    fn prefetch_after(self: &Arc<Self>, index: usize) {
        let video_id = {
            let mut st = self.state.lock().unwrap();
            let len = st.queue.len();
            let next = if self.shuffle.load(Ordering::Relaxed) {
                let n = random_index(len, Some(index));
                st.planned_next = Some(n);
                Some(n)
            } else {
                st.planned_next = None;
                // Resolve further ahead too (URLs only, a few hundred bytes each).
                let ahead: Vec<&str> = st.queue.iter().skip(index + 2).take(6).map(|t| t.video_id.as_str()).collect();
                youtube::warm(&ahead, false);
                (index + 1 < len).then_some(index + 1)
            };
            match next.and_then(|n| st.queue.get(n)) {
                Some(t) => t.video_id.clone(),
                None => return,
            }
        };
        {
            let mut pf = self.prefetch.lock().unwrap();
            if pf.video_id.as_deref() == Some(video_id.as_str()) {
                return;
            }
            *pf = Prefetch { video_id: Some(video_id.clone()), stream: None, in_flight: true };
        }
        let app = self.clone();
        std::thread::spawn(move || {
            let result = youtube::fetch_audio(&video_id, false).ok();
            let mut pf = app.prefetch.lock().unwrap();
            if pf.video_id.as_deref() == Some(video_id.as_str()) {
                pf.in_flight = false;
                pf.stream = result;
                if pf.stream.is_none() {
                    pf.video_id = None;
                }
            }
        });
    }

    fn toggle_play(self: &Arc<Self>) {
        let (has_track, playing) = {
            let st = self.state.lock().unwrap();
            (st.current.is_some(), st.playing)
        };
        if !has_track {
            let len = self.state.lock().unwrap().view.len();
            if len > 0 {
                let start = if self.shuffle.load(Ordering::Relaxed) { random_index(len, None) } else { 0 };
                self.play_from_view(start);
            }
            return;
        }
        self.state.lock().unwrap().playing = !playing;
        self.player.set_paused(playing);
        on_ui(&self.ui, move |ui| ui.set_playing(!playing));
    }

    fn set_shuffle(self: &Arc<Self>, on: bool) {
        self.shuffle.store(on, Ordering::Relaxed);
        // Re-plan the prefetch for the new mode.
        let current = self.state.lock().unwrap().current;
        if let Some(i) = current {
            self.prefetch_after(i);
        }
    }

    fn next(self: &Arc<Self>) {
        let shuffle = self.shuffle.load(Ordering::Relaxed);
        let next = {
            let mut st = self.state.lock().unwrap();
            let len = st.queue.len();
            let planned = st.planned_next.take();
            match st.current {
                _ if len == 0 => None,
                cur if shuffle => Some(planned.filter(|&n| n < len).unwrap_or_else(|| random_index(len, cur))),
                Some(i) if i + 1 < len => Some(i + 1),
                Some(_) => None,
                None => Some(0),
            }
        };
        match next {
            Some(i) => self.play_queue(i),
            None => {
                self.state.lock().unwrap().playing = false;
                on_ui(&self.ui, |ui| ui.set_playing(false));
            }
        }
    }

    fn prev(self: &Arc<Self>) {
        let cur = self.state.lock().unwrap().current;
        match cur {
            Some(i) if self.player.position() < 3.0 && i > 0 => self.play_queue(i - 1),
            Some(_) => self.player.seek(0.0),
            None => {}
        }
    }
}

fn random_index(len: usize, avoid: Option<usize>) -> usize {
    if len <= 1 {
        return 0;
    }
    loop {
        let i = (rand::random::<u64>() % len as u64) as usize;
        if Some(i) != avoid {
            return i;
        }
    }
}

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == ytlogin::ARG) {
        ytlogin::run(args.iter().any(|a| a == ytlogin::ARG_SILENT));
    }

    let ui = AppWindow::new()?;
    let weak = ui.as_weak();

    let events_ui = weak.clone();
    let player = Player::new(move |event| {
        let msg = match event {
            Event::Finished => None,
            Event::Error(e) => Some(e),
        };
        on_ui(&events_ui, move |ui| match msg {
            None => ui.invoke_next(),
            Some(e) => ui.set_status(format!("Playback error: {e}").into()),
        });
    })?;

    let app = Arc::new(App {
        ui: weak.clone(),
        state: Mutex::new(State::default()),
        player,
        prefetch: Mutex::new(Prefetch::default()),
        shuffle: AtomicBool::new(false),
        load_gen: AtomicU64::new(0),
        view_gen: AtomicU64::new(0),
    });

    let a = app.clone();
    ui.on_sign_in(move || a.sign_in());
    let a = app.clone();
    ui.on_sign_out(move || a.sign_out());
    let a = app.clone();
    ui.on_select_playlist(move |i| a.select_playlist(i as usize));
    let a = app.clone();
    ui.on_play_track(move |i| a.play_from_view(i as usize));
    let a = app.clone();
    ui.on_toggle_play(move || a.toggle_play());
    let a = app.clone();
    ui.on_next(move || a.next());
    let a = app.clone();
    ui.on_shuffle_toggled(move |on| a.set_shuffle(on));
    let a = app.clone();
    ui.on_prev(move || a.prev());
    let (a, w) = (app.clone(), weak.clone());
    ui.on_seek(move |secs| {
        a.player.seek(secs as f64);
        if let Some(ui) = w.upgrade() {
            ui.set_position(secs);
        }
    });
    let a = app.clone();
    ui.on_set_volume(move |v| a.player.set_volume(v));
    app.player.set_volume(ui.get_volume());

    // Position ticker: cheap, 4 Hz, only while playing.
    let ticker = slint::Timer::default();
    let (a, w) = (app.clone(), weak.clone());
    ticker.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
        let Some(ui) = w.upgrade() else { return };
        if a.state.lock().unwrap().playing {
            ui.set_position(a.player.position() as f32);
            ui.set_duration(a.player.duration() as f32);
        }
        // Keep the maximize/restore icon right after Win+Up, snapping, etc.
        let maximized = ui.window().is_maximized();
        if ui.get_is_maximized() != maximized {
            ui.set_is_maximized(maximized);
        }
    });

    // Custom title bar (the window has no native frame).
    let w = weak.clone();
    ui.on_start_drag(move || {
        if let Some(ui) = w.upgrade() {
            // The system move loop: keeps Windows snap and drag-to-maximize.
            ui.window().with_winit_window(|win| {
                let _ = win.drag_window();
            });
        }
    });
    let w = weak.clone();
    ui.on_start_resize(move |edge| {
        use slint::winit_030::winit::window::ResizeDirection as D;
        let direction = match edge {
            0 => D::North,
            1 => D::South,
            2 => D::East,
            3 => D::West,
            4 => D::NorthEast,
            5 => D::NorthWest,
            6 => D::SouthEast,
            _ => D::SouthWest,
        };
        if let Some(ui) = w.upgrade() {
            ui.window().with_winit_window(|win| {
                let _ = win.drag_resize_window(direction);
            });
        }
    });
    let w = weak.clone();
    ui.on_minimize(move || {
        if let Some(ui) = w.upgrade() {
            ui.window().set_minimized(true);
        }
    });
    let w = weak.clone();
    ui.on_toggle_maximize(move || {
        if let Some(ui) = w.upgrade() {
            let maximized = !ui.window().is_maximized();
            ui.window().set_maximized(maximized);
            ui.set_is_maximized(maximized);
        }
    });
    ui.on_close_window(|| {
        let _ = slint::quit_event_loop();
    });

    app.startup();
    ui.show()?;
    // The native window only exists once the event loop runs, so style it from there.
    #[cfg(windows)]
    {
        let w = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = w.upgrade() else { return };
            ui.window().with_winit_window(|win| {
                use slint::winit_030::winit::platform::windows::{CornerPreference, WindowExtWindows};
                // Without the native frame, ask Windows 11 for its usual shadow and rounded corners.
                win.set_undecorated_shadow(true);
                win.set_corner_preference(CornerPreference::Round);
                // Slint sets only the small window icon; the taskbar uses the big one.
                let png = include_bytes!("../assets/icon-256.png");
                if let Ok(img) = image::load_from_memory(png) {
                    let img = img.to_rgba8();
                    let (w, h) = img.dimensions();
                    if let Ok(icon) = slint::winit_030::winit::window::Icon::from_rgba(img.into_raw(), w, h) {
                        win.set_taskbar_icon(Some(icon));
                    }
                }
            });
        });
    }
    slint::run_event_loop()?;
    ui.hide()?;
    Ok(())
}
