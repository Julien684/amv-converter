// AMV Converter — convertit plusieurs vidéos au format AMV (petits lecteurs MP3/vidéo).
// Interface Qt6 (qtrs). Nécessite ffmpeg et ffprobe dans le PATH.
//
// Deux modes :
//   amv-converter [fichiers…]               → interface graphique
//   amv-converter --batch [options] f…      → conversion sans interface (tests)

use std::cell::RefCell;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use qtrs::prelude::*;
use qtrs::{Application, MainWindow, VBoxLayout, HBoxLayout, Widget, Label, PushButton,
           Slider, ComboBox, ListWidget, ProgressBar, Frame, Timer, FileDialog, ffi};
use cxx::let_cxx_string;

const VERSION: &str = "0.2.0";

const VIDEO_EXTS: &[&str] = &["mkv","mp4","avi","mov","webm","m4v","wmv","flv","mpg","mpeg","ts","amv","3gp","vob","mk3d"];

// Période d'émission des blocs `-progress` (secondes). 0,5 s par défaut laisse
// la barre figée sur les conversions courtes/rapides.
const STATS_PERIOD: &str = "0.2";

// ─────────────────────────────────────────────────────────────────────────────
// Accélération matérielle
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwAccel { Cpu, Vaapi, Cuda, Qsv }

impl HwAccel {
    /// Nom affiché dans l'interface.
    pub fn label(self) -> &'static str {
        match self {
            HwAccel::Cpu => "CPU (logiciel)",
            HwAccel::Vaapi => "GPU — VAAPI (Intel/AMD)",
            HwAccel::Cuda => "GPU — NVDEC (NVIDIA)",
            HwAccel::Qsv => "GPU — QSV (Intel)",
        }
    }

    /// Nom du décodeur pour `-hwaccel`.
    pub fn ffmpeg_name(self) -> Option<&'static str> {
        match self {
            HwAccel::Cpu => None,
            HwAccel::Vaapi => Some("vaapi"),
            HwAccel::Cuda => Some("cuda"),
            HwAccel::Qsv => Some("qsv"),
        }
    }

    /// Options à insérer AVANT `-i` pour décoder sur le GPU.
    pub fn decoder_args(self) -> Vec<String> {
        match self.ffmpeg_name() {
            None => Vec::new(),
            Some(name) => {
                let mut v = vec!["-hwaccel".to_string(), name.to_string()];
                if self == HwAccel::Vaapi {
                    v.push("-hwaccel_device".to_string());
                    v.push(vaapi_device());
                }
                v
            }
        }
    }
}

/// Premier nœud de rendu DRM disponible (VAAPI).
pub fn vaapi_device() -> String {
    for n in 128..136 {
        let p = format!("/dev/dri/renderD{n}");
        if Path::new(&p).exists() { return p; }
    }
    "/dev/dri/renderD128".to_string()
}

/// Analyse la sortie de `ffmpeg -hwaccels`.
pub fn parse_hwaccels_output(out: &str) -> Vec<HwAccel> {
    let mut found = Vec::new();
    for line in out.lines() {
        let l = line.trim();
        if l == "vaapi" { found.push(HwAccel::Vaapi); }
        else if l == "cuda" { found.push(HwAccel::Cuda); }
        else if l == "qsv" { found.push(HwAccel::Qsv); }
    }
    found
}

/// Décodeurs matériels réellement utilisables sur cette machine.
pub fn detect_hwaccels() -> Vec<HwAccel> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-hwaccels"])
        .output();
    let txt = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return Vec::new(),
    };
    // VAAPI n'est utilisable que s'il existe un nœud de rendu DRM.
    let has_render_node = (128..136).any(|n| Path::new(&format!("/dev/dri/renderD{n}")).exists());
    parse_hwaccels_output(&txt)
        .into_iter()
        .filter(|a| *a != HwAccel::Vaapi || has_render_node)
        .collect()
}

/// Résout le choix « Auto » : GPU si disponible, sinon CPU.
pub fn resolve_auto(available: &[HwAccel]) -> HwAccel {
    available.first().copied().unwrap_or(HwAccel::Cpu)
}

// ─────────────────────────────────────────────────────────────────────────────
// Temps / progression
// ─────────────────────────────────────────────────────────────────────────────

pub fn fmt_time(secs: f64) -> String {
    if secs < 0.0 || !secs.is_finite() { return "--:--".into(); }
    let s = secs.round() as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{sec:02}") } else { format!("{m:02}:{sec:02}") }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Progress {
    pub elapsed: f64,
    /// Pourcentage 0-100, `None` si la durée est inconnue.
    pub pct: Option<i32>,
    /// Secondes restantes, `None` si la durée est inconnue.
    pub remaining: Option<f64>,
}

/// Extrait le temps écoulé d'une ligne de `-progress`.
/// `out_time_us` et `out_time_ms` sont tous deux en MICROsecondes (piège ffmpeg).
pub fn parse_progress_line(line: &str) -> Option<f64> {
    let line = line.trim();
    for key in ["out_time_us=", "out_time_ms="] {
        if let Some(v) = line.strip_prefix(key) {
            let v = v.trim();
            if v.is_empty() || v == "N/A" { return None; }
            return v.parse::<f64>().ok().map(|us| us / 1_000_000.0);
        }
    }
    if let Some(v) = line.strip_prefix("out_time=") {
        return parse_hms(v.trim());
    }
    None
}

pub fn parse_hms(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 { return None; }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let sec: f64 = parts[2].parse().ok()?;
    let total = h * 3600.0 + m * 60.0 + sec;
    if total.is_finite() && total >= 0.0 { Some(total) } else { None }
}

