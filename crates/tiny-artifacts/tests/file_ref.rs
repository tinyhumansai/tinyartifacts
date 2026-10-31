//! File references: helpers, media types, and paths that cannot be read.

// Unwrapping is fine in tests (AGENTS.md); the workspace lints keep it out of
// library code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use tiny_artifacts::{media_type_for, FileRef, FileRefError, FileRoots};

fn dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiny-artifacts-ref-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn a_resolved_reference_knows_its_name_extension_and_type() {
    let root = dir();
    std::fs::write(root.join("Report.PDF"), b"%PDF").unwrap();
    let roots = FileRoots::new([root.clone()]);
    assert_eq!(roots.roots(), [root]);

    let file = roots.resolve("Report.PDF").await.unwrap();
    assert_eq!(file.file_name(), Some("Report.PDF"));
    assert_eq!(file.extension().as_deref(), Some("pdf"));
    assert_eq!(file.media_type.as_deref(), Some("application/pdf"));
    assert_eq!(file.size_bytes, 4);

    let bare = FileRef {
        key: "artifacts/a/README".into(),
        size_bytes: 0,
        media_type: None,
    };
    assert_eq!(bare.extension(), None);
}

#[test]
fn a_file_reference_serializes_a_storage_key_instead_of_a_host_path() {
    let file = FileRef {
        key: "artifacts/report/v1.pdf".into(),
        size_bytes: 4,
        media_type: Some("application/pdf".into()),
    };
    let serialized = serde_json::to_value(&file).unwrap();

    assert!(serialized.get("key").is_some(), "{serialized}");
    assert!(serialized.get("path").is_none(), "{serialized}");
    assert_eq!(
        serialized
            .get("sizeBytes")
            .and_then(serde_json::Value::as_u64),
        Some(4)
    );
}

#[test]
fn media_types_cover_the_common_artifact_formats() {
    let cases = [
        ("a.png", Some("image/png")),
        ("a.jpeg", Some("image/jpeg")),
        ("a.gif", Some("image/gif")),
        ("a.webp", Some("image/webp")),
        ("a.svg", Some("image/svg+xml")),
        ("a.mp4", Some("video/mp4")),
        ("a.webm", Some("video/webm")),
        ("a.mov", Some("video/quicktime")),
        (
            "a.docx",
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        ),
        (
            "a.pptx",
            Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        ),
        (
            "a.xlsx",
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        ),
        ("a.csv", Some("text/csv")),
        ("a.json", Some("application/json")),
        ("a.zip", Some("application/zip")),
        ("a.md", Some("text/markdown")),
        ("a.htm", Some("text/html")),
        ("a.txt", Some("text/plain")),
        ("a.unknown", None),
        ("no-extension", None),
    ];
    for (name, expected) in cases {
        assert_eq!(media_type_for(Path::new(name)), expected, "{name}");
    }
}

