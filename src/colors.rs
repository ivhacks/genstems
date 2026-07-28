//! derive two stem colors from cover artwork.
//!
//! works in CIELAB, where distance matches what the eye actually perceives and
//! color separates cleanly from brightness.
//!
//! every pixel has a weight of `chroma + light_boost`. vivid pixels score on
//! colorfulness; near-whites score on lightness. so a red/white cover and a
//! teal/green cover both produce two strong candidates without a special case
//! for "is this black and white?".
//!
//! near-greys have no stable hue (atan2 noise), so they land in lightness bins
//! as pure greys. everything else lands in a one-degree hue bin. candidates
//! from both bins compete in one scoring pass:
//!
//!   score = weight_i * weight_j * (hue_opposition + lightness_opposition)
//!
//! that single product is what picks the pair — saturated-and-far, or light-
//! and-dark, whichever the artwork actually supports.
//!
//! averages are usually muddier than the eye remembers. we push chroma out a
//! bit (same hue, same L), but only by a fixed factor — a 2% green cast on
//! black does not get to become neon green.
//!
//! if the two picks are still too close in both hue and lightness, we force a
//! light/dark split (hue kept) so the stems never look identical on a deck.

use std::path::Path;
use std::process::Command;

/// hue histogram resolution. 10° bins keep a real red from being split across
/// dozens of 1° slices and losing to a solid white/black mass.
const BINS: usize = 36;

/// lightness histogram for near-greys: one bin per L unit, 0..=100.
const LIGHT_BINS: usize = 101;

/// below this chroma a pixel is "basically grey": it votes by lightness only,
/// and we never push-saturate it into a fake accent. a delta-E of 1 is the
/// just-noticeable floor; we sit well above that so jpeg cast on a b&w cover
/// doesn't invent blue and green stems.
const MIN_REAL_CHROMA: f64 = 12.0;

/// how hard a pure-white grey pixel pulls, compared against `chroma²`.
/// white (~LIGHT_K) beats a 2-chroma cast but loses to real red (~C²≈8000).
const LIGHT_K: f64 = 2500.0;
/// dark greys still need *some* weight so a b&w cover can pick black+white,
/// but a sea of black background must not outrank actual color.
const DARK_K: f64 = 80.0;

/// darkest color we'll emit, as CIELAB lightness. a stem color is a UI accent
/// on a dark deck, so pure black would be invisible. this is the exact
/// lightness of rgb(50,50,50), so the floor round-trips to that hex.
const MIN_LIGHTNESS: f64 = 20.7878;

/// if the two hues sit at least this far apart on the wheel, they're distinct
/// enough as accents and we leave their lightness alone.
const MIN_HUE_SEP: f64 = 30.0_f64.to_radians();

/// when hues are too close, the two accents must still read as different
/// things on the deck. this many L units is enough.
const MIN_LIGHT_SEP: f64 = 25.0;

/// upper lightness we allow when forcing a light/dark split. past this the
/// max-chroma color washes out toward white.
const MAX_LIGHTNESS: f64 = 90.0;

/// max chroma multiplier when pushing toward the gamut edge. source C=20 can
/// reach 50; source C=2 tops out at 5 and stays muddy.
const PUSH_FACTOR: f64 = 2.5;

/// every cover is scaled to this before analysis. averages out jpeg rainbow
/// noise so we see what the eye sees, not what the DCT left behind.
const ANALYSIS_SIZE: u32 = 200;

#[derive(Debug, PartialEq, Eq)]
pub struct StemColors {
    pub vocal: String,
    pub instrumental: String,
}

/// pick the two colors from whatever cover art `path` carries. works on a bare
/// image file, on a tagged .flac/.mp3, and on a finished .stem.mp4 — ffmpeg
/// exposes an embedded cover as a video stream either way.
/// `None` when the file has no artwork at all.
pub fn from_cover_art(path: &Path) -> Option<StemColors> {
    if !has_cover_art(path) {
        return None;
    }
    Some(from_rgb(&decode_rgb(path)))
}

/// same thing on already-decoded rgb24 bytes. this is where the logic lives.
pub fn from_rgb(pixels: &[u8]) -> StemColors {
    let lab: Vec<Lab> = pixels.chunks_exact(3).map(Lab::from_rgb).collect();
    assert!(!lab.is_empty(), "cover art decoded to zero pixels");

    let (a, b) = finish(pick_pair(&lab));
    StemColors {
        vocal: b.to_hex(),
        instrumental: a.to_hex(),
    }
}

/// push both colors outward, then split by lightness if still too similar.
fn finish(pair: (Lab, Lab)) -> (Lab, Lab) {
    separate(pair.0.push_outward(), pair.1.push_outward())
}

