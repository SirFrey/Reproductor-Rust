//! YouTube's internal API ("InnerTube"), used with the user's own session (the
//! cookies the sign-in window captured): stream resolution and the user's
//! YouTube Music library. Request shapes follow yt-dlp's YouTube extractor; the
//! player's challenges are solved in `jsc`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use sha1::{Digest, Sha1};

use crate::Res;
use crate::remote_config::{self, Role};
use crate::youtube::{PlaylistInfo, TrackInfo, UserInfo};

/// Client definitions (names, versions, user agents) come from `remote_config`,
/// so they can be updated without rebuilding when YouTube changes something.
type Client = remote_config::ClientDef;

/// Clients whose stream URLs serve the whole file, in order of preference, with
/// recently failing ones moved to the back. Their URLs only become valid a few
/// seconds (sometimes >10 s) after being issued.
fn full_clients() -> Vec<Client> {
    let mut clients = remote_config::get().clients(Role::Full);
    let failed = FAILED_AT.lock().unwrap();
    clients.sort_by_key(|c| failed.get(&c.name).is_some_and(|t| t.elapsed() < DEMOTE_FOR));
    clients
}

/// When each client last failed for a reason that isn't video-specific.
static FAILED_AT: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const DEMOTE_FOR: Duration = Duration::from_secs(30 * 60);

fn record_client(name: &str, ok: bool) {
    let mut failed = FAILED_AT.lock().unwrap();
    if ok {
        failed.remove(name);
    } else {
        failed.insert(name.to_string(), Instant::now());
    }
}

/// A client whose URL serves the first MiB immediately (quick start), if any.
fn head_client() -> Option<Client> {
    remote_config::get().clients(Role::Head).into_iter().next()
}

/// YouTube Music's web client, for the account and library.
fn library_client() -> Res<Client> {
    Ok(remote_config::get().clients(Role::Library).into_iter().next().ok_or("no library client configured")?)
}

const PLAYER_CHECK: Duration = Duration::from_secs(3600);
const MAX_LIBRARY_PAGES: usize = 100;
const MAX_JSON_BYTES: u64 = 64 << 20;

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .into()
});

pub fn available() -> bool {
    crate::jsc::solver_dir().is_some() && crate::youtube::cookies_path().is_some()
}

// ---- session (cookies.txt, re-read when the sign-in window refreshes it) ----

struct Jar {
    modified: Option<SystemTime>,
    cookies: HashMap<String, String>,
}

static JAR: LazyLock<Mutex<Jar>> = LazyLock::new(|| Mutex::new(Jar { modified: None, cookies: HashMap::new() }));

fn cookies() -> HashMap<String, String> {
    let Some(path) = crate::youtube::cookies_path() else { return HashMap::new() };
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut jar = JAR.lock().unwrap();
    if jar.modified != modified || jar.cookies.is_empty() {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        jar.cookies = text
            .lines()
            .filter(|l| !l.is_empty() && (!l.starts_with('#') || l.starts_with("#HttpOnly_")))
            .map(|l| l.split('\t').collect::<Vec<_>>())
            .filter(|p| p.len() >= 7 && p[0].trim_start_matches("#HttpOnly_").ends_with("youtube.com"))
            .map(|p| (p[5].trim().to_string(), p[6].trim().to_string()))
            .collect();
        jar.modified = modified;
    }
    jar.cookies.clone()
}

