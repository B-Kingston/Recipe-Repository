use crate::thumbs::{MAX_THUMBNAILS, ThumbCandidate, extract_thumbnails};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// End-to-end run over a real ffmpeg-encoded clip. Skips quietly where
/// ffmpeg is unavailable so pure unit environments stay green; the Docker
/// test image installs ffmpeg, so the gate always runs there.
#[test]
fn real_video_yields_diverse_decodable_candidates() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffmpeg is not installed in this environment");
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "kindle-recipes-thumbs-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let video = dir.join("clip.mp4");
    // testsrc2 gives twelve seconds of moving, colourful, occasionally
    // text-bearing frames — close enough to reel content for the pipeline.
    let generated = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30:duration=12",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .output()
        .expect("ffmpeg should encode the fixture");
    assert!(generated.status.success(), "fixture encoding failed");

    let mut warnings = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    let candidates: Vec<ThumbCandidate> =
        extract_thumbnails(&video, &dir, &[], Some(12), &mut warnings, deadline);
    let video_path: PathBuf = video.clone();
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
    let _ = std::fs::remove_file(video_path);
    let _ = std::fs::remove_dir_all(&dir);
}
