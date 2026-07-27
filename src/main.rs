mod metadata;
mod split;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, exit};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;

// full NI schema (dsp knobs stay even when disabled — picky players expect them).
// parsed at runtime so a typo dies early; re-serialized compact for the udta box.
const STEM_JSON: &str = r##"{
  "version": 1,
  "mastering_dsp": {
    "compressor": {
      "enabled": false,
      "input_gain": 0, "output_gain": 0, "threshold": 0.0,
      "dry_wet": 0, "attack": 0.001, "release": 0.2, "ratio": 1.5, "hp_cutoff": 50
    },
    "limiter": {
      "enabled": false,
      "threshold": 0.0, "ceiling": -0.35, "release": 0.05
    }
  },
  "stems": [
    { "name": "Vocal", "color": "#ad65ff" },
    { "name": "Instrumental", "color": "#00e8e8" },
    { "name": "-", "color": "#3a3a3a" },
    { "name": "-", "color": "#3a3a3a" }
  ]
}"##;

fn main() {
    let args: Vec<String> = env::args().collect();

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

fn pack_stems(master: &str, vocal: &str, instrumental: &str, output: &str) {
    let stem_b64 = stem_payload_b64();

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

    // track layout: master, Vocal, Instrumental, silence, silence
    run_cmd(
        "MP4Box",
        &[
            "-add",
            &format!("{}#audio:name=Master", master_a.display()),
            "-add",
            &format!("{}#audio:disable:name=Vocal", vocal_a.display()),
            "-add",
            &format!(
                "{}#audio:disable:name=Instrumental",
                instrumental_a.display()
            ),
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

fn stem_payload_b64() -> String {
    let meta: Value =
        serde_json::from_str(STEM_JSON).unwrap_or_else(|e| die(&format!("STEM_JSON invalid: {e}")));
    let stems = meta
        .get("stems")
        .and_then(|s| s.as_array())
        .unwrap_or_else(|| die("STEM_JSON missing stems[]"));
    if stems.len() != 4 {
        die(&format!("STEM_JSON needs 4 stems, got {}", stems.len()));
    }
    B64.encode(serde_json::to_string(&meta).unwrap().as_bytes())
}

fn flag(args: &[String], name: &str) -> String {
    optional_flag(args, name).unwrap_or_else(|| {
        die(
            "usage:\n  genstems --master FILE --vocal FILE --instrumental FILE [--output FILE]\n  genstems --split FILE --token TOKEN [--output FILE]",
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

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    exit(1);
}
