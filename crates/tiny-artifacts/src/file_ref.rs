//! Portable references to stored artifact files and host path validation.
//!
//! A versioned artifact never carries binary content. A version records a
//! [`FileRef`] containing a storage key, never a local
//! filesystem path. [`FileRoots`] validates incoming host paths before a file
//! store reads them.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// A reference to file bytes held by a [`crate::FileStore`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRef {
    /// Opaque key understood by the configured file store.
    #[serde(alias = "path")]
    pub key: String,
    /// Size in bytes when the reference was made.
    pub size_bytes: u64,
    /// Media type guessed from the extension (e.g. `image/png`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

impl FileRef {
    /// The file name (last path component).
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        Path::new(&self.key).file_name().and_then(|n| n.to_str())
    }

    /// The lower-case extension, when the file has one.
    #[must_use]
    pub fn extension(&self) -> Option<String> {
        Path::new(&self.key)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
    }
}

/// Why a path could not become a [`FileRef`].
#[derive(Debug, thiserror::Error)]
pub enum FileRefError {
    /// The host allows no folders, so no file can be referenced.
    #[error("this host does not allow file artifacts")]
    NoRoots,
    /// The path is empty.
    #[error("the file path is empty")]
    Empty,
    /// Nothing exists at the path.
    #[error("no file at `{0}`")]
    Missing(String),
    /// The path is a directory or another non-file.
    #[error("`{0}` is not a regular file")]
    NotAFile(String),
    /// The path resolves outside every allowed folder.
    #[error("`{0}` is outside the folders artifacts may reference")]
    OutsideRoots(String),
    /// A host guard refused the path (see [`FileRoots::with_guard`] and
    /// [`FileRoots::with_async_guard`]).
    #[error("`{path}` cannot be referenced: {reason}")]
    Refused {
        /// The path as given.
        path: String,
        /// Why the host refused it.
        reason: String,
    },
    /// The file system refused a lookup.
    #[error("cannot read `{path}`: {source}")]
    Io {
        /// The path being resolved.
        path: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// A host's last word on a resolved path: `Some(reason)` refuses it.
pub type PathGuard = Arc<dyn Fn(&Path) -> Option<String> + Send + Sync>;

type PathGuardFuture = Pin<Box<dyn Future<Output = Option<String>> + Send>>;
type AsyncPathGuard = Arc<dyn Fn(PathBuf) -> PathGuardFuture + Send + Sync>;

/// A host path that passed [`FileRoots`] validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedFile {
    /// Canonical path that may be read by a file store.
    pub path: PathBuf,
    /// Size of the file when it was resolved.
    pub size_bytes: u64,
    /// Media type guessed from the extension, when known.
    pub media_type: Option<String>,
}

impl ResolvedFile {
    /// The source file name (last path component).
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.path.file_name().and_then(|name| name.to_str())
    }

    /// The lower-case source extension, when it has one.
    #[must_use]
    pub fn extension(&self) -> Option<String> {
        self.path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
    }
}

/// The folders a host lets artifacts reference.
///
/// A relative path is resolved against the first root (the agent's working
/// folder, typically). Every path is canonicalized before the containment
/// check, so `..` segments and symlinks cannot reach outside. A host can also
/// refuse paths inside the roots ([`Self::with_guard`] or
/// [`Self::with_async_guard`]), such as credential stores under a broad working
/// folder. Both guards run after containment, with the synchronous guard first.
#[derive(Clone, Default)]
pub struct FileRoots {
    roots: Vec<PathBuf>,
    guard: Option<PathGuard>,
    async_guard: Option<AsyncPathGuard>,
}

impl std::fmt::Debug for FileRoots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileRoots")
            .field("roots", &self.roots)
            .field("guard", &self.guard.is_some())
            .field("async_guard", &self.async_guard.is_some())
            .finish()
    }
}

