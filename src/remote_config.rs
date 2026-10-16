//! Settings that YouTube changes break: which InnerTube clients to use (and
//! their versions), plus a few tunables. Updatable without rebuilding the app:
//!
//! 1. `%APPDATA%\reproductor-rs\config.json` — a local override, if present;
//! 2. the last valid copy downloaded from `config_url` (cached on disk);
//! 3. the defaults built into the exe (`config/default.json`).
//!
//! On top of that, client identifiers (name, version, user agent) follow
//! yt-dlp's own client table, so its fixes arrive without anyone editing the gist.
//!
//! Both refresh every 6 h, and right away (rate-limited) when every client
//! fails. Because the config decides which hosts receive the YouTube session
//! cookies, hosts are restricted to YouTube (also after applying yt-dlp's
//! values) and the URL must be HTTPS.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::Res;

const BUNDLED: &str = include_str!("../config/default.json");
const ALLOWED_HOSTS: [&str; 2] = ["www.youtube.com", "music.youtube.com"];
const REFRESH_EVERY: Duration = Duration::from_secs(6 * 3600);
const FAILURE_REFRESH_COOLDOWN: Duration = Duration::from_secs(5 * 60);
const MAX_CONFIG_BYTES: u64 = 256 << 10;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Full,
    Head,
    Library,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientDef {
    pub name: String,
    pub roles: Vec<Role>,
    pub host: String,
    /// X-YouTube-Client-Name
    pub id: u32,
    pub client_name: String,
    pub version: String,
    pub user_agent: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Take this client's identifiers from yt-dlp (false pins the values above).
    #[serde(default = "enabled_by_default")]
    pub follow_ytdlp: bool,
    /// An embedded-player client: its requests need the embed page's
    /// `encryptedHostFlags` and a third-party embed URL (as yt-dlp sends).
    #[serde(default)]
    pub embedded: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub updated: String,
    pub clients: Vec<ClientDef>,
    pub warm_playlist_limit: usize,
    pub range_attempts: u32,
    #[serde(default)]
    pub config_url: Option<String>,
    /// Where client identifiers are followed from: yt-dlp's latest `release`,
    /// its `master` branch (fixes land sooner), or `off`.
    #[serde(default = "default_ytdlp_source")]
    pub ytdlp_source: String,
}

fn default_ytdlp_source() -> String {
    "release".into()
}

impl Config {
    /// Enabled clients with `role`, in priority order.
    pub fn clients(&self, role: Role) -> Vec<ClientDef> {
        self.clients.iter().filter(|c| c.enabled && c.roles.contains(&role)).cloned().collect()
    }

    fn validate(&self) -> Res<()> {
        for c in &self.clients {
            if !ALLOWED_HOSTS.contains(&c.host.as_str()) {
                return Err(format!("client {}: host {} is not allowed", c.name, c.host).into());
            }
            let clean = |s: &str| !s.is_empty() && !s.chars().any(char::is_control);
            if !clean(&c.name) || !clean(&c.client_name) || !clean(&c.version) || !clean(&c.user_agent) || c.id == 0 {
                return Err(format!("client {}: invalid fields", c.name).into());
            }
        }
        for role in [Role::Full, Role::Library] {
            if self.clients(role.clone()).is_empty() {
                return Err(format!("no enabled client with role {role:?}").into());
            }
        }
        if !["release", "master", "off"].contains(&self.ytdlp_source.as_str()) {
            return Err("ytdlp_source must be release, master or off".into());
        }
        if let Some(url) = &self.config_url {
            if !url.starts_with("https://") {
                return Err("config_url must be https".into());
            }
        }
        if !(1..=500).contains(&self.warm_playlist_limit) && self.warm_playlist_limit != 0 {
            return Err("warm_playlist_limit out of range".into());
        }
        if !(1..=200).contains(&self.range_attempts) {
            return Err("range_attempts out of range".into());
        }
        Ok(())
    }
}

fn parse(text: &str) -> Res<Config> {
    let c: Config = serde_json::from_str(text)?;
    c.validate()?;
    Ok(c)
}

fn override_file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("APPDATA").ok()?).join("reproductor-rs").join("config.json"))
}

fn cache_file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("reproductor-rs").join("remote-config.json"))
}

fn read_valid(path: Option<PathBuf>) -> Option<Config> {
    parse(&std::fs::read_to_string(path?).ok()?).ok()
}

