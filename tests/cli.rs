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

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "genstems-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
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

/// the four `{"color":..,"name":..}` entries from the stem udta, in track order.
fn stem_colors(path: &Path) -> Vec<String> {
    let out = Command::new("MP4Box")
        .args(["-info", path.to_str().unwrap()])
        .output()
        .expect("MP4Box -info");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("stem: "))
        .expect("no stem udta in file");
    let meta: serde_json::Value = serde_json::from_str(json).expect("stem udta is not json");
    meta["stems"]
        .as_array()
        .expect("stems[]")
        .iter()
        .map(|s| s["color"].as_str().expect("color").to_string())
        .collect()
}

/// build a flac carrying `image` as its cover picture.
fn make_flac_with_cover(path: &Path, image: &Path, freq: u32, secs: f64) {
    make_flac(path, freq, secs);
    let status = Command::new("metaflac")
        .arg(format!("--import-picture-from={}", image.display()))
        .arg(path)
        .status()
        .expect("spawn metaflac");
    assert!(status.success(), "metaflac failed on {}", path.display());
}

/// a two-tone cover: 2/3 `major`, 1/3 `minor`.
///
/// written as a ppm of literal rgb bytes, because ffmpeg's `color` filter goes
/// through yuv and hands back 253,0,0 for "red" — close enough to look right,
/// far enough to wreck an exact-hex assertion.
fn make_cover_of(path: &Path, major: [u8; 3], minor: [u8; 3]) {
    const W: usize = 240;
    const H: usize = 240;

    let mut ppm = format!("P6\n{W} {H}\n255\n").into_bytes();
    for _ in 0..H {
        for x in 0..W {
            ppm.extend(if x < W * 2 / 3 { major } else { minor });
        }
    }

    let ppm_path = path.with_extension("ppm");
    fs::write(&ppm_path, ppm).expect("write ppm");

    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-i",
            ppm_path.to_str().unwrap(),
            path.to_str().unwrap(),
        ])
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed writing {}", path.display());
}

