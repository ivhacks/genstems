//! copy tags + cover art from a source track onto a finished .stem.mp4 (preserves stem udta).
//! called automatically after packing.
//!
//! flac: metaflac tags + picture → MP4Box -itags
//! anything else (mp3, …): ffprobe tags + ffmpeg attached pic → MP4Box -itags

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// pull metadata from `source` (master / original) onto `stem` output.
pub fn from_source(source: &str, stem: &str) {
    let source_path = Path::new(source);
    let stem_path = Path::new(stem);

    if !source_path.is_file() {
        die(&format!("metadata source not found: {source}"));
    }
    if !stem_path.is_file() {
        die(&format!("stem file not found: {stem}"));
    }

    let in_ext = source_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match in_ext.as_str() {
        "flac" => transfer_flac(source_path, stem_path),
        _ => transfer_ffprobe(source_path, stem_path),
    }
}

fn transfer_flac(input: &Path, output: &Path) {
    require("metaflac");
    require("MP4Box");

    let tags = read_vorbis_comments(input);
    let work = work_dir();
    fs::create_dir_all(&work).expect("mkdir");
    let cover = export_cover_flac(input, &work);
    apply_itags(&work, &tags, cover.as_deref(), output);
    let _ = fs::remove_dir_all(&work);
}

fn transfer_ffprobe(input: &Path, output: &Path) {
    require("ffprobe");
    require("ffmpeg");
    require("MP4Box");

    let tags = read_ffprobe_tags(input);
    let work = work_dir();
    fs::create_dir_all(&work).expect("mkdir");
    let cover = export_cover_ffmpeg(input, &work);
    apply_itags(&work, &tags, cover.as_deref(), output);
    let _ = fs::remove_dir_all(&work);
}

