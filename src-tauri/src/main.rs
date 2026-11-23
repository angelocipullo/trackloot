// TrackLoot: scarica musica da SoundCloud, YouTube e Spotify con yt-dlp + ffmpeg.
//
// SoundCloud e YouTube: scarica l'audio originale.
// Spotify: legge titolo/artista/copertina dalla pagina pubblica, cerca il pezzo
// su YouTube (scegliendo il risultato con la durata più vicina) e lo tagga.
//
// Ogni file scaricato finisce nella cronologia (history.json) con un controllo
// di qualità e i BPM: così non si riscaricano tracce già prese.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bpm;
mod tools;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tools::cmd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

// parole che indicano una versione diversa dall'originale
const BAD_WORDS: &[&str] = &[
    "live", "cover", "karaoke", "instrumental", "remix", "sped up", "slowed",
    "reverb", "8d", "nightcore", "reaction", "tutorial", "1 hour",
];
// bande (Hz) usate per stimare il taglio delle frequenze alte
const BANDS: [f64; 3] = [15000.0, 16500.0, 18500.0];

#[derive(Clone, Serialize)]
struct Track {
    title: String,
    uploader: String,
    thumbnail: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Quality {
    label: String,         // High / Medium / Low / Very low
    cutoff_khz: f64,       // banda più alta con contenuto (0 = sotto 15 kHz)
    bitrate_kbps: u32,     // bitrate dell'mp3 finale
    source: Option<String>, // codec e bitrate della sorgente, es. "opus 64k"
    warning: Option<String>,
}

#[derive(Clone, Serialize)]
struct FileResult {
    name: String,
    quality: Option<Quality>,
    bpm: Option<u32>,
}

#[derive(Clone, Serialize)]
struct Job {
    id: String,
    url: String,
    status: String, // running / done / skipped / error
    progress: f64,
    message: String,
    tracks: Vec<Track>,
    files: Vec<FileResult>,
    errors: Vec<String>,
    skipped: Vec<String>,
    total: Option<usize>,
}

#[derive(Clone, Serialize, Deserialize)]
struct HistoryEntry {
    key: String, // "soundcloud:<id>", "youtube:<id>" o "spotify:<id>"
    source: String,
    title: String,
    artist: String,
    url: String,
    file: String,
    thumbnail: Option<String>,
    quality: Option<Quality>,
    #[serde(default)]
    bpm: Option<u32>,
    date: u64,
}

type Jobs = Arc<Mutex<HashMap<String, Job>>>;

struct AppState {
    jobs: Jobs,
    counter: AtomicU64,
}

static HISTORY: Mutex<Vec<HistoryEntry>> = Mutex::new(Vec::new());

fn update(jobs: &Jobs, id: &str, f: impl FnOnce(&mut Job)) {
    if let Some(job) = jobs.lock().unwrap().get_mut(id) {
        f(job);
    }
}

fn downloads_dir() -> PathBuf {
    dirs::download_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Downloads"))
}

/// ~/Library/Application Support/… su macOS, %APPDATA%\… su Windows.
pub fn data_dir() -> PathBuf {
    let dir = dirs::data_dir().unwrap_or_default().join("it.angelocipullo.trackloot");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn file_name(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

// ---------- cronologia ----------

fn history_path() -> PathBuf {
    data_dir().join("history.json")
}

fn load_history() {
    if let Ok(s) = fs::read_to_string(history_path()) {
        if let Ok(h) = serde_json::from_str(&s) {
            *HISTORY.lock().unwrap() = h;
        }
    }
}

fn add_history(entry: HistoryEntry) {
    let mut h = HISTORY.lock().unwrap();
    h.retain(|e| e.key != entry.key);
    h.push(entry);
    let _ = fs::write(history_path(), serde_json::to_string_pretty(&*h).unwrap_or_default());
}

fn history_has(key: &str) -> bool {
    HISTORY.lock().unwrap().iter().any(|e| e.key == key)
}

/// Archivio di yt-dlp per SoundCloud e YouTube, rigenerato dalla cronologia:
/// yt-dlp salta da solo le tracce già scaricate (utile per set e playlist).
fn write_archive() -> PathBuf {
    let path = data_dir().join("download-archive.txt");
    let lines: String = HISTORY.lock().unwrap().iter()
        .filter_map(|e| e.key.split_once(':'))
        .filter(|(src, _)| matches!(*src, "soundcloud" | "youtube"))
        .map(|(src, id)| format!("{src} {id}\n"))
        .collect();
    let _ = fs::write(&path, lines);
    path
}

// ---------- qualità ----------

/// Stima la qualità reale: cerca fino a che frequenza arriva il segnale
/// (un mp3 a 128k taglia a ~16 kHz anche se convertito a 320) e la combina
/// con il bitrate della sorgente.
fn analyze(path: &Path, src_codec: Option<&str>, src_abr: Option<f64>, warning: Option<String>) -> Option<Quality> {
    let splits: String = (0..BANDS.len()).map(|i| format!("[s{i}]")).collect();
    let mut graph = format!("[0:a]aresample=44100,asplit={}{splits}", BANDS.len());
    for (i, f) in BANDS.iter().enumerate() {
        graph += &format!(";[s{i}]highpass=f={f}:poles=2,highpass=f={f}:poles=2,volumedetect[o{i}]");
    }
    let mut ff = cmd("ffmpeg");
    ff.args(["-hide_banner", "-nostats", "-i"]).arg(path).args(["-filter_complex", &graph]);
    for i in 0..BANDS.len() {
        ff.args(["-map", &format!("[o{i}]"), "-f", "null", "-"]);
    }
    let out = ff.output().ok()?;
    let log = String::from_utf8_lossy(&out.stderr);
    let re = Regex::new(r"Parsed_volumedetect_(\d+) .*max_volume: (-?[\d.]+) dB").unwrap();
    let mut levels: Vec<(u32, f64)> = re.captures_iter(&log)
        .filter_map(|c| Some((c[1].parse().ok()?, c[2].parse().ok()?)))
        .collect();
    levels.sort_by_key(|l| l.0);
    if levels.len() != BANDS.len() {
        return None;
    }
    // banda più alta con segnale udibile
    let top = levels.iter().rposition(|l| l.1 > -60.0);
    let cutoff_khz = top.map_or(0.0, |i| BANDS[i] / 1000.0);
    let cutoff_score = top.map_or(0, |i| i + 1); // 0..=3

    // opus/aac rendono come un mp3 a bitrate ~1.5x
    let codec = match src_codec.unwrap_or("") {
        c if c.starts_with("mp4a") => "aac",
        c => c,
    };
    let efficient = matches!(codec, "aac" | "opus" | "vorbis");
    let abr_score = match src_abr.map(|a| if efficient { a * 1.5 } else { a }) {
        None => 3,
        Some(a) if a >= 160.0 => 3,
        Some(a) if a >= 100.0 => 2,
        Some(a) if a >= 64.0 => 1,
        _ => 0,
    };
    let label = ["Very low", "Low", "Medium", "High"][cutoff_score.min(abr_score)];

    let bitrate_kbps = cmd("ffprobe")
        .args(["-v", "error", "-show_entries", "format=bit_rate", "-of", "csv=p=0"])
        .arg(path)
        .output().ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
        .map_or(0, |b| (b / 1000.0).round() as u32);

    let source = Some(codec).filter(|c| !c.is_empty() && *c != "none").map(|c| match src_abr {
        Some(a) => format!("{c} {}k", a.round()),
        None => c.to_string(),
    });
    Some(Quality { label: label.into(), cutoff_khz, bitrate_kbps, source, warning })
}

/// Calcola i BPM e li scrive nel tag dell'mp3.
fn add_bpm(jobs: &Jobs, id: &str, path: &Path) -> Option<u32> {
    update(jobs, id, |j| j.message = "Finding BPM…".into());
    let bpm = bpm::detect(path)?;
    let _ = bpm::write_tag(path, bpm);
    Some(bpm)
}

// ---------- yt-dlp ----------

/// Esegue yt-dlp aggiornando la progress bar; con `show_tracks` mostra
/// le tracce che trova. Ritorna (ok, [(filepath, info della traccia)]).
fn run_ytdlp(jobs: &Jobs, id: &str, args: &[String], show_tracks: bool) -> (bool, Vec<(String, Value)>) {
    let child = cmd("yt-dlp")
        .args([
            "--newline", "--no-simulate", "--no-quiet", "--progress",
            "--progress-template", "download:PROG %(progress._percent_str)s",
            "--print", "before_dl:INFO %(.{id,title,uploader,artist,track,thumbnail,webpage_url,acodec,abr,duration})j",
            "--print", "after_move:FILE %(filepath)s",
        ])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            update(jobs, id, |j| j.message = format!("Can't start yt-dlp: {e}"));
            return (false, vec![]);
        }
    };

    let pct = Regex::new(r"([\d.]+)%").unwrap();
    // SoundCloud scrive "<id>: <titolo>", YouTube solo "<id>:"
    let archived = Regex::new(r"^\[download\] (?:[\w-]+: )?(.*?):? has already been recorded in the archive").unwrap();
    // stdout e stderr letti insieme, riga per riga
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let pipes: [Box<dyn Read + Send>; 2] = [
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    for pipe in pipes {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);

    let mut files = vec![];
    let mut info = Value::Null;
    for line in rx {
        let line = line.trim().to_string();
        update(jobs, id, |j| {
            if let Some(rest) = line.strip_prefix("PROG ") {
                if let Some(m) = pct.captures(rest) {
                    j.progress = m[1].parse().unwrap_or(j.progress);
                    j.message = "Downloading…".into();
                }
            } else if let Some(rest) = line.strip_prefix("INFO ") {
                if let Ok(v) = serde_json::from_str::<Value>(rest) {
                    if show_tracks { j.tracks.push(Track {
                        title: v["track"].as_str().or(v["title"].as_str()).unwrap_or("").into(),
                        uploader: v["artist"].as_str().or(v["uploader"].as_str()).unwrap_or("").into(),
                        thumbnail: v["thumbnail"].as_str().map(String::from),
                    }); }
                    j.progress = 0.0;
                    info = v;
                }
            } else if let Some(rest) = line.strip_prefix("FILE ") {
                files.push((rest.to_string(), info.clone()));
            } else if let Some(c) = archived.captures(&line) {
                j.skipped.push(c[1].to_string());
            } else if line.starts_with("ERROR") {
                j.message = line.clone();
            } else if line.contains("[ExtractAudio]") || line.contains("[EmbedThumbnail]") {
                j.message = "Converting to mp3…".into();
            }
        });
    }
    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    (ok, files)
}

// ---------- SoundCloud e YouTube ----------

/// Scarica l'audio originale; `source` è "soundcloud" o "youtube".
fn run_direct(jobs: &Jobs, id: &str, url: &str, source: &str, force: bool) {
    // su YouTube il titolo spesso contiene già l'artista: se yt-dlp conosce
    // artista e brano (canali "Topic", YouTube Music) usa quelli
    let out = downloads_dir().join("%(artist,uploader)s - %(track,title)s.%(ext)s");
    let mut args: Vec<String> = [
        "-x", "--audio-format", "mp3", "--audio-quality", "0",
        "--embed-thumbnail", "--embed-metadata",
        "-o", &out.to_string_lossy(),
    ]
    .iter().map(|s| s.to_string()).collect();
    if source == "youtube" {
        args.extend([
            // un link watch?v=…&list=… scarica solo il video, non tutta la playlist
            "--no-playlist",
            // copertina ritagliata quadrata come quelle degli album
            "--convert-thumbnails", "jpg",
            "--ppa", r#"ThumbnailsConvertor+FFmpeg_o:-c:v mjpeg -vf crop="'if(gt(ih,iw),iw,ih)':'if(gt(iw,ih),ih,iw)'""#,
            // "Artista - Brano (Official Video)" → artista e brano separati e puliti
            "--parse-metadata", r"title:^(?P<artist>.+?) [-–—] (?P<track>.+)$",
            "--replace-in-metadata", "track",
            r"\s*[(\[][^)\]]*\b(?i:official|lyrics?|audio|video|visuali[sz]er|hd|hq|4k|remaster(?:ed)?)\b[^)\]]*[)\]]", "",
            "--replace-in-metadata", "uploader", " - Topic$", "",
            // niente descrizione del video (link, testi, hashtag) nei tag
            "--parse-metadata", ":(?P<meta_description>)",
            "--parse-metadata", ":(?P<meta_synopsis>)",
        ].map(String::from));
    }
    if !force {
        args.push("--download-archive".into());
        args.push(write_archive().to_string_lossy().into_owned());
    }
    args.push(url.into());

    let (_, files) = run_ytdlp(jobs, id, &args, true);
    for (path, info) in files {
        update(jobs, id, |j| j.message = "Checking quality…".into());
        let s = |k: &str| info[k].as_str().unwrap_or("").to_string();
        let or = |a: &str, b: &str| info[a].as_str().map(String::from).unwrap_or_else(|| s(b));
        let quality = analyze(Path::new(&path), info["acodec"].as_str(), info["abr"].as_f64(), None);
        let bpm = add_bpm(jobs, id, Path::new(&path));
        let name = file_name(&path);
        add_history(HistoryEntry {
            key: format!("{source}:{}", s("id")),
            source: source.into(),
            title: or("track", "title"),
            artist: or("artist", "uploader"),
            url: s("webpage_url"),
            file: name.clone(),
            thumbnail: info["thumbnail"].as_str().map(String::from),
            quality: quality.clone(),
            bpm,
            date: now(),
        });
        update(jobs, id, |j| j.files.push(FileResult { name, quality, bpm }));
    }
}

// ---------- Spotify ----------

fn fetch(url: &str) -> Result<(String, Vec<u8>), String> {
    let resp = ureq::get(url)
        .set("User-Agent", "Mozilla/5.0")
        .call()
        .map_err(|e| e.to_string())?;
    let final_url = resp.get_url().to_string();
    let mut buf = vec![];
    resp.into_reader().read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok((final_url, buf))
}

fn spotify_entity(kind: &str, sid: &str) -> Result<Value, String> {
    let (_, html) = fetch(&format!("https://open.spotify.com/embed/{kind}/{sid}"))?;
    let html = String::from_utf8_lossy(&html);
    let re = Regex::new(r#"(?s)<script id="__NEXT_DATA__" type="application/json">(.*?)</script>"#).unwrap();
    let json = re.captures(&html).ok_or("can't read the Spotify page")?;
    let data: Value = serde_json::from_str(&json[1]).map_err(|e| e.to_string())?;
    data.pointer("/props/pageProps/state/data/entity")
        .cloned()
        .ok_or_else(|| "Spotify data missing".into())
}

fn biggest_image(entity: &Value) -> Option<String> {
    let imgs = entity.pointer("/visualIdentity/image")
        .or_else(|| entity.pointer("/coverArt/sources"))?
        .as_array()?;
    let size = |i: &Value| i["maxHeight"].as_u64().or(i["height"].as_u64()).unwrap_or(0);
    imgs.iter().max_by_key(|i| size(i))?["url"].as_str().map(String::from)
}

/// Ritorna (video id, durata) del risultato YouTube più simile.
fn pick_youtube(query: &str, duration: f64, title: &str) -> Option<(String, f64)> {
    let out = cmd("yt-dlp")
        .args(["--flat-playlist", "--print", "%(id)s\t%(duration)s\t%(title)s"])
        .arg(format!("ytsearch8:{query}"))
        .output()
        .ok()?;
    let title = title.to_lowercase();
    let mut best: Option<(f64, String, f64)> = None;
    for (i, line) in String::from_utf8_lossy(&out.stdout).lines().enumerate() {
        let mut parts = line.splitn(3, '\t');
        let (Some(vid), Some(dur), yt_title) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Ok(dur) = dur.parse::<f64>() else { continue };
        let yt_title = yt_title.unwrap_or("").to_lowercase();
        // i risultati in alto contano di più
        let mut score = (dur - duration).abs() + i as f64 * 2.0;
        score += BAD_WORDS.iter()
            .filter(|w| yt_title.contains(*w) && !title.contains(*w))
            .count() as f64 * 60.0;
        if best.as_ref().map_or(true, |(s, _, _)| score < *s) {
            best = Some((score, vid.to_string(), dur));
        }
    }
    best.map(|(_, v, d)| (v, d))
}

fn safe_name(s: &str) -> String {
    let s: String = s.chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '-' } else { c })
        .collect();
    s.trim().chars().take(180).collect()
}

struct SpotifyItem {
    id: String,
    title: String,
    artists: String,
    duration: f64,
    entity: Option<Value>,
}

fn run_spotify(jobs: &Jobs, id: &str, url: &str, force: bool) -> Result<(), String> {
    let (final_url, _) = fetch(url)?; // risolve anche i link spotify.link
    let re = Regex::new(r"/(track|album|playlist)/([A-Za-z0-9]+)").unwrap();
    let caps = re.captures(&final_url)
        .ok_or("Unsupported Spotify link (use a track, album or playlist)")?;
    let (kind, sid) = (caps[1].to_string(), caps[2].to_string());
    let entity = spotify_entity(&kind, &sid)?;
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();

    let items: Vec<SpotifyItem> = if kind == "track" {
        let artists = entity["artists"].as_array().into_iter().flatten()
            .map(|a| s(&a["name"])).collect::<Vec<_>>().join(", ");
        vec![SpotifyItem {
            id: sid.clone(),
            title: s(&entity["title"]),
            artists,
            duration: entity["duration"].as_f64().unwrap_or(0.0) / 1000.0,
            entity: Some(entity.clone()),
        }]
    } else {
        entity["trackList"].as_array().into_iter().flatten()
            .filter(|t| t["entityType"] == "track")
            .map(|t| SpotifyItem {
                id: s(&t["uri"]).rsplit(':').next().unwrap_or("").to_string(),
                title: s(&t["title"]),
                artists: s(&t["subtitle"]).replace('\u{a0}', " "),
                duration: t["duration"].as_f64().unwrap_or(0.0) / 1000.0,
                entity: None,
            })
            .collect()
    };
    let album = (kind == "album").then(|| s(&entity["name"]));
    let total = items.len();
    update(jobs, id, |j| j.total = Some(total));

    let tmp = std::env::temp_dir().join(format!("trackloot-{id}"));
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;

    for (n, it) in items.iter().enumerate() {
        if !force && history_has(&format!("spotify:{}", it.id)) {
            update(jobs, id, |j| j.skipped.push(it.title.clone()));
            continue;
        }
        // una traccia rotta non blocca le altre
        if let Err(e) = spotify_track(jobs, id, it, n + 1, album.as_deref(), &tmp) {
            update(jobs, id, |j| j.errors.push(format!("{}: {e}", it.title)));
        }
    }
    let _ = fs::remove_dir_all(&tmp);
    Ok(())
}

fn spotify_track(jobs: &Jobs, id: &str, it: &SpotifyItem, n: usize,
                 album: Option<&str>, tmp: &Path) -> Result<(), String> {
    let track_entity = match &it.entity {
        Some(e) => e.clone(),
        None => spotify_entity("track", &it.id)?,
    };
    let cover = biggest_image(&track_entity);
    update(jobs, id, |j| {
        j.tracks.push(Track {
            title: it.title.clone(),
            uploader: it.artists.clone(),
            thumbnail: cover.clone(),
        });
        j.progress = 0.0;
        j.message = "Searching YouTube…".into();
    });

    let main_artist = it.artists.split(',').next().unwrap_or("");
    let (vid, yt_duration) = pick_youtube(&format!("{main_artist} - {}", it.title), it.duration, &it.title)
        .ok_or("not found on YouTube")?;

    let out = tmp.join(format!("{n}.%(ext)s"));
    let args: Vec<String> = vec![
        "-x".into(), "--audio-format".into(), "mp3".into(), "--audio-quality".into(), "0".into(),
        "-o".into(), out.to_string_lossy().into_owned(),
        format!("https://www.youtube.com/watch?v={vid}"),
    ];
    let (ok, files) = run_ytdlp(jobs, id, &args, false);
    let Some((file, info)) = files.into_iter().next().filter(|_| ok) else {
        return Err("download failed".into());
    };

    update(jobs, id, |j| j.message = "Adding cover & tags…".into());
    let name = format!("{}.mp3", safe_name(&format!("{} - {}", it.artists, it.title)));
    let dest = downloads_dir().join(&name);
    let mut ff = cmd("ffmpeg");
    ff.args(["-y", "-loglevel", "error", "-i", &file]);
    if let Some(url) = &cover {
        let cover_path = tmp.join(format!("{n}.jpg"));
        fs::write(&cover_path, fetch(url)?.1).map_err(|e| e.to_string())?;
        ff.arg("-i").arg(&cover_path)
            .args(["-map", "0:a", "-map", "1",
                   "-metadata:s:v", "title=Album cover",
                   "-metadata:s:v", "comment=Cover (front)"]);
    }
    ff.args(["-c", "copy", "-id3v2_version", "3"])
        .arg("-metadata").arg(format!("title={}", it.title))
        .arg("-metadata").arg(format!("artist={}", it.artists));
    if let Some(album) = album {
        ff.arg("-metadata").arg(format!("album={album}"))
            .arg("-metadata").arg(format!("track={n}"));
    }
    let status = ff.arg(&dest).status().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("ffmpeg failed".into());
    }

