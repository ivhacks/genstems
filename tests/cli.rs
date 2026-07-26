//! end-to-end: real genstems binary + ffmpeg/ffprobe/mp4box on PATH
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_genstems"))
}

fn require_tool(name: &str) {
    let ok = Command::new(name)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        || Command::new(name)
            .arg("-h")
            .output()
            .map(|o| o.status.success() || o.status.code().is_some())
            .unwrap_or(false);
    // ffprobe/ffmpeg accept -version; base64 may not — just try which via empty run
    let found = Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(found || ok, "required tool missing from PATH: {name}");
}

fn make_flac(path: &Path, freq: u32, secs: f64) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=f={freq}:d={secs}"),
            "-c:a",
            "flac",
            path.to_str().unwrap(),
        ])
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed writing {}", path.display());
}

fn stream_count(path: &Path) -> usize {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe");
    assert!(out.status.success(), "ffprobe failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

fn stream_duration(path: &Path, index: usize) -> f64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            &format!("a:{index}"),
            "-show_entries",
            "stream=duration",
            "-of",
            "csv=p=0",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe duration");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("duration parse")
}

fn codec_name(path: &Path, index: usize) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            &format!("a:{index}"),
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe codec");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn has_stem_udta(path: &Path) -> bool {
    let out = Command::new("MP4Box")
        .args(["-info", path.to_str().unwrap()])
        .output()
        .expect("MP4Box -info");
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{text}{err}");
    combined.contains("stem:")
        && combined.contains(r#""version":1"#)
        && combined.contains(r#""name":"Vocal""#)
        && combined.contains(r#""name":"Instrumental""#)
}

#[test]
fn genstems_packs_flac_into_five_track_stem_mp4() {
    for tool in ["ffmpeg", "ffprobe", "MP4Box", "base64"] {
        require_tool(tool);
    }

    let dir = std::env::temp_dir().join(format!(
        "genstems-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();

    let master = dir.join("clarity.flac");
    let vocal = dir.join("clarity_vocal.flac");
    let instrumental = dir.join("clarity_instrumental.flac");
    let output = dir.join("out.stem.mp4");

    make_flac(&master, 440, 1.5);
    make_flac(&vocal, 880, 1.5);
    make_flac(&instrumental, 220, 1.5);

    let status = Command::new(bin())
        .args([
            "--master",
            master.to_str().unwrap(),
            "--vocal",
            vocal.to_str().unwrap(),
            "--instrumental",
            instrumental.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .status()
        .expect("run genstems");
    assert!(status.success(), "genstems exited non-zero");
    assert!(output.is_file(), "missing output {}", output.display());

    assert_eq!(stream_count(&output), 5, "expected 5 audio tracks");
    for i in 0..5 {
        assert_eq!(codec_name(&output, i), "alac", "track {i} not alac");
    }

    let master_d = stream_duration(&output, 0);
    let sil_a = stream_duration(&output, 3);
    let sil_b = stream_duration(&output, 4);
    assert!(
        (sil_a - master_d).abs() < 0.1,
        "silence track 3 duration {sil_a} != master {master_d}"
    );
    assert!(
        (sil_b - master_d).abs() < 0.1,
        "silence track 4 duration {sil_b} != master {master_d}"
    );
    assert!(
        (master_d - 1.5).abs() < 0.15,
        "master duration {master_d} not ~1.5s"
    );

    assert!(
        has_stem_udta(&output),
        "stem udta / 4-stem json missing from output"
    );

    let _ = fs::remove_dir_all(&dir);
}
