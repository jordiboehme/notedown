use super::*;
use std::cell::Cell;

const HOUR: u64 = 60 * 60 * 1000;
const NOW: u64 = 1_763_400_000_000;

/// A fake update handle: its version.
type Fake = String;

fn info(version: &str) -> UpdateInfo {
    UpdateInfo {
        version: version.into(),
        notes: Some(format!("# Notedown {version}")),
    }
}

fn found(version: &str) -> Checked<Fake> {
    Ok(Some((info(version), version.to_owned())))
}

/// Updates that log every state they tell.
fn updates() -> (Updates<Fake>, Arc<Mutex<Vec<UpdateState>>>) {
    let told = Arc::new(Mutex::new(Vec::new()));
    let log = told.clone();
    let updates = Updates::new(Arc::new(move |state: &UpdateState| {
        log.lock().unwrap().push(state.clone());
    }));
    (updates, told)
}

#[test]
fn should_check_once_a_day() {
    // Never checked: at once.
    assert!(should_check(None, NOW));
    assert!(!should_check(Some(NOW), NOW));
    assert!(!should_check(Some(NOW - HOUR), NOW));
    assert!(!should_check(Some(NOW - 24 * HOUR + 1), NOW));
    assert!(should_check(Some(NOW - 24 * HOUR), NOW));
    assert!(should_check(Some(NOW - 30 * 24 * HOUR), NOW));
    // The clock went back: due, instead of waiting until it catches up.
    assert!(should_check(Some(NOW + HOUR), NOW));
    assert_eq!(CHECK_EVERY_MS, 24 * HOUR);
}

#[test]
fn the_round_checks_only_when_should_check_says_so() {
    for (last, due) in [
        (None, true),
        (Some(NOW - HOUR), false),
        (Some(NOW - 23 * HOUR), false),
        (Some(NOW - 24 * HOUR), true),
        (Some(NOW + HOUR), true),
    ] {
        let (updates, _) = updates();
        let checked = Cell::new(0);
        let saved = Cell::new(None);
        let round = auto_round(
            &updates,
            last,
            NOW,
            |at| saved.set(Some(at)),
            || {
                checked.set(checked.get() + 1);
                Ok(None)
            },
        );
        assert_eq!(due, should_check(last, NOW), "{last:?}");
        if due {
            assert_eq!(checked.get(), 1, "{last:?}");
            assert_eq!(round, Round::Checked(Ok(None)));
            assert_eq!(saved.get(), Some(NOW), "{last:?}");
        } else {
            assert_eq!(checked.get(), 0, "{last:?}");
            assert_eq!(round, Round::NotDue);
            assert_eq!(saved.get(), None, "{last:?}");
        }
    }
}

#[test]
fn a_found_update_is_shown_and_kept_for_the_install() {
    let (updates, told) = updates();
    let round = auto_round(&updates, None, NOW, |_| {}, || found("0.2.0"));
    assert_eq!(round, Round::Checked(Ok(Some(info("0.2.0")))));
    let shown = UpdateState::Found {
        version: "0.2.0".into(),
        notes: Some("# Notedown 0.2.0".into()),
    };
    assert_eq!(updates.state(), shown);
    // The automatic check tells no Checking, only what it found.
    assert_eq!(*told.lock().unwrap(), vec![shown]);
    assert_eq!(
        updates.start_install(),
        Some((info("0.2.0"), "0.2.0".to_owned()))
    );
    assert_eq!(
        updates.state(),
        UpdateState::Installing {
            version: "0.2.0".into()
        }
    );
}

#[test]
fn a_failed_automatic_check_is_silent_and_tried_again() {
    let (updates, told) = updates();
    let saved = Cell::new(None);
    let round = auto_round(
        &updates,
        None,
        NOW,
        |at| saved.set(Some(at)),
        || Err("offline".into()),
    );
    assert_eq!(round, Round::Checked(Err("offline".into())));
    assert_eq!(updates.state(), UpdateState::Idle);
    assert!(told.lock().unwrap().is_empty());
    // Not saved: the next round checks again.
    assert_eq!(saved.get(), None);
}

