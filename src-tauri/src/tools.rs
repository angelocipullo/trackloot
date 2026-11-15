// Programmi esterni usati da TrackLoot: yt-dlp, ffmpeg/ffprobe e deno
// (serve a yt-dlp per YouTube). Se non sono già installati nel sistema,
// l'app li scarica nella propria cartella `bin`, che sta in testa al PATH.

use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

pub const TOOLS: [&str; 4] = ["yt-dlp", "ffmpeg", "ffprobe", "deno"];

/// Stato dell'installazione in corso, letto dall'interfaccia.
struct Install {
    running: bool,
    message: String,
    progress: f64,
    error: Option<String>,
}

static INSTALL: Mutex<Install> = Mutex::new(Install {
    running: false,
    message: String::new(),
    progress: 0.0,
    error: None,
});

/// Un download: url e file da estrarre (suffisso nello zip → nome in bin).
/// `files` vuoto = il download è già l'eseguibile.
struct Source {
    label: &'static str,
    url: String,
    raw_name: &'static str,
    files: Vec<(&'static str, &'static str)>,
    extract_dir: Option<&'static str>, // estrae tutto ciò che sta sotto questa cartella
}

/// Comando senza finestra di console su Windows.
pub fn cmd(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

pub fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

pub fn bin_dir() -> PathBuf {
    let dir = crate::data_dir().join("bin");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn on_path(name: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|d| d.join(exe(name)).is_file())
}

pub fn missing() -> Vec<&'static str> {
    TOOLS.iter().copied().filter(|t| !on_path(t)).collect()
}

pub fn ytdlp_version() -> Option<String> {
    let out = cmd("yt-dlp").arg("--version").output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sources(tool: &str) -> Vec<Source> {
    let gh = "https://github.com";
    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "amd64" };
    let deno_target = if cfg!(windows) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-apple-darwin"
    };
    match (tool, cfg!(windows)) {
        ("yt-dlp", false) => vec![Source {
            label: "yt-dlp",
            url: format!("{gh}/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos"),
            raw_name: "yt-dlp", files: vec![], extract_dir: None,
        }],
        ("yt-dlp", true) => vec![Source {
            label: "yt-dlp",
            url: format!("{gh}/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe"),
            raw_name: "yt-dlp.exe", files: vec![], extract_dir: None,
        }],
        ("ffmpeg" | "ffprobe", false) => vec![Source {
            label: if tool == "ffmpeg" { "ffmpeg" } else { "ffprobe" },
            url: format!("https://ffmpeg.martin-riedl.de/redirect/latest/macos/{arch}/release/{tool}.zip"),
            raw_name: "",
            files: vec![if tool == "ffmpeg" { ("ffmpeg", "ffmpeg") } else { ("ffprobe", "ffprobe") }],
            extract_dir: None,
        }],
        // su Windows ffmpeg e ffprobe arrivano insieme (con le loro dll)
        ("ffmpeg" | "ffprobe", true) => vec![Source {
            label: "ffmpeg",
            url: format!("{gh}/yt-dlp/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl-shared.zip"),
            raw_name: "", files: vec![], extract_dir: Some("bin/"),
        }],
        ("deno", _) => vec![Source {
            label: "deno",
            url: format!("{gh}/denoland/deno/releases/latest/download/deno-{deno_target}.zip"),
            raw_name: "",
            files: vec![if cfg!(windows) { ("deno.exe", "deno.exe") } else { ("deno", "deno") }],
            extract_dir: None,
        }],
        _ => vec![],
    }
}

fn set_status(f: impl FnOnce(&mut Install)) {
    f(&mut INSTALL.lock().unwrap());
}

/// Scarica `url` in memoria aggiornando la percentuale.
fn download(url: &str, label: &str, step: usize, steps: usize) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url).call().map_err(|e| format!("{label}: {e}"))?;
    let total: f64 = resp.header("Content-Length").and_then(|l| l.parse().ok()).unwrap_or(0.0);
    let mut reader = resp.into_reader();
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        let n = reader.read(&mut chunk).map_err(|e| format!("{label}: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if total > 0.0 {
            let part = buf.len() as f64 / total;
            set_status(|s| s.progress = (step as f64 + part) / steps as f64 * 100.0);
        }
    }
    Ok(buf)
}

fn write_exe(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("part");
    fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(data))
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn install_source(src: &Source, step: usize, steps: usize) -> Result<(), String> {
    set_status(|s| s.message = format!("Downloading {}…", src.label));
    let data = download(&src.url, src.label, step, steps)?;
    let bin = bin_dir();
    if !src.raw_name.is_empty() {
        return write_exe(&bin.join(src.raw_name), &data);
    }
    set_status(|s| s.message = format!("Unpacking {}…", src.label));
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| e.to_string())?;
    let mut found = 0;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        let dest = if let Some(dir) = src.extract_dir {
            name.find(dir).map(|i| name[i + dir.len()..].to_string()).filter(|n| !n.contains('/'))
        } else {
            src.files.iter()
                .find(|(suffix, _)| name == *suffix || name.ends_with(&format!("/{suffix}")))
                .map(|(_, out)| out.to_string())
        };
        if let Some(dest) = dest {
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
            write_exe(&bin.join(dest), &data)?;
            found += 1;
        }
    }
    if found == 0 {
        return Err(format!("{}: nothing to extract", src.label));
    }
    Ok(())
}

/// Installa i programmi mancanti (e yt-dlp se `update_ytdlp`), in background.
pub fn start_install(update_ytdlp: bool) {
    {
        let mut s = INSTALL.lock().unwrap();
        if s.running {
            return;
        }
        *s = Install { running: true, message: "Starting…".into(), progress: 0.0, error: None };
    }
    std::thread::spawn(move || {
        let mut todo = missing();
        if update_ytdlp && !todo.contains(&"yt-dlp") {
            todo.push("yt-dlp");
        }
        // ffmpeg e ffprobe su Windows sono un unico download
        let mut list: Vec<Source> = Vec::new();
        for t in todo {
            for src in sources(t) {
                if !list.iter().any(|s| s.url == src.url) {
                    list.push(src);
                }
            }
        }
        let steps = list.len().max(1);
        let mut error = None;
        for (i, src) in list.iter().enumerate() {
            if let Err(e) = install_source(src, i, steps) {
                error = Some(e);
                break;
            }
        }
        set_status(|s| {
            s.running = false;
            s.progress = 100.0;
            s.message = if error.is_some() { "Install failed".into() } else { "Ready".into() };
            s.error = error;
        });
    });
}

pub fn status() -> Value {
    let s = INSTALL.lock().unwrap();
    json!({
        "ytdlp": if s.running { None } else { ytdlp_version() },
        "managed": bin_dir().join(exe("yt-dlp")).is_file(),
        "missing": missing(),
        "installing": s.running,
        "message": s.message,
        "progress": s.progress,
        "error": s.error,
    })
}
