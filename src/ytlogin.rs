//! YouTube session capture, run as a separate short-lived process
//! (`reproductor --youtube-login [--silent]`) so the WebView2 engine never
//! lives in the player's memory.
//!
//! A persistent WebView2 profile keeps the YouTube Music sign-in, so a refresh
//! usually just reloads music.youtube.com hidden and re-exports the cookies.
//! The window is shown only when Google actually needs the user to sign in.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::{WebContext, WebView, WebViewBuilder};

pub const ARG: &str = "--youtube-login";
pub const ARG_SILENT: &str = "--silent";

/// Exit codes of the login process.
pub const EXIT_OK: i32 = 0;
pub const EXIT_NOT_SIGNED_IN: i32 = 2;

const MUSIC_URL: &str = "https://music.youtube.com/";
const SIGN_IN_URL: &str = "https://accounts.google.com/ServiceLogin?ltmpl=music&service=youtube&passive=true\
    &continue=https%3A%2F%2Fwww.youtube.com%2Fsignin%3Faction_handle_signin%3Dtrue%26next%3Dhttps%253A%252F%252Fmusic.youtube.com%252F";
/// How long a silent refresh waits for an existing session before giving up or showing the window.
const SILENT_WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(700);

pub fn profile_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("reproductor-rs").join("webview"))
}

/// Entry point of the login process. Never returns.
pub fn run(silent_only: bool) -> ! {
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("Reproductor — Sign in to YouTube Music")
        .with_window_icon(app_icon())
        .with_inner_size(LogicalSize::new(480.0, 720.0))
        .with_visible(false)
        .build(&event_loop)
        .unwrap_or_else(|_| std::process::exit(1));
    let mut context = WebContext::new(profile_dir());
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(MUSIC_URL)
        .build(&window)
        .unwrap_or_else(|_| std::process::exit(1));

    let started = Instant::now();
    let mut shown = false;
    // Dropping the WebView before exiting lets WebView2 flush its cookie store,
    // which the next silent refresh depends on.
    let mut webview = Some(webview);
    event_loop.run(move |event, _, control_flow| {
        let Some(view) = webview.as_ref() else { return };
        let exit_code = match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) | Event::NewEvents(StartCause::Init) => {
                if signed_in(view) {
                    Some(if export_cookies(view).is_ok() { EXIT_OK } else { 1 })
                } else if !shown && started.elapsed() > SILENT_WAIT {
                    if silent_only {
                        Some(EXIT_NOT_SIGNED_IN)
                    } else {
                        shown = true;
                        let _ = view.load_url(SIGN_IN_URL);
                        window.set_visible(true);
                        window.set_focus();
                        None
                    }
                } else {
                    None
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => Some(EXIT_NOT_SIGNED_IN),
            _ => return,
        };
        match exit_code {
            Some(code) => {
                window.set_visible(false);
                webview = None;
                *control_flow = ControlFlow::ExitWithCode(code);
            }
            None => *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL),
        }
    })
}

/// The app logo, for the sign-in window's title bar and taskbar entry.
fn app_icon() -> Option<tao::window::Icon> {
    let png = include_bytes!("../assets/icon-256.png");
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    tao::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Signed in once the page is back on YouTube Music with an authenticated session.
fn signed_in(webview: &WebView) -> bool {
    let on_music = webview.url().is_ok_and(|u| u.starts_with(MUSIC_URL));
    if !on_music {
        return false;
    }
    let cookies = webview.cookies().unwrap_or_default();
    let has = |name: &str| cookies.iter().any(|c| c.name() == name && is_youtube(c.domain()));
    has("LOGIN_INFO") && (has("SAPISID") || has("__Secure-3PAPISID"))
}

fn is_youtube(domain: Option<&str>) -> bool {
    domain.is_some_and(|d| d.trim_start_matches('.') == "youtube.com" || d.ends_with(".youtube.com"))
}

/// Write the youtube.com cookies in Netscape format (what yt-dlp reads), atomically.
fn export_cookies(webview: &WebView) -> std::io::Result<()> {
    let mut out = String::from("# Netscape HTTP Cookie File\n# Written by Reproductor. Do not share: this is a YouTube login.\n\n");
    for c in webview.cookies().unwrap_or_default() {
        let Some(domain) = c.domain().filter(|_| is_youtube(c.domain())) else { continue };
        let domain = if domain.starts_with('.') || c.name().starts_with("__Host-") {
            domain.to_string()
        } else {
            format!(".{domain}")
        };
        let include_subdomains = if domain.starts_with('.') { "TRUE" } else { "FALSE" };
        let expires = c.expires_datetime().map(|t| t.unix_timestamp().max(0)).unwrap_or(0);
        out.push_str(&format!(
            "{}{domain}\t{include_subdomains}\t{}\t{}\t{expires}\t{}\t{}\n",
            if c.http_only().unwrap_or(false) { "#HttpOnly_" } else { "" },
            c.path().unwrap_or("/"),
            if c.secure().unwrap_or(false) { "TRUE" } else { "FALSE" },
            c.name(),
            c.value(),
        ));
    }
    let target = crate::youtube::cookies_file().ok_or(std::io::ErrorKind::NotFound)?;
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let staged = target.with_extension("txt.new");
    std::fs::write(&staged, out)?;
    std::fs::rename(staged, target)
}