/// Where the active config came from (for diagnostics).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    Override,
    Remote,
    Bundled,
}

// ---- following yt-dlp ----
// yt-dlp keeps the same client table up to date (its `INNERTUBE_CLIENTS` in
// `yt_dlp/extractor/youtube/_base.py`, keyed by the same names we use), so the
// identifiers of our clients are taken from it. The config still decides which
// clients are used and in what order; `follow_ytdlp: false` pins a client.

/// One client's identifiers as yt-dlp has them (fields it doesn't set stay ours).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct YtdlpClient {
    client_name: Option<String>,
    version: Option<String>,
    user_agent: Option<String>,
    id: Option<u32>,
    host: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct YtdlpClients {
    /// Release tag or branch the table came from.
    source: String,
    clients: HashMap<String, YtdlpClient>,
}

fn ytdlp_cache_file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("reproductor-rs").join("ytdlp-clients.json"))
}

/// Value of `'key': '...'` in a Python dict block.
fn py_str(block: &str, key: &str) -> Option<String> {
    let start = block.find(&format!("'{key}': '"))? + key.len() + 5;
    let mut out = String::new();
    let mut chars = block[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '\'' => return Some(out),
            '\n' => return None,
            c => out.push(c),
        }
    }
    None
}

/// Value of `'key': 123` in a Python dict block.
fn py_int(block: &str, key: &str) -> Option<u32> {
    let start = block.find(&format!("'{key}': "))? + key.len() + 4;
    block[start..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// Parse yt-dlp's `INNERTUBE_CLIENTS` table: top-level entries are `    'name': {`
/// lines, each closed by a `    },` line.
fn parse_ytdlp_clients(py: &str) -> HashMap<String, YtdlpClient> {
    let Some(i) = py.find("INNERTUBE_CLIENTS = {") else { return HashMap::new() };
    let mut out = HashMap::new();
    let mut lines = py[i..].lines();
    while let Some(line) = lines.next() {
        if line == "}" {
            break;
        }
        let Some(name) = line.strip_prefix("    '").and_then(|l| l.strip_suffix("': {")) else { continue };
        let mut block = String::new();
        for l in lines.by_ref() {
            if l == "    }," {
                break;
            }
            block.push_str(l);
            block.push('\n');
        }
        let client = YtdlpClient {
            client_name: py_str(&block, "clientName"),
            version: py_str(&block, "clientVersion"),
            user_agent: py_str(&block, "userAgent"),
            id: py_int(&block, "INNERTUBE_CONTEXT_CLIENT_NAME"),
            host: py_str(&block, "INNERTUBE_HOST"),
        };
        if client.client_name.is_some() && client.version.is_some() {
            out.insert(name.to_string(), client);
        }
    }
    out
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into()
}

/// Download yt-dlp's client table from its latest release (or `master`).
fn fetch_ytdlp(channel: &str) -> Res<YtdlpClients> {
    let reference = crate::updates::ytdlp_reference(channel)?;
    let py = http()
        .get(format!("https://raw.githubusercontent.com/yt-dlp/yt-dlp/{reference}/yt_dlp/extractor/youtube/_base.py"))
        .call()?
        .body_mut()
        .with_config()
        .limit(2 << 20)
        .read_to_string()?;
    let clients = parse_ytdlp_clients(&py);
    if clients.is_empty() {
        return Err("yt-dlp client table not found (layout changed?)".into());
    }
    Ok(YtdlpClients { source: reference, clients })
}

/// The config with yt-dlp's identifiers applied to the clients that follow it.
/// If the result would be invalid (e.g. a host outside YouTube), keep `base`.
fn merge(base: &Config, ytdlp: Option<&YtdlpClients>) -> Config {
    let Some(yt) = ytdlp else { return base.clone() };
    if base.ytdlp_source == "off" {
        return base.clone();
    }
    let mut merged = base.clone();
    for c in merged.clients.iter_mut().filter(|c| c.follow_ytdlp) {
        let Some(y) = yt.clients.get(&c.name) else { continue };
        if let Some(v) = &y.client_name {
            c.client_name = v.clone();
        }
        if let Some(v) = &y.version {
            c.version = v.clone();
        }
        if let Some(v) = &y.user_agent {
            c.user_agent = v.clone();
        }
        if let Some(v) = y.id {
            c.id = v;
        }
        if let Some(v) = &y.host {
            c.host = v.clone();
        }
    }
    if merged.validate().is_ok() { merged } else { base.clone() }
}

// ---- active state ----

struct State {
    /// From the override, the gist, or the bundled defaults.
    base: Arc<Config>,
    source: Source,
    ytdlp: Option<YtdlpClients>,
    /// `base` with yt-dlp's identifiers applied: what the app uses.
    effective: Arc<Config>,
}

static STATE: LazyLock<RwLock<State>> = LazyLock::new(|| {
    let (base, source) = if let Some(c) = read_valid(override_file()) {
        (c, Source::Override)
    } else if let Some(c) = read_valid(cache_file()) {
        (c, Source::Remote)
    } else {
        (parse(BUNDLED).expect("bundled config is valid"), Source::Bundled)
    };
    let ytdlp: Option<YtdlpClients> =
        ytdlp_cache_file().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok());
    let effective = Arc::new(merge(&base, ytdlp.as_ref()));
    RwLock::new(State { base: Arc::new(base), source, ytdlp, effective })
});

/// The active config.
pub fn get() -> Arc<Config> {
    STATE.read().unwrap().effective.clone()
}

/// The URL to fetch updates from: the override's, else the active config's, else
/// the bundled one's.
fn config_url() -> Option<String> {
    read_valid(override_file())
        .and_then(|c| c.config_url)
        .or_else(|| get().config_url.clone())
        .or_else(|| parse(BUNDLED).ok().and_then(|c| c.config_url))
}

struct Refresh {
    last_ok: Option<Instant>,
    last_attempt: Option<Instant>,
}

static REFRESH: Mutex<Refresh> = Mutex::new(Refresh { last_ok: None, last_attempt: None });

fn fetch(url: &str) -> Res<Config> {
    // GitHub's CDN serves gists stale for a few minutes; a changing query skips that,
    // so a published fix reaches the app on its next failure-triggered refresh.
    let minute = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 60);
    let sep = if url.contains('?') { '&' } else { '?' };
    let text = http().get(format!("{url}{sep}t={minute}")).call()?.body_mut().with_config().limit(MAX_CONFIG_BYTES).read_to_string()?;
    parse(&text)
}

