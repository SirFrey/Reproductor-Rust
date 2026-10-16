//! Probe: run yt-dlp's official EJS solver scripts in embedded QuickJS-ng and
//! time each stage. Usage: qjs_probe <probe dir> <solver dir>
//! (probe dir holds player.js, requests.json, node_answers.json).

use std::time::Instant;

use rquickjs::{Context, Runtime};

fn solve(ctx: &Context, input: &str) -> String {
    ctx.with(|ctx| {
        ctx.globals().set("__in", input).unwrap();
        ctx.eval::<String, _>("JSON.stringify(jsc(JSON.parse(__in)))").unwrap()
    })
}

fn load_scripts(rt: &Runtime, lib: &str, core: &str) -> Context {
    let ctx = Context::full(rt).unwrap();
    ctx.with(|ctx| {
        ctx.eval::<(), _>(format!("{lib}\nObject.assign(globalThis, lib);\n{core}\n")).unwrap();
    });
    ctx
}

fn mem(rt: &Runtime) -> String {
    let m = rt.memory_usage();
    format!("{:.1} MB", m.malloc_size as f64 / 1048576.0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (dir, solver) = (args[1].clone(), args[2].clone());
    let cached_only = args.get(3).is_some_and(|a| a == "cached");
    // QuickJS recurses on the C stack while parsing: give it plenty.
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || if cached_only { run_cached(&dir, &solver) } else { run(&dir, &solver) })
        .unwrap()
        .join()
        .unwrap();
}

fn run(dir: &str, solver: &str) {
    let read = |p: String| std::fs::read_to_string(p).unwrap();
    let lib = read(format!("{solver}/lib.min.js"));
    let core = read(format!("{solver}/core.min.js"));
    let player = read(format!("{dir}/player.js"));
    let requests: serde_json::Value = serde_json::from_str(&read(format!("{dir}/requests.json"))).unwrap();
    let node: serde_json::Value = serde_json::from_str(&read(format!("{dir}/node_answers.json"))).unwrap();

    let rt = Runtime::new().unwrap();
    rt.set_max_stack_size(200 << 20);

    let t = Instant::now();
    let ctx = load_scripts(&rt, &lib, &core);
    println!("1. load solver scripts:        {:>7.0} ms   heap {}", t.elapsed().as_secs_f64() * 1e3, mem(&rt));

    let t = Instant::now();
    let input = serde_json::json!({ "type": "player", "player": player, "requests": requests, "output_preprocessed": true });
    let out: serde_json::Value = serde_json::from_str(&solve(&ctx, &input.to_string())).unwrap();
    println!("2. preprocess + solve (cold):  {:>7.0} ms   heap {}", t.elapsed().as_secs_f64() * 1e3, mem(&rt));
    let pre = out["preprocessed_player"].as_str().expect("preprocessed player").to_string();
    std::fs::write(format!("{dir}/preprocessed.js"), &pre).unwrap();
    println!("   type={} answers match Node: {}", out["type"], out["responses"] == node);
    drop(ctx);
    rt.run_gc();

    // What an app start costs: fresh engine, cached preprocessed player.
    let rt2 = Runtime::new().unwrap();
    rt2.set_max_stack_size(200 << 20);
    let t = Instant::now();
    let ctx2 = load_scripts(&rt2, &lib, &core);
    let input = serde_json::json!({ "type": "preprocessed", "preprocessed_player": pre, "requests": requests });
    let out: serde_json::Value = serde_json::from_str(&solve(&ctx2, &input.to_string())).unwrap();
    println!("3. fresh engine, cached solve: {:>7.0} ms   heap {}   match: {}", t.elapsed().as_secs_f64() * 1e3, mem(&rt2), out["responses"] == node);

    let t = Instant::now();
    let out: serde_json::Value = serde_json::from_str(&solve(&ctx2, &input.to_string())).unwrap();
    println!("4. second cached solve:        {:>7.0} ms   heap {}   match: {}", t.elapsed().as_secs_f64() * 1e3, mem(&rt2), out["responses"] == node);
    rt2.run_gc();
    println!("   after GC:                               heap {}", mem(&rt2));
}

/// Steady state only: fresh engine + cached preprocessed player, 5 solves.
fn run_cached(dir: &str, solver: &str) {
    let read = |p: String| std::fs::read_to_string(p).unwrap();
    let lib = read(format!("{solver}/lib.min.js"));
    let core = read(format!("{solver}/core.min.js"));
    let pre = read(format!("{dir}/preprocessed.js"));
    let requests: serde_json::Value = serde_json::from_str(&read(format!("{dir}/requests.json"))).unwrap();
    let node: serde_json::Value = serde_json::from_str(&read(format!("{dir}/node_answers.json"))).unwrap();
    let rt = Runtime::new().unwrap();
    rt.set_max_stack_size(200 << 20);
    let ctx = load_scripts(&rt, &lib, &core);
    let input = serde_json::json!({ "type": "preprocessed", "preprocessed_player": pre, "requests": requests }).to_string();
    for i in 1..=5 {
        let t = Instant::now();
        let out: serde_json::Value = serde_json::from_str(&solve(&ctx, &input)).unwrap();
        println!("cached solve {i}: {:>5.0} ms   heap {}   match: {}", t.elapsed().as_secs_f64() * 1e3, mem(&rt), out["responses"] == node);
        rt.run_gc();
    }
}