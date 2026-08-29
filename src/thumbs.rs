//! Local dish-thumbnail selection for imported social videos.
//!
//! While the downloaded video file still exists in the extraction work
//! directory, this module runs one cheap ffmpeg pass that emits small RGB
//! samples, scores every sample with pixel heuristics for "how much dish is
//! visible", picks up to four diverse winners, and crops each winner into a
//! square around the saliency centroid with one precise ffmpeg seek. No cloud
//! service, model download, or new dependency is involved: ffmpeg is already
//! a hard requirement of the media importer.

use crate::media::{env_flag, env_path, remaining_timeout, run_tool};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Upper bound on offered choices; the best-scoring candidate is first.
pub(crate) const MAX_THUMBNAILS: usize = 4;

/// Width of every analysed sample. Small enough that hundreds of samples
/// stay trivially cheap, large enough for stable blur and colour statistics.
const ANALYSIS_WIDTH: usize = 128;
/// Samples per second pulled from the video during the scoring pass.
const SCAN_HZ: f64 = 2.0;
/// Local analysis never looks past the same cap the OCR pipeline uses.
const ANALYSIS_SECONDS_CAP: u64 = 300;
/// Edge of the encoded square candidates, before the browser scales it down.
const OUTPUT_SIZE: u32 = 600;
/// Brand intros live at the very start; never nominate the first moments.
const HEAD_SKIP_SECONDS: f64 = 0.75;
/// End cards and "follow me" screens live at the end; skip this fraction.
const TAIL_SKIP_FRACTION: f32 = 0.04;
/// Frames whose timestamp sits this close to a detected OCR caption are
/// penalised: heavy on-screen text usually marks an ingredient card, not the
/// finished dish.
const TEXT_PENALTY_SECONDS: f64 = 2.0;
const TEXT_PENALTY_MULTIPLIER: f32 = 0.7;
/// Side of the tiny grayscale signature used to drop near-duplicate shots.
const SIGNATURE_SIDE: usize = 16;
/// Mean absolute luma difference per signature cell below which two samples
/// count as the same shot.
const DUPLICATE_MEAN_DISTANCE: f32 = 9.0;
/// Bound on the raw RGB scoring stream: 128-bit wide portrait samples over
/// five minutes fit far below this; the cap only stops pathological input.
const RAW_OUTPUT_MAX_BYTES: usize = 96 * 1024 * 1024;

/// Composite metric weights. Colour and centre texture carry the most
/// evidence of plated food; sharpness separates crisp hero shots from motion
/// smear; warmth and saturation break ties between kitchen scenes.
const WEIGHT_COLORFULNESS: f32 = 0.30;
const WEIGHT_SATURATION: f32 = 0.15;
const WEIGHT_WARMTH: f32 = 0.10;
const WEIGHT_TEXTURE: f32 = 0.25;
const WEIGHT_SHARPNESS: f32 = 0.20;

/// One cropped, encoded thumbnail choice ready to persist with the draft.
#[derive(Debug, Clone)]
pub(crate) struct ThumbCandidate {
    /// Wall-clock second of the source video the crop came from.
    pub(crate) seconds: u64,
    /// Raw composite quality score; kept for logging and tests.
    pub(crate) score: f32,
    /// Encoded square JPEG bytes.
    pub(crate) jpeg: Vec<u8>,
}

/// Whether thumbnail nomination should run at all. Read at extraction time so
/// custom installs can disable the extra ffmpeg passes without a rebuild.
pub(crate) fn enabled() -> bool {
    env_flag("THUMBNAILS_ENABLED", true)
}