/// Pourcentage écoulé + temps restant. Durée inconnue ⇒ pas de pourcentage
/// (barre indéterminée) plutôt qu'un faux 100 %.
pub fn compute_progress(elapsed: f64, duration: f64) -> Progress {
    if duration > 0.0 && duration.is_finite() {
        let ratio = (elapsed / duration).clamp(0.0, 1.0);
        Progress {
            elapsed,
            pct: Some((ratio * 100.0).round() as i32),
            remaining: Some((duration - elapsed).max(0.0)),
        }
    } else {
        Progress { elapsed, pct: None, remaining: None }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Ligne de commande ffmpeg
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct JobSpec { pub width: u32, pub height: u32, pub qv: i32, pub accel: HwAccel }

pub fn video_filter(w: u32, h: u32) -> String {
    format!("scale={w}:{h}:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,format=yuvj420p")
}

/// Construit les arguments ffmpeg complets (hors nom du programme).
/// Sans piste audio dans la source, une piste silencieuse est synthétisée :
/// le muxeur AMV exige deux flux et la plupart des lecteurs n'ouvrent pas un
/// fichier sans audio.
pub fn build_ffmpeg_args(input: &str, output: &str, spec: &JobSpec, audio: Option<u32>) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-y".into(), "-v".into(), "error".into(),
        "-nostats".into(), "-stats_period".into(), STATS_PERIOD.into(),
        "-progress".into(), "pipe:1".into(),
    ];
    a.extend(spec.accel.decoder_args());
    a.push("-i".into());
    a.push(input.to_string());
    if audio.is_none() {
        a.extend(["-f".into(), "lavfi".into(), "-i".into(),
                  "anullsrc=r=22050:cl=mono".to_string()]);
    }
    a.push("-vf".into());
    a.push(video_filter(spec.width, spec.height));
    a.push("-r".into()); a.push("30".into());
    a.push("-map".into()); a.push("0:v:0".into());
    match audio {
        Some(ai) => { a.push("-map".into()); a.push(format!("0:{ai}")); }
        None => { a.push("-map".into()); a.push("1:a:0".into()); }
    }
    a.extend([
        "-c:v".into(), "amv".into(),
        "-q:v".into(), spec.qv.to_string(),
        "-strict".into(), "-1".into(),
        "-c:a".into(), "adpcm_ima_amv".into(),
        "-block_size".into(), "735".into(),
        "-ar".into(), "22050".into(),
        "-ac".into(), "1".into(),
    ]);
    if audio.is_none() { a.push("-shortest".into()); }
    a.push(output.to_string());
    a
}

/// Position de `-i` (pour vérifier que `-hwaccel` est bien une option d'entrée).
pub fn input_option_index(args: &[String]) -> Option<usize> {
    args.iter().position(|s| s == "-i")
}

// ─────────────────────────────────────────────────────────────────────────────
// ffprobe
// ─────────────────────────────────────────────────────────────────────────────

/// Première valeur numérique d'une sortie `-of csv=p=0`.
pub fn parse_first_number(out: &str) -> Option<f64> {
    out.lines()
        .map(|l| l.trim())
        .find_map(|l| l.split(',').next().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|d| d.is_finite() && *d > 0.0)
}

pub fn parse_max_number(out: &str) -> Option<f64> {
    let best = out.lines()
        .filter_map(|l| l.trim().split(',').next().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|d| d.is_finite() && *d > 0.0)
        .fold(0.0f64, f64::max);
    if best > 0.0 { Some(best) } else { None }
}

pub fn parse_frac(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some((a, b)) = s.split_once('/') {
        let a: f64 = a.parse().ok()?; let b: f64 = b.parse().ok()?;
        if b > 0.0 { Some(a / b) } else { None }
    } else if s.is_empty() { None } else { s.parse::<f64>().ok() }
}

/// Durée en secondes : format=duration, sinon max des durées de flux, sinon frames/fps.
pub fn probe_duration(path: &str) -> Option<f64> {
    let run = |args: &[&str]| -> Option<String> {
        std::process::Command::new("ffprobe")
            .args(args).arg(path).output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    };
    if let Some(txt) = run(&["-v","error","-show_entries","format=duration","-of","csv=p=0"]) {
        if let Some(d) = parse_first_number(&txt) { return Some(d); }
    }
    if let Some(txt) = run(&["-v","error","-show_entries","stream=duration","-of","csv=p=0"]) {
        if let Some(d) = parse_max_number(&txt) { return Some(d); }
    }
    let txt = run(&["-v","error","-select_streams","v:0",
                    "-show_entries","stream=nb_frames,avg_frame_rate","-of","csv=p=0"])?;
    let first = txt.lines().next()?.trim().to_string();
    let parts: Vec<&str> = first.split(',').collect();
    let nf = parts.first().and_then(|s| s.trim().parse::<f64>().ok())?;
    let fps = parts.get(1).and_then(|s| parse_frac(s))?;
    if fps > 0.0 && nf > 0.0 { Some(nf / fps) } else { None }
}

/// Index du flux audio : français (fra/fre/fr) si présent, sinon premier audio.
pub fn parse_audio_streams(out: &str) -> Option<u32> {
    let mut first: Option<u32> = None;
    for line in out.lines() {
        let mut it = line.split(',');
        if let Some(idx) = it.next().and_then(|s| s.trim().parse::<u32>().ok()) {
            if first.is_none() { first = Some(idx); }
            let lang = it.next().unwrap_or("").trim().to_lowercase();
            if lang == "fra" || lang == "fre" || lang == "fr" { return Some(idx); }
        }
    }
    first
}

