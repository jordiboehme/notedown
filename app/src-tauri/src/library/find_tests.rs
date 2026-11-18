//! The name search against a scripted server on loopback: the tree pages are answered in order.

use super::listing_tests::{MD5_A, NOTE, file, folder, setup, setup_hooked, tree};
use super::*;

fn names(found: &Found) -> Vec<&str> {
    found.entries.iter().map(|e| e.name.as_str()).collect()
}

#[test]
fn a_name_matches_without_case() {
    assert!(find::name_matches("Plan.note", "plan"));
    assert!(find::name_matches("my plan.pdf", "PLAN"));
    assert!(find::name_matches("Über.note", "über"));
    assert!(!find::name_matches("Plan.note", "plans"));
    assert!(!find::name_matches("Plan.note", ""));
}

#[test]
fn find_walks_every_page() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], Some("t1:a")),
        tree(10, &[file(2, "NOTE/Note/x/Plan.note", MD5_A)], None),
    ]);
    let found = s.library.find("plan", "", 200).unwrap();
    assert_eq!(names(&found), ["Plan.note"]);
    assert!(!found.more);
    assert_eq!(
        s.targets(),
        [
            "/tree?path=&recursive=true&limit=1000",
            "/tree?path=&recursive=true&limit=1000&cursor=t1%3Aa",
        ]
    );
}

#[test]
fn find_is_case_insensitive() {
    let s = setup(vec![tree(
        10,
        &[
            file(1, "NOTE/Note/Plan.note", MD5_A),
            file(2, "NOTE/Note/PLANS.pdf", MD5_A),
            file(3, "NOTE/Note/other.note", MD5_A),
        ],
        None,
    )]);
    let found = s.library.find("plan", "", 200).unwrap();
    assert_eq!(names(&found), ["Plan.note", "PLANS.pdf"]);
}

#[test]
fn find_lists_files_only() {
    let s = setup(vec![tree(
        10,
        &[
            folder(1, "NOTE/Plans"),
            file(2, "NOTE/Plans/plan.note", MD5_A),
        ],
        None,
    )]);
    let found = s.library.find("plan", "", 200).unwrap();
    assert_eq!(names(&found), ["plan.note"]);
}

#[test]
fn find_stops_at_the_limit_and_says_more() {
    let s = setup(vec![
        tree(
            10,
            &[
                file(1, "NOTE/a1.note", MD5_A),
                file(2, "NOTE/b.note", MD5_A),
                file(3, "NOTE/a2.note", MD5_A),
            ],
            Some("t1:a"),
        ),
        tree(10, &[file(4, "NOTE/a3.note", MD5_A)], Some("t2:a")),
        tree(10, &[file(5, "NOTE/a4.note", MD5_A)], None),
    ]);
    let found = s.library.find("a", "", 2).unwrap();
    assert_eq!(names(&found), ["a1.note", "a2.note"]);
    assert!(found.more);
    // The third hit is on the second page: the walk ends there.
    assert_eq!(s.targets().len(), 2);
    // Exactly the limit is not more.
    let s = setup(vec![tree(
        10,
        &[
            file(1, "NOTE/a1.note", MD5_A),
            file(2, "NOTE/a2.note", MD5_A),
        ],
        None,
    )]);
    let found = s.library.find("a", "", 2).unwrap();
    assert_eq!(names(&found), ["a1.note", "a2.note"]);
    assert!(!found.more);
}

#[test]
fn find_below_a_folder_asks_that_path() {
    let s = setup(vec![tree(
        10,
        &[file(1, "NOTE/Note/plan.note", MD5_A)],
        None,
    )]);
    let found = s.library.find("plan", NOTE, 200).unwrap();
    assert_eq!(names(&found), ["plan.note"]);
    assert_eq!(
        s.targets(),
        ["/tree?path=NOTE%2FNote&recursive=true&limit=1000"]
    );
}

#[test]
fn an_empty_text_asks_nothing() {
    let s = setup(vec![]);
    let found = s.library.find("", "", 200).unwrap();
    assert!(found.entries.is_empty());
    assert!(s.targets().is_empty());
}

#[test]
fn a_changed_binding_gives_changed() {
    let s = setup_hooked(
        vec![
            tree(10, &[file(1, "NOTE/plan.note", MD5_A)], Some("t1:a")),
            tree(10, &[file(2, "NOTE/plan2.note", MD5_A)], None),
        ],
        |n, _, library| {
            if n == 0 {
                library.relogin();
            }
        },
    );
    assert_eq!(
        s.library.find("plan", "", 200).unwrap_err(),
        LibraryError::Changed
    );
    assert_eq!(s.targets().len(), 1);
}

#[test]
fn a_newer_find_stops_an_older_walk() {
    let s = setup_hooked(
        vec![
            tree(10, &[file(1, "NOTE/plan.note", MD5_A)], Some("t1:a")),
            tree(10, &[file(2, "NOTE/plan2.note", MD5_A)], None),
        ],
        |n, _, library| {
            if n == 0 {
                // Another find began while this page was asked for.
                library.begin_find();
            }
        },
    );
    assert_eq!(
        s.library.find("plan", "", 200).unwrap_err(),
        LibraryError::Changed
    );
    assert_eq!(s.targets().len(), 1);
}
