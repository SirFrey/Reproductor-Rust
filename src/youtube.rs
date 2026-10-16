//! YouTube Music: library (via the helper) and audio streams.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use symphonia::core::io::MediaSource;

use crate::Res;

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
});

pub struct UserInfo {
    pub name: String,
    pub picture: Option<String>,
}

pub struct PlaylistInfo {
    pub id: String,
    pub title: String,
    pub count: i32,
}

#[derive(Clone)]
pub struct TrackInfo {
    pub video_id: String,
    pub title: String,
    pub artist: String,
}

/// Run a library request. If the YouTube session has expired, refresh it
/// (showing the sign-in window if Google needs the user) and retry once.
fn library_request<T>(f: impl Fn() -> Res<T>) -> Res<T> {
    if cookies_path().is_none() {
        return Err("LOGIN_REQUIRED: not signed in".into());
    }
    match f() {
        Err(e) if is_session_error(&e.to_string()) && refresh_session(true) => f(),
        other => other,
    }
}

/// The signed-in YouTube Music account.
pub fn user_info() -> Res<UserInfo> {
    library_request(crate::innertube::account)
}

/// The user's YouTube Music library, with "Liked music" first.
pub fn playlists() -> Res<Vec<PlaylistInfo>> {
    library_request(crate::innertube::library)
}

pub fn playlist_tracks(playlist_id: &str) -> Res<Vec<TrackInfo>> {
    library_request(|| crate::innertube::playlist(playlist_id))
}

// --- Audio streams ---
// Resolution (video id -> stream URL) goes through InnerTube with the embedded
// challenge solver (`innertube`, ~0.5-1 s), with yt-dlp as the fallback (~6 s).
// Results are cached and can be warmed in the background; downloads stream
// straight to the decoder.

const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
/// Stream URLs expire after ~6 h.
const URL_TTL: Duration = Duration::from_secs(5 * 3600);
const MAX_AUDIO_BYTES: u64 = 200 << 20;

pub struct AudioStream {
    pub bytes: Vec<u8>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ResolvedStream {
    url: String,
    /// 0 when unknown.
    len: u64,
    user_agent: String,
    /// Unix seconds when resolved.
    at: u64,
    /// Serves only the first MiB, but right away (see `resolver.js`).
    #[serde(default)]
    head_url: Option<String>,
    #[serde(default)]
    head_user_agent: Option<String>,
}

impl ResolvedStream {
    fn fresh(&self) -> bool {
        unix_now().saturating_sub(self.at) < URL_TTL.as_secs()
    }

    /// The head URL is only worth using while the main URL may still be inactive.
    fn head(&self) -> Option<(&str, &str)> {
        let recent = unix_now().saturating_sub(self.at) < HEAD_USEFUL_SECS;
        match (&self.head_url, &self.head_user_agent) {
            (Some(u), Some(ua)) if recent => Some((u, ua)),
            _ => None,
        }
    }
}

/// A fresh main URL can take >10 s to become valid.
const HEAD_USEFUL_SECS: u64 = 20;

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

enum Slot {
    Pending,
    Ready(ResolvedStream),
}

/// Resolved stream URLs, persisted so tracks played earlier today start instantly
/// after a restart. (Signed for this IP and short-lived; not credentials.)
static RESOLVED: LazyLock<(Mutex<HashMap<String, Slot>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(load_stream_cache()), Condvar::new()));

fn stream_cache_file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("reproductor-rs").join("streams.json"))
}

fn load_stream_cache() -> HashMap<String, Slot> {
    let Some(text) = stream_cache_file().and_then(|p| std::fs::read_to_string(p).ok()) else { return HashMap::new() };
    let entries: HashMap<String, ResolvedStream> = serde_json::from_str(&text).unwrap_or_default();
    entries.into_iter().filter(|(_, r)| r.fresh()).map(|(k, r)| (k, Slot::Ready(r))).collect()
}

fn save_stream_cache(map: &HashMap<String, Slot>) {
    let entries: HashMap<&String, &ResolvedStream> = map
        .iter()
        .filter_map(|(k, s)| match s {
            Slot::Ready(r) if r.fresh() => Some((k, r)),
            _ => None,
        })
        .collect();
    if let (Some(path), Ok(json)) = (stream_cache_file(), serde_json::to_string(&entries)) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let staged = path.with_extension("json.new");
        if std::fs::write(&staged, json).is_ok() {
            let _ = std::fs::rename(staged, path);
        }
    }
}

