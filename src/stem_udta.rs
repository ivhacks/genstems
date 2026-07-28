//! the `stem` udta box: the NI metadata blob that makes a plain mp4 a stem file.
//!
//! owns the JSON template used when packing, and the read-modify-write used to
//! recolor a .stem.mp4 that already exists.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;

use crate::colors::StemColors;

// full NI schema (dsp knobs stay even when disabled — picky players expect them).
// parsed at runtime so a typo dies early; re-serialized compact for the udta box.
// the stem colors here are fallbacks, used when the master has no cover art.
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
    { "name": "Instrumental", "color": "#00e8e8" },
    { "name": "Vocal", "color": "#ad65ff" },
    { "name": "-", "color": "#3a3a3a" },
    { "name": "-", "color": "#3a3a3a" }
  ]
}"##;

/// the payload for `MP4Box -udta` when packing a fresh stem file.
/// `colors` of `None` keeps the template's fallback colors.
pub fn payload_b64(colors: Option<&StemColors>) -> String {
    let mut meta: Value =
        serde_json::from_str(STEM_JSON).unwrap_or_else(|e| die(&format!("STEM_JSON invalid: {e}")));
    if let Some(c) = colors {
        set_colors(&mut meta, c);
    }
    encode(&meta)
}

/// recolor a .stem.mp4 in place from its own cover art. this is `--colors`.
pub fn recolor_in_place(stem: &str) {
    let path = Path::new(stem);
    if !path.is_file() {
        die(&format!("stem file not found: {stem}"));
    }

    let Some(colors) = crate::colors::from_cover_art(path) else {
        die(&format!("{stem} has no cover art to take colors from"));
    };

    let mut meta = read_udta(path);
    set_colors(&mut meta, &colors);
    write_udta(path, &meta);

    eprintln!(
        "{stem}: instrumental {} / vocal {}",
        colors.instrumental, colors.vocal
    );
}

/// swap the instrumental and vocal tracks (audio + stem names/colors) on an
/// existing .stem.mp4. master and the two silence tracks stay put.
pub fn swap_tracks_in_place(stem: &str) {
    let path = Path::new(stem);
    if !path.is_file() {
        die(&format!("stem file not found: {stem}"));
    }

    let mut meta = read_udta(path);
    let (name0, name1) = {
        let stems = meta
            .get_mut("stems")
            .and_then(|s| s.as_array_mut())
            .unwrap_or_else(|| die("stem metadata missing stems[]"));
        if stems.len() != 4 {
            die(&format!("stem metadata needs 4 stems, got {}", stems.len()));
        }
        stems.swap(0, 1);
        (
            stems[0]["name"].as_str().unwrap_or("-").to_string(),
            stems[1]["name"].as_str().unwrap_or("-").to_string(),
        )
    };

    // remux: keep master (1) and silences (4,5); swap audio on stems 2 and 3.
    // names come from the already-swapped metadata so this is reversible.
    let src = path.to_str().unwrap();
    let tmp = temp_path("stem.mp4");
    let status = Command::new("MP4Box")
        .args([
            "-add",
            &format!("{src}#1:name=Master"),
            "-add",
            &format!("{src}#3:disable:name={name0}"),
            "-add",
            &format!("{src}#2:disable:name={name1}"),
            "-add",
            &format!("{src}#4:disable:name=-"),
            "-add",
            &format!("{src}#5:disable:name=-"),
            "-brand",
            "M4A ",
            "-ab",
            "isom",
            "-ab",
            "mp42",
            "-new",
            tmp.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .unwrap_or_else(|e| die(&format!("failed to run MP4Box: {e}")));
    if !status.success() {
        die("MP4Box remux failed while swapping tracks");
    }

    // stem udta first, then tags/cover from the original (same order as pack)
    write_udta(&tmp, &meta);
    crate::metadata::from_source(src, tmp.to_str().unwrap());

    fs::rename(&tmp, path).unwrap_or_else(|e| {
        // cross-device fallback
        fs::copy(&tmp, path).unwrap_or_else(|e2| die(&format!("replace stem failed: {e} / {e2}")));
        let _ = fs::remove_file(&tmp);
    });

    eprintln!("{stem}: swapped instrumental ↔ vocal");
}

/// stems[0] is Instrumental and stems[1] is Vocal — the layout we always write.
fn set_colors(meta: &mut Value, colors: &StemColors) {
    let stems = meta
        .get_mut("stems")
        .and_then(|s| s.as_array_mut())
        .unwrap_or_else(|| die("stem metadata missing stems[]"));
    if stems.len() != 4 {
        die(&format!("stem metadata needs 4 stems, got {}", stems.len()));
    }
    stems[0]["color"] = Value::String(colors.instrumental.clone());
    stems[1]["color"] = Value::String(colors.vocal.clone());
}

fn encode(meta: &Value) -> String {
    B64.encode(serde_json::to_string(meta).unwrap().as_bytes())
}

/// MP4Box dumps the box payload (raw json, no box header) to the `-out` path.
fn read_udta(stem: &Path) -> Value {
    let dump = temp_path("json");
    let status = Command::new("MP4Box")
        .args([
            "-dump-udta",
            "0:stem",
            "-out",
            dump.to_str().unwrap(),
            stem.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap_or_else(|e| die(&format!("failed to run MP4Box: {e}")));
    if !status.success() || !dump.is_file() {
        die(&format!(
            "{} has no stem metadata — is it a stem file?",
            stem.display()
        ));
    }

    let text = fs::read_to_string(&dump).expect("read udta dump");
    let _ = fs::remove_file(&dump);
    serde_json::from_str(&text).unwrap_or_else(|e| die(&format!("stem metadata is not json: {e}")))
}

/// re-setting an existing udta type replaces it, so this leaves exactly one box.
fn write_udta(stem: &Path, meta: &Value) {
    let status = Command::new("MP4Box")
        .args([
            "-udta",
            &format!("0:type=stem:src=base64,{}", encode(meta)),
            stem.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .unwrap_or_else(|e| die(&format!("failed to run MP4Box: {e}")));
    if !status.success() {
        die("MP4Box -udta failed");
    }
}

fn temp_path(ext: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("genstems-udta-{n}.{ext}"))
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(b64: &str) -> Value {
        let bytes = B64.decode(b64).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn payload_without_colors_keeps_template_defaults() {
        let meta = decode(&payload_b64(None));
        assert_eq!(meta["stems"][0]["name"], "Instrumental");
        assert_eq!(meta["stems"][0]["color"], "#00e8e8");
        assert_eq!(meta["stems"][1]["name"], "Vocal");
        assert_eq!(meta["stems"][1]["color"], "#ad65ff");
    }

    #[test]
    fn payload_with_colors_sets_vocal_and_instrumental_only() {
        let colors = StemColors {
            vocal: "#3dc2cc".into(),
            instrumental: "#3dcc74".into(),
        };
        let meta = decode(&payload_b64(Some(&colors)));

        assert_eq!(meta["stems"][0]["name"], "Instrumental");
        assert_eq!(meta["stems"][0]["color"], "#3dcc74");
        assert_eq!(meta["stems"][1]["name"], "Vocal");
        assert_eq!(meta["stems"][1]["color"], "#3dc2cc");
        // the two silent tracks stay grey, and the dsp block survives untouched
        assert_eq!(meta["stems"][2]["color"], "#3a3a3a");
        assert_eq!(meta["stems"][3]["color"], "#3a3a3a");
        assert_eq!(meta["version"], 1);
        assert_eq!(meta["mastering_dsp"]["limiter"]["ceiling"], -0.35);
    }
}