    update(jobs, id, |j| j.message = "Checking quality…".into());
    let diff = (yt_duration - it.duration).abs();
    let warning = (diff > 5.0).then(|| format!(
        "Length differs from Spotify by {}s: might be a different version", diff.round()));
    let quality = analyze(&dest, info["acodec"].as_str(), info["abr"].as_f64(), warning);
    let bpm = add_bpm(jobs, id, &dest);

    add_history(HistoryEntry {
        key: format!("spotify:{}", it.id),
        source: "spotify".into(),
        title: it.title.clone(),
        artist: it.artists.clone(),
        url: format!("https://open.spotify.com/track/{}", it.id),
        file: name.clone(),
        thumbnail: cover,
        quality: quality.clone(),
        bpm,
        date: now(),
    });
    update(jobs, id, |j| j.files.push(FileResult { name, quality, bpm }));
    Ok(())
}

// ---------- job ----------

fn is_youtube(url: &str) -> bool {
    Regex::new(r"^https?://(([\w-]+\.)?youtube\.com|youtu\.be)/").unwrap().is_match(url)
}

fn run_job(jobs: Jobs, id: String, url: String, force: bool) {
    if url.contains("spotify.") {
        if let Err(e) = run_spotify(&jobs, &id, &url, force) {
            update(&jobs, &id, |j| j.message = format!("Error: {e}"));
        }
    } else if is_youtube(&url) {
        run_direct(&jobs, &id, &url, "youtube", force);
    } else {
        run_direct(&jobs, &id, &url, "soundcloud", force);
    }
    update(&jobs, &id, |j| {
        let extra = |j: &Job| {
            let mut parts = vec![];
            if !j.skipped.is_empty() { parts.push(format!("{} already downloaded", j.skipped.len())); }
            if !j.errors.is_empty() { parts.push(format!("{} failed", j.errors.len())); }
            parts.iter().map(|p| format!(" · {p}")).collect::<String>()
        };
        if !j.files.is_empty() {
            j.status = "done".into();
            j.progress = 100.0;
            j.message = format!("Done{}", extra(j));
        } else if !j.skipped.is_empty() && j.errors.is_empty() {
            j.status = "skipped".into();
            j.message = if j.skipped.len() == 1 {
                "Already downloaded".into()
            } else {
                format!("All {} tracks already downloaded", j.skipped.len())
            };
            // per i doppioni di SoundCloud e YouTube mostriamo copertina e artista dalla cronologia
            if j.tracks.is_empty() {
                let h = HISTORY.lock().unwrap();
                let s = j.skipped[0].clone();
                if let Some(e) = h.iter().rev().find(|e| e.title == s || e.key.split_once(':').is_some_and(|k| k.1 == s)) {
                    j.skipped[0] = e.title.clone();
                    j.tracks.push(Track {
                        title: e.title.clone(),
                        uploader: e.artist.clone(),
                        thumbnail: e.thumbnail.clone(),
                    });
                } else {
                    j.tracks.push(Track { title: j.skipped[0].clone(), uploader: String::new(), thumbnail: None });
                }
            }
        } else {
            j.status = "error".into();
            if let Some(e) = j.errors.last() {
                j.message = e.clone();
            } else if !j.message.starts_with("ERROR") && !j.message.starts_with("Error") {
                j.message = "Download failed".into();
            }
        }
    });
}

