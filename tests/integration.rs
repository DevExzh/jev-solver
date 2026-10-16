mod support;

use std::{sync::atomic::Ordering, time::Duration};

use jev_solver::{
    browser::{self, BrowserOptions, BrowserStatus},
    client::{Answer, EvaluationResponse, Usage},
    config::Credentials,
    solver::{Decision, Grid, Thresholds, annotate, decode_image, prepare},
};
use support::{MockState, Server, png};

fn response(scores: &[f64]) -> EvaluationResponse {
    EvaluationResponse {
        model: "jev-1.14.0".to_owned(),
        usage: Usage::default(),
        answers: scores
            .iter()
            .enumerate()
            .map(|(i, p)| (format!("tile_{}", i + 1), Answer::Noul { noul: *p }))
            .collect(),
    }
}

fn scores(count: usize, indices: &[usize]) -> Vec<f64> {
    (1..=count)
        .map(|index| if indices.contains(&index) { 0.98 } else { 0.02 })
        .collect()
}

#[test]
fn grid_boundaries_cover_uneven_dimensions_without_gaps() {
    let grid = Grid::new(4, 4).unwrap();
    let mut covered = vec![0; 403 * 399];
    for index in 1..=grid.len() {
        let bounds = grid.bounds(index, 403, 399).unwrap();
        for y in bounds.y..bounds.y + bounds.height {
            for x in bounds.x..bounds.x + bounds.width {
                covered[(y * 403 + x) as usize] += 1;
            }
        }
    }
    assert!(covered.into_iter().all(|count| count == 1));
    assert!(grid.bounds(0, 403, 399).is_err());
    assert!(grid.bounds(17, 403, 399).is_err());
    assert!(Grid::new(0, 3).is_err());
    assert!(Grid::new(5, 3).is_err());
}

#[test]
fn uncertainty_and_threshold_boundaries_are_explicit() {
    let thresholds = Thresholds::default();
    assert_eq!(thresholds.decision(0.7), Decision::Select);
    assert_eq!(thresholds.decision(0.3), Decision::Skip);
    assert_eq!(thresholds.decision(0.5), Decision::Review);
    assert!(Thresholds::new(f64::NAN, 0.3).is_err());
    assert!(Thresholds::new(0.4, 0.3).is_err());
    assert!(Thresholds::new(0.7, 0.6).is_err());
}

#[test]
fn small_and_extreme_images_are_normalized_and_all_assets_are_referenced() {
    for (width, height) in [(9, 9), (301, 299), (2000, 10), (10, 2000)] {
        let prepared = prepare(
            &png(width, height),
            "Select bicycles",
            Grid::new(3, 3).unwrap(),
            "jev-1.14.0",
        )
        .unwrap();
        assert_eq!(prepared.request.questions.len(), 9);
        assert_eq!(prepared.uploads.len(), 10);
        let text = format!(
            "{}{}",
            prepared.request.state,
            serde_json::to_string(&prepared.request.questions).unwrap()
        );
        for reference in prepared.request.assets.keys() {
            assert_eq!(text.matches(reference).count(), 1);
        }
        for upload in prepared.uploads {
            let image = decode_image(&upload.png).unwrap();
            assert!((256..=1568).contains(&image.width()));
            assert!((256..=1568).contains(&image.height()));
            assert!(image.width() * image.height() >= 78_400);
        }
    }
}

#[test]
fn probabilities_are_mapped_to_row_major_tiles_and_annotations() {
    let bytes = png(303, 300);
    let prepared = prepare(
        &bytes,
        "Select bicycles",
        Grid::new(3, 3).unwrap(),
        "jev-1.14.0",
    )
    .unwrap();
    let solution = prepared
        .solution(
            response(&[0.98, 0.01, 0.5, 0.02, 0.75, 0.1, 0.01, 0.03, 0.9]),
            Thresholds::default(),
        )
        .unwrap();
    assert_eq!(solution.selected, [1, 5, 9]);
    assert_eq!(solution.uncertain, [3]);
    assert_eq!((solution.tiles[4].row, solution.tiles[4].col), (2, 2));
    let annotated = decode_image(&annotate(&bytes, &solution).unwrap())
        .unwrap()
        .to_rgb8();
    assert_eq!(annotated.get_pixel(0, 0).0, [30, 210, 90]);
    assert_eq!(annotated.get_pixel(202, 0).0, [255, 170, 0]);
}

