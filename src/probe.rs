// Diagnostic modes (no overlay window):
//   --probe [seconds]      progress every second → probe.txt
//   --gaps [seconds]       gaps between data packets on the game connection → gaps.txt
//   --render-demo <folder> sample overlay states as PNGs (README screenshots)

use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::thread;
use std::time::Duration;

use crate::capture::{CaptureManager, find_game_connections};
use crate::render::{self, Color, OverlayModel};
use crate::settings;
use crate::stats::PingStats;
use crate::util::ip_text;

fn opt(v: Option<f64>) -> String {
    v.map_or("-".into(), |x| format!("{x:.1}"))
}

pub fn run(seconds: u32) -> i32 {
    let Ok(mut w) = File::create(settings::dir().join("probe.txt")) else { return 1 };
    let stats = Arc::new(PingStats::new(false));
    {
        let _captures = CaptureManager::start(stats.clone());
        for i in 0..seconds {
            thread::sleep(Duration::from_secs(1));
            let (cur, _, _) = stats.snapshot();
            let (tmin, tmed) = stats.tcp_snapshot();
            let _ = writeln!(
                w,
                "{:>3}s  packets={:>6}  pings={:>4}  game={}  tcpMin2s={}  tcpMed2s={}  tcpN={}  losses={}  freezes={}  {}",
                i + 1,
                stats.packets.load(Relaxed),
                stats.pings.load(Relaxed),
                opt(cur),
                opt(tmin),
                opt(tmed),
                stats.tcp_samples.load(Relaxed),
                stats.losses.load(Relaxed),
                stats.freezes.load(Relaxed),
                stats.debug()
            );
        }
    }
    for c in find_game_connections() {
        let _ = writeln!(w, "conn {} -> {}:{}", ip_text(c.local), ip_text(c.remote), c.remote_port);
    }
    0
}

pub fn gaps(seconds: u32) -> i32 {
    let stats = Arc::new(PingStats::new(true));
    {
        let _captures = CaptureManager::start(stats.clone());
        thread::sleep(Duration::from_secs(seconds as u64));
    }

    let all = stats.trace.as_ref().unwrap().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Ok(mut w) = File::create(settings::dir().join("gaps.txt")) else { return 1 };
    let _ = writeln!(w, "server={} packets={} seconds={seconds}", stats.game_endpoint_text().unwrap_or_default(), all.len());
    for inbound in [true, false] {
        let t: Vec<f64> = all.iter().filter(|x| x.inbound == inbound).map(|x| x.at).collect();
        if t.len() < 2 {
            let _ = writeln!(w, "{}: too few packets", if inbound { "IN" } else { "OUT" });
            continue;
        }
        let gaps: Vec<(f64, f64)> = t.windows(2).map(|p| (p[1] - p[0], p[0])).collect();
        let mut sorted: Vec<f64> = gaps.iter().map(|g| g.0).collect();
        sorted.sort_by(f64::total_cmp);
        let p = |q: f64| sorted[(q * (sorted.len() - 1) as f64) as usize];
        let over = |ms: f64| sorted.iter().filter(|&&g| g > ms).count();
        let _ = writeln!(
            w,
            "{}: n={} rate={:.1}/s  p50={:.1} p90={:.1} p99={:.1} max={:.1} ms  >150:{} >250:{} >500:{} >1000:{}",
            if inbound { "IN  (server->you)" } else { "OUT (you->server)" },
            t.len(),
            t.len() as f64 / seconds as f64,
            p(0.5),
            p(0.9),
            p(0.99),
            sorted[sorted.len() - 1],
            over(150.0),
            over(250.0),
            over(500.0),
            over(1000.0)
        );
        let mut worst = gaps.clone();
        worst.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (gap, at) in worst.into_iter().take(8) {
            let burst = t.iter().filter(|&&x| x > at + gap && x <= at + gap + 30.0).count();
            let _ = writeln!(
                w,
                "    gap {gap:>7.1} ms at +{:>6.1}s, then {} packets within 30 ms",
                (at - t[0]) / 1000.0,
                burst + 1
            );
        }
    }
    0
}

pub fn render_demo(folder: &str) -> i32 {
    let _ = std::fs::create_dir_all(folder);
    let _gdip = render::GdiPlus::start();
    let r = render::Renderer::new();
    let green = Color::rgb(110, 220, 130);
    let s = |v: &str| Some(v.to_string());
    let model = |ping: &str, max: &str, hot: bool, head: Option<String>, tail: Option<String>, flash: bool| OverlayModel {
        quality: green,
        ping: ping.into(),
        network: s("18"),
        max: Some(max.into()),
        max_hot: hot,
        status: None,
        alert_head: head,
        alert_tail: tail,
        flash,
        server: None,
        unlocked: false,
    };
    let states = [
        ("overlay.png", model("38", "39", false, None, None, false)),
        ("overlay-alert.png", model("41", "1200", true, s("ping 1200"), s("server · freeze 450 · just now"), true)),
        ("overlay-recent.png", model("39", "1200", false, s("ping 1200"), s("server · freeze 450 · 34s ago"), false)),
    ];
    for (file, m) in states {
        let path = std::path::Path::new(folder).join(file);
        if !r.save_demo_png(&m, 1.5, 18, Color::rgb(27, 30, 34), &path) {
            return 1;
        }
    }
    0
}