#[tokio::test]
async fn an_empty_path_is_refused() {
    let roots = FileRoots::new([dir()]);
    assert!(matches!(
        roots.resolve("   ").await,
        Err(FileRefError::Empty)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn a_path_that_cannot_be_read_is_an_io_error() {
    use std::os::unix::fs::PermissionsExt;
    let root = dir();
    let locked = root.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("f.txt"), b"x").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let result = FileRoots::new([root]).resolve("locked/f.txt").await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Root can read anything; the refusal only shows for an ordinary user.
    if let Err(err) = result {
        assert!(matches!(err, FileRefError::Io { .. }), "{err}");
        assert!(err.to_string().contains("locked/f.txt"));
    }
}

#[tokio::test]
async fn the_host_guard_refuses_a_path_inside_the_roots() {
    let root = dir();
    std::fs::create_dir_all(root.join(".ssh")).unwrap();
    std::fs::write(root.join(".ssh").join("id_ed25519"), b"key").unwrap();
    std::fs::write(root.join("notes.txt"), b"fine").unwrap();
    let roots = FileRoots::new([root.clone()]).with_guard(|path| {
        path.components()
            .any(|c| c.as_os_str() == ".ssh")
            .then(|| "a credential store".to_string())
    });

    // Checked on the canonical path, whatever the spelling.
    for raw in [".ssh/id_ed25519", "./.ssh/id_ed25519"] {
        let result = roots.resolve(raw).await;
        assert!(
            matches!(
                &result,
                Err(FileRefError::Refused { path, reason })
                    if path == raw && reason == "a credential store"
            ),
            "{raw}: {result:?}"
        );
    }
    let err = roots.resolve(".ssh/id_ed25519").await.unwrap_err();
    assert!(err
        .to_string()
        .contains("cannot be referenced: a credential store"));
    assert!(roots.resolve("notes.txt").await.is_ok());
}

#[test]
fn a_guard_shows_in_debug_and_equality() {
    let plain = FileRoots::new([PathBuf::from("/a")]);
    let guarded = plain.clone().with_guard(|_| None);
    assert!(format!("{guarded:?}").contains("guard: true"));
    assert!(format!("{plain:?}").contains("guard: false"));
    assert_ne!(plain, guarded);
    assert_eq!(guarded, guarded.clone());
    assert_eq!(plain, FileRoots::new([PathBuf::from("/a")]));
    assert_ne!(guarded, plain.clone().with_guard(|_| None));
}

#[tokio::test]
async fn an_async_guard_is_awaited_with_the_canonical_path() {
    let root = dir();
    std::fs::write(root.join("notes.txt"), b"fine").unwrap();
    let canonical = std::fs::canonicalize(root.join("notes.txt")).unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = seen.clone();
    let roots = FileRoots::new([root.clone()]).with_async_guard(move |path| {
        let captured = captured.clone();
        async move {
            tokio::task::yield_now().await;
            captured.lock().unwrap().push(path);
            None
        }
    });

    let file = roots.resolve("./notes.txt").await.unwrap();
    assert_eq!(file.path, canonical);
    assert_eq!(*seen.lock().unwrap(), [canonical]);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn an_async_guard_refuses_a_path_with_the_hosts_reason() {
    let root = dir();
    std::fs::write(root.join("key.txt"), b"key").unwrap();
    let roots = FileRoots::new([root.clone()]).with_async_guard(|_| async {
        tokio::task::yield_now().await;
        Some("host authorization unavailable".to_string())
    });

    let error = roots.resolve("key.txt").await.unwrap_err();
    assert!(matches!(
        error,
        FileRefError::Refused { path, reason }
            if path == "key.txt" && reason == "host authorization unavailable"
    ));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn containment_and_sync_refusals_precede_the_async_guard() {
    let root = dir();
    let outside = dir();
    std::fs::write(root.join("notes.txt"), b"fine").unwrap();
    std::fs::write(outside.join("other.txt"), b"private").unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured = calls.clone();
    let roots = FileRoots::new([root.clone()])
        .with_guard(|_| Some("sync refusal".to_string()))
        .with_async_guard(move |_| {
            captured.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { None }
        });

    assert!(matches!(
        roots
            .resolve(outside.join("other.txt").to_str().unwrap())
            .await,
        Err(FileRefError::OutsideRoots(_))
    ));
    assert!(matches!(
        roots.resolve("notes.txt").await,
        Err(FileRefError::Refused { reason, .. }) if reason == "sync refusal"
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

#[tokio::test]
async fn an_async_guard_cannot_authorize_parent_traversal_outside_the_roots() {
    // Parent components exercise the same canonical containment check on all
    // platforms, without requiring symlink privileges on Windows.
    let root = dir();
    let inside = root.join("inside");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(root.join("outside.txt"), b"private").unwrap();
    let roots = FileRoots::new([inside]).with_async_guard(|_| async { None });
    assert!(matches!(
        roots.resolve("../outside.txt").await,
        Err(FileRefError::OutsideRoots(_))
    ));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn async_guards_are_shared_by_clones_and_distinct_in_debug_and_equality() {
    let plain = FileRoots::new([PathBuf::from("/a")]);
    let guarded = plain.clone().with_async_guard(|_| async { None });
    assert!(format!("{guarded:?}").contains("async_guard: true"));
    assert!(format!("{plain:?}").contains("async_guard: false"));
    assert_ne!(plain, guarded);
    assert_eq!(guarded, guarded.clone());
    assert_ne!(guarded, plain.clone().with_async_guard(|_| async { None }));
    assert_ne!(guarded, guarded.clone().with_guard(|_| None));
}