static COOKIE_WRITE: Mutex<()> = Mutex::new(());
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Fully downloaded audio (for prefetching the next track). `interactive` allows
/// showing the YouTube sign-in window if the session has expired.
pub fn fetch_audio(video_id: &str, interactive: bool) -> Res<AudioStream> {
    let dl = start_download(video_id, interactive)?;
    Ok(AudioStream { bytes: dl.wait_all()? })
}

/// Audio the decoder can start reading as soon as the first piece has arrived.
pub fn open_audio(video_id: &str, interactive: bool) -> Res<Box<dyn MediaSource>> {
    let dl = start_download(video_id, interactive)?;
    Ok(Box::new(StreamingSource { dl, pos: 0 }))
}

/// Resolve and start downloading. The first piece is awaited here so a stale URL
/// is caught (and resolved again, once) rather than failing mid-playback.
fn start_download(video_id: &str, interactive: bool) -> Res<Arc<Download>> {
    let first = resolve(video_id, interactive).and_then(|r| Download::start(&r));
    match first {
        Ok(dl) => Ok(dl),
        Err(_) => {
            forget(video_id);
            Download::start(&resolve(video_id, interactive)?)
        }
    }
}

/// Background resolution queue: one speculative lookup at a time, so warming
/// never competes much with a track the user actually asked for.
static WARM_QUEUE: LazyLock<(Mutex<std::collections::VecDeque<String>>, Condvar)> = LazyLock::new(|| {
    std::thread::Builder::new()
        .name("stream-warm".into())
        .spawn(|| loop {
            let id = {
                let (lock, cv) = &*WARM_QUEUE;
                let mut q = lock.lock().unwrap();
                loop {
                    match q.pop_front() {
                        Some(id) => break id,
                        None => q = cv.wait(q).unwrap(),
                    }
                }
            };
            let _ = resolve_with(&id, false, false);
        })
        .expect("spawn warm thread");
    (Mutex::new(std::collections::VecDeque::new()), Condvar::new())
});

/// Queue tracks to be resolved in the background so a later `fetch_audio` is quick.
/// `replace` drops whatever was queued before (e.g. when another playlist opens).
pub fn warm(video_ids: &[&str], replace: bool) {
    let known = |id: &str| matches!(RESOLVED.0.lock().unwrap().get(id), Some(Slot::Pending) | Some(Slot::Ready(_)));
    let (lock, cv) = &*WARM_QUEUE;
    let mut q = lock.lock().unwrap();
    if replace {
        q.clear();
    }
    for id in video_ids {
        if !known(id) && !q.iter().any(|q| q == id) {
            q.push_back(id.to_string());
        }
    }
    cv.notify_one();
}

fn forget(video_id: &str) {
    let mut map = RESOLVED.0.lock().unwrap();
    map.remove(video_id);
    save_stream_cache(&map);
}

/// Cached resolution; concurrent callers for the same id share one lookup.
fn resolve(video_id: &str, interactive: bool) -> Res<ResolvedStream> {
    resolve_with(video_id, interactive, true)
}

/// `allow_slow`: fall back to yt-dlp if the fast path fails (not for speculative warming).
fn resolve_with(video_id: &str, interactive: bool, allow_slow: bool) -> Res<ResolvedStream> {
    let (lock, cv) = &*RESOLVED;
    let mut map = lock.lock().unwrap();
    loop {
        match map.get(video_id) {
            Some(Slot::Pending) => map = cv.wait(map).unwrap(),
            Some(Slot::Ready(r)) if r.fresh() => return Ok(r.clone()),
            _ => break,
        }
    }
    map.retain(|_, s| !matches!(s, Slot::Ready(r) if !r.fresh()));
    map.insert(video_id.to_string(), Slot::Pending);
    drop(map);

    let result = resolve_uncached(video_id, interactive, allow_slow);

    let mut map = lock.lock().unwrap();
    match &result {
        Ok(r) => {
            map.insert(video_id.to_string(), Slot::Ready(r.clone()));
            save_stream_cache(&map);
        }
        Err(_) => {
            map.remove(video_id);
        }
    }
    cv.notify_all();
    result
}

