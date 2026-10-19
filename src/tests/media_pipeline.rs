use super::*;
use serde_json::json;
use std::sync::{Mutex, mpsc};

fn followup<X: Send + 'static>(
    run: impl FnOnce(&Path, &AtomicBool) -> X + Send + 'static,
) -> Option<Followup<X>> {
    Some(Box::new(run))
}

/// Waits until the job thread has released the slot (the slot is the last
/// thing it lets go of, after the work directory), then returns the permit.
async fn wait_for_slot(slots: &'static Semaphore) -> SemaphorePermit<'static> {
    tokio::time::timeout(Duration::from_secs(10), slots.acquire())
        .await
        .expect("the job should release the media slot")
        .unwrap()
}

#[tokio::test]
async fn extras_run_after_the_evidence_is_returned_and_clean_up_on_success() {
    // Each test owns a one-slot semaphore, so "the media slot was released"
    // is asserted without the global slot or other tests getting in the way.
    static TEST_SLOTS: Semaphore = Semaphore::const_new(1);
    let slots = &TEST_SLOTS;
    let permit = slots.acquire().await.unwrap();
    let seen_dir = Arc::new(Mutex::new(None::<PathBuf>));
    let (release, gate) = mpsc::channel::<()>();
    let recorded = seen_dir.clone();
    let (evidence, extras) = run_staged_job(permit, move |dir| {
        *recorded.lock().unwrap() = Some(dir.to_path_buf());
        fs::write(dir.join("video.mp4"), b"video").unwrap();
        Ok((
            "evidence",
            followup(move |dir: &Path, _cancel: &AtomicBool| {
                // The consumer already holds the evidence; the follow-up still
                // sees the downloaded video.
                gate.recv().unwrap();
                assert!(dir.join("video.mp4").is_file());
                7_u32
            }),
        ))
    })
    .await
    .unwrap();
    assert_eq!(evidence, "evidence");
    let dir = seen_dir.lock().unwrap().clone().unwrap();
    assert!(!extras.is_finished(), "the follow-up is still running");
    assert!(dir.is_dir(), "the work directory outlives the evidence");
    assert_eq!(slots.available_permits(), 0, "the media slot stays held");

    release.send(()).unwrap();
    assert_eq!(extras.finish().await, 7);
    assert!(!dir.exists(), "the work directory is removed afterwards");
    assert_eq!(slots.available_permits(), 1, "the media slot is released");
}

#[tokio::test]
async fn dropping_the_extras_handle_cancels_the_followup_and_still_cleans_up() {
    // Each test owns a one-slot semaphore, so "the media slot was released"
    // is asserted without the global slot or other tests getting in the way.
    static TEST_SLOTS: Semaphore = Semaphore::const_new(1);
    let slots = &TEST_SLOTS;
    let permit = slots.acquire().await.unwrap();
    let seen_dir = Arc::new(Mutex::new(None::<PathBuf>));
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let (release, gate) = mpsc::channel::<()>();
    let (recorded, observed) = (seen_dir.clone(), saw_cancel.clone());
    let (_, extras) = run_staged_job(permit, move |dir| {
        *recorded.lock().unwrap() = Some(dir.to_path_buf());
        Ok((
            (),
            followup(move |_dir: &Path, cancel: &AtomicBool| {
                gate.recv().unwrap();
                observed.store(cancel.load(Ordering::SeqCst), Ordering::SeqCst);
                0_u32
            }),
        ))
    })
    .await
    .unwrap();

    // The cleaner or draft step failed: the caller drops the handle.
    drop(extras);
    release.send(()).unwrap();
    let _permit = wait_for_slot(slots).await;
    assert!(
        saw_cancel.load(Ordering::SeqCst),
        "the follow-up should see that nobody wants its result"
    );
    let dir = seen_dir.lock().unwrap().clone().unwrap();
    assert!(!dir.exists(), "the work directory must not leak");
}

#[tokio::test]
async fn a_dropped_request_skips_the_followup_but_releases_everything() {
    // Each test owns a one-slot semaphore, so "the media slot was released"
    // is asserted without the global slot or other tests getting in the way.
    static TEST_SLOTS: Semaphore = Semaphore::const_new(1);
    let slots = &TEST_SLOTS;
    let permit = slots.acquire().await.unwrap();
    let seen_dir = Arc::new(Mutex::new(None::<PathBuf>));
    let ran = Arc::new(AtomicBool::new(false));
    let (release, gate) = mpsc::channel::<()>();
    let (recorded, ran_flag) = (seen_dir.clone(), ran.clone());
    let (receiver, _extras) = spawn_staged_job(permit, move |dir| {
        *recorded.lock().unwrap() = Some(dir.to_path_buf());
        gate.recv().unwrap();
        Ok((
            (),
            followup(move |_dir: &Path, _cancel: &AtomicBool| {
                ran_flag.store(true, Ordering::SeqCst);
                0_u32
            }),
        ))
    });
    // The HTTP request goes away while the extractor is still working.
    drop(receiver);
    release.send(()).unwrap();
    let _permit = wait_for_slot(slots).await;
    assert!(!ran.load(Ordering::SeqCst), "nobody is waiting for extras");
    let dir = seen_dir.lock().unwrap().clone().unwrap();
    assert!(!dir.exists(), "the work directory must not leak");
}

#[tokio::test]
async fn a_failed_extraction_reports_the_error_and_releases_everything() {
    // Each test owns a one-slot semaphore, so "the media slot was released"
    // is asserted without the global slot or other tests getting in the way.
    static TEST_SLOTS: Semaphore = Semaphore::const_new(1);
    let slots = &TEST_SLOTS;
    let permit = slots.acquire().await.unwrap();
    let seen_dir = Arc::new(Mutex::new(None::<PathBuf>));
    let recorded = seen_dir.clone();
    let outcome = run_staged_job::<(), u32>(permit, move |dir| {
        *recorded.lock().unwrap() = Some(dir.to_path_buf());
        Err(AppError::BadRequest("nothing readable".into()))
    })
    .await;
    assert!(matches!(outcome, Err(AppError::BadRequest(message)) if message == "nothing readable"));
    let _permit = wait_for_slot(slots).await;
    let dir = seen_dir.lock().unwrap().clone().unwrap();
    assert!(!dir.exists());
}

#[tokio::test]
async fn a_panic_in_the_work_still_removes_the_directory_and_frees_the_slot() {
    // Each test owns a one-slot semaphore, so "the media slot was released"
    // is asserted without the global slot or other tests getting in the way.
    static TEST_SLOTS: Semaphore = Semaphore::const_new(1);
    let slots = &TEST_SLOTS;
    let permit = slots.acquire().await.unwrap();
    let seen_dir = Arc::new(Mutex::new(None::<PathBuf>));
    let recorded = seen_dir.clone();
    let outcome = run_staged_job::<(), u32>(permit, move |dir| {
        *recorded.lock().unwrap() = Some(dir.to_path_buf());
        fs::write(dir.join("partial.mp4"), b"x").unwrap();
        panic!("simulated extractor crash");
    })
    .await;
    assert!(matches!(outcome, Err(AppError::Internal(_))));
    let _permit = wait_for_slot(slots).await;
    let dir = seen_dir.lock().unwrap().clone().unwrap();
    assert!(!dir.exists());
}

#[tokio::test]
async fn without_a_followup_the_directory_is_gone_before_the_evidence_arrives() {
    // Each test owns a one-slot semaphore, so "the media slot was released"
    // is asserted without the global slot or other tests getting in the way.
    static TEST_SLOTS: Semaphore = Semaphore::const_new(1);
    let slots = &TEST_SLOTS;
    let permit = slots.acquire().await.unwrap();
    let seen_dir = Arc::new(Mutex::new(None::<PathBuf>));
    let recorded = seen_dir.clone();
    let (evidence, extras) = run_staged_job::<_, u32>(permit, move |dir| {
        *recorded.lock().unwrap() = Some(dir.to_path_buf());
        Ok((5_u32, None))
    })
    .await
    .unwrap();
    assert_eq!(evidence, 5);
    let dir = seen_dir.lock().unwrap().clone().unwrap();
    assert!(
        !dir.exists(),
        "the CLI and debugger paths keep their cleanup guarantee"
    );
    assert!(extras.is_finished());
    assert_eq!(extras.finish().await, 0, "no follow-up means empty extras");
}

fn instagram_title(account: &str, caption: &str) -> String {
    format!("{account} on Instagram: \"{caption}\"")
}

#[test]
fn instagram_title_keeps_only_the_account() {
    let caption = "Creamy Miso Protein Pasta 🥣 Easily vegan\n\n- Pasta\n- Silken tofu";
    assert_eq!(
        account_title(
            &instagram_title("Sarah | Nutrition Student", caption),
            caption
        ),
        "Sarah | Nutrition Student on Instagram"
    );
}

#[test]
fn title_marks_around_the_quote_do_not_leak_into_the_account() {
    // Instagram wraps the colon and the closing quote in left-to-right marks.
    let title = "Mohammed Moosa on Instagram\u{200e}: \"حمسة عيش مع مشاكيك\n\n#food\"\u{200e}";
    assert_eq!(
        account_title(title, "حمسة عيش مع مشاكيك\n\n#food"),
        "Mohammed Moosa on Instagram"
    );
}

#[test]
fn platform_title_is_stripped_even_when_the_description_differs() {
    // The page caption can differ from yt-dlp's (truncation, edits); the
    // "on Instagram: "…"" shape still marks the caption as embedded.
    assert_eq!(
        account_title(
            &instagram_title("chef.anna", "Quick dinner"),
            "Totally other text"
        ),
        "chef.anna on Instagram"
    );
    assert_eq!(
        account_title(&instagram_title("chef.anna", "Quick dinner"), ""),
        "chef.anna on Instagram"
    );
}

#[test]
fn account_names_containing_a_colon_survive() {
    assert_eq!(
        account_title(&instagram_title("Chef: Bob", "Pasta night"), "Pasta night"),
        "Chef: Bob on Instagram"
    );
}

#[test]
fn a_quoted_title_that_repeats_the_caption_loses_the_caption() {
    assert_eq!(
        account_title(
            "Chef Bob: \"Quick pasta tonight\"",
            "Quick pasta tonight\n#food"
        ),
        "Chef Bob"
    );
}

#[test]
fn genuine_titles_are_never_shortened() {
    for title in [
        "Creamy Miso Pasta",
        "Video by sarah.natural.health.guide",
        "Gran's \"famous\" pasta",
        // Quoted, but neither a platform signature nor the caption.
        "Dinner: \"Pasta\"",
        "Recipe: 3 ingredients",
    ] {
        assert_eq!(
            account_title(title, "Something else entirely"),
            title,
            "{title:?} must be kept"
        );
    }
    assert_eq!(
        account_title("  Creamy Miso Pasta ", ""),
        "Creamy Miso Pasta"
    );
}

#[test]
fn duration_prefers_the_extractor_and_falls_back_to_the_container() {
    assert_eq!(resolve_duration(Some(34), Some(40.2)), Some(34));
    assert_eq!(resolve_duration(None, Some(33.6)), Some(34));
    assert_eq!(resolve_duration(None, Some(43.4)), Some(43));
    assert_eq!(resolve_duration(None, None), None);
    assert_eq!(resolve_duration(None, Some(0.0)), None);
    assert_eq!(resolve_duration(None, Some(f64::NAN)), None);
}

#[test]
fn a_zero_extractor_duration_counts_as_missing() {
    assert_eq!(metadata_duration(&json!({"duration": 33.6})), Some(34));
    assert_eq!(metadata_duration(&json!({"duration": 0})), None);
    assert_eq!(metadata_duration(&json!({"title": "no duration"})), None);
    let metadata_gap = metadata_duration(&json!({"duration": 0.0}));
    assert_eq!(resolve_duration(metadata_gap, Some(12.4)), Some(12));
}

#[test]
fn probe_reads_the_container_duration() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping: ffmpeg is not installed in this environment");
        return;
    }
    let dir = env::temp_dir().join(format!("kindle-recipes-probe-test-{}", Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let video = dir.join("clip.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
        ])
        .args([
            "testsrc2=size=320x240:rate=30:duration=3",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .status()
        .expect("ffmpeg should run");
    assert!(status.success());
    let probe = probe_video(&video).expect("the fixture should probe");
    let duration = probe
        .duration_seconds
        .expect("the container has a duration");
    assert!((2.9..3.2).contains(&duration), "duration was {duration}");
    assert_eq!(resolve_duration(None, probe.duration_seconds), Some(3));
    let _ = fs::remove_dir_all(&dir);
}

fn evidence(segments: Vec<TranscriptSegment>) -> MediaEvidence {
    MediaEvidence {
        source_url: "https://www.instagram.com/p/ABC/".into(),
        title: "Pasta".into(),
        description: "Caption".into(),
        duration_seconds: Some(34),
        audio_transcript: "chop the onions then add two tablespoons of oil".into(),
        audio_segments: segments,
        ocr: vec![OcrSnippet {
            timestamp_seconds: 4,
            text: "2 tbsp oil / 1 onion".into(),
        }],
        warnings: Vec::new(),
        cleaned_recipe_text: String::new(),
    }
}

fn segment(start: f64, text: &str) -> TranscriptSegment {
    TranscriptSegment {
        start_seconds: start,
        end_seconds: start + 3.0,
        text: text.into(),
    }
}

#[test]
fn cleaner_prompt_lists_spoken_segments_with_their_start_second() {
    let prompt = cleaner_prompt(&evidence(vec![
        segment(0.4, "chop the onions"),
        segment(5.9, "  "),
        segment(6.2, "then add two tablespoons of oil"),
    ]));
    assert!(
        prompt.contains("[0s] chop the onions\n[6s] then add two tablespoons of oil"),
        "{prompt}"
    );
    assert!(prompt.contains("each [Ns] line starts N seconds into the video"));
    assert!(
        !prompt.contains("chop the onions then add"),
        "the flat transcript is replaced, not repeated"
    );
}

#[test]
fn cleaner_prompt_explains_the_row_separator_of_on_screen_cards() {
    let prompt = cleaner_prompt(&evidence(Vec::new()));
    assert!(prompt.contains("separates the lines of one card"));
    assert!(prompt.contains("[4s] 2 tbsp oil / 1 onion"));
}

#[test]
fn cleaner_prompt_falls_back_to_the_flat_transcript_without_segments() {
    let prompt = cleaner_prompt(&evidence(Vec::new()));
    assert!(prompt.contains("chop the onions then add two tablespoons of oil"));
    assert!(!prompt.contains("each [Ns] line starts"));
    let blank = cleaner_prompt(&evidence(vec![segment(1.0, " ")]));
    assert!(
        blank.contains("chop the onions then add"),
        "blank segments do not count"
    );
    let mut silent = evidence(Vec::new());
    silent.audio_transcript.clear();
    assert!(cleaner_prompt(&silent).contains("(untrusted Whisper output):\n[none]"));
}

#[test]
fn evidence_stored_before_segments_existed_still_deserializes() {
    let stored = r#"{
        "source_url": "https://www.instagram.com/p/ABC/",
        "title": "Pasta",
        "description": "Caption",
        "duration_seconds": 42,
        "audio_transcript": "spoken",
        "ocr": [{"timestamp_seconds": 4, "text": "2 tbsp oil"}],
        "warnings": [],
        "cleaned_recipe_text": "cleaned"
    }"#;
    let evidence: MediaEvidence = serde_json::from_str(stored).unwrap();
    assert!(evidence.audio_segments.is_empty());
    assert_eq!(evidence.audio_transcript, "spoken");
    assert_eq!(evidence.cleaned_recipe_text, "cleaned");
}

#[test]
fn evidence_segments_survive_a_round_trip() {
    let stored = serde_json::to_string(&evidence(vec![segment(1.5, "chop the onions")])).unwrap();
    let evidence: MediaEvidence = serde_json::from_str(&stored).unwrap();
    assert_eq!(evidence.audio_segments.len(), 1);
    assert_eq!(evidence.audio_segments[0].start_seconds, 1.5);
    assert_eq!(evidence.audio_segments[0].end_seconds, 4.5);
}

#[test]
fn transcript_segments_skip_unusable_entries_and_clamp_ends() {
    let value = json!({
        "text": "ignored here",
        "segments": [
            {"start": 0.5, "end": 2.0, "text": " chop the onions "},
            {"start": 2.0, "end": 3.0, "text": "   "},
            {"start": -1.0, "end": 3.0, "text": "negative start"},
            {"end": 3.0, "text": "no start"},
            {"start": 5.0, "end": 4.0, "text": "end before start"},
            {"start": 6.0, "text": "no end"},
        ]
    });
    let segments = parse_transcript_segments(&value);
    let texts: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["chop the onions", "end before start", "no end"]);
    assert_eq!(segments[0].end_seconds, 2.0);
    assert_eq!(
        segments[1].end_seconds, 5.0,
        "an end before the start is clamped"
    );
    assert_eq!(segments[2].end_seconds, 6.0);
    assert!(parse_transcript_segments(&json!({"text": "no segments key"})).is_empty());
}

