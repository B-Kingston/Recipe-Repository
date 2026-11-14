use super::search::{fake_sidecar, sidecar_state};
use crate::embeddings::{DIMENSIONS, normalize, to_blob};
use crate::moments::{
    MomentKind, MomentVector, StepMoment, align_monotonic, audio_windows, match_steps,
    refresh_recipe, stills_around,
};
use crate::stamp;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::time::Duration;

/// A unit vector along one axis of a small test space, slightly mixed with a
/// shared axis so cosines are not all zero.
fn axis(index: usize, mix: f32) -> Vec<f32> {
    let mut vector = vec![0.0; 8];
    vector[index] = 1.0;
    vector[7] = mix;
    normalize(&mut vector);
    vector
}

fn moment(kind: MomentKind, seconds: f64, vector: Vec<f32>) -> MomentVector {
    MomentVector {
        kind,
        seconds,
        vector,
    }
}

#[test]
fn alignment_never_moves_a_later_step_before_an_earlier_one() {
    // Row 1's best column (0) sits before row 0's best column (2); summed
    // score is highest with both on column 2.
    let scores = vec![vec![0.1, 0.2, 0.9, 0.3], vec![0.8, 0.1, 0.7, 0.2]];
    assert_eq!(align_monotonic(&scores), vec![2, 2]);
    let free = vec![vec![0.9, 0.1, 0.1], vec![0.1, 0.1, 0.9]];
    assert_eq!(align_monotonic(&free), vec![0, 2]);
}

#[test]
fn narrated_audio_wins_and_steps_take_their_spoken_moment() {
    let steps = vec![axis(0, 0.3), axis(1, 0.3), axis(2, 0.3)];
    let mut pool = Vec::new();
    // Six windows; windows 1, 3 and 5 talk about steps 0, 1 and 2.
    for (window, topic) in [3, 0, 4, 1, 5, 2].into_iter().enumerate() {
        pool.push(moment(
            MomentKind::Audio,
            window as f64 * 6.0,
            axis(topic, 0.3),
        ));
    }
    // Frames are uninformative.
    for second in [0.0, 10.0, 20.0, 30.0] {
        pool.push(moment(MomentKind::Frame, second, axis(6, 0.3)));
    }
    let matched = match_steps(&steps, &pool);
    assert_eq!(
        matched,
        vec![
            StepMoment {
                step_index: 0,
                seconds: 6,
                source: MomentKind::Audio
            },
            StepMoment {
                step_index: 1,
                seconds: 18,
                source: MomentKind::Audio
            },
            StepMoment {
                step_index: 2,
                seconds: 30,
                source: MomentKind::Audio
            },
        ]
    );
}

#[test]
fn music_only_audio_falls_back_to_frames() {
    let steps = vec![axis(0, 0.3), axis(1, 0.3)];
    let mut pool = Vec::new();
    for window in 0..4 {
        pool.push(moment(MomentKind::Audio, window as f64 * 6.0, axis(5, 0.3)));
    }
    for (index, topic) in [6, 0, 6, 1].into_iter().enumerate() {
        pool.push(moment(
            MomentKind::Frame,
            index as f64 * 2.0,
            axis(topic, 0.3),
        ));
    }
    let matched = match_steps(&steps, &pool);
    assert_eq!(
        matched
            .iter()
            .map(|m| (m.seconds, m.source))
            .collect::<Vec<_>>(),
        vec![(2, MomentKind::Frame), (6, MomentKind::Frame)]
    );
}

#[test]
fn a_weak_spoken_step_takes_its_frame_only_between_its_spoken_neighbours() {
    let steps = vec![axis(0, 0.3), axis(1, 0.3), axis(2, 0.3)];
    // The narration covers steps 0 and 2 (at 6 s and 30 s) but not step 1.
    let audio = [3, 0, 4, 4, 5, 2]
        .into_iter()
        .enumerate()
        .map(|(window, topic)| moment(MomentKind::Audio, window as f64 * 6.0, axis(topic, 0.3)));
    let matched = |frame_seconds: f64| {
        let pool: Vec<MomentVector> = audio
            .clone()
            .chain([
                moment(MomentKind::Frame, 2.0, axis(0, 0.3)),
                moment(MomentKind::Frame, frame_seconds, axis(1, 0.3)),
            ])
            .collect();
        match_steps(&steps, &pool)
            .iter()
            .map(|m| (m.step_index, m.seconds, m.source))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        matched(20.0),
        vec![
            (0, 6, MomentKind::Audio),
            (1, 20, MomentKind::Frame),
            (2, 30, MomentKind::Audio),
        ]
    );
    // Step 1's frame comes after step 2 is spoken: no moment beats one out of order.
    assert_eq!(
        matched(40.0),
        vec![(0, 6, MomentKind::Audio), (2, 30, MomentKind::Audio)]
    );
}