fn write_cache(path: Option<PathBuf>, json: Result<String, serde_json::Error>) {
    let (Some(path), Ok(json)) = (path, json) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let staged = path.with_extension("json.new");
    if std::fs::write(&staged, json).is_ok() {
        let _ = std::fs::rename(staged, path);
    }
}

/// Download the gist and yt-dlp's client table; returns whether the effective
/// config changed. A local override always wins over the gist, but yt-dlp's
/// identifiers still apply to it.
fn refresh(force: bool) -> bool {
    let mut r = REFRESH.lock().unwrap();
    let now = Instant::now();
    let due = r.last_ok.is_none_or(|t| t.elapsed() >= REFRESH_EVERY);
    let cooled = r.last_attempt.is_none_or(|t| t.elapsed() >= FAILURE_REFRESH_COOLDOWN);
    if !(due || force) || !cooled {
        return false;
    }
    r.last_attempt = Some(now);

    let gist = config_url().map(|url| fetch(&url));
    match &gist {
        Some(Ok(c)) => write_cache(cache_file(), serde_json::to_string_pretty(c)),
        Some(Err(e)) => eprintln!("remote config: {e}"),
        None => {}
    }
    let channel = match &gist {
        Some(Ok(c)) => c.ytdlp_source.clone(),
        _ => get().ytdlp_source.clone(),
    };
    let ytdlp = (channel != "off").then(|| fetch_ytdlp(&channel));
    match &ytdlp {
        Some(Ok(y)) => write_cache(ytdlp_cache_file(), serde_json::to_string_pretty(y)),
        Some(Err(e)) => eprintln!("yt-dlp client table: {e}"),
        None => {}
    }
    if gist.as_ref().is_some_and(Result::is_ok) || ytdlp.as_ref().is_some_and(Result::is_ok) {
        r.last_ok = Some(now);
    }
    drop(r);

    let mut state = STATE.write().unwrap();
    if let Some(Ok(c)) = gist {
        if state.source != Source::Override {
            state.base = Arc::new(c);
            state.source = Source::Remote;
        }
    }
    if let Some(Ok(y)) = ytdlp {
        state.ytdlp = Some(y);
    }
    let effective = merge(&state.base, state.ytdlp.as_ref());
    if *state.effective == effective {
        return false;
    }
    state.effective = Arc::new(effective);
    true
}