#[test]
fn rejects_incomplete_mistyped_and_invalid_answers() {
    let prepared = prepare(
        &png(300, 300),
        "Select bicycles",
        Grid::new(3, 3).unwrap(),
        "jev-1.14.0",
    )
    .unwrap();
    assert!(
        prepared
            .solution(response(&[0.5; 8]), Thresholds::default())
            .is_err()
    );
    for invalid in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
        let mut answers = [0.01; 9];
        answers[4] = invalid;
        assert!(
            prepared
                .solution(response(&answers), Thresholds::default())
                .is_err()
        );
    }
    let mut wrong = response(&[0.01; 9]);
    wrong.answers.insert("tile_1".to_owned(), Answer::Other);
    assert!(prepared.solution(wrong, Thresholds::default()).is_err());
    let mut wrong_id = response(&[0.01; 9]);
    wrong_id.answers.remove("tile_1");
    wrong_id
        .answers
        .insert("other".to_owned(), Answer::Noul { noul: 0.1 });
    assert!(prepared.solution(wrong_id, Thresholds::default()).is_err());
    assert!(
        prepare(
            b"not an image",
            "Select bicycles",
            Grid::new(3, 3).unwrap(),
            "jev-1.14.0"
        )
        .is_err()
    );
    assert!(prepare(&png(300, 300), "", Grid::new(3, 3).unwrap(), "jev-1.14.0").is_err());
}

#[test]
fn credentials_require_secure_evaluation_endpoints() {
    assert!(Credentials::new("http://example.com/v1/systemone", "test-key".to_owned()).is_err());
    assert!(Credentials::new("https://openrouter.ai/api", "test-key".to_owned()).is_err());
    assert!(
        Credentials::new(
            "https://api.typesafe.ai/v1/systemone?key=secret",
            "test-key".to_owned()
        )
        .is_err()
    );
    assert!(Credentials::new("https://api.typesafe.ai/v1/systemone", String::new()).is_err());
}

