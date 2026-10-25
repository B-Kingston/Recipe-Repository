//! Video step moments ("watch this step").
//!
//! During a video import the downloaded reel is cut into overlapping 12 s
//! audio windows and sampled every two seconds for small stills. The
//! EmbeddingGemma 2 sidecar embeds the audio windows and a bounded set of the
//! stills. These time-stamped vectors (the moment pool) outlive the video, so
//! each recipe step's text can later be matched against them:
//!
//! - When the aligned audio clearly talks about the steps (mean similarity
//!   lift over the step's average window), every step takes its audio match.
//! - Otherwise, or for a single step whose audio match is weak, the frame
//!   match is used when it is clear enough.
//! - Steps are aligned monotonically: step N never matches earlier than step
//!   N-1. On narrated test reels this put 15 of 16 steps within 4 s of the
//!   moment the cook described them; frames alone managed 12 of 16.

use crate::embeddings::{self, BlockingEmbedder, Embedder, Input};
use crate::media::{create_private_dir, env_path, remaining_timeout, run_tool};
use crate::recipes::{
    draft_moment_vectors, recipe_moment_vectors, recipe_step_texts, replace_draft_step_moments,
    replace_recipe_step_moments,
};
use crate::{AppError, AppState, GeneratedRecipe, MomentView};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

/// Media and sidecar helpers report plain messages, like the `media.rs` tools.
type Outcome<T> = std::result::Result<T, String>;

/// Audio window length and hop, chosen on narrated cooking videos: 12 s
/// windows carry a full spoken instruction; 6 s hops keep moments precise.
const AUDIO_WINDOW_SECONDS: f64 = 12.0;
const AUDIO_HOP_SECONDS: f64 = 6.0;
/// Shorter trailing audio is not worth a window of its own.
const AUDIO_MIN_WINDOW_SECONDS: f64 = 4.0;
/// Stills cadence and size. They are shown beside each step and embedded for
/// the frame fallback.
const STILL_INTERVAL_SECONDS: u64 = 2;
const STILL_WIDTH: u32 = 360;
const MAX_STILLS_BYTES: usize = 24 * 1024 * 1024;
/// Frame embeddings cost about 0.7 s each on the CPU, so a long video embeds
/// an evenly spread subset of its stills.
const MAX_FRAME_VECTORS: usize = 40;
const MAX_SECONDS: u64 = 300;
/// Similarity lift (aligned cosine minus the step's mean cosine) thresholds,
/// calibrated on matched and mismatched reels: matched audio averaged
/// 0.08-0.10, unrelated audio 0.035.
const AUDIO_MIN_MEAN_LIFT: f32 = 0.06;
const AUDIO_MIN_STEP_LIFT: f32 = 0.045;
const FRAME_MIN_MEAN_LIFT: f32 = 0.05;
const FRAME_MIN_STEP_LIFT: f32 = 0.05;
/// Stills shown around a moment, as offsets from its stored second. An audio
/// moment starts at its window start, so its stills spread over the window.
const AUDIO_STILL_OFFSETS: [i64; 3] = [2, 6, 10];
const FRAME_STILL_OFFSETS: [i64; 3] = [-2, 0, 2];

/// Which channel a moment vector, and a matched moment, came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MomentKind {
    Audio,
    Frame,
}

impl MomentKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MomentKind::Audio => "audio",
            MomentKind::Frame => "frame",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "audio" => Some(MomentKind::Audio),
            "frame" => Some(MomentKind::Frame),
            _ => None,
        }
    }
}

/// One embedded moment: an audio window (seconds = window start) or a frame.
#[derive(Debug, Clone)]
pub(crate) struct MomentVector {
    pub(crate) kind: MomentKind,
    pub(crate) seconds: f64,
    pub(crate) vector: Vec<f32>,
}

/// One small still JPEG at a whole second of the video.
#[derive(Debug, Clone)]
pub(crate) struct MomentStill {
    pub(crate) seconds: u64,
    pub(crate) jpeg: Vec<u8>,
}