#[test]
fn transcript_segments_are_bounded_like_the_flat_text() {
    let long = "word ".repeat(TRANSCRIPT_MAX_CHARS / 10);
    let value = json!({"segments": [
        {"start": 0.0, "end": 1.0, "text": long},
        {"start": 1.0, "end": 2.0, "text": long},
        {"start": 2.0, "end": 3.0, "text": long},
    ]});
    let kept: usize = parse_transcript_segments(&value)
        .iter()
        .map(|segment| segment.text.chars().count())
        .sum();
    assert!(kept <= TRANSCRIPT_MAX_CHARS);
}

/// Writes an executable stand-in for yt-dlp. It logs every call's arguments
/// and answers according to the `mode` file next to it.
fn fake_ytdlp(dir: &Path, mode: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("yt-dlp");
    fs::write(dir.join("mode"), mode).unwrap();
    fs::write(
        &script,
        r#"#!/bin/sh
dir=$(dirname "$0")
[ "$1" = "--selftest" ] && exit 0
echo "$*" >> "$dir/calls.log"
case "$*" in
  *--skip-download*) printf '{"title":"Meta only","description":"D"}'; exit 0 ;;
esac
case "$(cat "$dir/mode")" in
  ok)
    out=""; prev=""
    for arg in "$@"; do [ "$prev" = "-o" ] && out="$arg"; prev="$arg"; done
    printf video > "${out%\%(ext)s}mp4"
    printf '{"title":"Combined","description":"D","duration":12}' ;;
  fail) echo boom >&2; exit 1 ;;
  garbage) echo "not json" ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    // A parallel test's fork can briefly hold the freshly written file open;
    // exec then reports ETXTBSY until that child execs.
    for _ in 0..200 {
        if Command::new(&script).arg("--selftest").status().is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    script.to_string_lossy().to_string()
}

