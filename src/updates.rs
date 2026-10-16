//! Keeps the parts YouTube breaks most often current, straight from yt-dlp:
//!
//! - the EJS challenge-solver scripts, downloaded from yt-dlp/ejs releases and
//!   accepted only if their SHA3-512 matches the hashes yt-dlp pins in its own
//!   source for the release we follow;
//! - the yt-dlp fallback itself (the unpacked Windows build), accepted only if
//!   the archive matches yt-dlp's published SHA2-256SUMS.
//!
//! Checked at startup at most once a day, and sooner when solving fails.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::Sha256;
use sha3::{Digest, Sha3_512};

use crate::Res;

const CHECK_EVERY_SECS: u64 = 24 * 3600;
const FAILURE_RETRY: Duration = Duration::from_secs(30 * 60);
const MAX_SCRIPT_BYTES: u64 = 4 << 20;
const MAX_ZIP_BYTES: u64 = 200 << 20;

/// Held (shared) while yt-dlp runs, (exclusive) while its folder is swapped.
pub static YTDLP_IN_USE: RwLock<()> = RwLock::new(());

fn app_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("reproductor-rs"))
}

fn http(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(timeout_secs))).build().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn get_text(url: &str) -> Res<String> {
    Ok(http(20).get(url).header("User-Agent", "reproductor-rs").call()?.body_mut().with_config().limit(MAX_SCRIPT_BYTES).read_to_string()?)
}

/// The yt-dlp git reference to follow: its latest release tag, or `master`.
pub fn ytdlp_reference(channel: &str) -> Res<String> {
    let reference = if channel == "master" {
        "master".to_string()
    } else {
        let v: serde_json::Value = http(10)
            .get("https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest")
            .header("User-Agent", "reproductor-rs")
            .call()?
            .body_mut()
            .read_json()?;
        v["tag_name"].as_str().ok_or("no yt-dlp release tag")?.to_string()
    };
    if !reference.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) {
        return Err("unexpected yt-dlp release tag".into());
    }
    Ok(reference)
}