impl PartialEq for FileRoots {
    fn eq(&self, other: &Self) -> bool {
        let same_guard = match (&self.guard, &other.guard) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        let same_async_guard = match (&self.async_guard, &other.async_guard) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        self.roots == other.roots && same_guard && same_async_guard
    }
}

impl Eq for FileRoots {}

impl FileRoots {
    /// Roots in priority order; the first one anchors relative paths.
    #[must_use]
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            roots: roots.into_iter().collect(),
            guard: None,
            async_guard: None,
        }
    }

    /// Ask `guard` about every resolved path, after the containment check, on
    /// its canonical form: `Some(reason)` refuses it with
    /// [`FileRefError::Refused`].
    #[must_use]
    pub fn with_guard(
        mut self,
        guard: impl Fn(&Path) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.guard = Some(Arc::new(guard));
        self
    }

    /// Await host authorization of each canonical path before returning it
    /// for file I/O. `Some(reason)` refuses it with [`FileRefError::Refused`];
    /// `None` permits resolution to continue.
    ///
    /// Containment and any synchronous [`Self::with_guard`] run first, so this
    /// guard cannot authorize a path outside the roots or override a refusal.
    /// Hosts should return a refusal when their authorization service is
    /// unavailable. Each call replaces the previous asynchronous guard without
    /// changing the synchronous guard. Clones share both guard closures.
    #[must_use]
    pub fn with_async_guard<F>(
        mut self,
        guard: impl Fn(PathBuf) -> F + Send + Sync + 'static,
    ) -> Self
    where
        F: Future<Output = Option<String>> + Send + 'static,
    {
        self.async_guard = Some(Arc::new(move |path| Box::pin(guard(path))));
        self
    }

    /// The allowed folders.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Resolve `raw` to a checked, canonical source path.
    ///
    /// # Errors
    ///
    /// Returns a [`FileRefError`] when there are no roots, the path is empty,
    /// nothing is there, it is not a regular file, or it resolves outside
    /// every root. A synchronous or asynchronous host guard refusal returns
    /// [`FileRefError::Refused`] with the host's reason.
    pub async fn resolve(&self, raw: &str) -> Result<ResolvedFile, FileRefError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(FileRefError::Empty);
        }
        let first = self.roots.first().ok_or(FileRefError::NoRoots)?;
        let candidate = Path::new(raw);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            first.join(candidate)
        };
        let canonical = match tokio::fs::canonicalize(&joined).await {
            Ok(path) => path,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(FileRefError::Missing(raw.to_string()));
            }
            Err(source) => {
                return Err(FileRefError::Io {
                    path: raw.to_string(),
                    source,
                })
            }
        };
        let mut inside = false;
        for root in &self.roots {
            if let Ok(root) = tokio::fs::canonicalize(root).await {
                if canonical.starts_with(root) {
                    inside = true;
                    break;
                }
            }
        }
        if !inside {
            return Err(FileRefError::OutsideRoots(raw.to_string()));
        }
        if let Some(reason) = self.guard.as_ref().and_then(|guard| guard(&canonical)) {
            return Err(FileRefError::Refused {
                path: raw.to_string(),
                reason,
            });
        }
        if let Some(guard) = &self.async_guard {
            if let Some(reason) = guard(canonical.clone()).await {
                return Err(FileRefError::Refused {
                    path: raw.to_string(),
                    reason,
                });
            }
        }
        let meta = tokio::fs::metadata(&canonical)
            .await
            .map_err(|source| FileRefError::Io {
                path: raw.to_string(),
                source,
            })?;
        if !meta.is_file() {
            return Err(FileRefError::NotAFile(raw.to_string()));
        }
        let media_type = media_type_for(&canonical).map(str::to_string);
        Ok(ResolvedFile {
            path: canonical,
            size_bytes: meta.len(),
            media_type,
        })
    }
}

/// Media type for a file extension, for the kinds artifacts commonly hold.
#[must_use]
pub fn media_type_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "txt" => "text/plain",
        _ => return None,
    })
}