/// Cookie header plus the SAPISIDHASH authorization the web apps send.
fn auth_headers(origin: &str) -> Res<(String, String)> {
    let c = cookies();
    let ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
    let auth: Vec<String> = [("SAPISIDHASH", "SAPISID"), ("SAPISID1PHASH", "__Secure-1PAPISID"), ("SAPISID3PHASH", "__Secure-3PAPISID")]
        .iter()
        .filter_map(|(scheme, name)| {
            let sid = c.get(*name)?;
            let hash: String = Sha1::digest(format!("{ts} {sid} {origin}").as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
            Some(format!("{scheme} {ts}_{hash}"))
        })
        .collect();
    if auth.is_empty() {
        return Err("LOGIN_REQUIRED: no YouTube session cookies".into());
    }
    let cookie = c.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
    Ok((auth.join(" "), cookie))
}

fn post(client: &Client, endpoint: &str, body: Value) -> Res<Value> {
    let origin = format!("https://{}", client.host);
    let (auth, cookie) = auth_headers(&origin)?;
    let mut resp = AGENT
        .post(format!("{origin}/youtubei/v1/{endpoint}?prettyPrint=false"))
        .header("Content-Type", "application/json")
        .header("User-Agent", client.user_agent.as_str())
        .header("X-YouTube-Client-Name", client.id.to_string())
        .header("X-YouTube-Client-Version", client.version.as_str())
        .header("Origin", origin.as_str())
        .header("X-Origin", origin.as_str())
        .header("X-Goog-AuthUser", "0")
        .header("X-Youtube-Bootstrap-Logged-In", "true")
        .header("Authorization", auth)
        .header("Cookie", cookie)
        .send_json(body)?;
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        return Err(format!("LOGIN_REQUIRED: {} answered {status}", client.host).into());
    }
    if !(200..300).contains(&status) {
        return Err(format!("{} HTTP {status}", client.host).into());
    }
    Ok(resp.body_mut().with_config().limit(MAX_JSON_BYTES).read_json()?)
}

fn context(client: &Client) -> Value {
    json!({ "client": {
        "clientName": client.client_name,
        "clientVersion": client.version,
        "userAgent": client.user_agent,
        "hl": "en",
        "timeZone": "UTC",
        "utcOffsetMinutes": 0,
    }})
}

// ---- player (version, signature timestamp, preprocessed code) ----

struct Player {
    id: String,
    sts: u64,
    checked: Instant,
}

static PLAYER: Mutex<Option<Player>> = Mutex::new(None);
static PREPARING: AtomicBool = AtomicBool::new(false);

/// True while a new player version is being preprocessed (~10 s); callers in a
/// hurry should use the slower yt-dlp path meanwhile.
pub fn preparing() -> bool {
    PREPARING.load(Ordering::Relaxed)
}

/// Make sure the current player is ready (run in the background at startup).
pub fn prepare() {
    if available() {
        let _ = player(false);
    }
}

fn get_text(url: &str) -> Res<String> {
    let mut resp = AGENT.get(url).call()?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {url}", resp.status()).into());
    }
    Ok(resp.body_mut().with_config().limit(MAX_JSON_BYTES).read_to_string()?)
}

