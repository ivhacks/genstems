use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, exit};
use std::time::{SystemTime, UNIX_EPOCH};

const STEM_JSON: &str = r##"{"version":1,"mastering_dsp":{"compressor":{"enabled":false,"input_gain":0,"output_gain":0,"threshold":0.0,"dry_wet":0,"attack":0.001,"release":0.2,"ratio":1.5,"hp_cutoff":50},"limiter":{"enabled":false,"threshold":0.0,"ceiling":-0.35,"release":0.05}},"stems":[{"name":"Vocal","color":"#ad65ff"},{"name":"Instrumental","color":"#00e8e8"},{"name":"-","color":"#3a3a3a"},{"name":"-","color":"#3a3a3a"}]}"##;

fn main() {
    let args: Vec<String> = env::args().collect();
    let master = flag(&args, "--master");
    let vocal = flag(&args, "--vocal");
    let instrumental = flag(&args, "--instrumental");
    let output = optional_flag(&args, "--output").unwrap_or_else(|| {
        let stem = Path::new(&master)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        format!("{stem}.stem.mp4")
    });

    for (name, path) in [
        ("--master", &master),
        ("--vocal", &vocal),
        ("--instrumental", &instrumental),
    ] {
        if !Path::new(path).is_file() {
            die(&format!("{name} file not found: {path}"));
        }
    }

    let work = work_dir();
    fs::create_dir_all(&work).expect("mkdir work dir");

    let master_m4a = work.join("master.m4a");
    let vocal_m4a = work.join("vocal.m4a");
    let instrumental_m4a = work.join("instrumental.m4a");
    let silence_m4a = work.join("silence.m4a");
    let stem_json = work.join("stem.json");

    to_alac(&master, &master_m4a);
    to_alac(&vocal, &vocal_m4a);
    to_alac(&instrumental, &instrumental_m4a);

    let duration = probe(&master_m4a, "format=duration");
    let sample_rate = probe(&master_m4a, "stream=sample_rate");
    make_silence(&silence_m4a, &duration, &sample_rate);

    fs::write(&stem_json, STEM_JSON).expect("write stem.json");
    let stem_b64 = base64_file(&stem_json);

    // track layout: master, Vocal, Instrumental, silence, silence
    run(
        "MP4Box",
        &[
            "-add",
            &format!("{}#audio:name=Master", master_m4a.display()),
            "-add",
            &format!("{}#audio:disable:name=Vocal", vocal_m4a.display()),
            "-add",
            &format!(
                "{}#audio:disable:name=Instrumental",
                instrumental_m4a.display()
            ),
            "-add",
            &format!("{}#audio:disable:name=-", silence_m4a.display()),
            "-add",
            &format!("{}#audio:disable:name=-", silence_m4a.display()),
            "-udta",
            &format!("0:type=stem:src=base64,{stem_b64}"),
            "-brand",
            "M4A ",
            "-ab",
            "isom",
            "-ab",
            "mp42",
            "-new",
            &output,
        ],
    );

    let _ = fs::remove_dir_all(&work);
    eprintln!("wrote {output}");
}

fn flag(args: &[String], name: &str) -> String {
    optional_flag(args, name).unwrap_or_else(|| {
        die(
            "usage: genstems --master FILE --vocal FILE --instrumental FILE [--output FILE]",
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

fn to_alac(input: &str, output: &Path) {
    run(
        "ffmpeg",
        &[
            "-y",
            "-i",
            input,
            "-c:a",
            "alac",
            "-map",
            "0:a:0",
            output.to_str().unwrap(),
        ],
    );
}

fn make_silence(output: &Path, duration: &str, sample_rate: &str) {
    run(
        "ffmpeg",
        &[
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("anullsrc=r={sample_rate}:cl=stereo"),
            "-t",
            duration,
            "-sample_fmt",
            "s16p",
            "-c:a",
            "alac",
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

fn base64_file(path: &Path) -> String {
    let output = Command::new("base64")
        .args(["-w0", path.to_str().unwrap()])
        .output()
        .expect("base64 failed to start");
    if !output.status.success() {
        die("base64 failed");
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn run(program: &str, args: &[&str]) {
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