// ---------- comandi per l'interfaccia ----------

#[tauri::command]
fn start_download(url: String, force: bool, state: tauri::State<AppState>) -> Result<String, String> {
    let url = url.trim().to_string();
    let valid = Regex::new(
        r"^https?://(([\w-]+\.)?(soundcloud\.com|snd\.sc|youtube\.com)|youtu\.be|open\.spotify\.com|spotify\.link)/",
    )
    .unwrap();
    if !valid.is_match(&url) {
        return Err("Paste a SoundCloud, YouTube or Spotify link".into());
    }
    let id = state.counter.fetch_add(1, Ordering::Relaxed).to_string();
    state.jobs.lock().unwrap().insert(id.clone(), Job {
        id: id.clone(),
        url: url.clone(),
        status: "running".into(),
        progress: 0.0,
        message: "Fetching info…".into(),
        tracks: vec![],
        files: vec![],
        errors: vec![],
        skipped: vec![],
        total: None,
    });
    let jobs = state.jobs.clone();
    let job_id = id.clone();
    std::thread::spawn(move || run_job(jobs, job_id, url, force));
    Ok(id)
}

#[tauri::command]
fn get_job(id: String, state: tauri::State<AppState>) -> Option<Job> {
    state.jobs.lock().unwrap().get(&id).cloned()
}