fn ytdlp_calls(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("kindle-recipes-{name}-{}", Uuid::new_v4()));
    fs::create_dir_all(dir.join("work")).unwrap();
    dir
}

#[test]
fn one_ytdlp_call_fetches_metadata_and_video_together() {
    let dir = scratch("ytdlp-ok");
    let ytdlp = fake_ytdlp(&dir, "ok");
    let work = dir.join("work");
    let mut warnings = Vec::new();
    let fetched = fetch_video_and_metadata(
        &ytdlp,
        "https://www.instagram.com/p/ABC/",
        &work,
        &[],
        &mut warnings,
        Instant::now() + Duration::from_secs(30),
    );
    let calls = ytdlp_calls(&dir);
    assert_eq!(
        calls.len(),
        1,
        "metadata and video share one call: {calls:?}"
    );
    assert!(calls[0].contains("--dump-single-json") && calls[0].contains("--no-simulate"));
    assert!(!calls[0].contains("--skip-download"));
    assert_eq!(fetched.metadata.unwrap()["title"], "Combined");
    assert!(fetched.video.is_some_and(|video| video.is_file()));
    assert!(warnings.is_empty(), "{warnings:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_combined_call_falls_back_to_the_metadata_only_call() {
    let dir = scratch("ytdlp-fail");
    let ytdlp = fake_ytdlp(&dir, "fail");
    let mut warnings = Vec::new();
    let fetched = fetch_video_and_metadata(
        &ytdlp,
        "https://www.instagram.com/p/ABC/",
        &dir.join("work"),
        &[],
        &mut warnings,
        Instant::now() + Duration::from_secs(30),
    );
    let calls = ytdlp_calls(&dir);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[1].contains("--skip-download"));
    assert_eq!(fetched.metadata.unwrap()["title"], "Meta only");
    assert!(fetched.video.is_none());
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("Video download failed")),
        "{warnings:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unreadable_combined_output_keeps_the_video_and_fetches_metadata_separately() {
    let dir = scratch("ytdlp-garbage");
    let ytdlp = fake_ytdlp(&dir, "garbage");
    let work = dir.join("work");
    // A prior download already left the file; the garbage call only prints.
    fs::write(work.join("video.mp4"), b"video").unwrap();
    let mut warnings = Vec::new();
    let fetched = fetch_video_and_metadata(
        &ytdlp,
        "https://www.instagram.com/p/ABC/",
        &work,
        &[],
        &mut warnings,
        Instant::now() + Duration::from_secs(30),
    );
    assert_eq!(ytdlp_calls(&dir).len(), 2);
    assert_eq!(fetched.metadata.unwrap()["title"], "Meta only");
    assert!(
        fetched.video.is_some(),
        "the downloaded video is not thrown away"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_description_only_import_uses_only_the_metadata_call() {
    let dir = scratch("ytdlp-meta");
    let ytdlp = fake_ytdlp(&dir, "ok");
    let mut warnings = Vec::new();
    let metadata = fetch_metadata(
        &ytdlp,
        "https://www.instagram.com/p/ABC/",
        &[],
        &mut warnings,
    );
    let calls = ytdlp_calls(&dir);
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("--skip-download"));
    assert_eq!(metadata.unwrap()["title"], "Meta only");
    let _ = fs::remove_dir_all(&dir);
}