/// Periodic refresh (call at startup; cheap when not due).
pub fn refresh_if_stale() {
    refresh(false);
}

/// Something stopped working: fetch now (at most once per 5 minutes). Returns
/// whether a different config is now active, i.e. a retry is worthwhile.
pub fn refresh_after_failure() -> bool {
    refresh(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_config_is_valid() {
        let c = parse(BUNDLED).unwrap();
        assert!(!c.clients(Role::Full).is_empty());
        assert_eq!(c.clients(Role::Library)[0].host, "music.youtube.com");
    }

    /// The published gist must stay valid: the app silently ignores a bad one.
    #[test]
    fn published_config_is_valid_and_cached() {
        let url = config_url().expect("a config_url is set");
        let remote = fetch(&url).expect("published config downloads and validates");
        assert!(!remote.clients(Role::Full).is_empty());
        if let Some(cache) = cache_file() {
            let _ = std::fs::remove_file(&cache);
            refresh_after_failure();
            assert!(read_valid(Some(cache)).is_some(), "a refresh caches the remote config on disk");
        }
    }

    /// yt-dlp's current release still has a parseable table with our clients.
    #[test]
    fn follows_ytdlp_release_table() {
        let yt = fetch_ytdlp("release").expect("yt-dlp client table");
        println!("yt-dlp {}: {} clients", yt.source, yt.clients.len());
        let base = parse(BUNDLED).unwrap();
        for c in &base.clients {
            let y = yt.clients.get(&c.name).unwrap_or_else(|| panic!("yt-dlp has no client {}", c.name));
            println!("  {}: {:?} {:?} id {:?}", c.name, y.client_name, y.version, y.id);
        }
        let merged = merge(&base, Some(&yt));
        assert!(merged.validate().is_ok());
        assert_eq!(merged.clients(Role::Library)[0].host, "music.youtube.com");
    }

    #[test]
    fn ytdlp_values_cannot_redirect_cookies() {
        let base = parse(BUNDLED).unwrap();
        let mut clients = HashMap::new();
        let evil = YtdlpClient { client_name: Some("X".into()), version: Some("1".into()), user_agent: None, id: Some(1), host: Some("evil.example.com".into()) };
        clients.insert("tv_downgraded".to_string(), evil);
        let merged = merge(&base, Some(&YtdlpClients { source: "test".into(), clients }));
        assert_eq!(merged, base, "an invalid merge keeps the base config");
    }

    #[test]
    fn parses_python_client_table() {
        let py = "INNERTUBE_CLIENTS = {\n    'web': {\n        'INNERTUBE_CONTEXT': {\n            'client': {\n                'clientName': 'WEB',\n                'clientVersion': '2.1',\n                'userAgent': 'A \\'quoted\\' UA',\n            },\n        },\n        'INNERTUBE_CONTEXT_CLIENT_NAME': 1,\n    },\n    'web_music': {\n        'INNERTUBE_HOST': 'music.youtube.com',\n        'INNERTUBE_CONTEXT': {\n            'client': {\n                'clientName': 'WEB_REMIX',\n                'clientVersion': '1.2',\n            },\n        },\n        'INNERTUBE_CONTEXT_CLIENT_NAME': 67,\n    },\n}\n";
        let t = parse_ytdlp_clients(py);
        assert_eq!(t["web"].user_agent.as_deref(), Some("A 'quoted' UA"));
        assert_eq!(t["web"].id, Some(1));
        assert_eq!(t["web_music"].host.as_deref(), Some("music.youtube.com"));
        assert_eq!(t["web_music"].user_agent, None);
    }

    #[test]
    fn rejects_foreign_hosts_and_plain_http() {
        let mut c = parse(BUNDLED).unwrap();
        c.clients[0].host = "evil.example.com".into();
        assert!(c.validate().is_err(), "cookies must never go to a non-YouTube host");
        let mut c = parse(BUNDLED).unwrap();
        c.config_url = Some("http://example.com/c.json".into());
        assert!(c.validate().is_err());
        let mut c = parse(BUNDLED).unwrap();
        c.clients.iter_mut().for_each(|c| c.enabled = false);
        assert!(c.validate().is_err());
    }
}
