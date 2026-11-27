#[cfg(windows)]
mod windows_tests {
    use super::super::{
        CredentialInput, REUSE_WINDOW_MS, SecretRecord, Vault,
        windows::{EMAIL_RECORD_MAGIC, protect},
    };
    use crate::qobuz::{AccessMode, Credentials, TokenTransport};
    use std::fs;
    use zeroize::Zeroizing;

    fn token_input(token: &str, force_discovery: bool) -> CredentialInput {
        CredentialInput {
            token: token.into(),
            app_id: None,
            secret: None,
            force_discovery,
        }
    }

    #[test]
    fn replacement_reuse_includes_exactly_twelve_hours() {
        let mut saved = Credentials::default();
        saved.token = "saved".into();
        saved.app_id = "123456789".into();
        saved.app_secret = "saved-key".into();
        let start = 1_700_000_000_000;
        for (offset, reuse) in [
            (43_199_000, true),
            (43_200_000, true),
            (43_200_001, false),
            (43_201_000, false),
        ] {
            let (selected, retry) = token_input("replacement", false)
                .prepare(Some(&saved), Some(start), Some(start + offset))
                .unwrap();
            assert_eq!(!selected.app_id.is_empty(), reuse);
            assert_eq!(retry, reuse);
            assert_eq!(selected.token, "replacement");
        }
        for stamp in [None, Some(0), Some(start + 1)] {
            let (selected, retry) = token_input("replacement", false)
                .prepare(Some(&saved), stamp, Some(start))
                .unwrap();
            assert!(selected.app_id.is_empty());
            assert!(!retry);
        }
        let (same, retry) = token_input("saved", false)
            .prepare(Some(&saved), Some(start), Some(start + REUSE_WINDOW_MS + 1))
            .unwrap();
        assert_eq!(same.app_id, "123456789");
        assert!(!retry);
        let (fresh, retry) = token_input("saved", true)
            .prepare(Some(&saved), Some(start), Some(start))
            .unwrap();
        assert_eq!(fresh.token, "saved");
        assert!(fresh.app_id.is_empty());
        assert!(!retry);
    }