/// Everything a video import keeps for later step matching.
#[derive(Debug, Clone, Default)]
pub(crate) struct MomentPool {
    pub(crate) vectors: Vec<MomentVector>,
    pub(crate) stills: Vec<MomentStill>,
}

/// The matched moment of one step (0-based index in step order).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StepMoment {
    pub(crate) step_index: usize,
    pub(crate) seconds: u64,
    pub(crate) source: MomentKind,
}

/// Builds the moment pool from a downloaded video and, when the audio
/// channel ran, its 16 kHz mono WAV. Blocking (ffmpeg plus sidecar calls);
/// failures add a warning and keep whatever part of the pool succeeded.
pub(crate) fn capture(
    video: &Path,
    audio: Option<&Path>,
    workdir: &Path,
    embedder: &BlockingEmbedder,
    warnings: &mut Vec<String>,
    deadline: Instant,
) -> MomentPool {
    let mut pool = MomentPool::default();
    match extract_stills(video, workdir, deadline) {
        Ok(stills) => pool.stills = stills,
        Err(error) => warnings.push(format!("Step moment stills were not captured: {error}")),
    }
    let frames = spread(pool.stills.len(), MAX_FRAME_VECTORS);
    if !frames.is_empty() {
        let inputs: Vec<Input> = frames
            .iter()
            .map(|&index| Input::Image(pool.stills[index].jpeg.clone()))
            .collect();
        match embedder.embed(&inputs) {
            Ok(vectors) => {
                pool.vectors
                    .extend(
                        frames
                            .iter()
                            .zip(vectors)
                            .map(|(&index, vector)| MomentVector {
                                kind: MomentKind::Frame,
                                seconds: pool.stills[index].seconds as f64,
                                vector,
                            }),
                    )
            }
            Err(error) => warnings.push(format!("Video frames were not embedded: {error}")),
        }
    }
    if let Some(audio) = audio {
        match std::fs::read(audio)
            .map_err(|error| error.to_string())
            .and_then(|bytes| audio_windows(&bytes))
        {
            Ok(windows) if !windows.is_empty() => {
                let inputs: Vec<Input> = windows
                    .iter()
                    .map(|(_, wav)| Input::Audio(wav.clone()))
                    .collect();
                match embedder.embed(&inputs) {
                    Ok(vectors) => pool.vectors.extend(windows.iter().zip(vectors).map(
                        |((start, _), vector)| MomentVector {
                            kind: MomentKind::Audio,
                            seconds: *start,
                            vector,
                        },
                    )),
                    Err(error) => {
                        warnings.push(format!("Audio windows were not embedded: {error}"))
                    }
                }
            }
            Ok(_) => {}
            Err(error) => warnings.push(format!("Audio windows were not readable: {error}")),
        }
    }
    pool
}

/// Samples one still every [`STILL_INTERVAL_SECONDS`] over the analysed span.
fn extract_stills(video: &Path, workdir: &Path, deadline: Instant) -> Outcome<Vec<MomentStill>> {
    let dir = workdir.join("moments");
    create_private_dir(&dir).map_err(|error| error.to_string())?;
    let timeout =
        remaining_timeout(deadline, Duration::from_secs(90)).ok_or("the media deadline passed")?;
    let args = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-t".into(),
        MAX_SECONDS.to_string(),
        "-i".into(),
        video.to_string_lossy().to_string(),
        "-vf".into(),
        format!("fps=1/{STILL_INTERVAL_SECONDS},scale={STILL_WIDTH}:-2"),
        "-q:v".into(),
        "5".into(),
        dir.join("still-%04d.jpg").to_string_lossy().to_string(),
    ];
    run_tool(
        &env_path("MEDIA_FFMPEG_PATH", "ffmpeg"),
        &args,
        timeout,
        1024 * 1024,
    )?;
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|error| error.to_string())?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("still-") && name.ends_with(".jpg"))
        .collect();
    names.sort();
    let mut stills = Vec::with_capacity(names.len());
    let mut total = 0usize;
    for (index, name) in names.iter().enumerate() {
        let jpeg = std::fs::read(dir.join(name)).map_err(|error| error.to_string())?;
        total += jpeg.len();
        if total > MAX_STILLS_BYTES {
            break;
        }
        stills.push(MomentStill {
            seconds: index as u64 * STILL_INTERVAL_SECONDS,
            jpeg,
        });
    }
    Ok(stills)
}