fn resolve_uncached(video_id: &str, interactive: bool, allow_slow: bool) -> Res<ResolvedStream> {
    let fast = || {
        // Warming (`!allow_slow`) skips the quick-start URL: it would be stale by playback time.
        crate::innertube::resolve(video_id, allow_slow).map(|r| ResolvedStream {
            url: r.url,
            len: r.len,
            user_agent: r.user_agent,
            at: unix_now(),
            head_url: r.head_url,
            head_user_agent: r.head_user_agent,
        })
    };
    let mut fast_err = None;
    // While a new YouTube player is being prepared (~10 s, every few days), a
    // track the user is waiting for goes through yt-dlp instead.
    let hurry = allow_slow && crate::innertube::preparing();
    if crate::innertube::available() && !hurry {
        match fast() {
            Ok(r) => return Ok(r),
            Err(e) if is_session_error(&e.to_string()) && refresh_session(interactive) => {
                if let Ok(r) = fast() {
                    return Ok(r);
                }
            }
            Err(e) => fast_err = Some(e.to_string()),
        }
    }
    if !allow_slow {
        return Err(fast_err.unwrap_or_else(|| "fast path unavailable".into()).into());
    }
    match resolve_ytdlp(video_id) {
        Err(e) if is_session_error(&e.to_string()) && refresh_session(interactive) => resolve_ytdlp(video_id),
        Err(e) => Err(match fast_err {
            Some(f) => format!("{e} (fast path: {f})").into(),
            None => e,
        }),
        ok => ok,
    }
}

/// Errors that a fresh YouTube session fixes.
fn is_session_error(msg: &str) -> bool {
    ["confirm you", "Sign in", "cookies", "LOGIN_REQUIRED", "logged out"].iter().any(|k| msg.contains(k))
}

/// Last refresh attempt, and whether it settles things for interactive callers too
/// (it succeeded, or the user already saw the window).
static SESSION_REFRESH: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
/// Don't relaunch the login process more often than this.
const SESSION_REFRESH_COOLDOWN: Duration = Duration::from_secs(60);

/// Re-export YouTube cookies via the login process (see `ytlogin`). Silent unless
/// `interactive`, in which case the sign-in window appears if Google needs it.
/// Concurrent callers wait for one refresh. Returns whether cookies are now fresh.
pub fn refresh_session(interactive: bool) -> bool {
    let mut last = SESSION_REFRESH.lock().unwrap();
    if let Some((at, settled)) = *last {
        if at.elapsed() < SESSION_REFRESH_COOLDOWN && (settled || !interactive) {
            return cookies_path().is_some();
        }
    }
    let ok = run_login_process(interactive);
    *last = Some((Instant::now(), ok || interactive));
    ok
}

/// Launch the login window on request, ignoring the cooldown.
pub fn reconnect_session() -> bool {
    let mut last = SESSION_REFRESH.lock().unwrap();
    let ok = run_login_process(true);
    *last = Some((Instant::now(), true));
    ok
}

fn run_login_process(interactive: bool) -> bool {
    // A silent refresh needs the login window's saved profile.
    if !interactive && !crate::ytlogin::profile_dir().is_some_and(|d| d.exists()) {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(crate::ytlogin::ARG);
    if !interactive {
        cmd.arg(crate::ytlogin::ARG_SILENT);
    }
    cmd.status().is_ok_and(|s| s.code() == Some(crate::ytlogin::EXIT_OK))
}

/// Forget the YouTube session entirely (cookies file and the login window's profile).
pub fn clear_session() {
    if let Some(p) = cookies_file() {
        let _ = std::fs::remove_file(p);
    }
    if let Some(dir) = crate::ytlogin::profile_dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
    let mut map = RESOLVED.0.lock().unwrap();
    map.clear();
    save_stream_cache(&map);
}

/// Prefer the unpacked ("onedir") yt-dlp build in the app's folder: it starts in
/// ~0.8 s vs ~1.8 s for the single-file exe, which unpacks itself on every run.
/// Then winget's copy, then PATH (apps started from Explorer may predate the PATH change).
fn ytdlp_path() -> PathBuf {
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let local = std::path::Path::new(&local);
        for p in [local.join("reproductor-rs/yt-dlp/yt-dlp.exe"), local.join("Microsoft/WinGet/Links/yt-dlp.exe")] {
            if p.exists() {
                return p;
            }
        }
    }
    "yt-dlp".into()
}