/// Extracts up to [`MAX_THUMBNAILS`] cropped dish-photo candidates from
/// `video_path`. Failures degrade to an empty list plus a warning string; a
/// missing thumbnail must never fail the recipe import itself.
pub(crate) fn extract_thumbnails(
    video_path: &Path,
    workdir: &Path,
    ocr_seconds: &[u64],
    duration_hint: Option<u64>,
    warnings: &mut Vec<String>,
    deadline: Instant,
) -> Vec<ThumbCandidate> {
    let Some((width, height)) = video_dimensions(video_path, warnings) else {
        return Vec::new();
    };
    if width == 0 || height == 0 {
        warnings.push("The video reported no usable frame size.".into());
        return Vec::new();
    }
    let analysis_height =
        (((height as f64) * ANALYSIS_WIDTH as f64 / width as f64).round() as usize).max(16);
    let frame_bytes = ANALYSIS_WIDTH * analysis_height * 3;

    // Pass one: constant-rate tiny RGB samples straight down the pipe.
    let ffmpeg = env_path("MEDIA_FFMPEG_PATH", "ffmpeg");
    let args = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        video_path.to_string_lossy().to_string(),
        "-t".into(),
        ANALYSIS_SECONDS_CAP.to_string(),
        "-vf".into(),
        format!("fps={SCAN_HZ},scale={ANALYSIS_WIDTH}:{analysis_height}"),
        "-pix_fmt".into(),
        "rgb24".into(),
        "-f".into(),
        "rawvideo".into(),
        "pipe:1".into(),
    ];
    let Some(timeout) = remaining_timeout(deadline, Duration::from_secs(120)) else {
        warnings.push("The media deadline passed before thumbnail sampling.".into());
        return Vec::new();
    };
    let raw = match run_tool(&ffmpeg, &args, timeout, RAW_OUTPUT_MAX_BYTES) {
        Ok(output) => output.stdout,
        Err(error) => {
            warnings.push(format!("Thumbnail frame sampling failed: {error}"));
            return Vec::new();
        }
    };
    if raw.len() < frame_bytes {
        warnings.push("Not enough video frames were readable for thumbnails.".into());
        return Vec::new();
    }
    let frames = raw.len() / frame_bytes;

    // Score every complete sample; trailing partial bytes are ignored.
    let mut samples: Vec<(FrameMetrics, Signature)> = Vec::with_capacity(frames);
    for index in 0..frames {
        let buffer = &raw[index * frame_bytes..(index + 1) * frame_bytes];
        let t = index as f64 / SCAN_HZ;
        if let Some(scored) = analyze_frame(buffer, ANALYSIS_WIDTH, analysis_height, t) {
            samples.push(scored);
        }
    }
    if samples.is_empty() {
        warnings.push("No decodable video frames were found for thumbnails.".into());
        return Vec::new();
    }

    let span = samples.last().map_or(0.0, |(metrics, _)| metrics.t);
    let scored = rank_samples(&samples, ocr_seconds, span);
    // Scene spread scales with the video so a reel and a ten-minute cook both
    // get distinct options rather than four frames of the same plate.
    let basis = duration_hint
        .map(|duration| duration as f64)
        .filter(|duration| *duration >= 1.0)
        .unwrap_or(span.max(4.0));
    let min_gap = (basis * 0.08).clamp(2.5, 20.0);
    let picked = select_diverse(scored, MAX_THUMBNAILS, min_gap);
    if picked.is_empty() {
        warnings.push("No suitable dish-photo frame was found.".into());
        return Vec::new();
    }

    // Pass two: one precise seek per winner, cropping a square around the
    // saliency centroid measured on that sample's own pixels.
    let out_dir = workdir.join("thumbs");
    if let Err(error) = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&out_dir)
    {
        warnings.push(format!("Could not create the thumbnail directory: {error}"));
        return Vec::new();
    }
    let mut candidates = Vec::with_capacity(picked.len());
    for (choice_index, sample) in picked.iter().enumerate() {
        let Some(timeout) = remaining_timeout(deadline, Duration::from_secs(60)) else {
            warnings.push("The media deadline passed while cropping thumbnails.".into());
            break;
        };
        let buffer = &raw[sample.index * frame_bytes..(sample.index + 1) * frame_bytes];
        let (cx, cy) = saliency_centroid(buffer, ANALYSIS_WIDTH, analysis_height);
        let (x, y, side) = square_crop_window(width, height, cx, cy);
        let out_path = out_dir.join(format!("thumb-{choice_index}.jpg"));
        let args = vec![
            "-y".into(),
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-ss".into(),
            format!("{:.3}", sample.t),
            "-i".into(),
            video_path.to_string_lossy().to_string(),
            "-frames:v".into(),
            "1".into(),
            "-vf".into(),
            format!("crop={side}:{side}:{x}:{y},scale={OUTPUT_SIZE}:{OUTPUT_SIZE}"),
            "-q:v".into(),
            "3".into(),
            out_path.to_string_lossy().to_string(),
        ];
        if let Err(error) = run_tool(&ffmpeg, &args, timeout, 1024 * 1024) {
            warn!(%error, second = sample.t, "Thumbnail crop failed");
            warnings.push(format!(
                "A thumbnail candidate could not be cropped: {error}"
            ));
            continue;
        }
        match std::fs::read(&out_path) {
            Ok(jpeg) if jpeg.len() > 2 => candidates.push(ThumbCandidate {
                seconds: sample.t.round() as u64,
                score: sample.score,
                jpeg,
            }),
            _ => warnings.push("A cropped thumbnail came back empty.".into()),
        }
        let _ = std::fs::remove_file(&out_path);
    }
    let best = candidates.first().map(|candidate| candidate.score);
    info!(
        samples = frames,
        candidates = candidates.len(),
        best_score = best.unwrap_or(0.0),
        "Dish thumbnail candidates selected"
    );
    candidates
}