pub fn pick_audio_stream(path: &str) -> Option<u32> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v","error","-select_streams","a","-show_entries","stream=index:stream_tags=language",
               "-of","csv=p=0", path])
        .output().ok()?;
    parse_audio_streams(&String::from_utf8_lossy(&out.stdout))
}

pub fn has_cmd(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ─────────────────────────────────────────────────────────────────────────────
// Messages UI
// ─────────────────────────────────────────────────────────────────────────────

pub enum Msg {
    Status(String),
    FileStarted { index: usize, total: usize, name: String, audio: Option<u32> },
    Progress(Progress),
    FileDone { ok: bool, out: String, note: Option<String> },
    AllDone { ok: usize, fail: usize, cancelled: bool },
}

// ─────────────────────────────────────────────────────────────────────────────
// Conversion
// ─────────────────────────────────────────────────────────────────────────────

pub struct Ctx {
    pub cancel: Arc<AtomicBool>,
    /// PID du ffmpeg en cours (pour « Arrêter »).
    pub pid: Arc<Mutex<Option<u32>>>,
}

fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Nom de sortie libre : `<base>_amv.amv`, puis `_2`, `_3`…
pub fn unique_output(input: &str) -> String {
    let p = Path::new(input);
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "video".into());
    let dir = p.parent().map(|d| d.to_path_buf()).unwrap_or_else(|| ".".into());
    let mut out = dir.join(format!("{stem}_amv.amv"));
    let mut n = 2;
    while out.exists() {
        out = dir.join(format!("{stem}_amv_{n}.amv"));
        n += 1;
    }
    out.to_string_lossy().to_string()
}

/// Exécute ffmpeg pour un fichier. `note` indique un repli GPU → CPU.
fn run_one(input: &str, out: &str, spec: &JobSpec, audio: Option<u32>, dur: f64,
           ctx: &Ctx, tx: &Sender<Msg>) -> (bool, Option<String>) {
    let mut note: Option<String> = None;
    let mut accel = spec.accel;
    let mut attempt = 0;

    loop {
        attempt += 1;
        let cur = JobSpec { accel, ..*spec };
        let args = build_ffmpeg_args(input, out, &cur, audio);
        let log = std::env::temp_dir().join(format!("amv-converter-{}.log", std::process::id()));
        let logf = std::fs::File::create(&log).ok();

        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.args(&args).stdout(std::process::Stdio::piped());
        match logf {
            Some(f) => { cmd.stderr(std::process::Stdio::from(f)); }
            None => { cmd.stderr(std::process::Stdio::null()); }
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => { let _ = std::fs::remove_file(&log); return (false, note); }
        };
        *ctx.pid.lock().unwrap() = Some(child.id());

        if let Some(so) = child.stdout.take() {
            let mut last: Option<f64> = None;
            for line in BufReader::new(so).lines().map_while(Result::ok) {
                if ctx.cancel.load(Ordering::Relaxed) { break; }
                if let Some(elapsed) = parse_progress_line(&line) {
                    // un bloc `-progress` répète la même valeur sous 3 clés
                    if last == Some(elapsed) { continue; }
                    last = Some(elapsed);
                    let _ = tx.send(Msg::Progress(compute_progress(elapsed, dur)));
                }
            }
        }
        let status = child.wait();
        *ctx.pid.lock().unwrap() = None;

        let ok = status.map(|s| s.success()).unwrap_or(false);
        let good_size = std::fs::metadata(out).map(|m| m.len() > 1024).unwrap_or(false);

        if ctx.cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&log);
            return (false, note);
        }

        if ok && good_size {
            let _ = std::fs::remove_file(&log);
            return (true, note);
        }

        // Repli automatique : GPU indisponible/incompatible → on refait en CPU.
        if accel != HwAccel::Cpu && attempt == 1 {
            note = Some(format!("{} indisponible → repli CPU", accel.label()));
            let _ = std::fs::remove_file(out);
            accel = HwAccel::Cpu;
            continue;
        }

        let _ = std::fs::remove_file(&log);
        return (false, note);
    }
}

pub fn convert_all(files: Vec<String>, spec: JobSpec, ctx: &Ctx, tx: &Sender<Msg>) -> (usize, usize, bool) {
    let mut ok = 0usize;
    let mut fail = 0usize;
    let total = files.len();

    for (i, f) in files.iter().enumerate() {
        if ctx.cancel.load(Ordering::Relaxed) { break; }
        let disp = Path::new(f).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let dur = probe_duration(f).unwrap_or(0.0);
        let audio = pick_audio_stream(f);
        let _ = tx.send(Msg::FileStarted { index: i + 1, total, name: disp, audio });
        let out = unique_output(f);
        let (success, note) = run_one(f, &out, &spec, audio, dur, ctx, tx);
        if success {
            ok += 1;
            let _ = tx.send(Msg::Progress(compute_progress(dur, dur)));
            let _ = tx.send(Msg::FileDone { ok: true, out, note });
        } else {
            fail += 1;
            let _ = tx.send(Msg::FileDone { ok: false, out, note });
        }
    }
    let cancelled = ctx.cancel.load(Ordering::Relaxed);
    let _ = tx.send(Msg::AllDone { ok, fail, cancelled });
    (ok, fail, cancelled)
}

// ─────────────────────────────────────────────────────────────────────────────
// Mode batch (sans interface) — utilisé par les tests
// ─────────────────────────────────────────────────────────────────────────────

struct BatchArgs {
    files: Vec<String>,
    spec: JobSpec,
}

