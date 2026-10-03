//! PathSafety tests (`workspace_and_config_test.exs` + symlink-loop guard).

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use symphony_core::path_safety::{MAX_SYMLINK_HOPS, PathError, canonicalize, expand_path};

fn real(path: &std::path::Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

#[test]
fn path_safety_returns_errors_for_invalid_path_segments() {
    let path = std::env::temp_dir().join("a".repeat(300));
    let expanded = expand_path(&path, None);
    match canonicalize(&path) {
        Err(PathError::CanonicalizeFailed { path, reason }) => {
            assert_eq!(path, expanded);
            assert_eq!(reason.name(), "enametoolong");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn missing_tail_is_appended_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = canonicalize(dir.path().join("missing/child")).unwrap();
    assert_eq!(canonical, real(dir.path()).join("missing/child"));
}

#[test]
fn existing_paths_match_the_os_realpath() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("a/b")).unwrap();
    assert_eq!(
        canonicalize(dir.path().join("a/b")).unwrap(),
        real(&dir.path().join("a/b"))
    );
}

#[test]
fn absolute_and_relative_symlinks_are_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let root = real(dir.path());
    fs::create_dir_all(root.join("real/inner")).unwrap();
    symlink(root.join("real"), root.join("abs-link")).unwrap();
    symlink("real/inner", root.join("rel-link")).unwrap();
    symlink("../../real", root.join("real/inner/up-link")).unwrap();

    assert_eq!(
        canonicalize(root.join("abs-link/inner")).unwrap(),
        root.join("real/inner")
    );
    assert_eq!(
        canonicalize(root.join("rel-link/new")).unwrap(),
        root.join("real/inner/new")
    );
    assert_eq!(
        canonicalize(root.join("real/inner/up-link")).unwrap(),
        root.join("real")
    );
}

#[test]
fn dot_dot_is_removed_lexically_before_symlinks_are_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let root = real(dir.path());
    fs::create_dir_all(root.join("elsewhere/deep")).unwrap();
    symlink(root.join("elsewhere/deep"), root.join("link")).unwrap();
    // POSIX realpath would give <root>/elsewhere; Elixir's lexical expansion gives <root>.
    assert_eq!(canonicalize(root.join("link/..")).unwrap(), root);
}

#[test]
fn symlink_loops_fail_with_eloop_instead_of_recursing_forever() {
    let dir = tempfile::tempdir().unwrap();
    let root = real(dir.path());
    symlink(root.join("b"), root.join("a")).unwrap();
    symlink(root.join("a"), root.join("b")).unwrap();
    symlink(root.join("self"), root.join("self")).unwrap();

    for start in ["a/child", "self"] {
        match canonicalize(root.join(start)) {
            Err(PathError::CanonicalizeFailed { path, reason }) => {
                assert_eq!(path, root.join(start));
                assert_eq!(reason.name(), "eloop");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(MAX_SYMLINK_HOPS, 40);
}

#[test]
fn long_but_finite_symlink_chains_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let root = real(dir.path());
    fs::create_dir(root.join("target")).unwrap();
    let mut previous = root.join("target");
    for i in 0..MAX_SYMLINK_HOPS {
        let link = root.join(format!("l{i}"));
        symlink(&previous, &link).unwrap();
        previous = link;
    }
    assert_eq!(canonicalize(&previous).unwrap(), root.join("target"));
}

#[test]
fn non_directory_parents_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    fs::write(&file, "x").unwrap();
    match canonicalize(file.join("child")) {
        Err(PathError::CanonicalizeFailed { reason, .. }) => assert_eq!(reason.name(), "enotdir"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn display_keeps_the_elixir_tag() {
    let err = canonicalize(std::env::temp_dir().join("b".repeat(300))).unwrap_err();
    assert!(err.to_string().starts_with("path_canonicalize_failed: "));
    assert!(err.to_string().ends_with(": enametoolong"));
}