/// pixel weight: chroma² + lightness. squaring chroma makes a real red outrun
/// a sea of near-white pixels; white/black still compete via their own terms.
/// mid-greys get ~0 lightness weight — only the extremes pull.
fn pixel_weight(px: &Lab) -> f64 {
    let c = px.chroma();
    let t = (px.l / 100.0).clamp(0.0, 1.0);
    // 0 at L=50, 1 at L=100 / L=0
    let from_mid = (t - 0.5).abs() * 2.0;
    let light = if t >= 0.5 {
        LIGHT_K * from_mid * from_mid
    } else {
        DARK_K * from_mid * from_mid
    };
    c * c + light
}

/// how "pale" a color is: light and not saturated. pure white → 1, neon → 0.
fn pale(c: &Lab) -> f64 {
    let t = (c.l / 100.0).clamp(0.0, 1.0);
    let sat = (c.chroma() / 50.0).min(1.0);
    t * (1.0 - sat)
}

/// opposition used in pair scoring:
/// - hue distance, amplified when *both* are chromatic (teal+green > teal+mist)
/// - lightness distance (white+black still works on a grey cover)
/// - vivid-vs-pale: saturated color + white is a real pair; two neons don't
///   get a free bonus just for both being colorful
fn opposition(a: &Lab, b: &Lab) -> f64 {
    let hue = 1.0 - a.hue_cos(b);
    let light = ((a.l - b.l).abs() / 50.0).min(2.0);
    let chroma_boost = 1.0 + (a.chroma() * b.chroma()) / 100.0;
    let vivid_pale = (a.chroma() * pale(b) + b.chroma() * pale(a)) / 50.0;
    hue * chroma_boost + light + vivid_pale.min(2.0)
}

/// build candidates from the image and pick the best-scoring pair.
fn pick_pair(lab: &[Lab]) -> (Lab, Lab) {
    // chromatic pixels → hue bins; near-greys → lightness bins (as pure grey).
    // each hue bin is represented by its most saturated pixel (not the muddy
    // average), so water-mist doesn't dilute foliage green.
    let mut hue_w = [0f64; BINS];
    let mut hue_peak = [Lab::ZERO; BINS];
    let mut hue_peak_c = [0f64; BINS];
    let mut light_w = [0f64; LIGHT_BINS];
    let mut light_sum_l = [0f64; LIGHT_BINS];

    for px in lab {
        let w = pixel_weight(px);
        if w <= 0.0 {
            continue;
        }
        if px.chroma() < MIN_REAL_CHROMA {
            let bin = (px.l.round() as isize).clamp(0, (LIGHT_BINS - 1) as isize) as usize;
            light_w[bin] += w;
            light_sum_l[bin] += px.l * w;
        } else {
            let bin = px.hue_bin();
            hue_w[bin] += w;
            let c = px.chroma();
            if c > hue_peak_c[bin] {
                hue_peak_c[bin] = c;
                hue_peak[bin] = *px;
            }
        }
    }

    let mut cands: Vec<(f64, Lab)> = Vec::new();
    for b in 0..BINS {
        if hue_w[b] > 0.0 {
            cands.push((hue_w[b], hue_peak[b]));
        }
    }
    for b in 0..LIGHT_BINS {
        if light_w[b] > 0.0 {
            cands.push((light_w[b], Lab::grey(light_sum_l[b] / light_w[b])));
        }
    }

    // empty / one candidate: invent nothing — duplicate and let separate() split
    match cands.len() {
        0 => {
            let l = lab.iter().map(|p| p.l).sum::<f64>() / lab.len() as f64;
            return (Lab::grey(l), Lab::grey(l));
        }
        1 => return (cands[0].1, cands[0].1),
        _ => {}
    }

    let mut best = (f64::NEG_INFINITY, 0usize, 0usize);
    for (i, (wi, ci)) in cands.iter().enumerate() {
        for (j, (wj, cj)) in cands.iter().enumerate().skip(i + 1) {
            // geometric mean so a 90%-black cover doesn't let raw black mass
            // crush every chromatic pair on its own.
            let score = (wi * wj).sqrt() * opposition(ci, cj);
            if score > best.0 {
                best = (score, i, j);
            }
        }
    }
    let (wi, ci) = cands[best.1];
    let (wj, cj) = cands[best.2];
    // heavier candidate → vocal
    if wi >= wj { (ci, cj) } else { (cj, ci) }
}