fn current_player_id() -> Res<String> {
    let page = get_text("https://www.youtube.com/iframe_api")?.replace("\\/", "/");
    let start = page.find("/s/player/").ok_or("player id not found")? + "/s/player/".len();
    let id: String = page[start..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    if id.len() != 8 {
        return Err("player id not found".into());
    }
    Ok(id)
}

/// `signatureTimestamp:12345` or `sts:12345` in the player code.
fn find_sts(code: &str) -> Option<u64> {
    for key in ["signatureTimestamp", "sts"] {
        for (i, _) in code.match_indices(key) {
            let rest = code[i + key.len()..].trim_start();
            if let Some(rest) = rest.strip_prefix(':') {
                let digits: String = rest.trim_start().chars().take_while(char::is_ascii_digit).collect();
                if digits.len() == 5 {
                    return digits.parse().ok();
                }
            }
        }
    }
    None
}

fn player_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("reproductor-rs"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PlayerCache {
    sts: u64,
    preprocessed: String,
    /// `jsc::solver_fingerprint` of the solver that preprocessed it.
    #[serde(default)]
    solver: String,
}

/// The cached preprocessed player, if it was made by the current solver.
fn load_cached_player(id: &str) -> Option<PlayerCache> {
    let text = std::fs::read_to_string(player_dir()?.join(format!("player-{id}.json"))).ok()?;
    let cache: PlayerCache = serde_json::from_str(&text).ok()?;
    (cache.solver == crate::jsc::solver_fingerprint()).then_some(cache)
}

fn save_cached_player(id: &str, cache: &PlayerCache) {
    let Some(dir) = player_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(dir.join(format!("player-{id}.json")), json);
    }
    // Old versions are useless once YouTube moves on.
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("player-") && name.ends_with(".json") && name != format!("player-{id}.json") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The current player's id and signature timestamp; its preprocessed code is on disk
/// (read per solve rather than held in memory). Checks for a
/// new version hourly (or when `force`d after a solver failure).
fn player(force: bool) -> Res<(u64, String)> {
    let mut slot = PLAYER.lock().unwrap();
    if let Some(p) = slot.as_ref() {
        if !force && p.checked.elapsed() < PLAYER_CHECK {
            return Ok((p.sts, p.id.clone()));
        }
    }
    let id = current_player_id()?;
    if let Some(p) = slot.as_mut() {
        if p.id == id && !force {
            p.checked = Instant::now();
            return Ok((p.sts, p.id.clone()));
        }
    }
    let cache = match load_cached_player(&id).filter(|_| !force) {
        Some(c) => c,
        None => {
            let code = get_text(&format!("https://www.youtube.com/s/player/{id}/player_ias.vflset/en_US/base.js"))?;
            let sts = find_sts(&code).ok_or("signature timestamp not found in player")?;
            PREPARING.store(true, Ordering::Relaxed);
            let preprocessed = crate::jsc::preprocess(&code);
            PREPARING.store(false, Ordering::Relaxed);
            let cache = PlayerCache { sts, preprocessed: preprocessed?, solver: crate::jsc::solver_fingerprint() };
            save_cached_player(&id, &cache);
            cache
        }
    };
    *slot = Some(Player { id: id.clone(), sts: cache.sts, checked: Instant::now() });
    Ok((cache.sts, id))
}

// ---- stream resolution ----

pub struct Resolved {
    pub url: String,
    pub len: u64,
    pub user_agent: String,
    /// A URL that serves only the first MiB of the same file, but immediately.
    pub head_url: Option<String>,
    pub head_user_agent: Option<String>,
}

/// Original-language AAC, without dynamic-range compression, preferring itag 140
/// (128 kbps), then the highest bitrate.
fn pick_format(formats: &[Value]) -> Option<&Value> {
    let aac: Vec<&Value> = formats
        .iter()
        .filter(|f| f["mimeType"].as_str().is_some_and(|m| m.starts_with("audio/mp4")))
        .filter(|f| f["url"].is_string() || f["signatureCipher"].is_string())
        .collect();
    let original: Vec<&Value> =
        aac.iter().copied().filter(|f| f["audioTrack"].is_null() || f["audioTrack"]["audioIsDefault"] == true).collect();
    let pool = if original.is_empty() { aac } else { original };
    pool.into_iter().min_by_key(|f| {
        (f["isDrc"] == true, f["itag"].as_i64() != Some(140), std::cmp::Reverse(f["bitrate"].as_i64().unwrap_or(0)))
    })
}

/// A stream URL as the player API returned it, with its challenges still unsolved.
struct Unsolved {
    url: url::Url,
    len: u64,
    n: Option<String>,
    /// Encrypted signature and the query parameter it goes into.
    sig: Option<(String, String)>,
}

/// Any valid non-YouTube page; YouTube refuses embedded playback without one.
const EMBED_URL: &str = "https://www.reddit.com/";

/// The embed page's `encryptedHostFlags`, which embedded-player requests carry
/// (as yt-dlp does: it downloads `/embed/<id>` as that client's config page).
fn embed_host_flags(client: &Client, video: &str) -> Option<String> {
    let origin = format!("https://{}", client.host);
    let (_, cookie) = auth_headers(&origin).ok()?;
    let mut resp = AGENT
        .get(format!("{origin}/embed/{video}?html5=1"))
        .header("User-Agent", client.user_agent.as_str())
        .header("Referer", EMBED_URL)
        .header("Cookie", cookie)
        .call()
        .ok()?;
    let page = resp.body_mut().with_config().limit(MAX_JSON_BYTES).read_to_string().ok()?;
    let key = "\"encryptedHostFlags\":\"";
    let start = page.find(key)? + key.len();
    let flags: String = page[start..].chars().take_while(|c| *c != '"').collect();
    (!flags.is_empty()).then_some(flags)
}

fn fetch_format(client: &Client, video: &str, sts: u64) -> Res<Unsolved> {
    let mut context = context(client);
    let mut playback = json!({ "html5Preference": "HTML5_PREF_WANTS", "signatureTimestamp": sts });
    if client.embedded {
        context["thirdParty"] = json!({ "embedUrl": EMBED_URL });
        if let Some(flags) = embed_host_flags(client, video) {
            playback["encryptedHostFlags"] = flags.into();
        }
    }
    let body = json!({
        "context": context,
        "videoId": video,
        "playbackContext": { "contentPlaybackContext": playback },
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let j = post(client, "player", body)?;
    let status = j["playabilityStatus"]["status"].as_str().unwrap_or("UNKNOWN");
    if status != "OK" {
        let reason = j["playabilityStatus"]["reason"].as_str().unwrap_or_default();
        return Err(format!("{status}: {reason}").into());
    }
    let formats = j["streamingData"]["adaptiveFormats"].as_array().map(Vec::as_slice).unwrap_or_default();
    let f = pick_format(formats).ok_or("no AAC audio format")?;
    let len = f["contentLength"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let (raw_url, sig) = match f["url"].as_str() {
        Some(u) => (u.to_string(), None),
        None => {
            let sc: HashMap<String, String> =
                url::form_urlencoded::parse(f["signatureCipher"].as_str().unwrap_or_default().as_bytes()).into_owned().collect();
            let u = sc.get("url").ok_or("cipher without url")?.clone();
            let param = sc.get("sp").cloned().unwrap_or_else(|| "signature".into());
            (u, sc.get("s").map(|s| (s.clone(), param)))
        }
    };
    let url = url::Url::parse(&raw_url)?;
    let n = url.query_pairs().find(|(k, _)| k == "n").map(|(_, v)| v.into_owned());
    Ok(Unsolved { url, len, n, sig })
}

/// Solve every challenge of several streams in one engine run (~0.4 s), since
/// each run has to load the whole preprocessed player.
fn solve_all(player_id: &str, streams: &mut [&mut Unsolved]) -> Res<()> {
    let ns: Vec<&String> = streams.iter().filter_map(|s| s.n.as_ref()).collect();
    let sigs: Vec<&String> = streams.iter().filter_map(|s| s.sig.as_ref().map(|(s, _)| s)).collect();
    if ns.is_empty() && sigs.is_empty() {
        return Ok(());
    }
    let mut requests = Vec::new();
    if !ns.is_empty() {
        requests.push(json!({ "type": "n", "challenges": ns }));
    }
    if !sigs.is_empty() {
        requests.push(json!({ "type": "sig", "challenges": sigs }));
    }
    let cache = load_cached_player(player_id).ok_or("solver: cached player missing")?;
    let answers = crate::jsc::solve(&cache.preprocessed, Value::Array(requests))?;
    let (n_answers, sig_answers) = match (ns.is_empty(), sigs.is_empty()) {
        (false, false) => (answers[0].clone(), answers[1].clone()),
        (false, true) => (answers[0].clone(), Value::Null),
        _ => (Value::Null, answers[0].clone()),
    };
    for s in streams.iter_mut() {
        let mut pairs: Vec<(String, String)> = s.url.query_pairs().into_owned().collect();
        if let Some(n) = &s.n {
            let solved = n_answers[n].as_str().ok_or("solver: no n answer")?;
            pairs.iter_mut().filter(|(k, _)| k == "n").for_each(|(_, v)| *v = solved.to_string());
        }
        if let Some((sig, param)) = &s.sig {
            let solved = sig_answers[sig].as_str().ok_or("solver: no sig answer")?;
            pairs.push((param.clone(), solved.to_string()));
        }
        s.url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    Ok(())
}

fn resolve_once(video: &str, head: bool, force_player: bool) -> Res<Resolved> {
    let (sts, player_id) = player(force_player)?;
    // Both player requests go out at once; then one solve covers both.
    let head_client = if head { head_client() } else { None };
    let (full, head_stream) = std::thread::scope(|s| {
        let head_job = head_client.as_ref().map(|c| s.spawn(move || fetch_format(c, video, sts).ok()));
        let mut errors = Vec::new();
        let mut full = None;
        for client in full_clients() {
            match fetch_format(&client, video, sts) {
                Ok(u) => {
                    record_client(&client.name, true);
                    full = Some((u, client.user_agent));
                    break;
                }
                Err(e) => {
                    // "Unavailable"/"private" are about the video, not the client.
                    let msg = e.to_string();
                    if !msg.starts_with("ERROR") && !msg.contains("LOGIN_REQUIRED") {
                        record_client(&client.name, false);
                    }
                    errors.push(format!("{}: {msg}", client.name));
                }
            }
        }
        (full.ok_or_else(|| errors.join(" | ")), head_job.and_then(|h| h.join().ok().flatten()))
    });
    let (mut full, user_agent) = full?;
    let mut head_stream = head_stream.filter(|h| h.len == full.len);
    match head_stream.as_mut() {
        Some(h) => solve_all(&player_id, &mut [&mut full, h])?,
        None => solve_all(&player_id, &mut [&mut full])?,
    }
    Ok(Resolved {
        url: full.url.to_string(),
        len: full.len,
        user_agent,
        head_user_agent: head_stream.as_ref().and(head_client.map(|c| c.user_agent)),
        head_url: head_stream.map(|h| h.url.to_string()),
    })
}

/// Resolve a video id to a stream URL (~0.5-1 s). `head`: also get the quick-start
/// URL for the first MiB, worth it only for a track about to play.
pub fn resolve(video: &str, head: bool) -> Res<Resolved> {
    resolve_once(video, head, false)
        .or_else(|e| {
            // A solver failure usually means YouTube shipped a new player: reload it once.
            if e.to_string().starts_with("solver:") { resolve_once(video, head, true) } else { Err(e) }
        })
        .or_else(|e| {
            // Still failing with a fresh player: yt-dlp may already ship a fixed solver.
            if e.to_string().starts_with("solver:") && crate::updates::check_after_solver_failure() {
                resolve_once(video, head, true)
            } else {
                Err(e)
            }
        })
        .or_else(|e| {
            // Every client failed for a reason a fresh session won't fix: maybe YouTube
            // changed something the published config already accounts for.
            let msg = e.to_string();
            if !msg.contains("LOGIN_REQUIRED") && remote_config::refresh_after_failure() {
                resolve_once(video, head, false)
            } else {
                Err(e)
            }
        })
}

// ---- library (YouTube Music's web API) ----

/// The response layouts shift often, so collect renderers wherever they appear.
fn collect<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if k == key {
                    out.push(child);
                }
                collect(child, key, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|child| collect(child, key, out)),
        _ => {}
    }
}

fn find<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    let mut out = Vec::new();
    collect(v, key, &mut out);
    out
}

fn text(t: &Value) -> String {
    match t["runs"].as_array() {
        Some(runs) => runs.iter().filter_map(|r| r["text"].as_str()).collect(),
        None => t["simpleText"].as_str().unwrap_or_default().to_string(),
    }
}

fn next_token(v: &Value) -> Option<String> {
    find(v, "continuationCommand")
        .into_iter()
        .filter_map(|c| c["token"].as_str())
        .chain(find(v, "nextContinuationData").into_iter().filter_map(|c| c["continuation"].as_str()))
        .next()
        .map(str::to_owned)
}

fn music(endpoint: &str, mut body: Value) -> Res<Value> {
    let client = library_client()?;
    body["context"] = json!({ "client": { "clientName": client.client_name, "clientVersion": client.version, "hl": "en" } });
    post(&client, endpoint, body)
}

/// Every `key` renderer of a browse page and its continuations.
fn browse_all(browse_id: &str, key: &str) -> Res<Vec<Value>> {
    let mut page = music("browse", json!({ "browseId": browse_id }))?;
    let mut items: Vec<Value> = find(&page, key).into_iter().cloned().collect();
    for _ in 1..MAX_LIBRARY_PAGES {
        let Some(token) = next_token(&page) else { break };
        page = music("browse", json!({ "continuation": token }))?;
        let more: Vec<Value> = find(&page, key).into_iter().cloned().collect();
        if more.is_empty() {
            break;
        }
        items.extend(more);
    }
    Ok(items)
}

pub fn account() -> Res<UserInfo> {
    let j = music("account/account_menu", json!({}))?;
    let h = *find(&j, "activeAccountHeaderRenderer").first().ok_or("LOGIN_REQUIRED: not signed in to YouTube Music")?;
    let photo = h["accountPhoto"]["thumbnails"].as_array().and_then(|t| t.last()).and_then(|t| t["url"].as_str());
    Ok(UserInfo { name: text(&h["accountName"]), picture: photo.map(str::to_owned) })
}

/// The user's library playlists, with "Liked music" first.
pub fn library() -> Res<Vec<PlaylistInfo>> {
    let mut out = vec![PlaylistInfo { id: "LM".into(), title: "Liked music".into(), count: -1 }];
    for item in browse_all("FEmusic_liked_playlists", "musicTwoRowItemRenderer")? {
        let Some(id) = item["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str().and_then(|b| b.strip_prefix("VL")) else {
            continue;
        };
        if out.iter().any(|p| p.id == id) {
            continue;
        }
        out.push(PlaylistInfo { id: id.to_string(), title: text(&item["title"]), count: -1 });
    }
    Ok(out)
}

pub fn playlist(id: &str) -> Res<Vec<TrackInfo>> {
    let mut tracks: Vec<TrackInfo> = Vec::new();
    for row in browse_all(&format!("VL{id}"), "musicResponsiveListItemRenderer")? {
        let video = row["playlistItemData"]["videoId"]
            .as_str()
            .or_else(|| find(&row, "watchEndpoint").first().and_then(|w| w["videoId"].as_str()));
        let Some(video) = video else { continue };
        if tracks.iter().any(|t| t.video_id == video) {
            continue;
        }
        let cols: Vec<String> = row["flexColumns"]
            .as_array()
            .map(|c| c.iter().map(|c| text(&c["musicResponsiveListItemFlexColumnRenderer"]["text"])).collect())
            .unwrap_or_default();
        tracks.push(TrackInfo {
            video_id: video.to_string(),
            title: cols.first().cloned().unwrap_or_default(),
            artist: cols.get(1).cloned().unwrap_or_default(),
        });
    }
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The new-player path: no cache, download + preprocess, then a solve with it.
    #[test]
    #[ignore = "takes ~10 s and replaces the cached player"]
    fn prepare_new_player_from_scratch() {
        if let Some(dir) = player_dir() {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                if e.file_name().to_string_lossy().starts_with("player-") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        *PLAYER.lock().unwrap() = None;
        let t = Instant::now();
        prepare();
        println!("prepare from scratch: {:?}", t.elapsed());
        let t = Instant::now();
        let r = resolve("OPf0YbXqDm0", true).expect("resolve with fresh player");
        println!("first resolve after: {:?}, quick-start url: {}", t.elapsed(), r.head_url.is_some());
    }
}