fn parse_batch_args(argv: &[String]) -> Result<BatchArgs, String> {
    let available = detect_hwaccels();
    let mut spec = JobSpec { width: 160, height: 120, qv: 2, accel: resolve_auto(&available) };
    let mut files = Vec::new();
    let mut it = argv.iter().skip_while(|a| a.as_str() != "--batch").skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--gpu" | "--accel" => {
                let v = it.next().ok_or("--gpu attend une valeur")?;
                spec.accel = match v.as_str() {
                    "auto" => resolve_auto(&available),
                    "cpu" => HwAccel::Cpu,
                    "vaapi" => HwAccel::Vaapi,
                    "cuda" | "nvdec" => HwAccel::Cuda,
                    "qsv" => HwAccel::Qsv,
                    other => return Err(format!("accélération inconnue : {other}")),
                };
            }
            "--quality" | "-q" => {
                let v = it.next().ok_or("--quality attend une valeur")?;
                let qv: i32 = v.parse().map_err(|_| format!("qualité invalide : {v}"))?;
                spec.qv = qv.clamp(1, 31);
            }
            "--size" | "-s" => {
                let v = it.next().ok_or("--size attend une valeur")?;
                let (w, h) = split_size(v)?;
                spec.width = w; spec.height = h;
            }
            other if other.starts_with("--") => return Err(format!("option inconnue : {other}")),
            other => files.push(other.to_string()),
        }
    }
    Ok(BatchArgs { files, spec })
}

pub fn split_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once('x').ok_or_else(|| format!("résolution invalide : {s}"))?;
    let w: u32 = w.parse().map_err(|_| format!("résolution invalide : {s}"))?;
    let h: u32 = h.parse().map_err(|_| format!("résolution invalide : {s}"))?;
    if w == 0 || h == 0 { return Err(format!("résolution invalide : {s}")); }
    Ok((w, h))
}

fn run_batch(argv: &[String]) -> i32 {
    let args = match parse_batch_args(argv) {
        Ok(a) => a,
        Err(e) => { eprintln!("ERREUR {e}"); return 2; }
    };
    if args.files.is_empty() {
        eprintln!("ERREUR aucun fichier");
        return 2;
    }
    println!("ACCEL {}", args.spec.accel.label());
    let (tx, rx) = mpsc::channel::<Msg>();
    let ctx = Ctx { cancel: Arc::new(AtomicBool::new(false)), pid: Arc::new(Mutex::new(None)) };
    let spec = args.spec;
    let files = args.files.clone();
    let worker = std::thread::spawn(move || {
        convert_all(files, spec, &ctx, &tx)
    });

    let mut result = (0usize, 0usize, false);
    // Affiche les messages au fil de l'eau (pas de Qt ici).
    for m in rx.iter() {
        match m {
            Msg::Status(s) => println!("STATUS {s}"),
            Msg::FileStarted { index, total, name, audio } => {
                println!("FILE {index}/{total} {name}");
                match audio {
                    Some(i) => println!("AUDIO {i}"),
                    None => println!("AUDIO none"),
                }
            }
            Msg::Progress(p) => println!(
                "PROGRESS pct={} elapsed={:.1} remaining={:.1}",
                p.pct.map(|v| v.to_string()).unwrap_or_else(|| "-1".into()),
                p.elapsed,
                p.remaining.unwrap_or(-1.0)
            ),
            Msg::FileDone { ok, out, note } => {
                println!("{} {}{}", if ok { "OK" } else { "FAIL" }, out,
                    note.map(|n| format!(" ({n})")).unwrap_or_default());
            }
            Msg::AllDone { ok, fail, cancelled } => {
                println!("DONE ok={ok} fail={fail} cancelled={cancelled}");
                result = (ok, fail, cancelled);
                break;
            }
        }
    }
    let _ = worker.join();
    if result.1 > 0 || result.2 { 1 } else { 0 }
}

// ─────────────────────────────────────────────────────────────────────────────
// Icône
// ─────────────────────────────────────────────────────────────────────────────

