// Settings: small text files in %APPDATA%\AionPingMeter.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let app_data = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let dir = app_data.join("AionPingMeter");
        // Migrate settings from early builds that were simply called PingMeter.
        let legacy = app_data.join("PingMeter");
        if !dir.exists() && legacy.is_dir() {
            let _ = fs::rename(&legacy, &dir);
        }
        let _ = fs::create_dir_all(&dir);
        dir
    })
}

fn file(name: &str) -> PathBuf {
    dir().join(name)
}

/// The saved overlay position, if there is one (on-screen checks are done by the caller).
pub fn load_position() -> Option<(i32, i32)> {
    let text = fs::read_to_string(file("position.txt")).ok()?;
    let (x, y) = text.trim().split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

pub fn save_position(x: i32, y: i32) {
    let _ = fs::write(file("position.txt"), format!("{x},{y}"));
}

pub fn load_threshold() -> i32 {
    fs::read_to_string(file("threshold.txt")).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(100)
}

pub fn save_threshold(ms: i32) {
    let _ = fs::write(file("threshold.txt"), ms.to_string());
}

pub fn load_show_server() -> bool {
    file("show-server.txt").exists()
}

pub fn save_show_server(show: bool) {
    let path = file("show-server.txt");
    let _ = if show { fs::write(path, "1") } else { fs::remove_file(path) };
}