#[test]
fn unrelated_video_matches_no_step() {
    let steps = vec![axis(0, 0.3), axis(1, 0.3)];
    let pool: Vec<MomentVector> = (0..4)
        .map(|i| moment(MomentKind::Audio, i as f64 * 6.0, axis(5, 0.3)))
        .chain((0..4).map(|i| moment(MomentKind::Frame, i as f64 * 2.0, axis(6, 0.3))))
        .collect();
    assert!(match_steps(&steps, &pool).is_empty());
}

fn wav(seconds: usize, with_list_chunk: bool) -> Vec<u8> {
    let rate = 16_000u32;
    let pcm = vec![0u8; seconds * rate as usize * 2];
    let mut out = b"RIFF\0\0\0\0WAVE".to_vec();
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    if with_list_chunk {
        // ffmpeg writes a LIST/INFO chunk between fmt and data.
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&5u32.to_le_bytes());
        out.extend_from_slice(b"INFOx\0");
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(&pcm);
    out
}

#[test]
fn audio_is_cut_into_overlapping_twelve_second_windows() {
    let windows = audio_windows(&wav(30, true)).unwrap();
    let starts: Vec<f64> = windows.iter().map(|(start, _)| *start).collect();
    // 0-12, 6-18, 12-24, 18-30; a 24-30 tail would repeat 18-30's end.
    assert_eq!(starts, vec![0.0, 6.0, 12.0, 18.0]);
    let first = &windows[0].1;
    let data_bytes = u32::from_le_bytes(first[40..44].try_into().unwrap());
    assert_eq!(data_bytes, 12 * 16_000 * 2);
    assert!(audio_windows(b"not a wav").is_err());
}

#[test]
fn short_audio_still_gets_one_window() {
    let windows = audio_windows(&wav(3, false)).unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].0, 0.0);
}

#[test]
fn stills_spread_over_an_audio_window_and_around_a_frame() {
    let available: Vec<u64> = (0..=40).step_by(2).collect();
    let audio = StepMoment {
        step_index: 0,
        seconds: 18,
        source: MomentKind::Audio,
    };
    assert_eq!(stills_around(&available, &audio), vec![20, 24, 28]);
    let frame = StepMoment {
        step_index: 0,
        seconds: 10,
        source: MomentKind::Frame,
    };
    assert_eq!(stills_around(&available, &frame), vec![8, 10, 12]);
    // Near the end the nearest still repeats; repeats are dropped.
    let end = StepMoment {
        step_index: 0,
        seconds: 40,
        source: MomentKind::Frame,
    };
    assert_eq!(stills_around(&available, &end), vec![38, 40]);
}

/// A unit vector of the sidecar's size along one axis.
fn one_hot(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0; DIMENSIONS];
    vector[index] = 1.0;
    vector
}

/// The fake sidecar's view of the step texts: boiling sits on axis 0,
/// frying on axis 1.
fn step_vector(text: &str) -> Vec<f32> {
    one_hot(if text.contains("Boil") {
        0
    } else if text.contains("Fry") {
        1
    } else {
        9
    })
}

async fn recipe_with_a_pool() -> SqlitePool {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    sqlx::query(
        "INSERT INTO recipes(id,title,description,user_id,created_at,updated_at) VALUES('r','Stew','','u1',?1,?1)",
    )
    .bind(stamp())
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('s0','r','step',0,'Boil the water slowly')")
        .execute(&db)
        .await
        .unwrap();
    // The narration boils at 0 s and fries at 6 s.
    for (seconds, topic) in [(0.0, 0), (6.0, 1), (12.0, 3), (18.0, 4)] {
        sqlx::query(
            "INSERT INTO recipe_moment_vectors(recipe_id,kind,seconds,vector) VALUES('r','audio',?,?)",
        )
        .bind(seconds)
        .bind(to_blob(&one_hot(topic)))
        .execute(&db)
        .await
        .unwrap();
    }
    db
}

#[tokio::test]
async fn a_slow_refresh_never_overwrites_the_moments_of_a_later_edit() {
    let db = recipe_with_a_pool().await;
    let sidecar = fake_sidecar(step_vector).await;
    let state = sidecar_state(db.clone(), &sidecar.url);
    let first = {
        let state = state.clone();
        tokio::spawn(async move { refresh_recipe(&state, "u1", "r").await })
    };
    // Once the first refresh has read the old step, the step is edited and
    // refreshed again; the old step's answer comes back last.
    while !sidecar
        .texts
        .lock()
        .iter()
        .any(|text| text.contains("Boil"))
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    sqlx::query("UPDATE recipe_blocks SET text='Fry the onions' WHERE id='s0'")
        .execute(&db)
        .await
        .unwrap();
    refresh_recipe(&state, "u1", "r").await.unwrap();
    first.await.unwrap().unwrap();

    let seconds: i64 =
        sqlx::query_scalar("SELECT seconds FROM recipe_step_moments WHERE step_id='s0'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(seconds, 6);
}
