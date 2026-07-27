//! copy tags + cover art from flac/mp3 onto an existing .stem.mp4 (preserves stem udta).
//! flac: vorbis comments + PICTURE via metaflac → MP4Box -itags
//! mp3: not yet (structure ready)

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run(input: &str, output: &str) {
    let input_path = Path::new(input);
    let output_path = Path::new(output);

    if !input_path.is_file() {
        die(&format!("--input not found: {input}"));
    }
    if !output_path.is_file() {
        die(&format!("--output not found: {output}"));
    }

    let in_ext = input_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let out_name = output_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if !out_name.ends_with(".stem.mp4") {
        die("--output must be a .stem.mp4 file");
    }

    match in_ext.as_str() {
        "flac" => transfer_flac(input_path, output_path),
        "mp3" => die("mp3 metadata transfer not implemented yet (flac only for now)"),
        _ => die(&format!("--input must be .flac or .mp3 (got .{in_ext})")),
    }

    eprintln!("transferred metadata {input} → {output}");
}

fn transfer_flac(input: &Path, output: &Path) {
    require("metaflac");
    require("MP4Box");

    let tags = read_vorbis_comments(input);
    let work = work_dir();
    fs::create_dir_all(&work).expect("mkdir");

    // prefer front cover (type 3); fall back to any picture
    let cover_path = export_cover(input, &work);

    let itags_path = work.join("itags.txt");
    write_itags_file(&itags_path, &tags, cover_path.as_deref());

    // in-place tag write; preserves stem udta (unlike mutagen/ffmpeg remux)
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

    let _ = fs::remove_dir_all(&work);
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
            // last wins for duplicate keys (rare); multi-value flac tags collapse to last
            map.insert(k.to_ascii_uppercase(), v.to_string());
        }
    }
    map
}

fn export_cover(input: &Path, work: &Path) -> Option<PathBuf> {
    // try front cover first (block type filter not available for export-picture-to simply);
    // metaflac --export-picture-to exports the first PICTURE block by default.
    // if multiple, prefer type=3 via --block-number after listing.
    let list = Command::new("metaflac")
        .args(["--list", "--block-type=PICTURE", input.to_str().unwrap()])
        .output()
        .ok()?;
    if !list.status.success() {
        return None;
    }
    let list_txt = String::from_utf8_lossy(&list.stdout);
    if !list_txt.contains("type: 6 (PICTURE)") && !list_txt.contains("PICTURE") {
        // also check "type: 3 (Cover"
        if !list_txt.contains("Cover") && !list_txt.contains("MIME type") {
            return None;
        }
    }

    // pick block number of Cover (front) if present
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
            block_num = cur; // first picture as fallback candidate
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

/// map vorbis keys → MP4Box -itags names; unknown keys go as QT/<KEY>
fn write_itags_file(path: &Path, tags: &BTreeMap<String, String>, cover: Option<&Path>) {
    // known mapping (vorbis UPPER → itags name)
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
        ("ISRC", "ISRC"), // 4cc freeform-ish; MP4Box allows 4-char codes
    ];

    let mut lines: Vec<String> = Vec::new();
    // cover first so multi-line lyrics don't swallow it
    if let Some(c) = cover {
        lines.push(format!("cover={}", c.display()));
    }

    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();

    // track / disc fractions
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

    for (vorbis, itag) in known {
        if used.contains(*vorbis) {
            continue;
        }
        if let Some(val) = tags.get(*vorbis) {
            if val.is_empty() {
                continue;
            }
            // multi-line values: first line TAG=val, rest bare continuation
            let mut parts = val.split('\n');
            if let Some(first) = parts.next() {
                lines.push(format!("{itag}={first}"));
                for cont in parts {
                    lines.push(cont.to_string());
                }
            }
            used.insert((*vorbis).into());
        }
    }

    // leftover vorbis comments as QT metadata keys (preserves custom fields)
    for (k, v) in tags {
        if used.contains(k) || v.is_empty() {
            continue;
        }
        // skip vendor-ish noise
        if k == "ENCODER" || k == "VENDOR" {
            continue;
        }
        let mut parts = v.split('\n');
        if let Some(first) = parts.next() {
            lines.push(format!("QT/{k}={first}"));
            for cont in parts {
                lines.push(cont.to_string());
            }
        }
    }

    if lines.is_empty() {
        die("input has no tags or cover art to transfer");
    }

    fs::write(path, lines.join("\n") + "\n").expect("write itags file");
}

fn work_dir() -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("genstems-meta-{n}"))
}

fn require(bin: &str) {
    let ok = Command::new(bin)
        .arg("-h")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success() || s.code().is_some())
        .unwrap_or(false)
        || Command::new("which")
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
    fn itags_file_maps_common_vorbis() {
        let mut tags = BTreeMap::new();
        tags.insert("TITLE".into(), "Lost At Sea".into());
        tags.insert("ARTIST".into(), "Zedd".into());
        tags.insert("TRACKNUMBER".into(), "4".into());
        tags.insert("TRACKTOTAL".into(), "10".into());
        tags.insert("LYRICS".into(), "line1\nline2".into());
        tags.insert("CUSTOMFOO".into(), "bar".into());

        let dir = work_dir();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("itags.txt");
        write_itags_file(&path, &tags, Some(Path::new("/tmp/c.jpg")));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("cover=/tmp/c.jpg"));
        assert!(text.contains("title=Lost At Sea"));
        assert!(text.contains("artist=Zedd"));
        assert!(text.contains("tracknum=4/10"));
        assert!(text.contains("lyrics=line1\nline2"));
        assert!(text.contains("QT/CUSTOMFOO=bar"));
        let _ = fs::remove_dir_all(&dir);
    }
}