/// if the hues are far enough apart, or lightness already separates them,
/// leave them. otherwise force a light / dark pair of the same hue family.
fn separate(mut a: Lab, mut b: Lab) -> (Lab, Lab) {
    if a.hue_angle(&b) >= MIN_HUE_SEP || (a.l - b.l).abs() >= MIN_LIGHT_SEP {
        return (a, b);
    }

    let mid = (a.l + b.l) / 2.0;
    let mut light = (mid + MIN_LIGHT_SEP / 2.0).min(MAX_LIGHTNESS);
    let mut dark = (mid - MIN_LIGHT_SEP / 2.0).max(MIN_LIGHTNESS);
    // near the top or bottom of the range the half-and-half split can still
    // land short of MIN_LIGHT_SEP — pin one end and push the other.
    if light - dark < MIN_LIGHT_SEP {
        if mid > 50.0 {
            light = MAX_LIGHTNESS;
            dark = light - MIN_LIGHT_SEP;
        } else {
            dark = MIN_LIGHTNESS;
            light = dark + MIN_LIGHT_SEP;
        }
    }

    // keep whichever was already lighter on the light side
    if a.l >= b.l {
        a.l = light;
        b.l = dark;
    } else {
        a.l = dark;
        b.l = light;
    }
    // max chroma depends on L, so re-push after the move
    (a.push_outward(), b.push_outward())
}