fn resolve_icon() -> Option<String> {
    let mut cands: Vec<String> = Vec::new();
    for size in ["128x128", "48x48", "24x24"] {
        cands.push(format!("/usr/share/icons/hicolor/{size}/apps/amv-converter.png"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("amv-converter-128.png").to_string_lossy().to_string());
            if let Some(up2) = dir.parent().and_then(|p| p.parent()) {
                cands.push(up2.join("assets/amv-converter-128.png").to_string_lossy().to_string());
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.join("assets/amv-converter-128.png").to_string_lossy().to_string());
        cands.push(cwd.join("amv-converter-128.png").to_string_lossy().to_string());
    }
    cands.into_iter().find(|p| Path::new(p).exists())
}

// ─────────────────────────────────────────────────────────────────────────────
// Interface graphique
// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    if argv.iter().any(|a| a == "--version" || a == "-V") {
        println!("amv-converter {VERSION}");
        return;
    }
    if argv.iter().any(|a| a == "--list-gpu") {
        let list = detect_hwaccels();
        if list.is_empty() { println!("aucune"); } else {
            for a in list { println!("{}", a.label()); }
        }
        return;
    }
    if argv.iter().any(|a| a == "--batch") {
        std::process::exit(run_batch(&argv));
    }

    let app = Application::new();
    if let Some(icon) = resolve_icon() { app.set_icon(&icon); }

    // ---- vérification des dépendances ----
    let missing: Vec<&str> = ["ffmpeg", "ffprobe"].into_iter().filter(|c| !has_cmd(c)).collect();
    if !missing.is_empty() {
        let list = missing.join(", ");
        unsafe {
            let_cxx_string!(title = "AMV Converter — dépendance manquante");
            let msg = format!(
                "Le(s) programme(s) suivant(s) sont introuvables dans le PATH :\n\n  {list}\n\n\
                 Installe-les puis relance l'application.\n\nSur Solus :  sudo eopkg install ffmpeg"
            );
            let_cxx_string!(body = msg.as_str());
            ffi::QMessageBox_critical(std::ptr::null_mut(), &title, &body);
        }
        eprintln!("Dépendance manquante: {list}");
        std::process::exit(1);
    }

    let window = MainWindow::new().window_title("AMV Converter").size(880, 660).build();
    let central = Widget::new().build();
    let mut root = VBoxLayout::with_parent(&central);

    let central_ptr = central.widget_ptr();
    let files: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let running = Arc::new(Mutex::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel::<Msg>();

    // ---- toolbar ----
    let topbar = Frame::new().parent(&central).build();
    let mut top_h = HBoxLayout::with_parent(&topbar);
    let btn_files = PushButton::new("\u{ff0b} Fichiers").parent(&topbar).on_clicked({
        let files = Rc::clone(&files);
        move || {
            let picked = FileDialog::open_files(None, "Choisir des vidéos", "",
                "Vidéos (*.mkv *.mp4 *.avi *.mov *.webm *.m4v *.wmv *.flv *.mpg *.mpeg *.ts *.amv *.3gp *.vob);;Tous (*)");
            for f in picked { add_file(&files, central_ptr, &f); }
        }
    }).build();
    let btn_folder = PushButton::new("\u{ff0b} Dossier").parent(&topbar).on_clicked({
        let files = Rc::clone(&files);
        move || {
            if let Some(dir) = FileDialog::select_directory(None, "Choisir un dossier", "") {
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    let mut v: Vec<String> = rd.flatten()
                        .map(|e| e.path().to_string_lossy().to_string())
                        .filter(|p| {
                            let low = p.to_lowercase();
                            VIDEO_EXTS.iter().any(|e| low.ends_with(&format!(".{e}")))
                        }).collect();
                    v.sort();
                    for f in v { add_file(&files, central_ptr, &f); }
                }
            }
        }
    }).build();
    let btn_clear = PushButton::new("Tout retirer").parent(&topbar).on_clicked({
        let files = Rc::clone(&files);
        move || {
            files.borrow_mut().clear();
            unsafe {
                let_cxx_string!(n = "filelist");
                let lw = ffi::QWidget_findListWidget(central_ptr, &n);
                if !lw.is_null() { ffi::QListWidget_clear(lw); }
            }
        }
    }).build();
    top_h.add(btn_files);
    top_h.add(btn_folder);
    top_h.add(btn_clear);
    root.add(topbar);

    // ---- options (2 lignes) ----
    let optbar = Frame::new().parent(&central).build();
    let mut opt_v = VBoxLayout::with_parent(&optbar);
    let row1 = Widget::new().parent(&optbar).build();
    let mut row1_h = HBoxLayout::with_parent(&row1);
    let lbl_q = Label::new("Qualité :").parent(&row1).build();
    let lbl_low = Label::new("Basse").parent(&row1).build();
    let quality = Slider::horizontal().range(1, 31).parent(&row1).build();
    quality.set_value(30);                 // 30 => q:v 2 (haute qualité par défaut)
    quality.set_object_name("quality");
    let lbl_high = Label::new("Haute").parent(&row1).build();
    let lbl_size = Label::new("    Résolution :").parent(&row1).build();
    let combo_size = ComboBox::new().items(&["160x120","128x96","176x144","160x128"])
        .parent(&row1).build();
    combo_size.set_object_name("size");
    row1_h.add(lbl_q); row1_h.add(lbl_low); row1_h.add(quality); row1_h.add(lbl_high);
    row1_h.add(lbl_size); row1_h.add(combo_size);
    opt_v.add(row1);

    let row2 = Widget::new().parent(&optbar).build();
    let mut row2_h = HBoxLayout::with_parent(&row2);
    let available = detect_hwaccels();
    let mut accel_items: Vec<String> = vec!["Auto (GPU si disponible)".into(), HwAccel::Cpu.label().into()];
    for a in &available { accel_items.push(a.label().to_string()); }
    let accel_refs: Vec<&str> = accel_items.iter().map(|s| s.as_str()).collect();
    let lbl_accel = Label::new("Accélération :").parent(&row2).build();
    let combo_accel = ComboBox::new().items(&accel_refs).parent(&row2).build();
    combo_accel.set_object_name("accel");
    let accel_hint = if available.is_empty() {
        "  (aucun GPU détecté — tout sur CPU)"
    } else {
        "  (décodage GPU, encodage AMV sur CPU)"
    };
    let lbl_accel_hint = Label::new(accel_hint).parent(&row2).build();
    let lbl_audio = Label::new("     Audio : français si présent").parent(&row2).build();
    row2_h.add(lbl_accel); row2_h.add(combo_accel); row2_h.add(lbl_accel_hint); row2_h.add(lbl_audio);
    opt_v.add(row2);
    root.add(optbar);

    // ---- file list ----
    let list = ListWidget::new().parent(&central).build();
    list.set_object_name("filelist");
    root.add(list);

    // ---- action row ----
    let actbar = Frame::new().parent(&central).build();
    let mut act_h = HBoxLayout::with_parent(&actbar);
    let btn_go = PushButton::new("Convertir tout").parent(&actbar).on_clicked({
        let files = Rc::clone(&files);
        let running = Arc::clone(&running);
        let cancel = Arc::clone(&cancel);
        let pid = Arc::clone(&pid);
        let available = available.clone();
        let tx = tx.clone();
        move || {
            let snapshot: Vec<String> = files.borrow().clone();
            if snapshot.is_empty() {
                let _ = tx.send(Msg::Status("Ajoute au moins une vidéo.".into()));
                return;
            }
            {
                let mut r = running.lock().unwrap();
                if *r { let _ = tx.send(Msg::Status("Conversion déjà en cours…".into())); return; }
                *r = true;
            }
            cancel.store(false, Ordering::SeqCst);
            let (w, h) = unsafe {
                let_cxx_string!(n = "size");
                let cb = ffi::QWidget_findComboBox(central_ptr, &n);
                let txt = if cb.is_null() { String::new() } else { ffi::QComboBox_currentText(cb) };
                split_size(&txt).unwrap_or((160, 120))
            };
            // slider: 1..31 (gauche=basse .. droite=haute). q:v ffmpeg: 1=meilleur.
            let qv = unsafe {
                let_cxx_string!(n = "quality");
                let s = ffi::QWidget_findSlider(central_ptr, &n);
                let v = if s.is_null() { 30 } else { ffi::QSlider_value(s) };
                32 - v
            }.clamp(1, 31);
            let accel = unsafe {
                let_cxx_string!(n = "accel");
                let cb = ffi::QWidget_findComboBox(central_ptr, &n);
                let txt = if cb.is_null() { String::new() } else { ffi::QComboBox_currentText(cb) };
                choose_accel(&txt, &available)
            };
            let spec = JobSpec { width: w, height: h, qv, accel };
            let tx2 = tx.clone();
            let cancel2 = Arc::clone(&cancel);
            let pid2 = Arc::clone(&pid);
            std::thread::spawn(move || {
                let ctx = Ctx { cancel: cancel2, pid: pid2 };
                convert_all(snapshot, spec, &ctx, &tx2);
            });
        }
    }).build();
    btn_go.set_object_name("btn_go");
    let btn_stop = PushButton::new("Arrêter").parent(&actbar).on_clicked({
        let tx = tx.clone();
        let running = Arc::clone(&running);
        let cancel = Arc::clone(&cancel);
        let pid = Arc::clone(&pid);
        move || {
            if !*running.lock().unwrap() { return; }
            cancel.store(true, Ordering::SeqCst);
            if let Some(p) = *pid.lock().unwrap() { kill_pid(p); }
            let _ = tx.send(Msg::Status("Arrêt demandé…".into()));
        }
    }).build();
    btn_stop.set_object_name("btn_stop");
    unsafe { ffi::QWidget_setEnabled(btn_stop.widget_ptr(), false); }
    let prog = ProgressBar::new().range(0, 100).value(0).parent(&actbar).build();
    prog.set_object_name("progress");
    act_h.add(btn_go); act_h.add(btn_stop); act_h.add(prog);
    root.add(actbar);

    // ---- status + time ----
    let status = Label::new("Prêt. Ajoute des vidéos puis clique « Convertir tout ».").parent(&central).build();
    status.set_object_name("status");
    root.add(status);
    let timing = Label::new("Écoulé : --:--   ·   Restant : --:--").parent(&central).build();
    timing.set_object_name("timing");
    root.add(timing);

    window.set_central_widget(&central);

    // ---- timer: pompe les messages du worker vers l'UI ----
    // ATTENTION: Timer::new(..).on_timeout(..) renvoie un Builder; sans .build()
    // aucun QTimer n'existe et la barre/le temps ne se mettent jamais à jour.
    let timer = Timer::new(100).on_timeout({
        let rx = rx;
        let running = Arc::clone(&running);
        move || {
            while let Ok(m) = rx.try_recv() {
                unsafe {
                    let_cxx_string!(nstat = "status");
                    let lbl = ffi::QWidget_findLabel(central_ptr, &nstat);
                    let_cxx_string!(nprog = "progress");
                    let bar = ffi::QWidget_findProgressBar(central_ptr, &nprog);
                    let_cxx_string!(ntime = "timing");
                    let tmlbl = ffi::QWidget_findLabel(central_ptr, &ntime);
                    let_cxx_string!(ngo = "btn_go");
                    let go = ffi::QWidget_findPushButton(central_ptr, &ngo);
                    let_cxx_string!(nstop = "btn_stop");
                    let stop = ffi::QWidget_findPushButton(central_ptr, &nstop);
                    if lbl.is_null() || bar.is_null() { continue; }
                    match m {
                        Msg::Status(s) => {
                            let_cxx_string!(t = s.as_str());
                            ffi::QLabel_setText(lbl, &t);
                        }
                        Msg::FileStarted { index, total, name, audio: _ } => {
                            let s = format!("({index}/{total}) Conversion : {name}");
                            let_cxx_string!(t = s.as_str());
                            ffi::QLabel_setText(lbl, &t);
                            ffi::QProgressBar_setRange(bar, 0, 100);
                            ffi::QProgressBar_setValue(bar, 0);
                            if !tmlbl.is_null() {
                                let_cxx_string!(t0 = "Écoulé : 00:00   ·   Restant : --:--");
                                ffi::QLabel_setText(tmlbl, &t0);
                            }
                            if !stop.is_null() { ffi::QWidget_setEnabled(stop as *mut ffi::QWidget, true); }
                            if !go.is_null() { ffi::QWidget_setEnabled(go as *mut ffi::QWidget, false); }
                        }
                        Msg::Progress(p) => {
                            match p.pct {
                                Some(v) => {
                                    ffi::QProgressBar_setRange(bar, 0, 100);
                                    ffi::QProgressBar_setValue(bar, v);
                                }
                                // Durée inconnue : barre « occupée » plutôt qu'un faux 0/100 %.
                                None => ffi::QProgressBar_setRange(bar, 0, 0),
                            }
                            if !tmlbl.is_null() {
                                let s = match p.remaining {
                                    Some(r) => format!("Écoulé : {}   ·   Restant : {}", fmt_time(p.elapsed), fmt_time(r)),
                                    None => format!("Écoulé : {}   ·   Restant : --:--", fmt_time(p.elapsed)),
                                };
                                let_cxx_string!(t = s.as_str());
                                ffi::QLabel_setText(tmlbl, &t);
                            }
                        }
                        Msg::FileDone { ok, out, note } => {
                            let mut s = if ok { format!("OK → {out}") } else { format!("ÉCHEC → {out}") };
                            if let Some(n) = note { s.push_str(&format!("  [{n}]")); }
                            let_cxx_string!(t = s.as_str());
                            ffi::QLabel_setText(lbl, &t);
                        }
                        Msg::AllDone { ok, fail, cancelled } => {
                            let s = if cancelled {
                                format!("Arrêté : {ok} réussi(s), {fail} échec(s).")
                            } else {
                                format!("Terminé : {ok} réussi(s), {fail} échec(s).")
                            };
                            let_cxx_string!(t = s.as_str());
                            ffi::QLabel_setText(lbl, &t);
                            if !cancelled {
                                ffi::QProgressBar_setRange(bar, 0, 100);
                                ffi::QProgressBar_setValue(bar, 100);
                            }
                            if !stop.is_null() { ffi::QWidget_setEnabled(stop as *mut ffi::QWidget, false); }
                            if !go.is_null() { ffi::QWidget_setEnabled(go as *mut ffi::QWidget, true); }
                            *running.lock().unwrap() = false;
                            // Pas de boîte modale depuis la pompe : on la différe.
                            notify_done(s.clone());
                        }
                    }
                }
            }
        }
    }).build();
    let _timer = timer;

    // Fichiers passés en argument → pré-remplis.
    for f in &argv {
        if !f.starts_with('-') { add_file(&files, central_ptr, f); }
    }

    window.show();
    app.exec();
}

