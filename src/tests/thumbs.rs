use crate::media::probe_video;
use crate::thumbs::{MAX_THUMBNAILS, ThumbCandidate, extract_thumbnails};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// ffmpeg-backed tests skip quietly where ffmpeg is unavailable so pure unit
/// environments stay green; the Docker test image installs ffmpeg.
fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

fn run_ffmpeg(args: &[&str], output: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .arg(output)
        .status()
        .expect("ffmpeg should run");
    assert!(status.success(), "ffmpeg failed for {}", output.display());
}

/// testsrc2 gives twelve seconds of moving, colourful, occasionally
/// text-bearing frames — close enough to reel content for the pipeline.
fn encode_fixture(path: &Path) {
    run_ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30:duration=12",
            "-pix_fmt",
            "yuv420p",
        ],
        path,
    );
}

fn scratch_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kindle-recipes-thumbs-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// End-to-end run over a real ffmpeg-encoded clip.
#[test]
fn real_video_yields_diverse_decodable_candidates() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg is not installed in this environment");
        return;
    }
    let dir = scratch_dir();
    let video = dir.join("clip.mp4");
    encode_fixture(&video);

    let mut warnings = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    let probe = probe_video(&video).expect("the fixture should probe");
    assert_eq!((probe.width, probe.height), (640, 360));
    let candidates: Vec<ThumbCandidate> =
        extract_thumbnails(&video, &probe, &dir, &[], Some(12), &mut warnings, deadline);
    assert!(!candidates.is_empty(), "no candidates: {warnings:?}");
    assert!(candidates.len() <= MAX_THUMBNAILS);
    for pair in candidates.windows(2) {
        assert!(
            pair[0].score >= pair[1].score,
            "candidates must stay ranked"
        );
    }
    for candidate in &candidates {
        assert!(candidate.seconds <= 12, "candidate outside the clip");
        assert_eq!(&candidate.jpeg[..2], &[0xFF, 0xD8], "not a JPEG SOI");
        assert!(candidate.jpeg.len() > 1024, "suspiciously tiny crop");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A phone clip stores landscape pixels with a quarter-turn display matrix.
/// The probe must report the displayed (portrait) size so the crop window
/// fits the frames ffmpeg decodes.
#[test]
fn rotated_phone_video_is_cropped_in_display_orientation() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg is not installed in this environment");
        return;
    }
    let dir = scratch_dir();
    let flat = dir.join("flat.mp4");
    let rotated = dir.join("rotated.mp4");
    encode_fixture(&flat);
    run_ffmpeg(
        &[
            "-i",
            flat.to_str().unwrap(),
            "-c",
            "copy",
            "-metadata:s:v:0",
            "rotate=90",
        ],
        &rotated,
    );

    let probe = probe_video(&rotated).expect("the rotated fixture should probe");
    assert_eq!((probe.width, probe.height), (360, 640));
    let mut warnings = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    let candidates = extract_thumbnails(
        &rotated,
        &probe,
        &dir,
        &[],
        Some(12),
        &mut warnings,
        deadline,
    );
    assert!(!candidates.is_empty(), "no candidates: {warnings:?}");
    for candidate in &candidates {
        assert_eq!(&candidate.jpeg[..2], &[0xFF, 0xD8], "not a JPEG SOI");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