/// Per-sample raw measurements, before cross-video normalisation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameMetrics {
    t: f64,
    luma_mean: f32,
    colorfulness: f32,
    saturation: f32,
    warmth: f32,
    texture: f32,
    sharpness: f32,
}

/// Tiny block-mean grayscale fingerprint of one sample.
type Signature = [u8; SIGNATURE_SIDE * SIGNATURE_SIDE];

/// Measures one RGB24 frame. Returns `None` only for degenerate sizes.
pub(crate) fn analyze_frame(
    rgb: &[u8],
    width: usize,
    height: usize,
    t: f64,
) -> Option<(FrameMetrics, Signature)> {
    if rgb.len() < width * height * 3 || width < 3 || height < 3 {
        return None;
    }
    let pixels = width * height;
    let mut gray = Vec::with_capacity(pixels);
    let mut luma_sum = 0.0_f32;
    // Centre weighting: a Gaussian bump over the normalised frame keeps the
    // composition preference without hard-cropping the statistics.
    let sigma2_inv = 1.0 / (2.0 * 0.28_f32 * 0.28);
    let mut row_weight = vec![0.0_f32; height];
    for (v, weight) in row_weight.iter_mut().enumerate() {
        let dv = (v as f32 + 0.5) / height as f32 - 0.5;
        *weight = (-dv * dv * sigma2_inv).exp();
    }
    let mut col_weight = vec![0.0_f32; width];
    for (u, weight) in col_weight.iter_mut().enumerate() {
        let du = (u as f32 + 0.5) / width as f32 - 0.5;
        *weight = (-du * du * sigma2_inv).exp();
    }
    let mut weighted_sat = 0.0_f32;
    let mut weight_total = 0.0_f32;
    let mut warm_count = 0_u64;
    let mut rg_mean = 0.0_f32;
    let mut yb_mean = 0.0_f32;
    let mut rg_sq = 0.0_f32;
    let mut yb_sq = 0.0_f32;
    for v in 0..height {
        let row = &rgb[v * width * 3..][..width * 3];
        for u in 0..width {
            let (r, g, b) = (
                row[u * 3] as f32,
                row[u * 3 + 1] as f32,
                row[u * 3 + 2] as f32,
            );
            let luma = 0.299 * r + 0.587 * g + 0.114 * b;
            gray.push(luma.round() as u8);
            luma_sum += luma;
            let (max, min) = (r.max(g).max(b), r.min(g).min(b));
            let weight = row_weight[v] * col_weight[u];
            weighted_sat += weight * (max - min) / 255.0;
            weight_total += weight;
            if r >= b + 14.0 && r >= g && luma > 40.0 {
                warm_count += 1;
            }
            let rg = r - g;
            let yb = 0.5 * (r + g) - b;
            rg_mean += rg;
            yb_mean += yb;
            rg_sq += rg * rg;
            yb_sq += yb * yb;
        }
    }
    let pixels_f = pixels as f32;
    let luma_mean = luma_sum / pixels_f;
    let saturation = weighted_sat / weight_total.max(f32::EPSILON);
    let warmth = warm_count as f32 / pixels_f;
    // Hasler-Süsstrunk colorfulness: opponent-channel standard deviations
    // plus a third of their mean magnitude.
    let rg_m = rg_mean / pixels_f;
    let yb_m = yb_mean / pixels_f;
    let colorfulness = ((rg_sq / pixels_f - rg_m * rg_m).max(0.0).sqrt()
        + (yb_sq / pixels_f - yb_m * yb_m).max(0.0).sqrt())
        + 0.3 * (rg_m * rg_m + yb_m * yb_m).max(0.0).sqrt();

    // Texture: Sobel edge density; sharpness: Laplacian response variance.
    let mut weighted_edges = 0.0_f32;
    let mut lap_sum = 0.0_f32;
    let mut lap_sq_sum = 0.0_f32;
    let mut lap_count = 0_u64;
    for v in 1..height - 1 {
        for u in 1..width - 1 {
            let at = |dv: usize, du: usize| gray[(v + dv - 1) * width + (u + du - 1)] as f32;
            let gx = at(2, 0) + 2.0 * at(2, 1) + at(2, 2) - (at(0, 0) + 2.0 * at(0, 1) + at(0, 2));
            let gy = at(0, 0) + 2.0 * at(1, 0) + at(2, 0) - (at(0, 2) + 2.0 * at(1, 2) + at(2, 2));
            let magnitude = gx.abs() + gy.abs();
            let lap = 4.0 * at(1, 1) - at(0, 1) - at(2, 1) - at(1, 0) - at(1, 2);
            lap_sum += lap;
            lap_sq_sum += lap * lap;
            lap_count += 1;
            if magnitude > 48.0 {
                weighted_edges += row_weight[v] * col_weight[u];
            }
        }
    }
    let interior = lap_count.max(1) as f32;
    let lap_mean = lap_sum / interior;
    let sharpness = (lap_sq_sum / interior - lap_mean * lap_mean).max(0.0);
    let texture = weighted_edges / weight_total.max(f32::EPSILON);
    Some((
        FrameMetrics {
            t,
            luma_mean,
            colorfulness,
            saturation,
            warmth,
            texture,
            sharpness,
        },
        block_signature(&gray, width, height),
    ))
}