/// Affiche le bilan de fin de conversion hors de la pompe de messages.
fn notify_done(msg: String) {
    Timer::single_shot(0, move || unsafe {
        let_cxx_string!(title = "AMV Converter");
        let_cxx_string!(body = msg.as_str());
        ffi::QMessageBox_information(std::ptr::null_mut(), &title, &body);
    });
}

fn choose_accel(text: &str, available: &[HwAccel]) -> HwAccel {
    if text.starts_with("Auto") { return resolve_auto(available); }
    for a in available {
        if a.label() == text { return *a; }
    }
    if text == HwAccel::Cpu.label() { return HwAccel::Cpu; }
    resolve_auto(available)
}

fn add_file(files: &Rc<RefCell<Vec<String>>>, cp: *mut ffi::QWidget, f: &str) {
    if !Path::new(f).exists() { return; }
    let low = f.to_lowercase();
    if !VIDEO_EXTS.iter().any(|e| low.ends_with(&format!(".{e}"))) { return; }
    {
        let mut v = files.borrow_mut();
        if v.iter().any(|x| x == f) { return; }
        v.push(f.to_string());
    }
    let name = Path::new(f).file_name()
        .map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| f.to_string());
    unsafe {
        let_cxx_string!(n = "filelist");
        let lw = ffi::QWidget_findListWidget(cp, &n);
        if lw.is_null() { return; }
        let_cxx_string!(t = name.as_str());
        ffi::QListWidget_addItem(lw, &t);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests unitaires (aucun widget Qt créé : `cargo test` reste sans affichage)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_time_cases() {
        assert_eq!(fmt_time(0.0), "00:00");
        assert_eq!(fmt_time(65.4), "01:05");
        assert_eq!(fmt_time(3725.0), "1:02:05");
        assert_eq!(fmt_time(-1.0), "--:--");
        assert_eq!(fmt_time(f64::NAN), "--:--");
        assert_eq!(fmt_time(f64::INFINITY), "--:--");
    }

    #[test]
    fn progress_keys_are_microseconds() {
        // ffmpeg nomme « ms » une valeur en microsecondes : piège classique.
        assert_eq!(parse_progress_line("out_time_ms=6000000"), Some(6.0));
        assert_eq!(parse_progress_line("out_time_us=6000000"), Some(6.0));
        assert_eq!(parse_progress_line("out_time=00:00:06.000000"), Some(6.0));
        assert_eq!(parse_progress_line("out_time_us=N/A"), None);
        assert_eq!(parse_progress_line("out_time_us="), None);
        assert_eq!(parse_progress_line("frame=180"), None);
        assert_eq!(parse_progress_line("out_time_us= 2500000 "), Some(2.5));
    }

    #[test]
    fn progress_percent_and_remaining() {
        let p = compute_progress(30.0, 120.0);
        assert_eq!(p.pct, Some(25));
        assert_eq!(p.remaining, Some(90.0));
        let p = compute_progress(120.0, 120.0);
        assert_eq!(p.pct, Some(100));
        assert_eq!(p.remaining, Some(0.0));
        // dépassement (audio plus long que la vidéo) : borné à 100 %
        let p = compute_progress(130.0, 120.0);
        assert_eq!(p.pct, Some(100));
        assert_eq!(p.remaining, Some(0.0));
        // durée inconnue : pas de faux 100 %
        let p = compute_progress(5.0, 0.0);
        assert_eq!(p.pct, None);
        assert_eq!(p.remaining, None);
    }

    #[test]
    fn hwaccel_detection_parsing() {
        let out = "Hardware acceleration methods:\nvulkan\ncuda\nvaapi\nqsv\nvdpau\n";
        assert_eq!(parse_hwaccels_output(out), vec![HwAccel::Cuda, HwAccel::Vaapi, HwAccel::Qsv]);
        assert!(parse_hwaccels_output("Hardware acceleration methods:\n").is_empty());
        // seul le mot exact compte
        assert!(parse_hwaccels_output("vaapi_extra").is_empty());
    }

    #[test]
    fn auto_prefers_gpu_then_cpu() {
        assert_eq!(resolve_auto(&[]), HwAccel::Cpu);
        assert_eq!(resolve_auto(&[HwAccel::Vaapi, HwAccel::Cuda]), HwAccel::Vaapi);
    }

    #[test]
    fn cpu_args_have_no_hwaccel_and_two_maps() {
        let spec = JobSpec { width: 160, height: 120, qv: 2, accel: HwAccel::Cpu };
        let a = build_ffmpeg_args("in.mkv", "out.amv", &spec, Some(1));
        assert!(!a.iter().any(|s| s == "-hwaccel"));
        assert_eq!(a.iter().filter(|s| *s == "-map").count(), 2);
        assert!(a.contains(&"out.amv".to_string()));
        assert!(a.contains(&"adpcm_ima_amv".to_string()));
        assert!(a.contains(&"-block_size".to_string()));
        assert!(a.contains(&"735".to_string()));
        assert!(a.contains(&"-strict".to_string()));
        let i = input_option_index(&a).unwrap();
        assert_eq!(a[i + 1], "in.mkv");
        // l'entrée vient après les options globales de progression
        assert!(i > a.iter().position(|s| s == "-progress").unwrap());
    }

    #[test]
    fn gpu_args_are_input_options() {
        for accel in [HwAccel::Vaapi, HwAccel::Cuda, HwAccel::Qsv] {
            let spec = JobSpec { width: 160, height: 120, qv: 2, accel };
            let a = build_ffmpeg_args("in.mkv", "out.amv", &spec, None);
            let i = input_option_index(&a).unwrap();
            let hw = a.iter().position(|s| s == "-hwaccel").expect("pas de -hwaccel");
            assert!(hw < i, "-hwaccel doit précéder -i pour {:?}", accel);
            assert_eq!(a[hw + 1], accel.ffmpeg_name().unwrap().to_string());
            // deux flux mappés dans tous les cas (vidéo + audio réel ou silencieux)
            assert_eq!(a.iter().filter(|s| *s == "-map").count(), 2);
            assert!(a.contains(&"1:a:0".to_string()), "piste silencieuse non mappée");
            assert!(a.contains(&"-shortest".to_string()));
            let an = a.iter().position(|s| s == "anullsrc=r=22050:cl=mono").unwrap();
            let m1 = a.iter().position(|s| s == "1:a:0").unwrap();
            assert!(an < m1);
        }
        // VAAPI a besoin d'un nœud de rendu
        let spec = JobSpec { width: 160, height: 120, qv: 2, accel: HwAccel::Vaapi };
        let a = build_ffmpeg_args("in.mkv", "out.amv", &spec, None);
        let d = a.iter().position(|s| s == "-hwaccel_device").unwrap();
        assert!(a[d + 1].starts_with("/dev/dri/renderD"));
    }

    #[test]
    fn duration_parsers() {
        let single = "123.45\n";
        assert_eq!(parse_first_number(single), Some(123.45));
        let na = "N/A\n";
        assert_eq!(parse_first_number(na), None);
        let multi = "10\n20\n15\n";
        assert_eq!(parse_max_number(multi), Some(20.0));
        assert_eq!(parse_frac("30/1"), Some(30.0));
        assert_eq!(parse_frac("30000/1001").map(|v| (v * 100.0).round() / 100.0), Some(29.97));
        assert_eq!(parse_frac("0/0"), None);
    }

    #[test]
    fn audio_stream_prefers_french() {
        assert_eq!(parse_audio_streams("1,eng\n2,fra\n"), Some(2));
        assert_eq!(parse_audio_streams("1,eng\n2,deu\n"), Some(1));
        assert_eq!(parse_audio_streams(""), None);
    }

    #[test]
    fn size_parsing() {
        assert_eq!(split_size("160x120"), Ok((160, 120)));
        assert!(split_size("160").is_err());
        assert!(split_size("0x120").is_err());
        assert!(split_size("axb").is_err());
    }

    #[test]
    fn output_name_is_unique_and_not_the_input() {
        let dir = std::env::temp_dir().join(format!("amv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("film.mkv");
        std::fs::write(&input, b"x").unwrap();
        let o1 = unique_output(input.to_str().unwrap());
        assert!(o1.ends_with("film_amv.amv"));
        std::fs::write(&o1, b"x").unwrap();
        let o2 = unique_output(input.to_str().unwrap());
        assert!(o2.ends_with("film_amv_2.amv"));
        assert_ne!(o1, o2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn accel_choice_from_label() {
        let avail = vec![HwAccel::Vaapi];
        assert_eq!(choose_accel("Auto (GPU si disponible)", &avail), HwAccel::Vaapi);
        assert_eq!(choose_accel("CPU (logiciel)", &avail), HwAccel::Cpu);
        assert_eq!(choose_accel("GPU — VAAPI (Intel/AMD)", &avail), HwAccel::Vaapi);
        // libellé GPU non disponible → repli sûr
        assert_eq!(choose_accel("GPU — NVDEC (NVIDIA)", &avail), HwAccel::Vaapi);
    }
}
