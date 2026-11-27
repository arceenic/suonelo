use super::*;
use super::discovery::player_bundle_path;
#[cfg(target_os = "macos")]
use zeroize::Zeroizing;

#[test]
fn preview_selection_preserves_platform_policy() {
    assert_eq!(
        AccessMode::Unknown.allows_preview_selection(),
        cfg!(target_os = "macos"),
    );
    assert!(AccessMode::Preview.allows_preview_selection());
    assert!(!AccessMode::Full.allows_preview_selection());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn rejected_discovered_app_id_can_use_the_remembered_pair() {
    use std::cell::RefCell;

    let mut input = Credentials::default();
    input.token = "fixture-subscription".into();
    let remembered = AppPair {
        app_id: "987654321".into(),
        secret: "manual-fixture-key".into(),
    };
    let calls = RefCell::new(Vec::new());
    let (credentials, id) = validate_token(
        input,
        false,
        Some(&remembered),
        || async {
            Ok(Candidates {
                app_id: "123456789".into(),
                secrets: vec!["discovered-fixture-key".into()],
            })
        },
        |candidate| {
            let app_id = candidate.app_id.clone();
            let accepted = app_id == "987654321";
            if accepted {
                assert_eq!(candidate.app_secret, "manual-fixture-key");
            }
            calls.borrow_mut().push(app_id);
            std::future::ready(if accepted {
                Ok("42".into())
            } else {
                Err(QobuzError::new("account_rejected", "Qobuz rejected the token or App ID without identifying which.", false))
            })
        },
    )
    .await
    .unwrap();

    assert_eq!(id, "42");
    assert_eq!(credentials.access_mode, AccessMode::Unknown);
    assert_eq!(calls.into_inner(), ["123456789", "987654321"]);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn remembered_pair_is_not_tried_after_an_unrelated_error_or_repeated_app_id() {
    use std::cell::RefCell;

    for (first_id, remembered_id, error_code, expected_calls) in [
        ("", "987654321", "invalid_token", &["123456789"][..]),
        ("", "987654321", "provider_blocked", &["123456789"][..]),
        ("", "987654321", "rate_limit", &["123456789"][..]),
        ("", "987654321", "network", &["123456789"][..]),
        ("", "123456789", "invalid_app_id", &["123456789"][..]),
        ("987654321", "987654321", "account_rejected", &["987654321", "123456789"][..]),
    ] {
        let input = Credentials {
            token: "fixture-token".into(),
            app_id: first_id.into(),
            app_secret: if first_id.is_empty() { String::new() } else { "previous-fixture-key".into() },
            transport: TokenTransport::Query,
            access_mode: AccessMode::Unknown,
            app_pair_discovered: false,
        };
        let remembered = AppPair {
            app_id: remembered_id.into(),
            secret: "manual-fixture-key".into(),
        };
        let calls = RefCell::new(Vec::new());
        let result = validate_token(
            input,
            !first_id.is_empty(),
            Some(&remembered),
            || async { Ok(Candidates {
                app_id: "123456789".into(),
                secrets: vec!["discovered-fixture-key".into()],
            }) },
            |candidate| {
                calls.borrow_mut().push(candidate.app_id.clone());
                std::future::ready(Err(QobuzError::new(error_code, "fixture", false)))
            },
        ).await;
        assert_eq!(result.err().unwrap().code, error_code);
        assert_eq!(calls.into_inner(), expected_calls);
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn token_validation_requests_only_identity_for_fresh_manual_saved_and_reused_pairs() {
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    let api_calls = Arc::new(Mutex::new(Vec::new()));
    let player_calls = Arc::new(Mutex::new(Vec::new()));
    let mut client = QobuzClient::new().unwrap();
    let salt = "z".repeat(44);
    client.fixture = Some(Arc::new(crate::qobuz::ProviderFixture {
        api: Box::new({
            let calls = api_calls.clone();
            move |endpoint, params, credentials| {
                calls.lock().unwrap().push((endpoint, credentials.app_id.clone()));
                assert_eq!(endpoint, "user/get");
                assert!(params.is_empty(), "identity needs no legacy parameters");
                assert_eq!(credentials.transport, TokenTransport::Query);
                if credentials.app_id == "111111111" {
                    return Err(QobuzError::new("invalid_app_id", "fixture", false).into());
                }
                if credentials.token == "invalid-identity" {
                    return Ok(json!({"user":{"id":0}}));
                }
                Ok(json!({"user":{"id":"00042"}}))
            }
        }),
        player_text: Box::new({
            let calls = player_calls.clone();
            move |path| {
                calls.lock().unwrap().push(path.to_owned());
                match path {
                    "/login" => Ok(Zeroizing::new(
                        r#"<script src="/resources/1.2.3/bundle.js"></script>"#.into(),
                    )),
                    "/resources/1.2.3/bundle.js" => Ok(Zeroizing::new(format!(
                        r#"production:{{api:{{appId:"987654321"}}}},
                        a.initialSeed("dGVz",window.utimezone.paris),
                        {{name:"Europe/Paris",info:"dC1z",extras:"ZWNyZXQ={salt}"}}"#
                    ))),
                    _ => panic!("unexpected player path"),
                }
            }
        }),
    }));
    for (app_id, retry, expected_api, discovered) in [
        ("", false, &["987654321"][..], true),
        ("123456789", false, &["123456789"][..], false),
        ("111111111", true, &["111111111", "987654321"][..], true),
    ] {
        let credentials = Credentials {
            token: "fixture-token".into(),
            app_id: app_id.into(),
            app_secret: if app_id.is_empty() { String::new() } else { "fixture-key".into() },
            transport: TokenTransport::Query,
            access_mode: AccessMode::Unknown,
            app_pair_discovered: false,
        };
        let (verified, id) = client.validate_account(&credentials, retry, None).await.unwrap();
        assert_eq!(id, "42");
        assert_eq!(verified.access_mode, AccessMode::Unknown);
        if discovered {
            assert_eq!(verified.app_secret, "test-secret");
        }
        let actual = std::mem::take(&mut *api_calls.lock().unwrap());
        assert_eq!(actual.iter().map(|(_, id)| id.as_str()).collect::<Vec<_>>(), expected_api);
        let actual = std::mem::take(&mut *player_calls.lock().unwrap());
        assert_eq!(actual, if discovered { vec!["/login", "/resources/1.2.3/bundle.js"] } else { vec![] });
    }
    let mut saved = Credentials {
        token: "fixture-token".into(),
        app_id: "123456789".into(),
        app_secret: "saved-key".into(),
        transport: TokenTransport::Query,
        access_mode: AccessMode::Unknown,
        app_pair_discovered: false,
    };
    let (_, id) = client.validate_account(&saved, false, None).await.unwrap();
    assert_eq!(id, "42");
    saved.token = "invalid-identity".into();
    assert_eq!(client.validate_account(&saved, true, None).await.err().unwrap().code, "invalid_response");
    assert_eq!(api_calls.lock().unwrap().len(), 2);
    assert!(player_calls.lock().unwrap().is_empty());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn hosted_validation_prefers_a_signed_full_offer_and_revalidates_saved_preview() {
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut client = QobuzClient::new().unwrap();
    client.fixture = Some(Arc::new(crate::qobuz::ProviderFixture {
        api: Box::new({
            let calls = calls.clone();
            move |endpoint, params, credentials| {
                calls.lock().unwrap().push(endpoint);
                assert_eq!(credentials.transport, TokenTransport::HeaderOnly);
                match endpoint {
                    "user/get" => {
                        assert!(params.is_empty());
                        Ok(json!({"id":42}))
                    }
                    "track/search" => Ok(json!({"tracks":{"items":[]}})),
                    "track/getFileUrl" => {
                        let format = params.iter().find(|(key, _)| *key == "format_id").unwrap().1.parse::<u32>().unwrap();
                        let preview = credentials.app_secret == "preview-key";
                        Ok(json!({
                            "sample":preview, "format_id":format, "url":"https://example.test/offer"
                        }))
                    }
                    _ => panic!("unexpected hosted endpoint"),
                }
            }
        }),
        player_text: Box::new(|_| panic!("hosted verification uses an existing pair or fetched bundle")),
    }));
    let input = Credentials {
        token: "hosted-token".into(),
        app_id: "123456789".into(),
        app_secret: "preview-key".into(),
        transport: TokenTransport::HeaderOnly,
        access_mode: AccessMode::Unknown,
        app_pair_discovered: false,
    };
    let (verified, _) = client.validate_account(&input, false, None).await.unwrap();
    assert_eq!(verified.access_mode, AccessMode::Preview);
    assert_eq!(*calls.lock().unwrap(), ["user/get", "track/getFileUrl", "track/getFileUrl", "track/search"]);
    calls.lock().unwrap().clear();

    let (verified, _) = client.validate_with_candidates(&input, Some(Candidates {
        app_id: input.app_id.clone(),
        secrets: vec!["preview-key".into(), "full-key".into()],
    })).await.unwrap();
    assert_eq!(verified.access_mode, AccessMode::Full);
    assert_eq!(verified.app_secret, "full-key");
    assert_eq!(*calls.lock().unwrap(), ["user/get", "track/getFileUrl", "track/getFileUrl", "track/search", "track/getFileUrl"]);
}

#[tokio::test]
async fn token_setup_only_verifies_identity_and_rediscovers_at_most_once() {
    use serde_json::json;
    use std::cell::{Cell, RefCell};
    for (saved, retry, rejection, expected, discoveries, success) in [
        (false, false, None, &["987654321"][..], 1, true),
        (true, false, None, &["123456789"][..], 0, true),
        (
            true,
            true,
            Some("invalid_app_id"),
            &["123456789", "987654321"][..],
            1,
            true,
        ),
        (
            true,
            true,
            Some("account_rejected"),
            &["123456789", "987654321"][..],
            1,
            false,
        ),
        (
            true,
            true,
            Some("invalid_token"),
            &["123456789"][..],
            0,
            false,
        ),
        (
            true,
            true,
            Some("provider_blocked"),
            &["123456789"][..],
            0,
            false,
        ),
        (true, true, Some("network"), &["123456789"][..], 0, false),
        (true, true, Some("rate_limit"), &["123456789"][..], 0, false),
        (
            true,
            false,
            Some("invalid_app_id"),
            &["123456789"][..],
            0,
            false,
        ),
    ] {
        let mut input = Credentials::default();
        input.token = "test-token".into();
        if saved {
            input.app_id = "123456789".into();
            input.app_secret = "saved-key".into();
        }
        let discovered = Cell::new(0);
        let observed = RefCell::new(Vec::new());
        let result = validate_token(
            input,
            retry,
            #[cfg(target_os = "macos")]
            None,
            || async {
                discovered.set(discovered.get() + 1);
                Ok(Candidates {
                    app_id: "987654321".into(),
                    secrets: vec!["new-key".into()],
                })
            },
            |credential| {
                observed.borrow_mut().push(credential.app_id.clone());
                let result = match rejection {
                    Some("invalid_app_id") if retry && credential.app_id == "987654321" => {
                        verified_account_id(&json!({"user":{"id":"00042"}}))
                    }
                    Some(code) => Err(QobuzError::new(code, "fixture", false)),
                    None => verified_account_id(&json!({"id":42})),
                };
                std::future::ready(result)
            },
        )
        .await;
        assert_eq!(discovered.get(), discoveries);
        assert_eq!(observed.into_inner(), expected);
        assert_eq!(result.is_ok(), success);
        if let Ok((credentials, id)) = result {
            assert_eq!(id, "42");
            assert_eq!(credentials.access_mode, AccessMode::Unknown);
        }
    }
    let mut partial = Credentials::default();
    partial.token = "test-token".into();
    partial.app_id = "123456789".into();
    let result = validate_token(
        partial,
        false,
        #[cfg(target_os = "macos")]
        None,
        || async { panic!("partial pair cannot discover") },
        |_| async { panic!("partial pair cannot verify identity") },
    )
    .await;
    assert_eq!(result.err().unwrap().code, "incomplete_app_pair");
}

#[test]
fn account_identity_does_not_depend_on_legacy_entitlement_metadata() {
    let account = serde_json::json!({ "user": { "id": 123 } });
    assert_eq!(verified_account_id(&account).unwrap(), "123");
    assert!(verified_account_id(&serde_json::json!({})).is_err());
}

#[test]
fn finds_production_id_and_assembles_only_complete_candidates() {
    let salt = "z".repeat(44);
    let bundle = format!(
        r#"
        staging:{{api:{{appId:"111111111"}}}}, production:{{api:{{appId:"123456789"}}}},
        a.initialSeed("bad",window.utimezone.oslo),
        b.initialSeed("dGVz",window.utimezone.paris),
        {{name:"Europe/Paris",info:"dC1z",extras:"ZWNyZXQ={salt}"}}
    "#
    );
    let candidates = parse_player_bundle(&bundle).expect("discovery succeeds");
    assert_eq!(candidates.app_id, "123456789");
    assert_eq!(candidates.secrets, ["test-secret"]);
}

#[test]
fn requires_one_distinct_well_formed_production_id_and_deduplicates_keys() {
    let salt = "z".repeat(44);
    let parts = format!(
        r#"
        a.initialSeed("dGVz",window.utimezone.PARIS);
        b.initialSeed("dGVz",window.utimezone.London);
        c.initialSeed("a",window.utimezone.oslo);
        d.initialSeed("bad-",window.utimezone.berlin);
        {{name:"Europe/Paris",info:"dC1z",extras:"ZWNyZXQ={salt}"}}
        {{name:"Europe/LONDON",info:"dC1z",extras:"ZWNyZXQ{salt}"}}
        {{name:"Europe/Oslo",info:"b",extras:"c"}}
        {{name:"Europe/Berlin",info:"YWJj",extras:"eHk{salt}"}}
    "#
    );
    let valid = r#"production:{api:{appId:"123456789"}}"#;
    let duplicate = r#"'production' : { "api" : { 'appId' : '123456789', other: 1 } }"#;
    let candidates = parse_player_bundle(&format!("{valid},{duplicate}{parts}")).unwrap();
    assert_eq!(candidates.secrets, ["test-secret"]);
    assert!(
        parse_player_bundle(&format!(r#"staging:{{api:{{appId:"123456789"}}}}{parts}"#))
            .is_none()
    );
    for extra in [
        r#"production:{api:{appId:"987654321"}}"#,
        r#"production:{api:{appId:"malformed"}}"#,
        r#"production:{api:{appId:123456789}}"#,
    ] {
        assert!(parse_player_bundle(&format!("{valid},{extra}{parts}")).is_none());
    }
    let only_unpadded = format!(
        r#"{valid}
        a.initialSeed("dGVz",window.utimezone.LONDON);
        {{name:"Europe/London",info:"dC1z",extras:"ZWNyZXQ{salt}"}}
    "#
    );
    assert_eq!(
        parse_player_bundle(&only_unpadded).unwrap().secrets,
        ["test-secret"]
    );
}

#[test]
fn only_allows_normalized_player_bundle_paths() {
    let html = r#"<script async src='/resources/9.12-a/ab/bundle.js'></script>"#;
    assert_eq!(
        player_bundle_path(html),
        Some("/resources/9.12-a/ab/bundle.js")
    );
    for path in [
        "//other.test/resources/v1/bundle.js",
        "https://other.test/resources/v1/bundle.js",
        "https://user@play.qobuz.com/resources/v1/bundle.js",
        "/resources/../bundle.js",
        "/resources/%2e%2e/bundle.js",
        "/resources/v1/bundle.js?query=1",
        "/unknown/v1/bundle.js",
        "/resources/bundle.js",
    ] {
        assert!(player_bundle_path(&format!(r#"<script src="{path}"></script>"#)).is_none());
    }
    assert!(
        player_bundle_path(r#"<script data-src="/resources/v1/bundle.js"></script>"#).is_none()
    );
}

#[test]
fn distinguishes_full_streams_and_explicit_mp3_previews() {
    use serde_json::json;
    let fail = |code| Err(QobuzError::new(code, "synthetic upstream text", true).into());
    assert_eq!(
        verify_discovered_offer(fail("invalid_app_pair"), 5).unwrap(),
        SigningProbe::Rejected
    );
    let valid =
        json!({"sample":false,"format_id":5,"url":"https://example.test/stream?sig=fixture"});
    assert_eq!(
        verify_discovered_offer(Ok(valid.clone()), 5).unwrap(),
        SigningProbe::Verified
    );
    let cd =
        json!({"sample":false,"format_id":6,"url":"https://example.test/stream?sig=fixture"});
    assert_eq!(
        verify_discovered_offer(Ok(cd), 6).unwrap(),
        SigningProbe::Verified
    );
    let mut preview = valid.clone();
    preview["sample"] = json!(true);
    assert_eq!(
        verify_discovered_offer(Ok(preview.clone()), 5).unwrap(),
        SigningProbe::Preview
    );
    assert_eq!(
        verify_discovered_offer(Ok(preview), 6).unwrap(),
        SigningProbe::Unavailable
    );
    for (field, value) in [
        ("sample", Value::Null),
        ("format_id", json!(6)),
        ("url", Value::Null),
        ("url", json!("http://example.test/stream")),
        ("url", json!("https://username@example.test/stream")),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert_eq!(
            verify_discovered_offer(Ok(invalid), 5).unwrap(),
            SigningProbe::Unavailable
        );
    }
    for code in [
        "resource_unavailable",
        "quality_unavailable",
        "invalid_response",
    ] {
        assert_eq!(
            verify_discovered_offer(fail(code), 5).unwrap(),
            SigningProbe::Unavailable
        );
    }
    assert_eq!(
        verify_discovered_offer(fail("qobuz_request"), 5)
            .unwrap_err()
            .code,
        "signing_inconclusive"
    );
    for code in ["rate_limit", "api_timeout"] {
        let failure = verify_discovered_offer(fail(code), 5).unwrap_err();
        assert_eq!(failure.code, code);
        assert!(!failure.message.contains("synthetic upstream text"));
    }
}

#[tokio::test]
async fn unavailable_initial_track_retries_catalog_tracks_until_a_full_offer() {
    use serde_json::json;
    use std::cell::RefCell;

    let tracks = json!({"tracks":{"items":[
        {"id":5966783,"streamable":true},
        {"id":91,"streamable":false},
        {"id":0,"streamable":true},
        {"id":"x","streamable":true},
        {"id":42,"streamable":true},
        {"id":"42","streamable":true},
        {"id":43,"streamable":true},
        {"id":44,"streamable":true}
    ]}});
    let mut attempts = SigningProbes::new();
    let probes = RefCell::new(Vec::new());
    let outcome = probe_candidate(
        &mut attempts,
        |track: String, format_id| {
            probes.borrow_mut().push((track.clone(), format_id));
            async move {
                let offer = json!({
                    "sample": !(track == "42" && format_id == 6),
                    "format_id":format_id,
                    "url":"https://example.test/stream?sig=fixture"
                });
                verify_discovered_offer(Ok(offer), format_id)
            }
        },
        || async { Ok(alternate_track_ids(&tracks)) },
    )
    .await
    .unwrap();
    assert_eq!(outcome, SigningProbe::Verified);
    assert_eq!(attempts.alternatives.unwrap(), ["42", "43"]);
    assert_eq!(
        probes.into_inner(),
        [
            (PROBE_TRACK.to_owned(), 5),
            (PROBE_TRACK.to_owned(), 6),
            ("42".to_owned(), 5),
            ("42".to_owned(), 6)
        ]
    );
    assert_eq!(attempts.remaining, SIGNING_PROBE_LIMIT - 4);
}

#[tokio::test]
async fn uses_verified_mp3_preview_after_exhausting_full_offer_alternatives() {
    use serde_json::json;

    let mut probes = SigningProbes::new();
    let outcome = probe_candidate(
        &mut probes,
        |track, format_id| async move {
            let response = if track == PROBE_TRACK {
                json!({})
            } else {
                json!({"sample":true,"format_id":format_id,"url":"https://example.test/preview"})
            };
            verify_discovered_offer(Ok(response), format_id)
        },
        || async { Ok(vec!["42".to_owned(), "43".to_owned()]) },
    )
    .await
    .unwrap();
    assert_eq!(outcome, SigningProbe::Preview);
    assert_eq!(probes.remaining, SIGNING_PROBE_LIMIT - 6);
}

#[tokio::test]
async fn hosted_candidates_share_sixteen_probes_and_two_alternate_tracks() {
    use std::cell::{Cell, RefCell};

    let tracks = serde_json::json!({"tracks":{"items":[
        {"id":42,"streamable":true}, {"id":43,"streamable":true},
        {"id":44,"streamable":true}
    ]}});
    let calls = RefCell::new(Vec::new());
    let searches = Cell::new(0);
    let mut probes = SigningProbes::new();
    for key in 0..4 {
        assert_eq!(
            probe_candidate(
                &mut probes,
                |track, format| {
                    calls.borrow_mut().push((key, track, format));
                    std::future::ready(Ok(SigningProbe::Unavailable))
                },
                || {
                    searches.set(searches.get() + 1);
                    std::future::ready(Ok(alternate_track_ids(&tracks)))
                },
            )
            .await
            .unwrap(),
            SigningProbe::Unavailable
        );
    }

    assert_eq!(searches.get(), 1);
    assert_eq!(probes.remaining, 0);
    let calls = calls.into_inner();
    assert_eq!(calls.len(), 16);
    for (index, (key, track, format)) in calls.iter().enumerate() {
        assert_eq!(*key, index / 6);
        assert_eq!(track, [PROBE_TRACK, "42", "43"][index % 6 / 2]);
        assert_eq!(*format, [5, 6][index % 2]);
    }
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
#[ignore = "exercises the real 45-second validation deadline"]
async fn validation_times_out_at_forty_five_seconds() {
    let start = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(50),
        validation_deadline(std::future::pending::<Result<(), QobuzError>>()),
    )
    .await
    .expect("validation must end before the guard timeout");
    assert_eq!(result.unwrap_err().code, "validation_timeout");
    assert!(start.elapsed() >= Duration::from_secs(45));
}

#[cfg(target_os = "macos")]
#[tokio::test(start_paused = true)]
async fn rediscovery_shares_the_original_forty_five_second_deadline() {
    use std::cell::Cell;

    let input = Credentials {
        token: "test-token".into(),
        app_id: "123456789".into(),
        app_secret: "fixture-key".into(),
        transport: TokenTransport::Query,
        access_mode: AccessMode::Unknown,
        app_pair_discovered: false,
    };
    let identity_calls = Cell::new(0);
    let discovery_calls = Cell::new(0);
    let start = tokio::time::Instant::now();
    let result = validation_deadline(validate_token(
        input,
        true,
        None,
        || async {
            discovery_calls.set(discovery_calls.get() + 1);
            tokio::time::sleep(Duration::from_secs(20)).await;
            Ok(Candidates { app_id: "987654321".into(), secrets: vec!["fresh-key".into()] })
        },
        |_| async {
            identity_calls.set(identity_calls.get() + 1);
            tokio::time::sleep(Duration::from_secs(30)).await;
            Err(QobuzError::new("invalid_app_id", "fixture", false))
        },
    )).await;
    assert_eq!(result.err().unwrap().code, "validation_timeout");
    assert_eq!(start.elapsed(), Duration::from_secs(45));
    assert_eq!((identity_calls.get(), discovery_calls.get()), (1, 1));
}

#[tokio::test]
async fn a_definite_rejection_or_throttle_does_not_probe_other_tracks() {
    for failure_code in ["invalid_app_pair", "rate_limit"] {
        let mut probes = SigningProbes::new();
        let result = probe_candidate(
            &mut probes,
            |track: String, format_id| async move {
                assert_eq!((track.as_str(), format_id), (PROBE_TRACK, 5));
                verify_discovered_offer(
                    Err(QobuzError::new(failure_code, "fixture", false).into()),
                    format_id,
                )
            },
            || async { Err(signing_inconclusive()) },
        )
        .await;
        if failure_code == "invalid_app_pair" {
            assert_eq!(result.unwrap(), SigningProbe::Rejected);
        } else {
            assert_eq!(result.unwrap_err().code, failure_code);
        }
        assert_eq!(probes.remaining, SIGNING_PROBE_LIMIT - 1);
    }
}