/// Nearest-neighbour block-mean grayscale signature.
fn block_signature(gray: &[u8], width: usize, height: usize) -> Signature {
    let mut signature = [0_u8; SIGNATURE_SIDE * SIGNATURE_SIDE];
    for (cell, value) in signature.iter_mut().enumerate() {
        let u = (cell % SIGNATURE_SIDE) * width / SIGNATURE_SIDE;
        let v = (cell / SIGNATURE_SIDE) * height / SIGNATURE_SIDE;
        *value = gray[v * width + u];
    }
    signature
}

fn signature_distance(a: &Signature, b: &Signature) -> f32 {
    let total: u32 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
        .sum();
    total as f32 / (SIGNATURE_SIDE * SIGNATURE_SIDE) as f32
}

/// One scored sample ready for diversity-aware picking.
#[derive(Debug, Clone)]
pub(crate) struct ScoredSample {
    pub(crate) index: usize,
    pub(crate) t: f64,
    pub(crate) score: f32,
    signature: Signature,
}

/// Normalises metrics across the video, composites them, and orders the
/// survivors best-first. Samples inside the head/tail skip zones or with
/// degenerate exposure never enter the ranking; OCR-caption neighbourhoods
/// are demoted rather than removed.
pub(crate) fn rank_samples(
    samples: &[(FrameMetrics, Signature)],
    ocr_seconds: &[u64],
    span_seconds: f64,
) -> Vec<ScoredSample> {
    let head_skip = HEAD_SKIP_SECONDS;
    let tail_skip = span_seconds * TAIL_SKIP_FRACTION as f64;
    let eligible: Vec<usize> = samples
        .iter()
        .enumerate()
        .filter(|(_, (m, _))| {
            m.luma_mean >= 24.0
                && m.luma_mean <= 232.0
                && m.t >= head_skip
                && (span_seconds <= 0.0 || m.t <= span_seconds - tail_skip)
        })
        .map(|(index, _)| index)
        .collect();
    if eligible.is_empty() {
        return Vec::new();
    }
    let metric = |index: usize, pick: fn(&FrameMetrics) -> f32| pick(&samples[index].0);
    let mut ranges = [
        (f32::INFINITY, f32::NEG_INFINITY),
        (f32::INFINITY, f32::NEG_INFINITY),
        (f32::INFINITY, f32::NEG_INFINITY),
        (f32::INFINITY, f32::NEG_INFINITY),
        (f32::INFINITY, f32::NEG_INFINITY),
    ];
    let pickers: [fn(&FrameMetrics) -> f32; 5] = [
        |m| m.colorfulness,
        |m| m.saturation,
        |m| m.warmth,
        |m| m.texture,
        |m| m.sharpness,
    ];
    for &index in &eligible {
        for (slot, picker) in pickers.iter().enumerate() {
            let value = metric(index, *picker);
            let range = &mut ranges[slot];
            range.0 = range.0.min(value);
            range.1 = range.1.max(value);
        }
    }
    let normalise = |slot: usize, value: f32| -> f32 {
        let (low, high) = ranges[slot];
        if (high - low).abs() < f32::EPSILON {
            // A flat video (single scene) gives every sample a neutral mark
            // instead of a meaningless zero or one.
            0.5
        } else {
            ((value - low) / (high - low)).clamp(0.0, 1.0)
        }
    };

    let mut scored: Vec<ScoredSample> = eligible
        .iter()
        .map(|&index| {
            let (m, signature) = (&samples[index].0, &samples[index].1);
            let composite = WEIGHT_COLORFULNESS * normalise(0, m.colorfulness)
                + WEIGHT_SATURATION * normalise(1, m.saturation)
                + WEIGHT_WARMTH * normalise(2, m.warmth)
                + WEIGHT_TEXTURE * normalise(3, m.texture)
                + WEIGHT_SHARPNESS * normalise(4, m.sharpness);
            let exposure = exposure_factor(m.luma_mean);
            let near_text = ocr_seconds
                .iter()
                .any(|second| (m.t - *second as f64).abs() <= TEXT_PENALTY_SECONDS);
            let penalty = if near_text {
                TEXT_PENALTY_MULTIPLIER
            } else {
                1.0
            };
            ScoredSample {
                index,
                t: m.t,
                score: composite * exposure * penalty,
                signature: *signature,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal))
    });
    scored
}