/// the red/cyan cover the colour assertions are written against.
fn make_cover(path: &Path) {
    make_cover_of(path, [255, 0, 0], [0, 255, 255]);
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
    for tool in ["ffmpeg", "ffprobe", "MP4Box"] {
        require_tool(tool);
    }

    let dir = temp_dir();

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
        assert_eq!(codec_name(&output, i), "flac", "track {i} not flac");
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

/// normal path: packing a master that has cover art colors the stems from it.
#[test]
fn genstems_colors_stems_from_master_cover_art() {
    for tool in ["ffmpeg", "ffprobe", "MP4Box", "metaflac"] {
        require_tool(tool);
    }

    let dir = temp_dir();
    let cover = dir.join("cover.png");
    let master = dir.join("song.flac");
    let vocal = dir.join("song_vocal.flac");
    let instrumental = dir.join("song_instrumental.flac");
    let output = dir.join("out.stem.mp4");

    make_cover(&cover);
    make_flac_with_cover(&master, &cover, 440, 1.5);
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

    // track order: instrumental then vocal. red dominates, cyan is runner-up.
    let colors = stem_colors(&output);
    assert_eq!(
        colors[0], "#ff0000",
        "instrumental (stem 0) should be the cover's red"
    );
    assert_eq!(
        colors[1], "#00ffff",
        "vocal (stem 1) should be the cover's cyan"
    );
    // the two silent tracks stay grey
    assert_eq!(colors[2], "#3a3a3a");
    assert_eq!(colors[3], "#3a3a3a");

    let _ = fs::remove_dir_all(&dir);
}

/// backfill path: --colors recolors an existing .stem.mp4 in place.
#[test]
fn genstems_colors_flag_recolors_existing_stem_file() {
    for tool in ["ffmpeg", "ffprobe", "MP4Box", "metaflac"] {
        require_tool(tool);
    }

    let dir = temp_dir();
    let cover = dir.join("cover.png");
    let master = dir.join("song.flac");
    let vocal = dir.join("song_vocal.flac");
    let instrumental = dir.join("song_instrumental.flac");
    let output = dir.join("out.stem.mp4");

    // pack from a bare master, so the stem file starts on the default colors
    make_cover(&cover);
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
    assert_eq!(
        stem_colors(&output)[0],
        "#00e8e8",
        "a master with no cover art should leave the default instrumental color"
    );

    // now give it art, the way an already-packed file in the library would have it
    let status = Command::new("MP4Box")
        .args([
            "-itags",
            &format!("cover={}", cover.display()),
            output.to_str().unwrap(),
        ])
        .status()
        .expect("MP4Box -itags");
    assert!(status.success(), "adding cover art failed");

    let status = Command::new(bin())
        .args(["--colors", output.to_str().unwrap()])
        .status()
        .expect("run genstems --colors");
    assert!(status.success(), "genstems --colors exited non-zero");

    let colors = stem_colors(&output);
    assert_eq!(
        colors[0], "#ff0000",
        "instrumental (stem 0) should be the cover's red"
    );
    assert_eq!(
        colors[1], "#00ffff",
        "vocal (stem 1) should be the cover's cyan"
    );
    assert_eq!(colors[2], "#3a3a3a");
    assert_eq!(colors[3], "#3a3a3a");

    // recoloring must not disturb the audio or the rest of the stem metadata
    assert_eq!(stream_count(&output), 5, "expected 5 audio tracks");
    assert!(has_stem_udta(&output), "stem udta damaged by --colors");

    let _ = fs::remove_dir_all(&dir);
}

/// a black-and-white cover has no hues, so the stems take its lightest and
/// darkest tones instead.
#[test]
fn genstems_colors_a_black_and_white_cover_by_lightness() {
    for tool in ["ffmpeg", "ffprobe", "MP4Box", "metaflac"] {
        require_tool(tool);
    }

    let dir = temp_dir();
    let cover = dir.join("cover.png");
    let master = dir.join("song.flac");
    let vocal = dir.join("song_vocal.flac");
    let instrumental = dir.join("song_instrumental.flac");
    let output = dir.join("out.stem.mp4");

    // white over near-black. the black is below the visible floor on purpose.
    make_cover_of(&cover, [255, 255, 255], [10, 10, 10]);
    make_flac_with_cover(&master, &cover, 440, 1.5);
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

    let colors = stem_colors(&output);
    // heavier extreme (white) → instrumental; dark floor → vocal
    assert_eq!(
        colors[0], "#ffffff",
        "instrumental (stem 0) should be the cover's white"
    );
    assert_eq!(
        colors[1], "#323232",
        "vocal (stem 1) should be the cover's black, floored"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// --swap-tracks flips instrumental/vocal audio + metadata on an existing file.
#[test]
fn genstems_swap_tracks_flips_instrumental_and_vocal() {
    for tool in ["ffmpeg", "ffprobe", "MP4Box", "metaflac"] {
        require_tool(tool);
    }

    let dir = temp_dir();
    let cover = dir.join("cover.png");
    let master = dir.join("song.flac");
    let vocal = dir.join("song_vocal.flac");
    let instrumental = dir.join("song_instrumental.flac");
    let output = dir.join("out.stem.mp4");

    make_cover(&cover);
    make_flac_with_cover(&master, &cover, 440, 1.5);
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

    // new layout: instrumental first, vocal second
    let before = stem_names_and_colors(&output);
    assert_eq!(before[0].0, "Instrumental");
    assert_eq!(before[1].0, "Vocal");
    assert_eq!(before[0].1, "#ff0000");
    assert_eq!(before[1].1, "#00ffff");

    let status = Command::new(bin())
        .args(["--swap-tracks", output.to_str().unwrap()])
        .status()
        .expect("run genstems --swap-tracks");
    assert!(status.success(), "genstems --swap-tracks exited non-zero");

    let after = stem_names_and_colors(&output);
    assert_eq!(after[0].0, "Vocal");
    assert_eq!(after[1].0, "Instrumental");
    assert_eq!(after[0].1, "#00ffff");
    assert_eq!(after[1].1, "#ff0000");
    assert_eq!(stream_count(&output), 5, "expected 5 audio tracks");
    assert!(has_stem_udta(&output), "stem udta damaged by --swap-tracks");

    let _ = fs::remove_dir_all(&dir);
}

fn stem_names_and_colors(path: &Path) -> Vec<(String, String)> {
    let dump = Command::new("MP4Box")
        .args(["-info", path.to_str().unwrap()])
        .output()
        .expect("MP4Box -info");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&dump.stdout),
        String::from_utf8_lossy(&dump.stderr)
    );
    let json = combined
        .lines()
        .find_map(|l| l.trim().strip_prefix("stem: "))
        .expect("no stem udta in file");
    let meta: serde_json::Value = serde_json::from_str(json).expect("stem udta is not json");
    meta["stems"]
        .as_array()
        .expect("stems[]")
        .iter()
        .map(|s| {
            (
                s["name"].as_str().expect("name").to_string(),
                s["color"].as_str().expect("color").to_string(),
            )
        })
        .collect()
}
