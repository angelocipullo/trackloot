// Stima dei BPM dal file audio, senza servizi online.
//
// L'audio (mono, 11 kHz) viene diviso in finestre: per ognuna si misura quanto
// cresce l'energia dello spettro rispetto alla precedente (gli "attacchi" di
// cassa, rullante, accordi). Il tempo è il periodo che si ripete di più in
// questa curva (autocorrelazione), cercato tra 70 e 180 BPM.

use crate::tools::cmd;
use rustfft::{num_complex::Complex, FftPlanner};
use std::path::Path;

const SR: usize = 11025;
const WIN: usize = 1024;
const HOP: usize = 64;
const MIN_BPM: f64 = 70.0;
const MAX_BPM: f64 = 180.0;

/// BPM del file, arrotondati all'intero.
pub fn detect(path: &Path) -> Option<u32> {
    let out = cmd("ffmpeg")
        .args(["-v", "error", "-i"]).arg(path)
        .args(["-t", "600", "-ac", "1", "-ar", &SR.to_string(), "-f", "f32le", "-"])
        .output().ok()?;
    let samples: Vec<f32> = out.stdout.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    estimate(&samples).map(|b| b.round() as u32)
}

/// Scrive i BPM nel tag ID3 (TBPM), letto da Rekordbox, Serato, Music…
pub fn write_tag(path: &Path, bpm: u32) -> Result<(), String> {
    use id3::TagLike;
    let mut tag = id3::Tag::read_from_path(path).unwrap_or_default();
    let version = tag.version();
    tag.set_text("TBPM", bpm.to_string());
    tag.write_to_path(path, version).map_err(|e| e.to_string())
}

pub fn estimate(samples: &[f32]) -> Option<f64> {
    if samples.len() < SR * 15 {
        return None;
    }
    let env = onset_envelope(samples);
    let fps = SR as f64 / HOP as f64;

    // autocorrelazione della curva degli attacchi
    let max_lag = (fps * 60.0 / MIN_BPM * 4.0).ceil() as usize + 2;
    if env.len() < max_lag * 2 {
        return None;
    }
    let ac: Vec<f64> = (0..=max_lag)
        .map(|lag| env[lag..].iter().zip(&env).map(|(a, b)| a * b).sum::<f64>() / (env.len() - lag) as f64)
        .collect();
    let at = |lag: f64| {
        let i = lag.floor() as usize;
        let f = lag - i as f64;
        ac[i] * (1.0 - f) + ac[i + 1] * f
    };

    // un tempo vero si ripete anche a 2, 3 e 4 battiti: sommarli rende il
    // picco più netto e preciso; la preferenza per ~120 BPM sceglie tra
    // un tempo e la sua metà (stessa periodicità)
    let comb = |bpm: f64| {
        let lag = fps * 60.0 / bpm;
        (1..=4).map(|h| at(lag * h as f64)).sum::<f64>()
    };
    let score = |bpm: f64| comb(bpm) * (-0.5 * ((bpm / 120.0).log2() / 0.9).powi(2)).exp();
    let best = |from: f64, to: f64, step: f64| {
        let steps = ((to - from) / step).round() as usize;
        (0..=steps).map(|i| from + i as f64 * step)
            .max_by(|a, b| score(*a).total_cmp(&score(*b)))
            .unwrap()
    };
    let rough = best(MIN_BPM, MAX_BPM, 0.5);
    let fine = best((rough - 0.5).max(MIN_BPM), (rough + 0.5).min(MAX_BPM), 0.02);
    if comb(fine) <= 0.0 {
        return None;
    }
    // la preferenza dimezza i pezzi veloci (171 → 85): se il doppio tempo è
    // quasi forte quanto quello trovato, è quello vero (hip hop a 90 con
    // hi-hat in ottavi ~0.78, Blinding Lights a 171 ~0.89)
    let double = fine * 2.0;
    Some(if double <= MAX_BPM && comb(double) / comb(fine) > 0.85 { double } else { fine })
}

/// Curva degli attacchi: crescita del log-spettro tra finestre consecutive,
/// meno la media locale (così resta solo ciò che "spicca").
fn onset_envelope(samples: &[f32]) -> Vec<f64> {
    let fft = FftPlanner::<f32>::new().plan_fft_forward(WIN);
    let window: Vec<f32> = (0..WIN)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / WIN as f32).cos())
        .collect();
    let mut buf = vec![Complex::default(); WIN];
    let mut prev = vec![0f32; WIN / 2];
    let mut flux = vec![];
    for start in (0..samples.len() - WIN).step_by(HOP) {
        for (i, c) in buf.iter_mut().enumerate() {
            *c = Complex::new(samples[start + i] * window[i], 0.0);
        }
        fft.process(&mut buf);
        let mut sum = 0.0;
        for k in 1..WIN / 2 {
            let m = (1.0 + 1000.0 * buf[k].norm()).ln();
            sum += (m - prev[k]).max(0.0) as f64;
            prev[k] = m;
        }
        flux.push(sum);
    }

    let half = SR / HOP / 4; // media su ~0.5 s
    let mut acc = vec![0.0; flux.len() + 1];
    for (i, f) in flux.iter().enumerate() {
        acc[i + 1] = acc[i] + f;
    }
    (0..flux.len())
        .map(|i| {
            let (a, b) = (i.saturating_sub(half), (i + half + 1).min(flux.len()));
            (flux[i] - (acc[b] - acc[a]) / (b - a) as f64).max(0.0)
        })
        .collect()
}