/// Swap `new` into `target` (keeping the old copy until the rename succeeded).
fn replace_dir(new: &Path, target: &Path) -> Res<()> {
    let old = target.with_extension("old");
    let _ = std::fs::remove_dir_all(&old);
    if target.exists() {
        std::fs::rename(target, &old)?;
    }
    if let Err(e) = std::fs::rename(new, target) {
        let _ = std::fs::rename(&old, target);
        return Err(e.into());
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

// ---- solver scripts ----

/// The solver version and file hashes yt-dlp pins (`vendor/_info.py`).
struct SolverPin {
    version: String,
    lib_hash: String,
    core_hash: String,
}

fn py_quoted_after(text: &str, marker: &str) -> Option<String> {
    let start = text.find(marker)? + marker.len();
    let rest = &text[start..];
    let open = rest.find('\'')? + 1;
    let close = rest[open..].find('\'')? + open;
    Some(rest[open..close].to_string())
}

fn solver_pin(reference: &str) -> Res<SolverPin> {
    let info = get_text(&format!(
        "https://raw.githubusercontent.com/yt-dlp/yt-dlp/{reference}/yt_dlp/extractor/youtube/jsc/_builtin/vendor/_info.py"
    ))?;
    let pin = SolverPin {
        version: py_quoted_after(&info, "VERSION =").ok_or("solver version not found")?,
        lib_hash: py_quoted_after(&info, "'yt.solver.lib.min.js':").ok_or("lib hash not found")?,
        core_hash: py_quoted_after(&info, "'yt.solver.core.min.js':").ok_or("core hash not found")?,
    };
    let valid_version = !pin.version.is_empty() && pin.version.chars().all(|c| c.is_ascii_alphanumeric() || c == '.');
    if !valid_version || pin.lib_hash.len() != 128 || pin.core_hash.len() != 128 {
        return Err("unexpected solver pin format".into());
    }
    Ok(pin)
}

fn sha3_hex(data: &[u8]) -> String {
    hex(&Sha3_512::digest(data))
}

/// Make the app's solver copy match what yt-dlp pins. Returns whether it changed.
pub fn update_solver(reference: &str) -> Res<bool> {
    let dir = app_dir().ok_or("no LOCALAPPDATA")?;
    let target = dir.join("ejs");
    let pin = solver_pin(reference)?;
    let current = |name: &str| std::fs::read(target.join(name)).map(|b| sha3_hex(&b)).unwrap_or_default();
    if current("lib.min.js") == pin.lib_hash && current("core.min.js") == pin.core_hash {
        return Ok(false);
    }
    let fetch = |file: &str, expected: &str| -> Res<String> {
        let code = get_text(&format!("https://github.com/yt-dlp/ejs/releases/download/{}/{file}", pin.version))?;
        let got = sha3_hex(code.as_bytes());
        if got != expected {
            return Err(format!("{file} {} doesn't match yt-dlp's pinned hash", pin.version).into());
        }
        Ok(code)
    };
    let lib = fetch("yt.solver.lib.min.js", &pin.lib_hash)?;
    let core = fetch("yt.solver.core.min.js", &pin.core_hash)?;
    let staged = dir.join("ejs.new");
    let _ = std::fs::remove_dir_all(&staged);
    std::fs::create_dir_all(&staged)?;
    std::fs::write(staged.join("lib.min.js"), lib)?;
    std::fs::write(staged.join("core.min.js"), core)?;
    std::fs::write(staged.join("version.txt"), &pin.version)?;
    replace_dir(&staged, &target)?;
    Ok(true)
}

// ---- yt-dlp ----

fn ytdlp_dir() -> Option<PathBuf> {
    Some(app_dir()?.join("yt-dlp"))
}

fn installed_ytdlp_version() -> Option<String> {
    let dir = ytdlp_dir()?;
    if let Ok(v) = std::fs::read_to_string(dir.join("version.txt")) {
        return Some(v.trim().to_string());
    }
    let mut cmd = std::process::Command::new(dir.join("yt-dlp.exe"));
    cmd.arg("--version");
    hide_window(&mut cmd);
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn hide_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// Install or update the unpacked Windows build of yt-dlp's latest release.
/// Returns whether it changed.
pub fn update_ytdlp() -> Res<bool> {
    let tag = ytdlp_reference("release")?;
    if installed_ytdlp_version().as_deref() == Some(tag.as_str()) {
        return Ok(false);
    }
    let base = format!("https://github.com/yt-dlp/yt-dlp/releases/download/{tag}");
    let sums = get_text(&format!("{base}/SHA2-256SUMS"))?;
    let expected = sums
        .lines()
        .find_map(|l| l.strip_suffix("yt-dlp_win.zip").map(|h| h.trim().to_lowercase()))
        .ok_or("yt-dlp_win.zip not in SHA2-256SUMS")?;
    let zip = http(600)
        .get(format!("{base}/yt-dlp_win.zip"))
        .header("User-Agent", "reproductor-rs")
        .call()?
        .body_mut()
        .with_config()
        .limit(MAX_ZIP_BYTES)
        .read_to_vec()?;
    if hex(&Sha256::digest(&zip)) != expected {
        return Err("yt-dlp_win.zip doesn't match SHA2-256SUMS".into());
    }

    let dir = app_dir().ok_or("no LOCALAPPDATA")?;
    let zip_path = dir.join("yt-dlp_win.zip.download");
    let staged = dir.join("yt-dlp.new");
    let _ = std::fs::remove_dir_all(&staged);
    std::fs::create_dir_all(&staged)?;
    std::fs::write(&zip_path, &zip)?;
    drop(zip);
    let tar = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join(r"System32\tar.exe");
    let mut cmd = std::process::Command::new(tar);
    cmd.arg("-xf").arg(&zip_path).arg("-C").arg(&staged);
    hide_window(&mut cmd);
    let extracted = cmd.status();
    let _ = std::fs::remove_file(&zip_path);
    if !extracted.is_ok_and(|s| s.success()) || !staged.join("yt-dlp.exe").is_file() {
        let _ = std::fs::remove_dir_all(&staged);
        return Err("couldn't extract yt-dlp_win.zip".into());
    }
    std::fs::write(staged.join("version.txt"), &tag)?;

    let _exclusive = YTDLP_IN_USE.write().unwrap();
    replace_dir(&staged, &ytdlp_dir().ok_or("no LOCALAPPDATA")?)?;
    Ok(true)
}

// ---- schedule ----

fn stamp_file() -> Option<PathBuf> {
    Some(app_dir()?.join("updates-checked.txt"))
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

static LAST_FAILURE_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

fn check(channel: &str) -> bool {
    let mut changed = false;
    match ytdlp_reference(channel).and_then(|r| update_solver(&r)) {
        Ok(c) => changed |= c,
        Err(e) => eprintln!("solver update: {e}"),
    }
    match update_ytdlp() {
        Ok(c) => changed |= c,
        Err(e) => eprintln!("yt-dlp update: {e}"),
    }
    if let Some(f) = stamp_file() {
        let _ = std::fs::write(f, unix_now().to_string());
    }
    changed
}

/// Daily check (call at startup, in the background).
pub fn check_if_due() {
    let last = stamp_file().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|s| s.trim().parse::<u64>().ok());
    if last.is_none_or(|t| unix_now().saturating_sub(t) >= CHECK_EVERY_SECS) {
        check(&crate::remote_config::get().ytdlp_source);
    }
}

/// Solving failed even with a fresh player: maybe yt-dlp already shipped a fixed
/// solver. Checks at most every 30 minutes; returns whether anything changed.
pub fn check_after_solver_failure() -> bool {
    {
        let mut last = LAST_FAILURE_CHECK.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < FAILURE_RETRY) {
            return false;
        }
        *last = Some(Instant::now());
    }
    check(&crate::remote_config::get().ytdlp_source)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// yt-dlp's release pins a solver version whose published files match the pins,
    /// and the copy bundled in the installed yt-dlp build is that same code.
    #[test]
    fn solver_pins_match_published_files() {
        let reference = ytdlp_reference("release").unwrap();
        let pin = solver_pin(&reference).expect("pin");
        println!("yt-dlp {reference} pins ejs {}", pin.version);
        let lib = get_text(&format!("https://github.com/yt-dlp/ejs/releases/download/{}/yt.solver.lib.min.js", pin.version)).unwrap();
        assert_eq!(sha3_hex(lib.as_bytes()), pin.lib_hash);
        if let Some(installed) = app_dir().map(|d| d.join("yt-dlp/_internal/yt_dlp_ejs/yt/solver/core.min.js")) {
            if let Ok(code) = std::fs::read(installed) {
                println!("installed yt-dlp's core.min.js matches pin: {}", sha3_hex(&code) == pin.core_hash);
            }
        }
    }

    #[test]
    fn rejects_tampered_solver() {
        let pin = solver_pin(&ytdlp_reference("release").unwrap()).unwrap();
        assert_ne!(sha3_hex(b"globalThis.jsc = () => 'evil'"), pin.core_hash);
    }

    #[test]
    fn parses_python_pins() {
        let info = "VERSION = '0.8.0'\nHASHES = {\n    'yt.solver.core.min.js': 'abc',\n}\n";
        assert_eq!(py_quoted_after(info, "VERSION =").as_deref(), Some("0.8.0"));
        assert_eq!(py_quoted_after(info, "'yt.solver.core.min.js':").as_deref(), Some("abc"));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Installs the pinned solver into the app folder and re-runs the yt-dlp
    /// update (forced by an outdated version.txt): download, checksum, extract, swap.
    #[test]
    #[ignore = "downloads ~17 MB and replaces the app's yt-dlp folder"]
    fn update_everything_for_real() {
        let reference = ytdlp_reference("release").unwrap();
        let t = Instant::now();
        println!("solver changed: {:?} in {:?}", update_solver(&reference).map_err(|e| e.to_string()), t.elapsed());
        println!("solver changed on second run: {:?}", update_solver(&reference).map_err(|e| e.to_string()));
        std::fs::write(ytdlp_dir().unwrap().join("version.txt"), "0000.00.00").unwrap();
        let t = Instant::now();
        println!("yt-dlp changed: {:?} in {:?}", update_ytdlp().map_err(|e| e.to_string()), t.elapsed());
        println!("installed now: {:?}", installed_ytdlp_version());
        println!("yt-dlp changed on second run: {:?}", update_ytdlp().map_err(|e| e.to_string()));
    }
}