/// Cronologia dalla più recente, con l'indicazione se il file esiste ancora.
#[tauri::command]
fn get_history() -> Vec<Value> {
    let dir = downloads_dir();
    HISTORY.lock().unwrap().iter().rev().map(|e| {
        let mut v = serde_json::to_value(e).unwrap_or_default();
        v["exists"] = dir.join(&e.file).is_file().into();
        v
    }).collect()
}

#[tauri::command]
fn remove_history(key: String) {
    let mut h = HISTORY.lock().unwrap();
    h.retain(|e| e.key != key);
    let _ = fs::write(history_path(), serde_json::to_string_pretty(&*h).unwrap_or_default());
}

#[tauri::command]
fn reveal(file: String) {
    let full = downloads_dir().join(file_name(&file));
    if full.is_file() {
        let _ = tauri_plugin_opener::reveal_item_in_dir(full);
    }
}

#[tauri::command]
fn open_folder() {
    let _ = tauri_plugin_opener::open_path(downloads_dir(), None::<&str>);
}

#[tauri::command]
fn open_site() {
    let _ = tauri_plugin_opener::open_url("https://www.gange.wtf", None::<&str>);
}

#[tauri::command]
fn read_clipboard(app: tauri::AppHandle) -> String {
    app.clipboard().read_text().unwrap_or_default().trim().chars().take(2000).collect()
}