    #[test]
    fn token_commit_timestamp_survives_restart_and_same_token_pair_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.vault");
        let mut vault = Vault::new(path.clone());
        assert!(vault.load_error().is_none());
        let mut input = Credentials::default();
        input.token = "first".into();
        input.app_id = "123456789".into();
        input.app_secret = "test-key".into();
        vault
            .save_at(input.clone(), "42".into(), Some(1000))
            .unwrap();
        let mut vault = Vault::new(path.clone());
        assert_eq!(
            vault.record.as_ref().unwrap().token_committed_at_ms,
            Some(1000)
        );
        input.app_id = "987654321".into();
        vault
            .save_at(input.clone(), "42".into(), Some(2000))
            .unwrap();
        assert_eq!(
            vault.record.as_ref().unwrap().token_committed_at_ms,
            Some(1000)
        );
        input.token = "second".into();
        vault.save_at(input, "42".into(), Some(3000)).unwrap();
        assert_eq!(
            Vault::new(path).record.unwrap().token_committed_at_ms,
            Some(3000)
        );
    }

    #[test]
    fn email_vault_is_header_only_and_older_reader_rejects_its_payload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.vault");
        let mut vault = Vault::new(path.clone());
        let credentials = Credentials {
            token: "fixture-session".into(),
            app_id: "123456789".into(),
            app_secret: "fixture-key".into(),
            transport: TokenTransport::HeaderOnly,
            access_mode: AccessMode::Preview,
        };
        vault.save(credentials.clone(), "123".into()).unwrap();
        let mut encrypted = fs::read(&path).unwrap();
        let bytes = protect(&mut encrypted, false).unwrap();
        assert!(serde_json::from_slice::<SecretRecord>(&bytes).is_err());
        assert!(bytes.starts_with(EMAIL_RECORD_MAGIC));
        let reloaded = Vault::new(path.clone());
        assert_eq!(
            reloaded.credentials().unwrap().access_mode,
            AccessMode::Unknown
        );
        assert_eq!(
            reloaded.credentials().unwrap().transport,
            TokenTransport::HeaderOnly
        );
        let retry = CredentialInput {
            token: String::new(),
            app_id: None,
            secret: None,
            force_discovery: false,
        };
        assert_eq!(
            reloaded.prepare_token(retry).unwrap().0.transport,
            TokenTransport::HeaderOnly
        );
        let update = CredentialInput {
            token: String::new(),
            app_id: Some("123456789".into()),
            secret: Some("fixture-key".into()),
            force_discovery: false,
        };
        assert!(reloaded.prepare_token(update).is_err());

        // Removing or changing the envelope cannot convert a protected email token to legacy URL transport.
        let mut payload = Zeroizing::new(bytes[EMAIL_RECORD_MAGIC.len()..].to_vec());
        fs::write(&path, &*protect(&mut payload, true).unwrap()).unwrap();
        assert!(Vault::new(path).credentials().is_none());
    }

    #[test]
    fn explicit_token_discovery_does_not_reuse_the_saved_app_pair() {
        let saved = Credentials {
            token: "saved".into(),
            app_id: "123456789".into(),
            app_secret: "saved-secret".into(),
            transport: TokenTransport::Query,
            access_mode: AccessMode::Unknown,
        };
        let explicit = |token: &str, app_id: Option<&str>, secret: Option<&str>| CredentialInput {
            token: token.into(),
            app_id: app_id.map(str::to_owned),
            secret: secret.map(str::to_owned),
            force_discovery: false,
        };
        let fresh = explicit("new", None, None)
            .prepare(Some(&saved), None, None)
            .map(|(value, _)| value)
            .unwrap();
        assert!(fresh.app_id.is_empty() && fresh.app_secret.is_empty());
        let retry = explicit("", None, None)
            .prepare(Some(&saved), None, None)
            .map(|(value, _)| value)
            .unwrap();
        assert!(retry == saved);
        let manual = explicit("new", Some("987654321"), Some("manual"))
            .prepare(Some(&saved), None, None)
            .map(|(value, _)| value)
            .unwrap();
        assert_eq!(manual.app_id, "987654321");
        assert_eq!(manual.app_secret, "manual");
        assert!(
            explicit("new", Some("987654321"), None)
                .prepare(Some(&saved), None, None)
                .map(|(value, _)| value)
                .is_err()
        );
    }

    #[test]
    fn preserves_legacy_record_until_replaced_and_round_trips_dpapi() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("credentials.vault");
        let legacy = b"QBDLXVT1 encrypted legacy record";
        fs::write(&path, legacy).unwrap();
        let mut vault = Vault::new(path.clone());
        assert!(vault.credentials().is_none());
        assert!(vault.replacement_reason().is_some());
        assert_eq!(fs::read(&path).unwrap(), legacy);
        let credentials = Credentials {
            token: "example-sensitive-token".into(),
            app_id: "000000001".into(),
            app_secret: "example-sensitive-secret".into(),
            transport: TokenTransport::Query,
            access_mode: AccessMode::Unknown,
        };
        let key = vault.save(credentials.clone(), "123".into()).unwrap();
        let encrypted = fs::read(&path).unwrap();
        assert!(
            !encrypted
                .windows(credentials.token.len())
                .any(|bytes| bytes == credentials.token.as_bytes())
        );

        let mut restarted = Vault::new(path.clone());
        assert_eq!(restarted.credentials().unwrap().token, credentials.token);
        assert_eq!(restarted.account_id(), Some("123"));
        assert_eq!(restarted.save(credentials, "123".into()).unwrap(), key);
        restarted.reset().unwrap();
        let signed_out = Vault::new(path);
        assert!(!signed_out.has_saved_credentials());
        assert!(signed_out.credentials().is_none());
        assert_eq!(signed_out.account_id(), Some("123"));
    }
}

#[cfg(target_os = "macos")]
mod token_tests {
    use super::super::{
        CredentialInput, REUSE_WINDOW_MS, SecretRecord, Vault,
        macos::{
            KEYCHAIN_RECORD_MAGIC, decode_keychain_record, encode_keychain_record, keychain_error,
            vault_io,
        },
    };
    use crate::qobuz::{AccessMode, Credentials, TokenTransport};

    const COMMITTED: u64 = 1_000_000;
    const NOW: u64 = COMMITTED + REUSE_WINDOW_MS;

    fn input(token: &str) -> CredentialInput {
        CredentialInput {
            token: token.into(),
            app_id: None,
            secret: None,
            force_discovery: false,
            remember_saved_app_pair: false,
        }
    }

    fn pair(token: &str, transport: TokenTransport) -> Credentials {
        Credentials {
            token: token.into(),
            app_id: "123456789".into(),
            app_secret: "fixture-key".into(),
            transport,
            access_mode: AccessMode::Unknown,
            app_pair_discovered: false,
        }
    }