/// Up to `limit` indices spread evenly over `0..len`, first and last kept.
fn spread(len: usize, limit: usize) -> Vec<usize> {
    if len <= limit {
        return (0..len).collect();
    }
    (0..limit)
        .map(|i| (i * (len - 1) + (limit - 1) / 2) / (limit - 1))
        .collect()
}

/// Splits a 16-bit PCM WAV into overlapping windows, each re-encoded as its
/// own WAV. Returns `(window start seconds, wav bytes)`.
pub(crate) fn audio_windows(wav: &[u8]) -> Outcome<Vec<(f64, Vec<u8>)>> {
    let (rate, channels, data) = pcm16_data(wav).ok_or("not a 16-bit PCM WAV file")?;
    let frame_bytes = 2 * channels as usize;
    let frames = data.len() / frame_bytes;
    let max_frames = (MAX_SECONDS * rate as u64) as usize;
    let frames = frames.min(max_frames);
    let window = (AUDIO_WINDOW_SECONDS * rate as f64) as usize;
    let hop = (AUDIO_HOP_SECONDS * rate as f64) as usize;
    let min_window = (AUDIO_MIN_WINDOW_SECONDS * rate as f64) as usize;
    let mut windows = Vec::new();
    let mut start = 0usize;
    while start < frames {
        let end = (start + window).min(frames);
        if end - start < min_window && !windows.is_empty() {
            break;
        }
        let bytes = &data[start * frame_bytes..end * frame_bytes];
        windows.push((start as f64 / rate as f64, wav_file(bytes, rate, channels)));
        if end == frames {
            break;
        }
        start += hop;
    }
    Ok(windows)
}

/// Finds the `fmt ` and `data` chunks of a PCM WAV.
fn pcm16_data(wav: &[u8]) -> Option<(u32, u16, &[u8])> {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return None;
    }
    let mut offset = 12;
    let mut format: Option<(u32, u16)> = None;
    while offset + 8 <= wav.len() {
        let id = &wav[offset..offset + 4];
        let size = u32::from_le_bytes(wav[offset + 4..offset + 8].try_into().ok()?) as usize;
        let body_start = offset + 8;
        let body_end = body_start.checked_add(size)?.min(wav.len());
        let body = &wav[body_start..body_end];
        if id == b"fmt " && body.len() >= 16 {
            let audio_format = u16::from_le_bytes([body[0], body[1]]);
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let bits = u16::from_le_bytes([body[14], body[15]]);
            if audio_format != 1 || bits != 16 || channels == 0 || rate == 0 {
                return None;
            }
            format = Some((rate, channels));
        } else if id == b"data" {
            let (rate, channels) = format?;
            return Some((rate, channels, body));
        }
        offset = body_start + size + (size & 1);
    }
    None
}

