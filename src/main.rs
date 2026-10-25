mod colors;
mod metadata;
mod split;
mod stem_udta;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, exit};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = env::args().collect();

    // backfill: recolor a .stem.mp4 that already exists, in place
    if let Some(path) = optional_flag(&args, "--colors") {
        stem_udta::recolor_in_place(&path);
        return;
    }

    // swap instrumental/vocal track order on an existing .stem.mp4
    if let Some(path) = optional_flag(&args, "--swap-tracks") {
        stem_udta::swap_tracks_in_place(&path);
        return;
    }

    // break a .stem.mp4 back into its tracks, stream-copied (no transcoding)
    if let Some(path) = optional_flag(&args, "--dissociate") {
        require_tools(&["ffmpeg", "ffprobe"]);
        let output =
            optional_flag(&args, "--output").unwrap_or_else(|| default_dissociate_dir(&path));
        dissociate(&path, &output);
        return;
    }

    if let Some(path) = optional_flag(&args, "--split") {
        let token = optional_flag(&args, "--token")
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                die(
                    "split needs --token <next-token>.\n\
                     after logging in at vocalremover.org: devtools → application → localStorage → next-token\n\
                     then: genstems --split FILE --token <next-token>",
                )
            });
        let output = optional_flag(&args, "--output").unwrap_or_else(|| default_stem_out(&path));

        // check before the split: it burns vocalremover quota, so don't die after it
        require_tools(&["curl", "ffmpeg", "ffprobe", "metaflac", "MP4Box"]);
        let split = split::run(&path, &token);
        pack_stems(
            &path,
            split.vocal.to_str().unwrap(),
            split.instrumental.to_str().unwrap(),
            &output,
        );
        let _ = fs::remove_dir_all(&split.work);
        return;
    }

    let master = flag(&args, "--master");
    let vocal = flag(&args, "--vocal");
    let instrumental = flag(&args, "--instrumental");
    let output = optional_flag(&args, "--output").unwrap_or_else(|| default_stem_out(&master));

    require_tools(&["ffmpeg", "ffprobe", "metaflac", "MP4Box"]);

    for (name, path) in [
        ("--master", &master),
        ("--vocal", &vocal),
        ("--instrumental", &instrumental),
    ] {
        if !Path::new(path).is_file() {
            die(&format!("{name} file not found: {path}"));
        }
    }

    pack_stems(&master, &vocal, &instrumental, &output);
}

fn default_stem_out(master: &str) -> String {
    let stem = Path::new(master)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    format!("{stem}.stem.mp4")
}

fn default_dissociate_dir(stem_file: &str) -> String {
    let p = Path::new(stem_file);
    let dir = p.parent().unwrap_or(Path::new(""));
    dir.join(format!("{} stems", title_of(stem_file)))
        .display()
        .to_string()
}

fn title_of(stem_file: &str) -> String {
    let name = Path::new(stem_file)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    name.strip_suffix(".stem.mp4").unwrap_or(name).to_string()
}

// one file per track, in its native codec. tracks named "-" are our silent filler, skipped.
fn dissociate(stem_file: &str, out_dir: &str) {
    if !Path::new(stem_file).is_file() {
        die(&format!("--dissociate file not found: {stem_file}"));
    }
    let listing = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_name:stream_tags=handler_name",
            "-of",
            "csv=p=0",
            stem_file,
        ])
        .output()
        .unwrap_or_else(|e| die(&format!("failed to run ffprobe: {e}")));
    if !listing.status.success() {
        die(&format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&listing.stderr)
        ));
    }
    // NI stem layout when tracks are unnamed: master, then 4 stems
    let defaults = ["Master", "Drums", "Bass", "Other", "Vocals"];
    let title = title_of(stem_file);
    fs::create_dir_all(out_dir).expect("mkdir output dir");

    for (i, line) in String::from_utf8_lossy(&listing.stdout).lines().enumerate() {
        let (codec, handler) = line.split_once(',').unwrap_or((line, ""));
        let name = match handler.trim() {
            "-" => continue,
            "" | "SoundHandler" => defaults.get(i).copied().unwrap_or("Track").to_string(),
            h => h.to_string(),
        };
        let ext = match codec {
            "flac" => "flac",
            "mp3" => "mp3",
            "opus" | "vorbis" => "ogg",
            _ => "m4a", // aac, alac
        };
        let out = Path::new(out_dir).join(format!("{title} - {name}.{ext}"));
        run_cmd(
            "ffmpeg",
            &[
                "-v",
                "error",
                "-y",
                "-i",
                stem_file,
                "-map",
                &format!("0:a:{i}"),
                "-map_metadata",
                "-1",
                "-c",
                "copy",
                out.to_str().unwrap(),
            ],
        );
        eprintln!("wrote {}", out.display());
    }
}