    #[test]
    fn replacement_reuse_is_inclusive_and_rejects_unknown_or_future_timestamps() {
        let saved = pair("saved-token", TokenTransport::Query);
        for (committed, elapsed, reuse) in [
            (Some(COMMITTED), Some(NOW - 1_000), true),
            (Some(COMMITTED), Some(NOW), true),
            (Some(COMMITTED), Some(NOW + 1_000), false),
            (Some(COMMITTED), None, false),
            (None, Some(NOW), false),
            (Some(0), Some(NOW), false),
            (Some(NOW + 1), Some(NOW), false),
            (Some(u64::MAX), Some(NOW), false),
        ] {
            let (selected, retry) = input(" replacement-token ")
                .prepare(Some(&saved), committed, elapsed)
                .unwrap();
            assert_eq!(selected.token, "replacement-token");
            assert_eq!(
                (
                    !selected.app_id.is_empty(),
                    !selected.app_secret.is_empty(),
                    retry
                ),
                (reuse, reuse, reuse)
            );
        }

        let (same, retry) = input(" saved-token ")
            .prepare(Some(&saved), None, Some(NOW))
            .unwrap();
        assert!(same == saved && !retry);
        let (empty, retry) = input("").prepare(Some(&saved), None, Some(NOW)).unwrap();
        assert!(empty == saved && !retry);
        let (fresh, retry) = input("first-token")
            .prepare(None, None, Some(NOW))
            .unwrap();
        assert!(fresh.app_id.is_empty() && fresh.app_secret.is_empty() && !retry);
    }

    #[test]
    fn manual_pair_has_priority_and_forced_discovery_ignores_manual_drafts() {
        let saved = pair("saved-token", TokenTransport::Query);
        let mut draft = input("new-token");
        draft.app_id = Some(" 987654321 ".into());
        assert_eq!(
            draft
                .prepare(Some(&saved), Some(COMMITTED), Some(NOW))
                .err()
                .unwrap()
                .code,
            "incomplete_app_pair"
        );
        draft.secret = Some(" manual-key ".into());
        let (manual, retry) = draft
            .prepare(Some(&saved), Some(COMMITTED), Some(NOW))
            .unwrap();
        assert_eq!(
            (manual.app_id.as_str(), manual.app_secret.as_str(), retry),
            ("987654321", "manual-key", false)
        );

        draft.app_id = Some("not-an-id".into());
        assert_eq!(
            draft
                .prepare(Some(&saved), Some(COMMITTED), Some(NOW))
                .err()
                .unwrap()
                .code,
            "invalid_app_id"
        );
        draft.force_discovery = true;
        let (forced, retry) = draft
            .prepare(Some(&saved), Some(COMMITTED), Some(NOW))
            .unwrap();
        assert!(forced.app_id.is_empty() && forced.app_secret.is_empty() && !retry);
        let mut blank = input("");
        blank.app_id = Some("unsubmitted-draft".into());
        blank.force_discovery = true;
        assert_eq!(
            blank.prepare(Some(&saved), None, None).unwrap().0.token,
            "saved-token"
        );
        for token in ["bad token".to_owned(), "t".repeat(4097)] {
            assert_eq!(
                input(&token)
                    .prepare(None, None, None)
                    .err()
                    .unwrap()
                    .code,
                "invalid_token"
            );
        }
    }

    #[test]
    fn saved_hosted_token_cannot_be_converted_to_query_transport() {
        let saved = pair("hosted-token", TokenTransport::HeaderOnly);
        for token in ["", " hosted-token "] {
            let mut draft = input(token);
            let (reused, retry) = draft.prepare(Some(&saved), None, None).unwrap();
            assert!(reused == saved && !retry);
            draft.app_id = Some("987654321".into());
            draft.secret = Some("manual-key".into());
            assert!(draft.prepare(Some(&saved), None, None).is_err());
            draft.force_discovery = true;
            assert!(draft.prepare(Some(&saved), None, None).is_err());
        }
        let (different, retry) = input("different-token")
            .prepare(Some(&saved), Some(COMMITTED), Some(NOW))
            .unwrap();
        assert_eq!(different.transport, TokenTransport::Query);
        assert!(!retry && different.app_id.is_empty());
    }

