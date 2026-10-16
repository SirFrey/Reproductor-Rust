//! YouTube's `n`/signature challenges, solved by yt-dlp's official EJS solver
//! scripts running in an embedded QuickJS-ng engine. The engine is created for
//! each call and dropped afterwards, so it only uses memory while solving.

use std::path::PathBuf;
use std::sync::Mutex;

use rquickjs::{Context, Runtime};
use serde_json::{Value, json};

use crate::Res;

/// One engine at a time keeps peak memory bounded (~80 MB per solve).
static ENGINE: Mutex<()> = Mutex::new(());
/// QuickJS recurses on the C stack while parsing the ~4 MB player: give its
/// thread a large stack (reserved, only committed as used).
const THREAD_STACK: usize = 128 << 20;
const JS_STACK: usize = 100 << 20;
const JS_MEMORY_LIMIT: usize = 768 << 20;

/// yt-dlp's EJS solver scripts: a copy in the app folder, or inside the yt-dlp build.
pub fn solver_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("reproductor-rs");
    [dir.join("ejs"), dir.join("yt-dlp/_internal/yt_dlp_ejs/yt/solver")]
        .into_iter()
        .find(|d| d.join("lib.min.js").is_file() && d.join("core.min.js").is_file())
}

/// Identifies the solver scripts in use, so a player preprocessed by an older
/// solver is redone after an update.
pub fn solver_fingerprint() -> String {
    use sha3::{Digest, Sha3_256};
    let Some(dir) = solver_dir() else { return String::new() };
    let mut h = Sha3_256::new();
    for f in ["lib.min.js", "core.min.js"] {
        h.update(std::fs::read(dir.join(f)).unwrap_or_default());
    }
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn run(input: Value) -> Res<Value> {
    let _engine = ENGINE.lock().unwrap();
    let dir = solver_dir().ok_or("solver: scripts not found")?;
    let lib = std::fs::read_to_string(dir.join("lib.min.js"))?;
    let core = std::fs::read_to_string(dir.join("core.min.js"))?;
    let input = input.to_string();

    let output = std::thread::Builder::new()
        .name("jsc".into())
        .stack_size(THREAD_STACK)
        .spawn(move || -> Result<String, String> {
            let rt = Runtime::new().map_err(|e| e.to_string())?;
            rt.set_max_stack_size(JS_STACK);
            rt.set_memory_limit(JS_MEMORY_LIMIT);
            let ctx = Context::full(&rt).map_err(|e| e.to_string())?;
            ctx.with(|ctx| {
                ctx.eval::<(), _>(format!("{lib}\nObject.assign(globalThis, lib);\n{core}\n")).map_err(|e| e.to_string())?;
                ctx.globals().set("__in", input).map_err(|e| e.to_string())?;
                ctx.eval::<String, _>("JSON.stringify(jsc(JSON.parse(__in)))").map_err(|e| e.to_string())
            })
        })?
        .join()
        .map_err(|_| "solver: engine thread crashed")?
        .map_err(|e| format!("solver: {e}"))?;

    release_heap();
    let v: Value = serde_json::from_str(&output)?;
    if v["type"] == "error" {
        return Err(format!("solver: {}", v["error"]).into());
    }
    Ok(v)
}

/// A solve allocates ~80 MB that the heap would otherwise keep committed after the
/// engine is dropped; hand it back to the OS.
fn release_heap() {
    #[cfg(windows)]
    unsafe {
        unsafe extern "C" {
            fn _heapmin() -> i32;
        }
        _heapmin();
    }
}

/// Parse and rewrite a new player version once (~10 s); the result is cached by
/// the caller and makes every later solve fast.
pub fn preprocess(player_code: &str) -> Res<String> {
    let v = run(json!({ "type": "player", "player": player_code, "requests": [], "output_preprocessed": true }))?;
    Ok(v["preprocessed_player"].as_str().ok_or("solver: no preprocessed player")?.to_string())
}

/// Solve challenges (`[{"type": "n" | "sig", "challenges": [...]}]`) against the
/// preprocessed player (~0.4 s). Returns one `{challenge: answer}` map per request.
pub fn solve(preprocessed: &str, requests: Value) -> Res<Vec<Value>> {
    let v = run(json!({ "type": "preprocessed", "preprocessed_player": preprocessed, "requests": requests }))?;
    v["responses"]
        .as_array()
        .ok_or("solver: no responses")?
        .iter()
        .map(|r| match r["type"].as_str() {
            Some("error") => Err(format!("solver: {}", r["error"]).into()),
            _ => Ok(r["data"].clone()),
        })
        .collect()
}