/// Full credit through the well-exposed middle, tapering toward washed-out
/// and crushed ends. The gate multiplies the composite rather than removing
/// the sample outright, keeping dim-but-real dish shots eligible.
fn exposure_factor(luma_mean: f32) -> f32 {
    if luma_mean < 70.0 {
        ((luma_mean - 24.0) / (70.0 - 24.0)).clamp(0.15, 1.0)
    } else if luma_mean > 190.0 {
        ((232.0 - luma_mean) / (232.0 - 190.0)).clamp(0.15, 1.0)
    } else {
        1.0
    }
}

/// Greedy best-first pick enforcing a minimum temporal gap and a perceptual
/// duplicate check, so the four options show different moments of the cook.
/// A relaxed-gap second sweep tops up short videos where strict spacing
/// cannot fill the quota.
pub(crate) fn select_diverse(
    scored: Vec<ScoredSample>,
    max: usize,
    min_gap: f64,
) -> Vec<ScoredSample> {
    let mut picked: Vec<ScoredSample> = Vec::with_capacity(max);
    for relax in [false, true] {
        let gap = if relax { min_gap / 3.0 } else { min_gap };
        for sample in &scored {
            if picked.len() >= max {
                break;
            }
            if picked
                .iter()
                .any(|chosen: &ScoredSample| chosen.index == sample.index)
            {
                continue;
            }
            let spaced = picked
                .iter()
                .all(|chosen| (chosen.t - sample.t).abs() >= gap);
            let fresh = picked.iter().all(|chosen| {
                signature_distance(&chosen.signature, &sample.signature) >= DUPLICATE_MEAN_DISTANCE
            });
            if spaced && fresh {
                picked.push(sample.clone());
            }
        }
        if picked.len() >= max {
            break;
        }
    }
    picked
}

/// Squared-crop window centred near the given normalised point, clamped to
/// the frame. Returns `(x, y, side)` in full-resolution pixels.
pub(crate) fn square_crop_window(width: u32, height: u32, cx: f32, cy: f32) -> (u32, u32, u32) {
    let side = width.min(height);
    let clamp_x = (width - side) as f32;
    let clamp_y = (height - side) as f32;
    let x = (cx * width as f32 - side as f32 / 2.0)
        .round()
        .clamp(0.0, clamp_x) as u32;
    let y = (cy * height as f32 - side as f32 / 2.0)
        .round()
        .clamp(0.0, clamp_y) as u32;
    (x, y, side)
}