fn pack_stems(master: &str, vocal: &str, instrumental: &str, output: &str) {
    // stem colors come from the master's cover art; without art we keep the defaults
    let stem_b64 = stem_udta::payload_b64(colors::from_cover_art(Path::new(master)).as_ref());

    let work = work_dir();
    fs::create_dir_all(&work).expect("mkdir work dir");

    // flac @ compression 12, in single-track mp4s (mp4box mis-times raw silent .flac)
    let master_a = work.join("master.mp4");
    let vocal_a = work.join("vocal.mp4");
    let instrumental_a = work.join("instrumental.mp4");
    let silence_a = work.join("silence.mp4");

    to_flac(master, &master_a);
    to_flac(vocal, &vocal_a);
    to_flac(instrumental, &instrumental_a);

    let duration = probe(&master_a, "format=duration");
    let sample_rate = probe(&master_a, "stream=sample_rate");
    make_silence(&silence_a, &duration, &sample_rate);

    // track layout: master, Instrumental, Vocal, silence, silence
    // (mixxx draws stem 2 on top of stem 1, so vocal sits above instrumental)
    run_cmd(
        "MP4Box",
        &[
            "-add",
            &format!("{}#audio:name=Master", master_a.display()),
            "-add",
            &format!(
                "{}#audio:disable:name=Instrumental",
                instrumental_a.display()
            ),
            "-add",
            &format!("{}#audio:disable:name=Vocal", vocal_a.display()),
            "-add",
            &format!("{}#audio:disable:name=-", silence_a.display()),
            "-add",
            &format!("{}#audio:disable:name=-", silence_a.display()),
            "-udta",
            &format!("0:type=stem:src=base64,{stem_b64}"),
            "-brand",
            "M4A ",
            "-ab",
            "isom",
            "-ab",
            "mp42",
            "-new",
            output,
        ],
    );

    let _ = fs::remove_dir_all(&work);

    // tags/cover from the master (original) track
    metadata::from_source(master, output);

    eprintln!("wrote {output}");
}

fn flag(args: &[String], name: &str) -> String {
    optional_flag(args, name).unwrap_or_else(|| {
        die(
            "usage:\n  genstems --master FILE --vocal FILE --instrumental FILE [--output FILE]\n  genstems --split FILE --token TOKEN [--output FILE]\n  genstems --colors FILE.stem.mp4\n  genstems --swap-tracks FILE.stem.mp4\n  genstems --dissociate FILE.stem.mp4 [--output DIR]",
        )
    })
}

fn optional_flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn work_dir() -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    env::temp_dir().join(format!("genstems-{n}"))
}

fn to_flac(input: &str, output: &Path) {
    run_cmd(
        "ffmpeg",
        &[
            "-y",
            "-i",
            input,
            "-map",
            "0:a:0",
            "-c:a",
            "flac",
            "-compression_level",
            "12",
            output.to_str().unwrap(),
        ],
    );
}

fn make_silence(output: &Path, duration: &str, sample_rate: &str) {
    run_cmd(
        "ffmpeg",
        &[
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("anullsrc=r={sample_rate}:cl=stereo"),
            "-t",
            duration,
            "-c:a",
            "flac",
            "-compression_level",
            "12",
            output.to_str().unwrap(),
        ],
    );
}

fn probe(file: &Path, entries: &str) -> String {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            entries,
            "-of",
            "csv=p=0",
            file.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe failed to start");
    if !output.status.success() {
        die(&format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn run_cmd(program: &str, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .unwrap_or_else(|e| die(&format!("failed to run {program}: {e}")));
    if !status.success() {
        die(&format!("{program} failed"));
    }
}

/// die naming every missing tool and how to get it, before doing any real work.
fn require_tools(tools: &[&str]) {
    let path = env::var_os("PATH").unwrap_or_default();
    let missing: Vec<&str> = tools
        .iter()
        .copied()
        .filter(|tool| !env::split_paths(&path).any(|dir| dir.join(tool).is_file()))
        .collect();
    if missing.is_empty() {
        return;
    }
    let mut msg = String::from("required tools not on PATH:");
    for tool in missing {
        msg += &format!("\n  {tool}: {}", install_hint(tool));
    }
    die(&msg);
}

fn install_hint(tool: &str) -> &'static str {
    match tool {
        "ffmpeg" | "ffprobe" => "debian: apt install ffmpeg | fedora: dnf install ffmpeg",
        "metaflac" => "debian: apt install flac | fedora: dnf install flac",
        "MP4Box" => {
            "debian: apt install gpac (or build from https://github.com/gpac/gpac) | fedora: dnf install gpac"
        }
        "curl" => "debian: apt install curl | fedora: dnf install curl",
        _ => "install it and put it on PATH",
    }
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    exit(1);
}
