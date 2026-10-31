//! Shared storage behavior across independent artifact-service instances.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tiny_artifacts::{
    ArtifactKind, ArtifactPublisher, ArtifactRecord, ArtifactStore, ArtifactVersion, Artifacts,
    Author, BlobFileStore, Content, FileRoots, FileStore, PublishOptions, PublishedLink,
};
use tinystoragedrivers_api::{BlobStore, StorageBackend};
use tinystoragedrivers_testkit::memory_backend;
use tinystoragedrivers_types::Scope;

#[derive(Clone, Default)]
struct Records(Arc<Mutex<HashMap<String, ArtifactRecord>>>);

#[async_trait::async_trait]
impl ArtifactStore for Records {
    async fn list(&self) -> anyhow::Result<Vec<ArtifactRecord>> {
        Ok(self.0.lock().unwrap().values().cloned().collect())
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<ArtifactRecord>> {
        Ok(self.0.lock().unwrap().get(id).cloned())
    }

    async fn upsert(&self, record: &ArtifactRecord) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(record.id.clone(), record.clone());
        Ok(())
    }

    async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self.0.lock().unwrap().remove(id).is_some())
    }
}

struct ReadingPublisher(Arc<dyn BlobStore>);

#[async_trait::async_trait]
impl ArtifactPublisher for ReadingPublisher {
    async fn publish(
        &self,
        record: &ArtifactRecord,
        version: &ArtifactVersion,
        options: &PublishOptions,
    ) -> anyhow::Result<PublishedLink> {
        let file = version.file.as_ref().unwrap();
        let bytes = self
            .0
            .get(&file.key)
            .await?
            .expect("published version is present in shared storage")
            .bytes;
        assert_eq!(bytes, b"version one");
        Ok(PublishedLink {
            url: format!("https://share.example/{}/v{}", record.id, version.version),
            expires_at_millis: Some(u64::MAX),
            published_at_millis: 1,
            public: options.public,
        })
    }
}