fn wav_file(pcm: &[u8], rate: u32, channels: u16) -> Vec<u8> {
    let block = channels as u32 * 2;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * block).to_le_bytes());
    out.extend_from_slice(&(block as u16).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// Embeds the step texts as retrieval queries and matches them to the pool.
pub(crate) async fn step_moments(
    embedder: &Embedder,
    steps: &[String],
    vectors: &[MomentVector],
) -> Outcome<Vec<StepMoment>> {
    if steps.is_empty() || vectors.is_empty() {
        return Ok(Vec::new());
    }
    let inputs: Vec<Input> = steps
        .iter()
        .map(|step| Input::Text(embeddings::query(step)))
        .collect();
    let step_vectors = embedder.embed(&inputs).await?;
    Ok(match_steps(&step_vectors, vectors))
}

/// Matches unit step vectors to the pool (see the module docs for the rule).
pub(crate) fn match_steps(steps: &[Vec<f32>], pool: &[MomentVector]) -> Vec<StepMoment> {
    let audio = channel(pool, MomentKind::Audio);
    let frames = channel(pool, MomentKind::Frame);
    let audio_plan = plan(steps, &audio);
    let frame_plan = plan(steps, &frames);
    let mean = |plan: &Option<Plan>| plan.as_ref().map_or(f32::MIN, Plan::mean_lift);
    let use_audio = mean(&audio_plan) >= AUDIO_MIN_MEAN_LIFT;
    let use_frames = mean(&frame_plan) >= FRAME_MIN_MEAN_LIFT;
    let mut moments = Vec::new();
    for step_index in 0..steps.len() {
        let matched = if use_audio {
            pick(&audio_plan, &audio, step_index, AUDIO_MIN_STEP_LIFT)
                .or_else(|| pick(&frame_plan, &frames, step_index, FRAME_MIN_STEP_LIFT))
        } else if use_frames {
            pick(&frame_plan, &frames, step_index, FRAME_MIN_STEP_LIFT)
        } else {
            None
        };
        if let Some(vector) = matched {
            moments.push(StepMoment {
                step_index,
                seconds: vector.seconds.max(0.0).round() as u64,
                source: vector.kind,
            });
        }
    }
    moments
}

/// The planned moment of one step when its lift clears `min_lift`.
fn pick<'a>(
    plan: &Option<Plan>,
    channel: &[&'a MomentVector],
    step_index: usize,
    min_lift: f32,
) -> Option<&'a MomentVector> {
    let plan = plan.as_ref()?;
    (plan.lifts[step_index] >= min_lift).then(|| channel[plan.columns[step_index]])
}

fn channel(pool: &[MomentVector], kind: MomentKind) -> Vec<&MomentVector> {
    let mut vectors: Vec<&MomentVector> = pool.iter().filter(|v| v.kind == kind).collect();
    vectors.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
    vectors
}

/// A monotonic alignment of steps to one channel's moments.
struct Plan {
    columns: Vec<usize>,
    lifts: Vec<f32>,
}

impl Plan {
    fn mean_lift(&self) -> f32 {
        self.lifts.iter().sum::<f32>() / self.lifts.len().max(1) as f32
    }
}

fn plan(steps: &[Vec<f32>], channel: &[&MomentVector]) -> Option<Plan> {
    if steps.is_empty() || channel.is_empty() {
        return None;
    }
    let scores: Vec<Vec<f32>> = steps
        .iter()
        .map(|step| {
            channel
                .iter()
                .map(|m| embeddings::dot(step, &m.vector))
                .collect()
        })
        .collect();
    let columns = align_monotonic(&scores);
    let lifts = scores
        .iter()
        .zip(&columns)
        .map(|(row, &column)| row[column] - row.iter().sum::<f32>() / row.len() as f32)
        .collect();
    Some(Plan { columns, lifts })
}

/// One column per row, non-decreasing down the rows, maximising the summed
/// score (a Viterbi pass over prefix maxima).
pub(crate) fn align_monotonic(scores: &[Vec<f32>]) -> Vec<usize> {
    let Some(first) = scores.first() else {
        return Vec::new();
    };
    let mut totals: Vec<Vec<f32>> = vec![first.clone()];
    for row in &scores[1..] {
        let previous = totals.last().expect("totals starts non-empty");
        let mut best = f32::MIN;
        let next = row
            .iter()
            .zip(previous)
            .map(|(score, total)| {
                best = best.max(*total);
                score + best
            })
            .collect();
        totals.push(next);
    }
    let argmax = |row: &[f32]| {
        row.iter()
            .enumerate()
            .fold(
                (0, f32::MIN),
                |acc, (i, v)| if *v > acc.1 { (i, *v) } else { acc },
            )
            .0
    };
    let mut columns = vec![argmax(totals.last().expect("non-empty"))];
    for row in totals[..totals.len() - 1].iter().rev() {
        let limit = *columns.last().expect("non-empty");
        columns.push(argmax(&row[..=limit]));
    }
    columns.reverse();
    columns
}