/// Where the YouTube session cookies (Netscape cookies.txt) live.
pub fn cookies_file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("APPDATA").ok()?).join("reproductor-rs").join("cookies.txt"))
}

/// The cookies file, if a session has been captured.
pub fn cookies_path() -> Option<PathBuf> {
    cookies_file().filter(|p| p.exists())
}

/// Ask yt-dlp for the stream URL only (no download). Each run gets a private copy
/// of the cookie jar; the refreshed jar is written back afterwards so YouTube's
/// cookie rotation is kept and parallel runs never write the same file.
fn resolve_ytdlp(video_id: &str) -> Res<ResolvedStream> {
    let mut cmd = std::process::Command::new(ytdlp_path());
    // Best AAC track; yt-dlp's default ordering prefers the original language on dubbed videos.
    cmd.args(["-f", "ba[ext=m4a]/ba[acodec^=mp4a]", "--no-playlist", "--no-warnings"])
        .args(["-O", "%(filesize,filesize_approx|0)s\t%(http_headers.User-Agent)s\t%(url)s"]);
    let jar = cookies_path().map(|real| {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("reproductor-cookies-{}-{n}.txt", std::process::id()));
        let _ = std::fs::copy(&real, &tmp);
        (real, tmp)
    });
    if let Some((_, tmp)) = &jar {
        cmd.arg("--cookies").arg(tmp);
    }
    cmd.arg(format!("https://www.youtube.com/watch?v={video_id}"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    // An update swaps yt-dlp's folder; never while it runs.
    let out = {
        let _running = crate::updates::YTDLP_IN_USE.read().unwrap();
        cmd.output()
    };

    if let Some((real, tmp)) = jar {
        if out.as_ref().is_ok_and(|o| o.status.success()) {
            let _guard = COOKIE_WRITE.lock().unwrap();
            let staged = real.with_extension("txt.new");
            if std::fs::copy(&tmp, &staged).is_ok() {
                let _ = std::fs::rename(&staged, &real);
            }
        }
        let _ = std::fs::remove_file(tmp);
    }

    let out = out.map_err(|e| format!("yt-dlp not available: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().find(|l| l.starts_with("ERROR")).unwrap_or(err.trim());
        return Err(format!("yt-dlp: {}", line.split(". Use --cookies").next().unwrap_or(line)).into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.trim().splitn(3, '\t');
    let len = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let user_agent = parts.next().filter(|ua| *ua != "NA" && !ua.is_empty()).unwrap_or(BROWSER_USER_AGENT).to_string();
    let url = parts.next().filter(|u| u.starts_with("http")).ok_or("yt-dlp returned no URL")?.to_string();
    Ok(ResolvedStream { url, len, user_agent, at: unix_now(), head_url: None, head_user_agent: None })
}

// --- Download ---

static AGENT_V4: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .ip_family(ureq::config::IpFamily::Ipv4Only)
        .build()
        .into()
});

/// The file comes down in 1 MiB `range=` pieces (large ranges get rejected and
/// unranged downloads throttled), a few in parallel, in order, so playback can
/// start once the first piece is in.
const CHUNK: u64 = 1 << 20;
const PARALLEL_CHUNKS: usize = 4;
/// A freshly signed URL answers 403 until it becomes valid (a few seconds,
/// sometimes >10 s), so each piece is retried at a steady pace.
fn range_attempts() -> u32 {
    crate::remote_config::get().range_attempts
}
const RANGE_RETRY_PAUSE: Duration = Duration::from_millis(300);

/// Stream URLs are bound to the IP that resolved them (`ip=` parameter), so the
/// download must use the same address family or googlevideo answers 403.
fn agent_for(url: &str) -> &'static ureq::Agent {
    let ipv4 = url::Url::parse(url)
        .ok()
        .and_then(|u| u.query_pairs().find(|(k, _)| k == "ip").map(|(_, v)| !v.contains(':')))
        .unwrap_or(false);
    if ipv4 { &AGENT_V4 } else { &AGENT }
}

fn fetch_range(url: &str, user_agent: &str, start: u64, end: u64, attempts: u32) -> Res<Vec<u8>> {
    let agent = agent_for(url);
    let mut attempt = 0;
    loop {
        let result = agent
            .get(format!("{url}&range={start}-{end}"))
            .header("User-Agent", user_agent)
            .call()
            .and_then(|mut r| r.body_mut().with_config().limit(CHUNK + 1).read_to_vec());
        match result {
            Ok(data) => return Ok(data),
            Err(_) if attempt + 1 < attempts => {
                attempt += 1;
                std::thread::sleep(RANGE_RETRY_PAUSE);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

struct DownloadState {
    bytes: Vec<u8>,
    ready: Vec<bool>,
    error: Option<String>,
}

/// An in-progress download that readers can consume piece by piece.
struct Download {
    len: u64,
    state: Mutex<DownloadState>,
    cv: Condvar,
}

impl Download {
    fn complete(bytes: Vec<u8>) -> Arc<Self> {
        let chunks = (bytes.len() as u64).div_ceil(CHUNK) as usize;
        Arc::new(Self {
            len: bytes.len() as u64,
            state: Mutex::new(DownloadState { bytes, ready: vec![true; chunks], error: None }),
            cv: Condvar::new(),
        })
    }

    /// Start the workers and wait for the first piece.
    fn start(s: &ResolvedStream) -> Res<Arc<Self>> {
        if s.len == 0 {
            // Unknown size: sequential pieces until a short one.
            let mut bytes = Vec::new();
            loop {
                let start = bytes.len() as u64;
                let chunk = fetch_range(&s.url, &s.user_agent, start, start + CHUNK - 1, range_attempts())?;
                let done = (chunk.len() as u64) < CHUNK;
                bytes.extend_from_slice(&chunk);
                if done || bytes.len() as u64 >= MAX_AUDIO_BYTES {
                    break;
                }
            }
            return if bytes.is_empty() { Err("Empty stream".into()) } else { Ok(Self::complete(bytes)) };
        }
        if s.len > MAX_AUDIO_BYTES {
            return Err("Track too large".into());
        }

        let chunks = s.len.div_ceil(CHUNK) as usize;
        let dl = Arc::new(Self {
            len: s.len,
            state: Mutex::new(DownloadState { bytes: vec![0; s.len as usize], ready: vec![false; chunks], error: None }),
            cv: Condvar::new(),
        });
        let next = Arc::new(AtomicUsize::new(0));
        for _ in 0..PARALLEL_CHUNKS.min(chunks) {
            let (dl, next, s) = (dl.clone(), next.clone(), s.clone());
            std::thread::spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= chunks || dl.state.lock().unwrap().error.is_some() {
                    return;
                }
                let start = i as u64 * CHUNK;
                let end = (start + CHUNK).min(dl.len) - 1;
                // The first piece can come from the instantly valid head URL while
                // the main URL is still activating; everything else waits for it.
                let head = if i == 0 { s.head() } else { None };
                let result = match head {
                    Some((hu, hua)) => fetch_range(hu, hua, start, end, 1)
                        .or_else(|_| fetch_range(&s.url, &s.user_agent, start, end, range_attempts())),
                    None => fetch_range(&s.url, &s.user_agent, start, end, range_attempts()),
                };
                match result {
                    Ok(data) if data.len() as u64 == end - start + 1 => {
                        let mut st = dl.state.lock().unwrap();
                        st.bytes[start as usize..=end as usize].copy_from_slice(&data);
                        st.ready[i] = true;
                    }
                    Ok(_) => dl.fail("Stream ended early"),
                    Err(e) => dl.fail(&e.to_string()),
                }
                dl.cv.notify_all();
            });
        }
        dl.wait_chunk(0)?;
        Ok(dl)
    }

    fn fail(&self, msg: &str) {
        let mut st = self.state.lock().unwrap();
        if st.error.is_none() {
            st.error = Some(msg.to_string());
        }
    }

    fn wait_chunk(&self, i: usize) -> Res<()> {
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some(e) = &st.error {
                return Err(e.clone().into());
            }
            if i >= st.ready.len() || st.ready[i] {
                return Ok(());
            }
            st = self.cv.wait(st).unwrap();
        }
    }

    fn wait_all(&self) -> Res<Vec<u8>> {
        let chunks = self.len.div_ceil(CHUNK) as usize;
        for i in 0..chunks {
            self.wait_chunk(i)?;
        }
        Ok(std::mem::take(&mut self.state.lock().unwrap().bytes))
    }
}

/// Reader over a `Download` that blocks until the piece it needs has arrived.
struct StreamingSource {
    dl: Arc<Download>,
    pos: u64,
}

impl Read for StreamingSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.dl.len || buf.is_empty() {
            return Ok(0);
        }
        let i = (self.pos / CHUNK) as usize;
        self.dl.wait_chunk(i).map_err(|e| io::Error::other(e.to_string()))?;
        let chunk_end = ((i as u64 + 1) * CHUNK).min(self.dl.len);
        let n = (chunk_end - self.pos).min(buf.len() as u64) as usize;
        let st = self.dl.state.lock().unwrap();
        buf[..n].copy_from_slice(&st.bytes[self.pos as usize..self.pos as usize + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StreamingSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(d) => self.dl.len as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = (target as u64).min(self.dl.len);
        Ok(self.pos)
    }
}

impl MediaSource for StreamingSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.dl.len)
    }
}

