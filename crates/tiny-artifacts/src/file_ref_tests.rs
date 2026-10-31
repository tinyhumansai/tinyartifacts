//! Secure source opening rejects noncanonical input and swapped components.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn directory() -> PathBuf {
    let path = std::env::temp_dir().join(format!("tiny-artifacts-open-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    std::fs::canonicalize(path).unwrap()
}

#[test]
fn secure_open_refuses_relative_paths_without_ambient_resolution() {
    let error = open_canonical_file(Path::new("relative.txt")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn secure_open_refuses_parent_components_instead_of_escaping_its_directory_handle() {
    let root = directory();
    std::fs::create_dir(root.join("allowed")).unwrap();
    std::fs::write(root.join("notes.txt"), b"private!").unwrap();
    let error = open_canonical_file(&root.join("allowed/../notes.txt")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn secure_open_requires_a_file_component_after_the_filesystem_anchor() {
    let directory = directory();
    let anchor = directory.ancestors().last().unwrap();
    let error = open_canonical_file(anchor).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn secure_open_reports_a_missing_parent_directory() {
    let root = directory();
    let error = open_canonical_file(&root.join("missing/notes.txt")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn secure_open_never_follows_a_final_symlink_in_an_already_canonical_spelling() {
    let root = directory();
    std::fs::write(root.join("hidden.txt"), b"private!").unwrap();
    std::os::unix::fs::symlink(root.join("hidden.txt"), root.join("approved.txt")).unwrap();
    assert!(open_canonical_file(&root.join("approved.txt")).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn secure_open_never_follows_a_parent_symlink_even_inside_the_same_root() {
    let root = directory();
    let protected = root.join(".ssh");
    std::fs::create_dir(&protected).unwrap();
    std::fs::write(protected.join("key"), b"private!").unwrap();
    std::os::unix::fs::symlink(&protected, root.join("approved")).unwrap();
    assert!(open_canonical_file(&root.join("approved/key")).is_err());
    std::fs::remove_dir_all(root).unwrap();
}