/// Seconds of the stored stills to show beside a moment: the nearest still
/// to each offset, without repeats.
pub(crate) fn stills_around(available: &[u64], moment: &StepMoment) -> Vec<u64> {
    let offsets = match moment.source {
        MomentKind::Audio => AUDIO_STILL_OFFSETS,
        MomentKind::Frame => FRAME_STILL_OFFSETS,
    };
    let mut picked: Vec<u64> = Vec::new();
    for offset in offsets {
        let target = moment.seconds as i64 + offset;
        if let Some(&nearest) = available
            .iter()
            .min_by_key(|&&seconds| (seconds as i64 - target).abs())
            && !picked.contains(&nearest)
        {
            picked.push(nearest);
        }
    }
    picked
}

/// "m:ss" for a moment label.
pub(crate) fn clock(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The page view of one matched moment. `still_base` is the route prefix
/// that serves the pool's stills by second.
pub(crate) fn view(moment: &StepMoment, still_seconds: &[u64], still_base: &str) -> MomentView {
    MomentView {
        label: clock(moment.seconds),
        source: match moment.source {
            MomentKind::Audio => "Matched to the narration".into(),
            MomentKind::Frame => "Matched to the video frames".into(),
        },
        stills: stills_around(still_seconds, moment)
            .into_iter()
            .map(|seconds| format!("{still_base}/{seconds}"))
            .collect(),
    }
}

/// Matches a draft's steps to its moment pool (its own, or the pool of the
/// recipe it alters) and stores the result. A draft without a pool, a
/// switched-off setting, or a missing sidecar stores nothing.
pub(crate) async fn match_draft(
    state: &AppState,
    user_id: &str,
    draft_id: &str,
    recipe_id: Option<&str>,
    recipe: &GeneratedRecipe,
) -> crate::Result<()> {
    let Some(embedder) = &state.embedder else {
        return Ok(());
    };
    if !crate::step_moments_enabled(&state.db, user_id).await? {
        return Ok(());
    }
    let vectors = draft_moment_vectors(&state.db, draft_id, recipe_id).await?;
    if vectors.is_empty() {
        return Ok(());
    }
    let steps: Vec<String> = recipe.steps.iter().map(|step| step.text.clone()).collect();
    let moments = step_moments(embedder, &steps, &vectors)
        .await
        .map_err(AppError::Internal)?;
    replace_draft_step_moments(&state.db, draft_id, &moments).await
}

/// Re-matches a saved recipe's steps to its moment pool after a step edit.
pub(crate) async fn refresh_recipe(
    state: &AppState,
    user_id: &str,
    recipe_id: &str,
) -> crate::Result<()> {
    let Some(embedder) = &state.embedder else {
        return Ok(());
    };
    if !crate::step_moments_enabled(&state.db, user_id).await? {
        return Ok(());
    }
    let vectors = recipe_moment_vectors(&state.db, recipe_id).await?;
    if vectors.is_empty() {
        return Ok(());
    }
    let steps = recipe_step_texts(&state.db, recipe_id).await?;
    let texts: Vec<String> = steps.iter().map(|(_, text)| text.clone()).collect();
    let moments = step_moments(embedder, &texts, &vectors)
        .await
        .map_err(AppError::Internal)?;
    let keyed: Vec<(String, StepMoment)> = moments
        .into_iter()
        .filter_map(|moment| Some((steps.get(moment.step_index)?.0.clone(), moment)))
        .collect();
    replace_recipe_step_moments(&state.db, recipe_id, &keyed).await
}

/// Runs [`refresh_recipe`] in the background so a step edit returns at once.
/// Recipes without a moment pool cost one empty query.
pub(crate) fn spawn_recipe_refresh(state: &Arc<AppState>, user_id: &str, recipe_id: &str) {
    if state.embedder.is_none() {
        return;
    }
    let state = state.clone();
    let user_id = user_id.to_string();
    let recipe_id = recipe_id.to_string();
    tokio::spawn(async move {
        if let Err(error) = refresh_recipe(&state, &user_id, &recipe_id).await {
            warn!(%recipe_id, %error, "Step moments were not refreshed");
        }
    });
}