fn apply_itags(work: &Path, tags: &BTreeMap<String, String>, cover: Option<&Path>, output: &Path) {
    let itags_path = work.join("itags.txt");
    write_itags_file(&itags_path, tags, cover);
    if !itags_path.is_file() {
        return;
    }

    let status = Command::new("MP4Box")
        .args([
            "-itags",
            itags_path.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .unwrap_or_else(|e| die(&format!("MP4Box failed: {e}")));
    if !status.success() {
        die("MP4Box -itags failed");
    }
}

fn read_vorbis_comments(input: &Path) -> BTreeMap<String, String> {
    let out = Command::new("metaflac")
        .args(["--export-tags-to=-", input.to_str().unwrap()])
        .output()
        .unwrap_or_else(|e| die(&format!("metaflac failed: {e}")));
    if !out.status.success() {
        die(&format!(
            "metaflac export tags failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = BTreeMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_ascii_uppercase(), v.to_string());
        }
    }
    map
}

fn read_ffprobe_tags(input: &Path) -> BTreeMap<String, String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format_tags",
            "-of",
            "json",
            input.to_str().unwrap(),
        ])
        .output()
        .unwrap_or_else(|e| die(&format!("ffprobe failed: {e}")));
    if !out.status.success() {
        die(&format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let v: Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| die(&format!("ffprobe json: {e}")));
    let Some(tags) = v
        .get("format")
        .and_then(|f| f.get("tags"))
        .and_then(|t| t.as_object())
    else {
        return BTreeMap::new();
    };

    let mut map = BTreeMap::new();
    for (k, val) in tags {
        let Some(s) = val.as_str() else { continue };
        let key = normalize_tag_key(k);
        // normalize newlines from id3
        let value = s.replace("\r\n", "\n").replace('\r', "\n");
        map.insert(key, value);
    }
    map
}

/// map ffprobe/id3-ish keys onto the same UPPERCASE keys write_itags_file expects
fn normalize_tag_key(k: &str) -> String {
    let u = k.to_ascii_uppercase().replace('-', "_");
    match u.as_str() {
        "ALBUM_ARTIST" => "ALBUMARTIST".into(),
        "ENCODED_BY" => "ENCODEDBY".into(),
        "LYRICS_ENG" | "LYRICS" => "LYRICS".into(),
        "TRACK" => "TRACKNUMBER".into(),
        "DISC" => "DISCNUMBER".into(),
        "TBPM" | "BPM" => "BPM".into(),
        "TSRC" | "ISRC" => "ISRC".into(),
        "YEAR" => "DATE".into(),
        other => other.into(),
    }
}

fn export_cover_flac(input: &Path, work: &Path) -> Option<PathBuf> {
    let list = Command::new("metaflac")
        .args(["--list", "--block-type=PICTURE", input.to_str().unwrap()])
        .output()
        .ok()?;
    if !list.status.success() {
        return None;
    }
    let list_txt = String::from_utf8_lossy(&list.stdout);
    if !list_txt.contains("MIME type") && !list_txt.contains("PICTURE") {
        return None;
    }

    let mut block_num: Option<u32> = None;
    let mut cur: Option<u32> = None;
    for line in list_txt.lines() {
        if let Some(rest) = line.trim().strip_prefix("METADATA block #") {
            cur = rest.trim().parse().ok();
        }
        if line.contains("type: 3 (Cover") {
            block_num = cur;
            break;
        }
        if block_num.is_none() && line.contains("MIME type:") {
            block_num = cur;
        }
    }

    let cover = work.join("cover.bin");
    let mut args = vec![
        "--export-picture-to".to_string(),
        cover.to_str().unwrap().to_string(),
    ];
    if let Some(n) = block_num {
        args.push(format!("--block-number={n}"));
    }
    args.push(input.to_str().unwrap().to_string());

    let status = Command::new("metaflac").args(&args).status().ok()?;
    if !status.success() || !cover.is_file() || cover.metadata().ok()?.len() == 0 {
        return None;
    }
    Some(cover)
}

fn export_cover_ffmpeg(input: &Path, work: &Path) -> Option<PathBuf> {
    // attached pic is the video stream on mp3 with apic
    let cover = work.join("cover.jpg");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            input.to_str().unwrap(),
            "-an",
            "-c:v",
            "copy",
            "-update",
            "1",
            cover.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() || !cover.is_file() || cover.metadata().ok()?.len() == 0 {
        return None;
    }
    Some(cover)
}

/// map common keys → MP4Box -itags names; unknown keys go as QT/<KEY>
fn write_itags_file(path: &Path, tags: &BTreeMap<String, String>, cover: Option<&Path>) {
    // MP4Box itags text files are line-oriented; keep every value on one line.
    let known: &[(&str, &str)] = &[
        ("TITLE", "title"),
        ("ARTIST", "artist"),
        ("ALBUM", "album"),
        ("ALBUMARTIST", "album_artist"),
        ("DATE", "created"),
        ("YEAR", "created"),
        ("GENRE", "genre"),
        ("COPYRIGHT", "copyright"),
        ("COMPOSER", "composer"),
        ("COMMENT", "comment"),
        ("DESCRIPTION", "comment"),
        ("LYRICS", "lyrics"),
        ("UNSYNCEDLYRICS", "lyrics"),
        ("BPM", "tempo"),
        ("TEMPO", "tempo"),
        ("GROUPING", "group"),
        ("CONDUCTOR", "conductor"),
        ("LYRICIST", "lyricist"),
        ("ENCODEDBY", "encoder"),
        ("ENCODER", "encoder"),
        ("TOOL", "tool"),
        ("PUBLISHER", "publisher"),
        ("ORGANIZATION", "publisher"),
    ];

    let mut lines: Vec<String> = Vec::new();
    if let Some(c) = cover {
        lines.push(format!("cover={}", c.display()));
    }

    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();

    if let Some(tn) = tags.get("TRACKNUMBER") {
        let total = tags.get("TRACKTOTAL").map(|s| s.as_str()).unwrap_or("0");
        let num = tn.split('/').next().unwrap_or(tn);
        let tot = if tn.contains('/') {
            tn.split('/').nth(1).unwrap_or(total)
        } else {
            total
        };
        lines.push(format!("tracknum={num}/{tot}"));
        used.insert("TRACKNUMBER".into());
        used.insert("TRACKTOTAL".into());
    }
    if let Some(dn) = tags.get("DISCNUMBER") {
        let total = tags.get("DISCTOTAL").map(|s| s.as_str()).unwrap_or("0");
        let num = dn.split('/').next().unwrap_or(dn);
        let tot = if dn.contains('/') {
            dn.split('/').nth(1).unwrap_or(total)
        } else {
            total
        };
        lines.push(format!("disk={num}/{tot}"));
        used.insert("DISCNUMBER".into());
        used.insert("DISCTOTAL".into());
    }

    for (src, itag) in known {
        if used.contains(*src) {
            continue;
        }
        if let Some(val) = tags.get(*src) {
            if val.is_empty() {
                continue;
            }
            lines.push(format!("{itag}={}", flat_line(val)));
            used.insert((*src).into());
        }
    }

    // note: don't dump arbitrary leftover keys as QT/* — MP4Box drops the normal
    // itunes tags when those freeform QT keys are mixed into the same -itags file.

    if lines.is_empty() {
        return; // nothing to write
    }

    fs::write(path, lines.join("\n") + "\n").expect("write itags file");
}

fn flat_line(s: &str) -> String {
    s.replace('\r', "").replace('\n', " / ")
}

fn work_dir() -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("genstems-meta-{n}"))
}

fn require(bin: &str) {
    let ok = Command::new("which")
        .arg(bin)
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        die(&format!("required tool not on PATH: {bin}"));
    }
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn itags_file_maps_common_keys() {
        let mut tags = BTreeMap::new();
        tags.insert("TITLE".into(), "Lost At Sea".into());
        tags.insert("ARTIST".into(), "Zedd".into());
        tags.insert("TRACKNUMBER".into(), "4".into());
        tags.insert("TRACKTOTAL".into(), "10".into());
        tags.insert("LYRICS".into(), "line1\nline2".into());

        let dir = work_dir();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("itags.txt");
        write_itags_file(&path, &tags, Some(Path::new("/tmp/c.jpg")));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("cover=/tmp/c.jpg"));
        assert!(text.contains("title=Lost At Sea"));
        assert!(text.contains("artist=Zedd"));
        assert!(text.contains("tracknum=4/10"));
        assert!(text.contains("lyrics=line1 / line2"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_ffprobe_keys() {
        assert_eq!(normalize_tag_key("album_artist"), "ALBUMARTIST");
        assert_eq!(normalize_tag_key("lyrics-eng"), "LYRICS");
        assert_eq!(normalize_tag_key("TBPM"), "BPM");
        assert_eq!(normalize_tag_key("track"), "TRACKNUMBER");
        assert_eq!(normalize_tag_key("TSRC"), "ISRC");
    }
}