/// Centre of mass of the food-evidence signal (centre-weighted saturation
/// times edges), blended toward the geometric centre so a stray glint cannot
/// shove the crop to a corner. Falls back to the exact centre on flat input.
pub(crate) fn saliency_centroid(rgb: &[u8], width: usize, height: usize) -> (f32, f32) {
    if rgb.len() < width * height * 3 {
        return (0.5, 0.5);
    }
    let gray = |u: usize, v: usize| -> f32 {
        let at = &rgb[(v * width + u) * 3..][..3];
        0.299 * at[0] as f32 + 0.587 * at[1] as f32 + 0.114 * at[2] as f32
    };
    let mut sum_w = 0.0_f32;
    let mut sum_x = 0.0_f32;
    let mut sum_y = 0.0_f32;
    for v in 1..height.saturating_sub(1) {
        for u in 1..width.saturating_sub(1) {
            let at = &rgb[(v * width + u) * 3..][..3];
            let (r, g, b) = (at[0] as f32, at[1] as f32, at[2] as f32);
            let (max, min) = (r.max(g).max(b), r.min(g).min(b));
            let sat = (max - min) / 255.0;
            let gx = gray(u + 1, v) - gray(u - 1, v);
            let gy = gray(u, v + 1) - gray(u, v - 1);
            let edge = (gx.abs() + gy.abs()) / 255.0;
            let weight = (0.15 + sat) * (0.25 + edge);
            sum_w += weight;
            sum_x += weight * (u as f32 + 0.5);
            sum_y += weight * (v as f32 + 0.5);
        }
    }
    if sum_w <= f32::EPSILON {
        return (0.5, 0.5);
    }
    let cx = sum_x / sum_w / width as f32;
    let cy = sum_y / sum_w / height as f32;
    (
        0.75 * cx.clamp(0.0, 1.0) + 0.25 * 0.5,
        0.75 * cy.clamp(0.0, 1.0) + 0.25 * 0.5,
    )
}