/// a cover shows up as an attached-pic video stream. no video stream, no art.
fn has_cover_art(path: &Path) -> bool {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
            path.to_str().unwrap(),
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffprobe: {e}"));
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

/// decode the first frame as rgb24, scaled to ANALYSIS_SIZE² with lanczos so
/// jpeg rainbow noise averages out before we pick colors.
fn decode_rgb(path: &Path) -> Vec<u8> {
    let size = ANALYSIS_SIZE.to_string();
    let vf = format!("scale={size}:{size}:flags=lanczos");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            path.to_str().unwrap(),
            "-frames:v",
            "1",
            "-vf",
            &vf,
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffmpeg: {e}"));
    if !out.status.success() {
        panic!(
            "ffmpeg could not decode {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let expect = (ANALYSIS_SIZE * ANALYSIS_SIZE * 3) as usize;
    assert_eq!(
        out.stdout.len(),
        expect,
        "expected {expect} rgb bytes from {}, got {}",
        path.display(),
        out.stdout.len()
    );
    out.stdout
}

/// a color in CIELAB: `l` is lightness, `a`/`b` place it on the color wheel.
#[derive(Clone, Copy, Debug)]
struct Lab {
    l: f64,
    a: f64,
    b: f64,
}

/// D65 white point, the reference white for sRGB.
const WHITE: [f64; 3] = [0.95047, 1.0, 1.08883];
/// sRGB primaries as linear-rgb → xyz. the standard matrix.
const RGB_TO_XYZ: [[f64; 3]; 3] = [
    [0.4124564, 0.3575761, 0.1804375],
    [0.2126729, 0.7151522, 0.0721750],
    [0.0193339, 0.1191920, 0.9503041],
];
const XYZ_TO_RGB: [[f64; 3]; 3] = [
    [3.2404542, -1.5371385, -0.4985314],
    [-0.9692660, 1.8760108, 0.0415560],
    [0.0556434, -0.2040259, 1.0572252],
];
/// the knee where CIELAB's cube root gives way to a linear segment: 6/29.
const DELTA: f64 = 6.0 / 29.0;

impl Lab {
    const ZERO: Lab = Lab {
        l: 0.0,
        a: 0.0,
        b: 0.0,
    };

    fn grey(l: f64) -> Lab {
        Lab { l, a: 0.0, b: 0.0 }
    }

    fn from_rgb(px: &[u8]) -> Lab {
        let lin: Vec<f64> = px
            .iter()
            .map(|&c| srgb_to_linear(c as f64 / 255.0))
            .collect();
        let xyz: Vec<f64> = (0..3)
            .map(|i| {
                let m = RGB_TO_XYZ[i];
                (m[0] * lin[0] + m[1] * lin[1] + m[2] * lin[2]) / WHITE[i]
            })
            .map(lab_curve)
            .collect();
        Lab {
            l: 116.0 * xyz[1] - 16.0,
            a: 500.0 * (xyz[0] - xyz[1]),
            b: 200.0 * (xyz[1] - xyz[2]),
        }
    }

    fn to_hex(self) -> String {
        // a stem color is an accent on a dark deck; too dark and it vanishes
        let l = self.l.max(MIN_LIGHTNESS);
        let fy = (l + 16.0) / 116.0;
        let f = [fy + self.a / 500.0, fy, fy - self.b / 200.0];

        let xyz: Vec<f64> = (0..3).map(|i| lab_curve_inv(f[i]) * WHITE[i]).collect();
        let rgb: Vec<u8> = (0..3)
            .map(|i| {
                let m = XYZ_TO_RGB[i];
                let lin = m[0] * xyz[0] + m[1] * xyz[1] + m[2] * xyz[2];
                let c = linear_to_srgb(lin.clamp(0.0, 1.0));
                (c * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect();
        format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
    }

    /// true when this Lab converts to sRGB without clipping any channel.
    fn in_srgb(self) -> bool {
        let l = self.l.max(MIN_LIGHTNESS);
        let fy = (l + 16.0) / 116.0;
        let f = [fy + self.a / 500.0, fy, fy - self.b / 200.0];
        let xyz: [f64; 3] = std::array::from_fn(|i| lab_curve_inv(f[i]) * WHITE[i]);
        for i in 0..3 {
            let m = XYZ_TO_RGB[i];
            let lin = m[0] * xyz[0] + m[1] * xyz[1] + m[2] * xyz[2];
            if !(-1e-4..=1.0 + 1e-4).contains(&lin) {
                return false;
            }
        }
        true
    }

    /// same hue and lightness, chroma raised toward the gamut edge. the boost
    /// scales with how colorful the source already is. below MIN_REAL_CHROMA
    /// we strip the cast entirely and keep lightness only — almost-greyscale
    /// never becomes a neon accent.
    fn push_outward(self) -> Lab {
        let c = self.chroma();
        let l = self.l.max(MIN_LIGHTNESS);
        if c < MIN_REAL_CHROMA {
            return Lab::grey(l);
        }
        let ua = self.a / c;
        let ub = self.b / c;
        let factor = 1.0 + (PUSH_FACTOR - 1.0) * (c / 40.0).min(1.0);
        let want = c * factor;
        // binary search the largest chroma ≤ want that still fits in sRGB
        let mut lo = 0.0;
        let mut hi = want.min(150.0);
        for _ in 0..40 {
            let mid = (lo + hi) / 2.0;
            let cand = Lab {
                l,
                a: ua * mid,
                b: ub * mid,
            };
            if cand.in_srgb() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Lab {
            l,
            a: ua * lo,
            b: ub * lo,
        }
    }

    /// how colorful this is: distance from the neutral grey axis.
    fn chroma(&self) -> f64 {
        self.a.hypot(self.b)
    }

    /// which hue slice this color sits in.
    fn hue_bin(&self) -> usize {
        let degrees = self.b.atan2(self.a).to_degrees().rem_euclid(360.0);
        ((degrees / (360.0 / BINS as f64)) as usize) % BINS
    }

    /// cosine of the angle between two hues: 1 = identical, -1 = opposite.
    /// computed from the vectors directly, so no trig and no wraparound.
    fn hue_cos(&self, other: &Lab) -> f64 {
        let mag = self.chroma() * other.chroma();
        if mag == 0.0 {
            return 1.0; // a neutral has no hue, so treat it as "same hue"
        }
        ((self.a * other.a + self.b * other.b) / mag).clamp(-1.0, 1.0)
    }

    /// absolute hue angle between two colors, in radians, 0..π.
    fn hue_angle(&self, other: &Lab) -> f64 {
        self.hue_cos(other).acos()
    }
}

fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f64) -> f64 {
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn lab_curve(t: f64) -> f64 {
    if t > DELTA * DELTA * DELTA {
        t.cbrt()
    } else {
        t / (3.0 * DELTA * DELTA) + 4.0 / 29.0
    }
}

fn lab_curve_inv(t: f64) -> f64 {
    if t > DELTA {
        t * t * t
    } else {
        3.0 * DELTA * DELTA * (t - 4.0 / 29.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(hex: &str) -> Lab {
        let n = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap();
        Lab::from_rgb(&[(n >> 16) as u8, (n >> 8) as u8, n as u8])
    }

    /// hue in degrees, the way a color wheel is usually described.
    fn hue(hex: &str) -> f64 {
        let c = parse(hex);
        c.b.atan2(c.a).to_degrees().rem_euclid(360.0)
    }

    fn solid(color: [u8; 3], n: usize) -> Vec<u8> {
        color.repeat(n)
    }

    fn is_near_white(hex: &str) -> bool {
        let c = parse(hex);
        c.l > 85.0 && c.chroma() < 25.0
    }

    fn is_near_grey(hex: &str) -> bool {
        parse(hex).chroma() < 10.0
    }

    #[test]
    fn language_cover_gives_vibrant_cyan_and_green() {
        let colors = from_cover_art(Path::new("images/language.jpg")).unwrap();
        println!("{colors:?}");

        assert!(
            (100.0..180.0).contains(&hue(&colors.vocal)),
            "expected green vocal, got {}",
            colors.vocal
        );
        assert!(
            (180.0..250.0).contains(&hue(&colors.instrumental)),
            "expected teal/cyan instrumental, got {}",
            colors.instrumental
        );
        // both should be properly saturated, not muddy averages
        assert!(
            parse(&colors.vocal).chroma() > 25.0,
            "vocal too dull: {}",
            colors.vocal
        );
        assert!(
            parse(&colors.instrumental).chroma() > 25.0,
            "instrumental too dull: {}",
            colors.instrumental
        );
    }

    #[test]
    fn sunset_cover_gives_blue_and_orange() {
        let colors = from_cover_art(Path::new("images/one_more_day.jpg")).unwrap();
        println!("{colors:?}");

        assert!(
            (0.0..90.0).contains(&hue(&colors.vocal)),
            "expected an orange vocal, got {}",
            colors.vocal
        );
        assert!(
            (240.0..330.0).contains(&hue(&colors.instrumental)),
            "expected a blue instrumental, got {}",
            colors.instrumental
        );
    }

    #[test]
    fn picks_the_two_colors_present_in_flat_artwork() {
        let mut px = solid([255, 0, 0], 60);
        px.extend(solid([0, 255, 255], 40));

        let colors = from_rgb(&px);
        assert_eq!(colors.instrumental, "#ff0000", "red covers more of the image");
        assert_eq!(colors.vocal, "#00ffff");
    }

    #[test]
    fn a_tiny_vivid_accent_still_beats_a_near_neutral_wash() {
        // a big muddy background and a small saturated accent: the accent is
        // what the artwork reads as, so it has to survive.
        let mut px = solid([120, 118, 122], 950);
        px.extend(solid([255, 140, 0], 50));

        let colors = from_rgb(&px);
        assert!(
            (0.0..90.0).contains(&hue(&colors.instrumental))
                || (0.0..90.0).contains(&hue(&colors.vocal)),
            "the orange accent was lost: {colors:?}"
        );
    }

    #[test]
    fn black_and_white_cover_gives_its_lightest_and_darkest_tones() {
        let mut px = solid([255, 255, 255], 30);
        px.extend(solid([128, 128, 128], 30));
        px.extend(solid([10, 10, 10], 30));

        let colors = from_rgb(&px);
        // mid-grey is boring; the extremes should win
        assert!(is_near_white(&colors.vocal) || is_near_white(&colors.instrumental));
        assert!(
            parse(&colors.vocal).l.max(parse(&colors.instrumental).l) > 90.0,
            "expected a near-white: {colors:?}"
        );
        assert!(
            parse(&colors.vocal).l.min(parse(&colors.instrumental).l) < MIN_LIGHTNESS + 5.0,
            "expected a near-black floored for visibility: {colors:?}"
        );
        assert!(is_near_grey(&colors.vocal) && is_near_grey(&colors.instrumental));
    }

    #[test]
    fn a_dark_cover_never_emits_something_invisible() {
        let mut px = solid([2, 2, 2], 50);
        px.extend(solid([0, 0, 0], 50));

        let colors = from_rgb(&px);
        for hex in [&colors.vocal, &colors.instrumental] {
            assert!(
                parse(hex).l >= MIN_LIGHTNESS - 0.01,
                "{hex} is too dark to see on a deck"
            );
        }
    }

    #[test]
    fn a_single_flat_color_splits_by_lightness() {
        // one hue only — keep it, but make the two stems light vs dark
        let colors = from_rgb(&solid([200, 40, 40], 100));
        assert_ne!(
            colors.vocal, colors.instrumental,
            "same hue still needs two distinct accents"
        );
        assert!(
            (0.0..90.0).contains(&hue(&colors.vocal)),
            "expected the artwork's own red, got {}",
            colors.vocal
        );
        assert!(
            (0.0..90.0).contains(&hue(&colors.instrumental)),
            "expected the artwork's own red, got {}",
            colors.instrumental
        );
        let dv = (parse(&colors.vocal).l - parse(&colors.instrumental).l).abs();
        assert!(
            dv >= MIN_LIGHT_SEP - 0.5,
            "lightness split too small ({dv}): {colors:?}"
        );
    }

    #[test]
    fn lab_round_trips_through_rgb() {
        for rgb in [
            [255u8, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [18, 200, 190],
            [255, 255, 255],
        ] {
            let hex = Lab::from_rgb(&rgb).to_hex();
            let expect = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
            assert_eq!(hex, expect);
        }
    }
}