/// Raw bytes of a small image (cover art or avatar).
pub fn fetch_image(url: &str) -> Res<Vec<u8>> {
    Ok(AGENT.get(url).call()?.body_mut().read_to_vec()?)
}

pub fn thumbnail_url(video_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_path_resolves_quickly() {
        assert!(crate::innertube::available(), "solver scripts and cookies must be present");
        let t = Instant::now();
        let r = crate::innertube::resolve("9bZkp7q19f0", true).expect("fast resolve");
        let first = t.elapsed();
        let t = Instant::now();
        crate::innertube::resolve("kJQP7kiw5Fk", true).expect("second fast resolve");
        let second = t.elapsed();
        println!("fast path: first {first:?}, second {second:?}");
        assert!(r.url.starts_with("https://") && r.len > 0);
        assert!(second < Duration::from_secs(2), "warm resolve took {second:?}");
    }

    #[test]
    fn cached_fetch_is_fast() {
        let id = "9bZkp7q19f0";
        forget(id);
        let t = Instant::now();
        let a = fetch_audio(id, false).expect("cold fetch");
        let cold = t.elapsed();
        let t = Instant::now();
        let b = fetch_audio(id, false).expect("warm fetch");
        let warm = t.elapsed();
        println!("cold {cold:?} ({} bytes), warm {warm:?}", a.bytes.len());
        assert_eq!(a.bytes.len(), b.bytes.len());
        assert!(warm < cold);
    }
}

