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

#[test]
fn a_second_install_while_one_runs_gets_nothing() {
    let (updates, told) = updates();
    updates.check_now(|| found("0.2.0")).unwrap();
    assert!(updates.start_install().is_some());
    let before = told.lock().unwrap().len();
    // A double click on Install: no second download over the same bundle.
    assert_eq!(updates.start_install(), None);
    assert!(updates.is_installing());
    assert_eq!(
        updates.state(),
        UpdateState::Installing {
            version: "0.2.0".into()
        }
    );
    assert_eq!(told.lock().unwrap().len(), before);
    // After a failed install the next try may start.
    updates.install_failed("x");
    assert!(!updates.is_installing());
    assert!(updates.start_install().is_some());
}

#[test]
fn about_opens_once_per_new_version() {
    // Never shown: open.
    assert!(should_show("0.2.0", None));
    // Shown already: not again each day.
    assert!(!should_show("0.2.0", Some("0.2.0")));
    // A newer one opens again; an older one (a release taken back) does not.
    assert!(should_show("0.2.1", Some("0.2.0")));
    assert!(should_show("0.10.0", Some("0.9.0")));
    assert!(!should_show("0.1.9", Some("0.2.0")));
    // A version that is not semver: open when it is another one.
    assert!(should_show("next", Some("0.2.0")));
    assert!(!should_show("next", Some("next")));
}

#[test]
fn a_quiet_check_without_an_update_clears_a_failed_install() {
    let (updates, told) = updates();
    updates.check_now(|| found("0.2.0")).unwrap();
    updates.start_install().unwrap();
    updates.install_failed("x");
    assert_eq!(updates.check_quietly(|| Ok(None)), Ok(None));
    // No Install button for an update that is gone.
    assert_eq!(updates.state(), UpdateState::Idle);
    assert_eq!(told.lock().unwrap().last(), Some(&UpdateState::Idle));
}

#[test]
fn two_checks_never_run_at_once() {
    let (updates, _) = updates();
    let inner = Cell::new(None);
    let outer = updates.check_now(|| {
        // A second check (the timer, a second click) while this one runs.
        inner.set(Some(updates.check_now(|| panic!("a second check"))));
        assert!(
            updates
                .check_quietly(|| panic!("a second quiet check"))
                .is_err()
        );
        Ok(None)
    });
    assert_eq!(outer, Ok(None));
    assert!(inner.take().unwrap().is_err());
    assert_eq!(updates.state(), UpdateState::UpToDate);
    // Once it is done, the next check runs.
    assert_eq!(
        updates.check_now(|| found("0.2.0")).map(|f| f.is_some()),
        Ok(true)
    );
}

#[test]
fn a_quiet_check_does_not_end_an_install_that_started_while_it_ran() {
    for answer in [found("0.2.0"), Ok(None), Err("offline".to_owned())] {
        let (updates, _) = updates();
        updates.check_now(|| found("0.2.0")).unwrap();
        let round = auto_round(
            &updates,
            None,
            NOW,
            |_| {},
            || {
                // The user clicks Install while the daily check waits for GitHub.
                updates.start_install().unwrap();
                answer.clone()
            },
        );
        assert!(matches!(round, Round::Checked(_)), "{round:?}");
        let installing = UpdateState::Installing {
            version: "0.2.0".into(),
        };
        assert_eq!(updates.state(), installing, "{answer:?}");
        // No Install button again: a second click does not start a second install.
        assert_eq!(updates.start_install(), None, "{answer:?}");
        // A failed install still knows its update.
        updates.install_failed("x");
        assert!(
            matches!(updates.state(), UpdateState::InstallFailed { .. }),
            "{answer:?}"
        );
    }
}

#[test]
fn a_quiet_check_keeps_a_failed_install_that_came_while_it_ran() {
    let (updates, _) = updates();
    updates.check_now(|| found("0.2.0")).unwrap();
    updates
        .check_quietly(|| {
            updates.start_install().unwrap();
            updates.install_failed("the signature does not match");
            found("0.2.0")
        })
        .unwrap();
    assert_eq!(
        updates.state(),
        UpdateState::InstallFailed {
            version: "0.2.0".into(),
            reason: "the signature does not match".into()
        }
    );
}