fn temp_dir() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("tiny-artifact-share-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[tokio::test]
async fn separate_services_read_share_and_delete_shared_file_versions() {
    let input = temp_dir();
    std::fs::write(input.join("report.pdf"), b"version one").unwrap();
    let records = Records::default();
    let backend = memory_backend();
    let scope = Scope::new("artifact-shared-files").unwrap();
    let first_storage = backend.for_scope(&scope).unwrap();
    let first_blobs = first_storage.blobs().clone();
    let first_files = Arc::new(BlobFileStore::new(&first_storage));
    let first = Artifacts::new(Arc::new(records.clone()))
        .with_file_roots(FileRoots::new([input.clone()]))
        .with_file_store(first_files.clone());

    let id = first
        .create(
            ArtifactKind::File,
            "Report",
            Content::File("report.pdf".into()),
            &Author::agent("writer"),
            Some("shared-report".into()),
        )
        .await
        .unwrap()
        .record
        .id;
    let first_key = first
        .get(&id)
        .await
        .unwrap()
        .unwrap()
        .version(1)
        .unwrap()
        .file
        .as_ref()
        .unwrap()
        .key
        .clone();

    std::fs::write(input.join("report.pdf"), b"version two").unwrap();
    first
        .revise(
            &id,
            Content::File("report.pdf".into()),
            &Author::agent("writer"),
            None,
        )
        .await
        .unwrap();
    std::fs::remove_dir_all(&input).unwrap();

    let second_storage = backend.for_scope(&scope).unwrap();
    let second_blobs = second_storage.blobs().clone();
    let second_files = Arc::new(BlobFileStore::new(&second_storage));
    let second = Artifacts::new(Arc::new(records.clone()))
        .with_file_store(second_files.clone())
        .with_publisher(Arc::new(ReadingPublisher(second_blobs.clone())));
    let history = second.get(&id).await.unwrap().unwrap();
    assert_eq!(history.versions.len(), 2);
    assert_eq!(second_files.read(&first_key).await.unwrap(), b"version one");
    assert_eq!(
        second
            .publish(&id, Some(1), &PublishOptions::default())
            .await
            .unwrap()
            .version,
        1
    );

    assert!(second.delete(&id).await.unwrap().0);
    assert_eq!(
        first_blobs
            .list("artifacts/files/shared-report/")
            .await
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn async_host_refusal_prevents_file_import_and_record_creation() {
    let input = temp_dir();
    std::fs::write(input.join("key.txt"), b"private").unwrap();
    let records = Records::default();
    let backend = memory_backend();
    let scope = Scope::new("artifact-denied-import").unwrap();
    let storage = backend.for_scope(&scope).unwrap();
    let artifacts = Artifacts::new(Arc::new(records.clone()))
        .with_file_roots(FileRoots::new([input.clone()]).with_async_guard(|_| async {
            tokio::task::yield_now().await;
            Some("protected location".to_string())
        }))
        .with_file_store(Arc::new(BlobFileStore::new(&storage)));

    let error = artifacts
        .create(
            ArtifactKind::File,
            "Key",
            Content::File("key.txt".into()),
            &Author::agent("writer"),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("protected location"), "{error}");
    assert!(records.list().await.unwrap().is_empty());
    assert!(storage.blobs().list("").await.unwrap().is_empty());
    std::fs::remove_dir_all(input).unwrap();
}

#[tokio::test]
async fn an_async_guard_is_rechecked_before_importing_a_revised_file() {
    let input = temp_dir();
    std::fs::write(input.join("report.txt"), b"version one").unwrap();
    let records = Records::default();
    let backend = memory_backend();
    let scope = Scope::new("artifact-denied-revision").unwrap();
    let storage = backend.for_scope(&scope).unwrap();
    let refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let captured = refused.clone();
    let artifacts = Artifacts::new(Arc::new(records))
        .with_file_roots(FileRoots::new([input.clone()]).with_async_guard(move |_| {
            let captured = captured.clone();
            async move {
                tokio::task::yield_now().await;
                captured
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .then(|| "protected location".to_string())
            }
        }))
        .with_file_store(Arc::new(BlobFileStore::new(&storage)));
    let saved = artifacts
        .create(
            ArtifactKind::File,
            "Report",
            Content::File("report.txt".into()),
            &Author::agent("writer"),
            None,
        )
        .await
        .unwrap();
    refused.store(true, std::sync::atomic::Ordering::SeqCst);
    std::fs::write(input.join("report.txt"), b"must not import").unwrap();

    let error = artifacts
        .revise(
            &saved.record.id,
            Content::File("report.txt".into()),
            &Author::agent("writer"),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("protected location"), "{error}");
    assert_eq!(
        artifacts
            .get(&saved.record.id)
            .await
            .unwrap()
            .unwrap()
            .versions
            .len(),
        1
    );
    assert_eq!(storage.blobs().list("").await.unwrap().len(), 1);
    std::fs::remove_dir_all(input).unwrap();
}

#[cfg(unix)]
async fn refuse_a_source_swapped_while_its_import_is_authorized(
    swap_parent: bool,
    protected_inside_root: bool,
) {
    let input = temp_dir();
    let outside = temp_dir();
    let approved_dir = input.join("approved");
    std::fs::create_dir_all(&approved_dir).unwrap();
    let source = approved_dir.join("report.txt");
    std::fs::write(&source, b"approved").unwrap();
    let hidden_dir = if protected_inside_root {
        input.join(".ssh")
    } else {
        outside.clone()
    };
    std::fs::create_dir_all(&hidden_dir).unwrap();
    let hidden = hidden_dir.join("report.txt");
    // Equal sizes ensure the old size-change check cannot mask the leak.
    std::fs::write(&hidden, b"private!").unwrap();
    let records = Records::default();
    let backend = memory_backend();
    let scope = Scope::new("artifact-swapped-source").unwrap();
    let storage = backend.for_scope(&scope).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = calls.clone();
    let roots = FileRoots::new([input.clone()]).with_async_guard(move |path| {
        let calls = captured_calls.clone();
        let source = source.clone();
        let approved_dir = approved_dir.clone();
        let hidden = hidden.clone();
        let hidden_dir = hidden_dir.clone();
        async move {
            if path.components().any(|part| part.as_os_str() == ".ssh") {
                return Some("protected location".to_string());
            }
            // The first resolution prepares the version. The second one is
            // immediately before the actual import; replace the approved name
            // only after that import's guard has received its canonical path.
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                if swap_parent {
                    std::fs::rename(&approved_dir, approved_dir.with_extension("saved")).unwrap();
                    std::os::unix::fs::symlink(hidden_dir, approved_dir).unwrap();
                } else {
                    std::fs::rename(&source, source.with_extension("saved")).unwrap();
                    std::os::unix::fs::symlink(hidden, source).unwrap();
                }
            }
            tokio::task::yield_now().await;
            None
        }
    });
    let artifacts = Artifacts::new(Arc::new(records.clone()))
        .with_file_roots(roots)
        .with_file_store(Arc::new(BlobFileStore::new(&storage)));

    let result = artifacts
        .create(
            ArtifactKind::File,
            "Report",
            Content::File("approved/report.txt".into()),
            &Author::agent("writer"),
            None,
        )
        .await;
    if let Ok(saved) = result {
        let file = saved.record.versions[0].file.as_ref().unwrap();
        let bytes = storage.blobs().get(&file.key).await.unwrap().unwrap().bytes;
        assert_eq!(
            bytes, b"approved",
            "only the approved open file may be imported"
        );
        assert_eq!(records.list().await.unwrap().len(), 1);
        assert_eq!(storage.blobs().list("").await.unwrap().len(), 1);
    } else {
        assert!(records.list().await.unwrap().is_empty());
        assert!(storage.blobs().list("").await.unwrap().is_empty());
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    std::fs::remove_dir_all(input).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn an_async_guard_cannot_be_bypassed_by_swapping_the_file_for_an_outside_symlink() {
    refuse_a_source_swapped_while_its_import_is_authorized(false, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn an_async_guard_cannot_be_bypassed_by_swapping_the_file_for_a_protected_symlink() {
    refuse_a_source_swapped_while_its_import_is_authorized(false, true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn an_async_guard_cannot_be_bypassed_by_swapping_a_parent_for_an_outside_symlink() {
    refuse_a_source_swapped_while_its_import_is_authorized(true, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn an_async_guard_cannot_be_bypassed_by_swapping_a_parent_for_a_protected_symlink() {
    refuse_a_source_swapped_while_its_import_is_authorized(true, true).await;
}