#[tauri::command(async)]
fn tools_status() -> Value {
    tools::status()
}

/// Installa i programmi mancanti; con `update_ytdlp` riscarica anche yt-dlp.
#[tauri::command]
fn install_tools(update_ytdlp: bool) {
    tools::start_install(update_ytdlp);
}

/// Mette in testa al PATH la cartella dei programmi installati dall'app.
/// Su macOS le app aperte dal Finder non ereditano il PATH della shell:
/// lo recuperiamo dalla login shell e aggiungiamo i percorsi tipici.
fn fix_path() {
    let mut parts = vec![tools::bin_dir()];
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = cmd("/bin/zsh").args(["-lc", "printf '\\n%s' \"$PATH\""]).output() {
            if let Some(last) = String::from_utf8_lossy(&out.stdout).lines().last() {
                parts.extend(std::env::split_paths(last));
            }
        }
        parts.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    }
    parts.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    if let Ok(path) = std::env::join_paths(parts) {
        std::env::set_var("PATH", path);
    }
}

fn main() {
    fix_path();
    load_history();
    // modalità di test senza interfaccia: trackloot --cli <url> [--force]
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--cli") {
        let jobs: Jobs = Arc::default();
        let url = args.get(2).cloned().unwrap_or_default();
        jobs.lock().unwrap().insert("cli".into(), Job {
            id: "cli".into(), url: url.clone(), status: "running".into(), progress: 0.0,
            message: String::new(), tracks: vec![], files: vec![], errors: vec![],
            skipped: vec![], total: None,
        });
        run_job(jobs.clone(), "cli".into(), url, args.iter().any(|a| a == "--force"));
        println!("{}", serde_json::to_string_pretty(&jobs.lock().unwrap()["cli"]).unwrap());
        return;
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState { jobs: Arc::default(), counter: AtomicU64::new(1) })
        .invoke_handler(tauri::generate_handler![
            start_download, get_job, get_history, remove_history, reveal, open_folder,
            open_site, read_clipboard, tools_status, install_tools
        ])
        .run(tauri::generate_context!())
        .expect("error while starting TrackLoot");
}