/// Reads the first video stream's pixel size as `(width, height)`; `None`
/// when ffprobe is unavailable or reports nothing usable.
fn video_dimensions(video_path: &Path, warnings: &mut Vec<String>) -> Option<(u32, u32)> {
    let ffprobe = env_path("MEDIA_FFPROBE_PATH", "ffprobe");
    let args = vec![
        "-v".into(),
        "error".into(),
        "-select_streams".into(),
        "v:0".into(),
        "-show_entries".into(),
        "stream=width,height".into(),
        "-of".into(),
        "csv=s=x:p=0".into(),
        video_path.to_string_lossy().to_string(),
    ];
    let output = match run_tool(&ffprobe, &args, Duration::from_secs(20), 4 * 1024) {
        Ok(output) => output,
        Err(error) => {
            warnings.push(format!("Video dimensions could not be probed: {error}"));
            return None;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next().unwrap_or_default().trim();
    let (w, h) = line.split_once('x')?;
    match (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
        (Ok(w), Ok(h)) => Some((w, h)),
        _ => {
            warnings.push("Video dimensions were not readable.".into());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, rgb: [u8; 3]) -> Vec<u8> {
        (0..width * height)
            .flat_map(|_| [rgb[0], rgb[1], rgb[2]])
            .collect()
    }

    /// A saturated, textured warm disc (the "dish") on a neutral kitchen
    /// backdrop, centred at `(cx, cy)` in normalised coordinates.
    fn dish_frame(width: usize, height: usize, cx: f32, cy: f32) -> Vec<u8> {
        let mut rgb = vec![0_u8; width * height * 3];
        let radius = (width.min(height) as f32 * 0.32).max(4.0);
        let centre = (cx * width as f32, cy * height as f32);
        for v in 0..height {
            for u in 0..width {
                let at = &mut rgb[(v * width + u) * 3..][..3];
                let dx = u as f32 - centre.0;
                let dy = v as f32 - centre.1;
                if dx * dx + dy * dy <= radius * radius {
                    let grain = ((u * 7 + v * 13) % 37) as u8;
                    at[0] = 205_u8.saturating_sub(grain / 2);
                    at[1] = 96 + grain / 3;
                    at[2] = 52;
                } else {
                    at.copy_from_slice(&[118, 116, 112]);
                }
            }
        }
        rgb
    }

    fn sample(rgb: &[u8], width: usize, height: usize, t: f64) -> (FrameMetrics, Signature) {
        analyze_frame(rgb, width, height, t).expect("frame should analyse")
    }

    #[test]
    fn textured_colourful_food_outranks_a_flat_field() {
        let busy = sample(&dish_frame(128, 72, 0.5, 0.5), 128, 72, 1.0);
        let flat = sample(&solid(128, 72, [118, 116, 112]), 128, 72, 1.5);
        let ranked = rank_samples(&[busy, flat], &[], 2.0);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].index, 0);
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn caption_neighbourhoods_are_demoted_not_removed() {
        // Identical frames: the one near a detected OCR caption loses.
        let early = sample(&dish_frame(128, 72, 0.5, 0.5), 128, 72, 2.0);
        let late = sample(&dish_frame(128, 72, 0.5, 0.5), 128, 72, 25.0);
        let ranked = rank_samples(&[early, late], &[2], 30.0);
        assert_eq!(ranked[0].index, 1);
    }

    #[test]
    fn washed_out_extremes_never_rank() {
        let dark = sample(&solid(128, 72, [8, 8, 8]), 128, 72, 5.0);
        assert!(rank_samples(&[dark], &[], 5.0).is_empty());
    }

    #[test]
    fn centroid_follows_the_dish_and_flats_stay_centred() {
        let left = saliency_centroid(&dish_frame(128, 72, 0.22, 0.5), 128, 72);
        let middle = saliency_centroid(&dish_frame(128, 72, 0.5, 0.5), 128, 72);
        assert!(left.0 < middle.0 - 0.04, "{left:?} vs {middle:?}");
        // The centre prior keeps every estimate anchored near the frame.
        assert!(left.0 > 0.18 && left.0 < 0.45);
        let blank = saliency_centroid(&solid(128, 72, [120, 120, 120]), 128, 72);
        assert!((blank.0 - 0.5).abs() < 0.01 && (blank.1 - 0.5).abs() < 0.01);
    }

    #[test]
    fn crop_windows_are_square_and_clamped_inside_the_frame() {
        let (x, y, side) = square_crop_window(1920, 1080, 0.95, 0.05);
        assert_eq!(side, 1080);
        assert_eq!(x, 1920 - 1080);
        assert_eq!(y, 0);
        let (x, y, side) = square_crop_window(720, 1280, 0.5, 0.99);
        assert_eq!(side, 720);
        assert_eq!(y, 1280 - 720);
        assert_eq!(x, 0);
        let (_, _, side) = square_crop_window(600, 600, 0.0, 0.0);
        assert_eq!(side, 600);
    }

    fn scored(t: f64, score: f32, fill: u8) -> ScoredSample {
        ScoredSample {
            index: (t * 10.0) as usize,
            t,
            score,
            signature: [fill; SIGNATURE_SIDE * SIGNATURE_SIDE],
        }
    }

    #[test]
    fn selection_enforces_scene_gap_then_relaxes_to_fill() {
        // Distinct signatures; spacing violates the strict gap everywhere
        // except t=0 and t=10, so only the relaxed sweep reaches 5.0.
        let candidates = vec![
            scored(0.0, 1.0, 10),
            scored(5.0, 0.9, 70),
            scored(10.0, 0.8, 140),
            scored(15.0, 0.7, 210),
        ];
        let picked = select_diverse(candidates, 3, 8.0);
        let mut times: Vec<f64> = picked.iter().map(|s| s.t).collect();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(times, vec![0.0, 5.0, 10.0]);
    }

    #[test]
    fn near_duplicate_shots_are_dropped_even_when_spaced_apart() {
        let candidates = vec![scored(0.0, 1.0, 42), scored(9.0, 0.95, 42)];
        let picked = select_diverse(candidates, 2, 5.0);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].t, 0.0);
    }

    #[test]
    fn metrics_land_in_sane_ranges_for_real_frames() {
        let (m, _) = sample(&dish_frame(128, 72, 0.5, 0.5), 128, 72, 1.0);
        assert!((24.0..=232.0).contains(&m.luma_mean));
        assert!(m.colorfulness >= 0.0 && m.saturation >= 0.0 && m.texture >= 0.0);
        assert!(m.sharpness.is_finite() && m.sharpness >= 0.0);
    }
}