#[test]
fn an_automatic_check_without_an_update_tells_nothing() {
    let (updates, told) = updates();
    assert_eq!(updates.check_quietly(|| Ok(None)), Ok(None));
    assert_eq!(updates.state(), UpdateState::Idle);
    assert!(told.lock().unwrap().is_empty());
}

#[test]
fn a_check_the_user_asked_for_tells_each_step() {
    let (updates, told) = updates();
    assert_eq!(updates.check_now(|| Ok(None)), Ok(None));
    assert_eq!(updates.state(), UpdateState::UpToDate);
    assert_eq!(
        updates.check_now(|| Err("no answer".into())),
        Err("no answer".into())
    );
    assert_eq!(
        updates.state(),
        UpdateState::Failed {
            reason: "no answer".into()
        }
    );
    assert_eq!(
        updates.check_now(|| found("0.3.0")),
        Ok(Some(info("0.3.0")))
    );
    assert_eq!(
        *told.lock().unwrap(),
        vec![
            UpdateState::Checking,
            UpdateState::UpToDate,
            UpdateState::Checking,
            UpdateState::Failed {
                reason: "no answer".into()
            },
            UpdateState::Checking,
            UpdateState::Found {
                version: "0.3.0".into(),
                notes: Some("# Notedown 0.3.0".into()),
            },
        ]
    );
}

#[test]
fn a_check_without_an_update_forgets_the_one_found_before() {
    let (updates, _) = updates();
    updates.check_now(|| found("0.2.0")).unwrap();
    updates.check_now(|| Ok(None)).unwrap();
    assert_eq!(updates.start_install(), None);
    // A failed check keeps what was found.
    updates.check_now(|| found("0.2.0")).unwrap();
    let _ = updates.check_now(|| Err("offline".into()));
    assert_eq!(updates.start_install().map(|(i, _)| i), Some(info("0.2.0")));
}

#[test]
fn nothing_found_nothing_to_install() {
    let (updates, told) = updates();
    assert_eq!(updates.start_install(), None);
    assert_eq!(updates.state(), UpdateState::Idle);
    assert!(told.lock().unwrap().is_empty());
}

#[test]
fn no_check_while_an_update_installs() {
    let (updates, _) = updates();
    updates.check_now(|| found("0.2.0")).unwrap();
    updates.start_install().unwrap();
    assert!(
        updates
            .check_now(|| panic!("a check during the install"))
            .is_err()
    );
    let round = auto_round(
        &updates,
        None,
        NOW,
        |_| {},
        || panic!("a check during the install"),
    );
    assert!(matches!(round, Round::Checked(Err(_))), "{round:?}");
    assert_eq!(
        updates.state(),
        UpdateState::Installing {
            version: "0.2.0".into()
        }
    );
}

#[test]
fn a_failed_install_keeps_the_update_for_another_try() {
    let (updates, told) = updates();
    updates.check_now(|| found("0.2.0")).unwrap();
    updates.start_install().unwrap();
    updates.install_failed("the signature does not match");
    assert_eq!(
        updates.state(),
        UpdateState::InstallFailed {
            version: "0.2.0".into(),
            reason: "the signature does not match".into()
        }
    );
    assert_eq!(told.lock().unwrap().last(), Some(&updates.state()));
    assert_eq!(updates.start_install().map(|(i, _)| i), Some(info("0.2.0")));
}

#[test]
fn the_state_reaches_the_webview_in_its_names() {
    let found = serde_json::to_value(UpdateState::Found {
        version: "0.2.0".into(),
        notes: None,
    })
    .unwrap();
    assert_eq!(
        found,
        serde_json::json!({ "kind": "found", "version": "0.2.0", "notes": null })
    );
    let failed = serde_json::to_value(UpdateState::InstallFailed {
        version: "0.2.0".into(),
        reason: "x".into(),
    })
    .unwrap();
    assert_eq!(
        failed,
        serde_json::json!({ "kind": "installFailed", "version": "0.2.0", "reason": "x" })
    );
    assert_eq!(
        serde_json::to_value(UpdateState::UpToDate).unwrap(),
        serde_json::json!({ "kind": "upToDate" })
    );
}