#[cfg(test)]
mod start_latency {
    use super::*;

    /// Time until the decoder can start: resolve + first piece, on tracks not cached yet.
    #[test]
    fn time_to_first_audio() {
        for id in ["OPf0YbXqDm0", "JGwWNGJdvx8", "YQfdzbUjES4"] {
            forget(id);
            let t = Instant::now();
            let mut src = open_audio(id, false).expect("open");
            let ready = t.elapsed();
            let mut head = vec![0u8; 4096];
            src.read_exact(&mut head).expect("read");
            println!("{id}: first audio after {ready:?}");
            assert!(ready < Duration::from_secs(4), "{id} took {ready:?}");
        }
    }
}

#[cfg(test)]
mod library {
    use super::*;

    #[test]
    fn account_library_and_playlist() {
        let t = Instant::now();
        let user = user_info().expect("account");
        let lists = playlists().expect("library");
        let liked = playlist_tracks("LM").expect("liked music");
        println!("account ok (name {} chars), {} playlists, Liked music {} tracks, {:?}", user.name.len(), lists.len(), liked.len(), t.elapsed());
        assert!(!user.name.is_empty());
        assert_eq!(lists.first().map(|p| p.id.as_str()), Some("LM"));
        assert!(liked.iter().all(|t| t.video_id.len() == 11 && !t.title.is_empty()));
    }
}