    #[test]
    fn remembers_a_legacy_pair_in_keychain_without_changing_the_token_commit_time() {
        let original = pair("legacy-token", TokenTransport::Query);
        let mut vault = Vault::new("fixture".into());
        let fingerprint = vault
            .save_with(original, "42".into(), false, Some(COMMITTED), |_, _| Ok(()))
            .unwrap();
        assert!(vault.can_remember_saved_app_pair());

        let mut adopt = input("");
        adopt.remember_saved_app_pair = true;
        let prepared = vault.prepare_token_at(adopt, Some(NOW)).unwrap();
        assert!(prepared.remember_pair && prepared.remembered_fallback.is_none());
        let mut protected = None;
        let adopted = vault
            .save_with(
                prepared.credentials,
                "42".into(),
                true,
                Some(NOW),
                |_, record| {
                    protected = Some(encode_keychain_record(record)?);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(adopted, fingerprint);
        vault.record = decode_keychain_record(&protected.unwrap());
        assert_eq!(
            vault.record.as_ref().unwrap().token_committed_at_ms,
            Some(COMMITTED)
        );
        assert!(vault.has_remembered_app_pair());
        assert!(!vault.can_remember_saved_app_pair());

        let same = vault
            .prepare_token_at(input("legacy-token"), Some(NOW + 1))
            .unwrap();
        assert!(same.remembered_fallback.is_none() && !same.retry_reused_pair);
        let replacement = vault
            .prepare_token_at(input("replacement"), Some(NOW + 1))
            .unwrap();
        assert!(replacement.credentials.app_id.is_empty());
        assert!(
            replacement
                .remembered_fallback
                .as_ref()
                .is_some_and(|pair| pair.matches(vault.credentials().unwrap()))
        );
        let mut forced = input("legacy-token");
        forced.force_discovery = true;
        assert!(
            vault
                .prepare_token_at(forced, Some(NOW))
                .unwrap()
                .remembered_fallback
                .is_some()
        );
    }

    #[test]
    fn protects_the_remembered_pair_across_account_changes_and_failed_writes() {
        let mut vault = Vault::new("fixture".into());
        let mut manual = input("original");
        manual.app_id = Some("123456789".into());
        manual.secret = Some("fixture-key".into());
        let prepared = vault.prepare_token(manual).unwrap();
        assert!(prepared.remember_pair && prepared.remembered_fallback.is_none());
        vault
            .save_with(
                prepared.credentials,
                "42".into(),
                true,
                Some(COMMITTED),
                |_, _| Ok(()),
            )
            .unwrap();

        let mut replacement = pair("replacement", TokenTransport::Query);
        replacement.app_id = "987654321".into();
        assert!(
            vault
                .save_with(replacement.clone(), "43".into(), true, Some(NOW), |_, _| {
                    Err(vault_io())
                })
                .is_err()
        );
        assert_eq!(vault.account_id(), Some("42"));
        assert!(
            vault
                .remembered_app_pair()
                .unwrap()
                .matches(vault.credentials().unwrap())
        );

        vault
            .save_with(replacement, "43".into(), false, Some(NOW), |_, _| Ok(()))
            .unwrap();
        assert_eq!(vault.account_id(), Some("43"));
        assert!(
            !vault
                .remembered_app_pair()
                .unwrap()
                .matches(vault.credentials().unwrap())
        );
        vault
            .save_with(
                pair("hosted-token", TokenTransport::HeaderOnly),
                "43".into(),
                false,
                Some(NOW),
                |_, _| Ok(()),
            )
            .unwrap();
        let hosted = vault.prepare_token(input("")).unwrap();
        assert_eq!(hosted.credentials.transport, TokenTransport::HeaderOnly);
        assert!(hosted.remembered_fallback.is_none());
        let next = vault.prepare_token(input("next-token")).unwrap();
        assert!(next.remembered_fallback.as_ref().is_some_and(|remembered| {
            remembered.matches(&pair("ignored", TokenTransport::Query))
        }));
    }

    #[test]
    fn keychain_metadata_write_preserves_legacy_and_current_token_timestamps() {
        for timestamp in [None, Some(COMMITTED)] {
            let mut previous = pair("saved-token", TokenTransport::Query);
            previous.app_secret = " fixture-key ".into();
            let bytes = encode_keychain_record(&SecretRecord {
                fingerprint_key: [7; 32],
                credentials: previous,
                account_id: Some("42".into()),
                token_committed_at_ms: timestamp,
                remembered_app_pair: None,
            })
            .unwrap();
            let mut vault = Vault::new("fixture".into());
            vault.record = decode_keychain_record(&bytes);
            let verified = pair("saved-token", TokenTransport::Query);
            let mut stored = None;
            let fingerprint = vault
                .save_with(verified.clone(), "42".into(), false, Some(NOW), |_, record| {
                    stored = Some(encode_keychain_record(record)?);
                    Ok(())
                })
                .unwrap();
            let stored = stored.expect("normalized, verified metadata must be persisted");
            let restored = decode_keychain_record(&stored).unwrap();
            assert_eq!(restored.fingerprint_key, [7; 32]);
            assert_eq!(restored.token_committed_at_ms, timestamp);
            assert!(restored.credentials == verified);

            vault.record = Some(restored);
            assert_eq!(
                vault
                    .save_with(verified, "42".into(), false, Some(NOW), |_, _| {
                        panic!("identical verified record should not be rewritten")
                    })
                    .unwrap(),
                fingerprint
            );
        }
    }

    #[test]
    fn keychain_timestamp_is_written_only_with_a_successful_token_commit() {
        let mut vault = Vault::new("fixture".into());
        let mut writes = 0;
        let mut check_write = |_: &Vault, record: &SecretRecord| {
            let stored = encode_keychain_record(record)?;
            let decoded = decode_keychain_record(&stored).unwrap();
            assert_eq!(decoded.credentials.access_mode, AccessMode::Unknown);
            writes += 1;
            Ok(())
        };
        vault
            .save_with(
                pair("first-token", TokenTransport::Query),
                "42".into(),
                false,
                Some(COMMITTED),
                &mut check_write,
            )
            .unwrap();
        assert_eq!(
            vault.record.as_ref().unwrap().token_committed_at_ms,
            Some(COMMITTED)
        );

        let replacement = pair("next-token", TokenTransport::Query);
        let error = vault
            .save_with(replacement.clone(), "43".into(), false, Some(NOW), |_, _| {
                Err(vault_io())
            })
            .unwrap_err();
        assert_eq!(error.code, vault_io().code);
        assert_eq!(vault.credentials().unwrap().token, "first-token");
        assert_eq!(vault.account_id(), Some("42"));
        assert_eq!(
            vault.record.as_ref().unwrap().token_committed_at_ms,
            Some(COMMITTED)
        );

        vault
            .save_with(replacement, "43".into(), false, Some(NOW), &mut check_write)
            .unwrap();
        assert_eq!(
            vault.record.as_ref().unwrap().token_committed_at_ms,
            Some(NOW)
        );
        let mut hosted = pair("hosted-token", TokenTransport::HeaderOnly);
        hosted.access_mode = AccessMode::Preview;
        vault
            .save_with(hosted, "43".into(), false, Some(NOW + 1), &mut check_write)
            .unwrap();
        assert_eq!(vault.record.as_ref().unwrap().token_committed_at_ms, None);
        assert_eq!(writes, 3);
    }

    #[test]
    fn keychain_ignores_unusable_timestamp_values_without_dropping_the_record() {
        let mut data = serde_json::json!({
            "fingerprint_key": vec![0; 32],
            "credentials": pair("old-token", TokenTransport::Query),
            "account_id": "42",
        });
        for value in [
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!("unexpected"),
            serde_json::json!({"wrong": 1}),
            serde_json::json!(1.5),
        ] {
            data["token_committed_at_ms"] = value;
            let mut bytes = KEYCHAIN_RECORD_MAGIC.to_vec();
            serde_json::to_writer(&mut bytes, &data).unwrap();
            assert_eq!(
                decode_keychain_record(&bytes)
                    .unwrap()
                    .token_committed_at_ms,
                None
            );
        }
        data["credentials"] = serde_json::to_value(Credentials::default()).unwrap();
        data.as_object_mut()
            .unwrap()
            .remove("token_committed_at_ms");
        let mut bytes = KEYCHAIN_RECORD_MAGIC.to_vec();
        serde_json::to_writer(&mut bytes, &data).unwrap();
        let signed_out = decode_keychain_record(&bytes).unwrap();
        assert!(signed_out.credentials.token.is_empty());
        assert_eq!(signed_out.account_id.as_deref(), Some("42"));
        assert_eq!(signed_out.token_committed_at_ms, None);
    }

    #[test]
    fn keychain_access_denial_blocks_replacement_and_removal() {
        let mut vault = Vault::new("fixture".into());
        vault.load_error = Some(keychain_error(-25293));
        let candidate = vault
            .prepare_token(input("new-token"))
            .unwrap()
            .credentials;
        assert_eq!(
            vault
                .save_with(candidate, "42".into(), false, Some(NOW), |_, _| {
                    panic!("access denial must block the write")
                })
                .unwrap_err()
                .code,
            "vault_access"
        );
        assert_eq!(vault.reset().unwrap_err().code, "vault_access");
        assert!(vault.record.is_none());
    }
}