#[tokio::test]
async fn http_integration_uploads_valid_multipart_and_preserves_resolved_model() {
    let server = Server::start(MockState {
        scores: vec![scores(9, &[1, 5, 9])],
        ..Default::default()
    })
    .await;
    let solution = server
        .solver(Duration::from_secs(5), 0)
        .solve(&png(300, 300), "Select bicycles", Grid::new(3, 3).unwrap())
        .await
        .unwrap();
    assert_eq!(solution.selected, [1, 5, 9]);
    assert_eq!(solution.model, "jev-1.14.0");
    assert_eq!(solution.usage.input_tokens, 1000);
    assert_eq!(server.state.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retries_rate_limits_and_rebuilds_consumed_multipart() {
    let server = Server::start(MockState {
        scores: vec![scores(9, &[2])],
        retry_first: true,
        ..Default::default()
    })
    .await;
    let solution = server
        .solver(Duration::from_secs(5), 1)
        .solve(&png(300, 300), "Select bicycles", Grid::new(3, 3).unwrap())
        .await
        .unwrap();
    assert_eq!(solution.selected, [2]);
    assert_eq!(server.state.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn authentication_errors_are_not_retried_or_echoed() {
    let server = Server::start(MockState {
        status: Some(401),
        ..Default::default()
    })
    .await;
    let error = server
        .solver(Duration::from_secs(5), 3)
        .solve(&png(300, 300), "Select bicycles", Grid::new(3, 3).unwrap())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("401"));
    assert!(!format!("{error:#}").contains("test-key"));
    assert_eq!(server.state.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refinement_changes_only_uncertain_tiles_and_preserves_initial_evidence() {
    let mut initial = scores(9, &[1]);
    initial[4] = 0.5;
    let server = Server::start(MockState {
        scores: vec![initial, scores(9, &[5])],
        ..Default::default()
    })
    .await;
    let solution = server
        .solver(Duration::from_secs(5), 0)
        .solve(&png(300, 300), "Select bicycles", Grid::new(3, 3).unwrap())
        .await
        .unwrap();
    assert_eq!(solution.selected, [1, 5]);
    assert!(solution.uncertain.is_empty());
    assert_eq!(solution.tiles[0].probability, 0.98);
    assert_eq!(solution.tiles[4].initial_probability, 0.5);
    assert_eq!(solution.tiles[4].probability, 0.98);
    assert_eq!(solution.refinements.len(), 1);
    assert_eq!(solution.refinements[0].judgments.len(), 1);
    assert_eq!(solution.refinements[0].judgments[0].index, 5);
    assert_eq!(solution.usage.input_tokens, 2000);
    assert_eq!(server.state.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn refinement_budget_is_bounded_and_can_be_disabled() {
    for passes in [0, 2] {
        let mut probabilities = scores(9, &[]);
        probabilities[0] = 0.5;
        let server = Server::start(MockState {
            scores: vec![probabilities],
            ..Default::default()
        })
        .await;
        let solution = server
            .solver(Duration::from_secs(5), 0)
            .with_refinement_passes(passes)
            .unwrap()
            .solve(&png(300, 300), "Select bicycles", Grid::new(3, 3).unwrap())
            .await
            .unwrap();
        assert_eq!(solution.uncertain, [1]);
        assert_eq!(solution.refinements.len(), passes);
        assert_eq!(server.state.calls.load(Ordering::SeqCst), passes + 1);
    }
}

#[tokio::test]
async fn incomplete_refinement_is_rejected_before_browser_actions() {
    let mut probabilities = scores(9, &[]);
    probabilities[0] = 0.5;
    let server = Server::start(MockState {
        scores: vec![probabilities],
        omit_refinement: true,
        ..Default::default()
    })
    .await;
    let error = server
        .solver(Duration::from_secs(5), 0)
        .solve(&png(300, 300), "Select bicycles", Grid::new(3, 3).unwrap())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("refinement answers"));
    assert_eq!(server.state.calls.load(Ordering::SeqCst), 2);
}

async fn browser_case(
    mode: &str,
    state: MockState,
    max_rounds: usize,
) -> (browser::BrowserReport, Vec<usize>) {
    browser_case_with_reloads(mode, state, max_rounds, 0).await
}

async fn browser_case_with_reloads(
    mode: &str,
    state: MockState,
    max_rounds: usize,
    max_reloads: usize,
) -> (browser::BrowserReport, Vec<usize>) {
    let server = Server::start(state).await;
    let dir = tempfile::tempdir().unwrap();
    let options = BrowserOptions {
        url: format!("{}/fixture?mode={mode}", server.url),
        webdriver_url: std::env::var("TEST_WEBDRIVER_URL").ok(),
        chrome_binary: std::env::var("TEST_CHROME_BINARY").ok(),
        headless: true,
        widget_index: 0,
        max_rounds,
        max_reloads,
        wait_timeout: Duration::from_secs(5),
        artifact_dir: dir.path().join("run"),
    };
    let report = browser::run(&server.solver(Duration::from_secs(5), 0), &options)
        .await
        .unwrap();
    assert!(options.artifact_dir.join("report.json").exists());
    assert!(options.artifact_dir.join("final.png").exists());
    let clicks = server.state.clicks.lock().unwrap().clone();
    (report, clicks)
}

#[tokio::test]
#[ignore = "requires installed Chrome; manages/downloads ChromeDriver unless TEST_WEBDRIVER_URL is set"]
async fn browser_solves_static_3x3() {
    let (report, clicks) = browser_case(
        "static",
        MockState {
            scores: vec![scores(9, &[1, 5, 9])],
            ..Default::default()
        },
        4,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Solved, "{}", report.reason);
    assert_eq!(report.token.as_deref(), Some("fixture-token"));
    assert_eq!(clicks, [1, 5, 9]);
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_solves_static_4x4() {
    let (report, clicks) = browser_case(
        "four",
        MockState {
            scores: vec![scores(16, &[1, 6, 16])],
            ..Default::default()
        },
        4,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Solved, "{}", report.reason);
    assert_eq!(clicks, [1, 6, 16]);
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_waits_for_replacements_after_animation_ends() {
    let (report, clicks) = browser_case(
        "dynamic",
        MockState {
            scores: vec![scores(9, &[1]), scores(9, &[])],
            check_visible_images: true,
            ..Default::default()
        },
        4,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Solved, "{}", report.reason);
    assert_eq!(clicks, [1]);
    assert_eq!(report.rounds.len(), 2);
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_discards_inference_if_the_grid_changes() {
    let (report, clicks) = browser_case(
        "stale",
        MockState {
            scores: vec![scores(9, &[1]), scores(9, &[2])],
            stale: true,
            ..Default::default()
        },
        4,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Solved, "{}", report.reason);
    assert_eq!(report.rounds[0].action, "discarded_stale_result");
    assert_eq!(clicks, [2]);
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_does_not_click_uncertain_tiles() {
    let (report, clicks) = browser_case(
        "static",
        MockState {
            scores: vec![vec![0.5; 9]],
            ..Default::default()
        },
        4,
    )
    .await;
    assert_eq!(
        report.status,
        BrowserStatus::NeedsReview,
        "{}",
        report.reason
    );
    assert!(clicks.is_empty());
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_stops_on_verification_rejection() {
    let (report, clicks) = browser_case(
        "reject",
        MockState {
            scores: vec![scores(9, &[1])],
            ..Default::default()
        },
        4,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Failed, "{}", report.reason);
    assert!(report.reason.contains("rejected"));
    assert_eq!(clicks, [1]);
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_recognizes_blocked_unsupported_and_already_checked_widgets() {
    for (mode, expected) in [
        ("blocked", BrowserStatus::Blocked),
        ("audio", BrowserStatus::Unsupported),
        ("checked", BrowserStatus::Solved),
    ] {
        let (report, clicks) = browser_case(mode, MockState::default(), 2).await;
        assert_eq!(report.status, expected, "{}", report.reason);
        assert!(clicks.is_empty());
        assert!(report.rounds.is_empty());
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_honors_round_limit() {
    let (report, clicks) = browser_case(
        "dynamic",
        MockState {
            scores: vec![scores(9, &[1])],
            ..Default::default()
        },
        1,
    )
    .await;
    assert_eq!(
        report.status,
        BrowserStatus::LimitReached,
        "{}",
        report.reason
    );
    assert_eq!(clicks, [1]);
    assert_eq!(report.rounds.len(), 1);
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_refines_uncertain_tiles_then_returns_token() {
    let mut initial = scores(9, &[1, 9]);
    initial[4] = 0.5;
    let (report, clicks) = browser_case(
        "static",
        MockState {
            scores: vec![initial, scores(9, &[5])],
            ..Default::default()
        },
        4,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Solved, "{}", report.reason);
    assert_eq!(report.token.as_deref(), Some("fixture-token"));
    assert_eq!(report.rounds[0].solution.refinements.len(), 1);
    assert_eq!(clicks, [1, 5, 9]);
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_reloads_unresolved_grid_without_guessing_then_solves() {
    let (report, clicks) = browser_case_with_reloads(
        "static",
        MockState {
            scores: vec![
                vec![0.5; 9],
                vec![0.5; 9],
                vec![0.5; 9],
                scores(9, &[1, 5, 9]),
            ],
            ..Default::default()
        },
        4,
        1,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Solved, "{}", report.reason);
    assert_eq!(report.reloads, 1);
    assert_eq!(report.rounds[0].action, "reload_uncertain");
    assert_eq!(clicks, [1, 5, 9]);
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_honors_reload_budget_when_uncertainty_remains() {
    let (report, clicks) = browser_case_with_reloads(
        "static",
        MockState {
            scores: vec![vec![0.5; 9]],
            ..Default::default()
        },
        5,
        1,
    )
    .await;
    assert_eq!(
        report.status,
        BrowserStatus::NeedsReview,
        "{}",
        report.reason
    );
    assert_eq!(report.reloads, 1);
    assert_eq!(report.rounds.len(), 2);
    assert!(clicks.is_empty());
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_waits_for_delayed_public_api_token() {
    let (report, _) = browser_case(
        "callback",
        MockState {
            scores: vec![scores(9, &[1, 5, 9])],
            ..Default::default()
        },
        4,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Solved, "{}", report.reason);
    assert_eq!(report.token.as_deref(), Some("fixture-callback-token"));
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_does_not_report_success_without_response_token() {
    let (report, clicks) = browser_case("missing-token", MockState::default(), 2).await;
    assert_eq!(report.status, BrowserStatus::Failed, "{}", report.reason);
    assert!(report.reason.contains("no response token"));
    assert!(report.token.is_none());
    assert!(clicks.is_empty());
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_recovers_from_rejected_selection_with_fresh_challenge() {
    let (report, clicks) = browser_case_with_reloads(
        "reject",
        MockState {
            scores: vec![scores(9, &[1]), scores(9, &[2])],
            ..Default::default()
        },
        5,
        1,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Solved, "{}", report.reason);
    assert_eq!(report.token.as_deref(), Some("fixture-token"));
    assert_eq!(report.reloads, 1);
    assert_eq!(report.rounds[0].action, "select_verify_reload_rejected");
    assert_eq!(clicks, [1, 2]);
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_waits_for_grid_motion_before_inference() {
    let (report, clicks) = browser_case(
        "motion",
        MockState {
            scores: vec![scores(9, &[1, 5, 9])],
            check_no_motion: true,
            ..Default::default()
        },
        4,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Solved, "{}", report.reason);
    assert_eq!(clicks, [1, 5, 9]);
}

#[tokio::test]
#[ignore = "requires installed Chrome"]
async fn browser_reopens_challenge_hidden_during_inference() {
    let (report, clicks) = browser_case(
        "hidden",
        MockState {
            scores: vec![scores(9, &[2]), scores(9, &[1, 5, 9])],
            stale: true,
            ..Default::default()
        },
        4,
    )
    .await;
    assert_eq!(report.status, BrowserStatus::Solved, "{}", report.reason);
    assert_eq!(report.rounds[0].action, "discarded_hidden_challenge");
    assert_eq!(clicks, [1, 5, 9]);
}
