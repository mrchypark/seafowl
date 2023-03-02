use crate::config::schema;
use crate::config::schema::{Local, S3};
use bytes::Bytes;
use futures::{stream::BoxStream, StreamExt, TryFutureExt};
use log::debug;
use object_store::{
    path::Path, Error, GetResult, ListResult, MultipartId, ObjectMeta, ObjectStore,
    Result,
};
use std::fmt::{Debug, Display, Formatter};
use std::ops::Range;
use tokio::io::AsyncWrite;

use tokio::fs::{copy, create_dir_all, remove_file, rename};

use deltalake::storage::DeltaObjectStore;
use itertools::Itertools;
use object_store::prefix::PrefixObjectStore;
use std::path::Path as StdPath;
use std::str::FromStr;
use std::sync::Arc;
use url::Url;

/// Wrapper around the object_store crate that holds on to the original config
/// in order to provide a more efficient "upload" for the local object store (since it's
/// stored on the local filesystem, we can just move the file to it instead).
#[derive(Debug, Clone)]
pub struct InternalObjectStore {
    pub inner: Arc<dyn ObjectStore>,
    pub root_uri: Url,
    pub config: schema::ObjectStore,
}

impl InternalObjectStore {
    pub fn new(inner: Arc<dyn ObjectStore>, config: schema::ObjectStore) -> Self {
        let root_uri = match config.clone() {
            schema::ObjectStore::Local(Local { data_dir }) => {
                let canonical_path = StdPath::new(&data_dir).canonicalize().unwrap();
                Url::from_directory_path(canonical_path).unwrap()
            }
            schema::ObjectStore::InMemory(_) => Url::from_str("memory://").unwrap(),
            schema::ObjectStore::S3(S3 { bucket, .. }) => {
                Url::from_str(&format!("s3://{bucket}")).unwrap()
            }
        };

        Self {
            inner,
            root_uri,
            config,
        }
    }

    // Wrap our object store with a prefixed one corresponding to the full path to the actual table
    // root, and then wrap that with a delta object store. This is done because:
    // 1. `DeltaObjectStore` needs an object store with "/" pointing at delta table root
    //     (i.e. where `_delta_log` is located), hence the `PrefixObjectStore`.
    // 2. We want to re-use the underlying object store that we build initially during startup,
    //     instead of re-building one from scratch whenever we need it (not necessarily for perf
    //     reasons, but rather because the memory object store doesn't work otherwise). However,
    //     `PrefixObjectStore` has a trait bound of `T: ObjectStore`, which isn't satisfied by
    //     `Arc<dyn ObjectStore>`, so we need another intermediary, which is where
    //     `InternalObjectStore` comes in.
    // This does mean that we have 3 layers of indirection before we hit the "real" object store
    // (`DeltaObjectStore` -> `PrefixObjectStore` -> `InternalObjectStore` -> `inner`).
    pub fn for_delta_table(
        &self,
        database_name: &str,
        collection_name: &str,
        table_name: &str,
    ) -> Arc<DeltaObjectStore> {
        let prefix = format!("{}/{}/{}", database_name, collection_name, table_name);
        let prefixed_store: PrefixObjectStore<InternalObjectStore> =
            PrefixObjectStore::new(self.clone(), prefix.clone());

        Arc::from(DeltaObjectStore::new(
            Arc::from(prefixed_store),
            Url::from_str(format!("{}/{}", self.root_uri.as_str(), prefix).as_str())
                .unwrap(),
        ))
    }

    /// Delete all objects under a given prefix
    pub async fn delete_in_prefix(&self, prefix: &Path) -> Result<(), Error> {
        let mut path_stream = self.inner.list(Some(prefix)).await?;
        while let Some(maybe_object) = path_stream.next().await {
            if let Ok(ObjectMeta { location, .. }) = maybe_object {
                self.inner.delete(&location).await?;
            }
        }
        Ok(())
    }

    /// Create any missing intermediate directories in the given path, so that further actions don't
    /// error out. Applicable only to local FS, since in other stores the directories are virtual.
    pub async fn ensure_path_exists(&self, path: &Path) -> Result<(), Error> {
        if let schema::ObjectStore::Local(_) = self.config.clone() {
            let full_path = StdPath::new(self.root_uri.path()).join(path.to_string());
            create_dir_all(full_path)
                .await
                .map_err(|e| Error::Generic {
                    store: "local",
                    source: Box::new(e),
                })?;
        }

        Ok(())
    }

    /// Moving all objects in paths with `from_prefix` to paths with `to_prefix`. The main purpose
    /// of this is to ensure that `to_prefix` actually exists when using the local FS store, otherwise
    /// we get "No such file or directory" on rename.
    pub async fn rename_in_prefix(
        &self,
        from_prefix: &Path,
        to_prefix: &Path,
    ) -> Result<(), Error> {
        // Go over all objects with the `from_prefix` prefix and rename them to be with `to_prefix`
        let mut path_stream = self.inner.list(Some(from_prefix)).await?;

        while let Some(maybe_object) = path_stream.next().await {
            if let Ok(ObjectMeta { location, .. }) = maybe_object {
                // We unwrap since the path must match the from prefix
                let mut new_path_parts = to_prefix.parts().collect_vec();
                new_path_parts
                    .extend(location.prefix_match(from_prefix).unwrap().collect_vec());
                let new_location = Path::from_iter(new_path_parts);
                self.inner.rename(&location, &new_location).await?;
            }
        }
        Ok(())
    }

    /// For local filesystem object stores, try "uploading" by just moving the file.
    /// Returns a None if the store isn't local.
    pub async fn fast_upload(
        &self,
        from: &StdPath,
        to: &Path,
    ) -> Option<Result<(), Error>> {
        let object_store_path = match &self.config {
            schema::ObjectStore::Local(Local { data_dir }) => data_dir,
            _ => return None,
        };

        let target_path =
            StdPath::new(&object_store_path).join(StdPath::new(to.to_string().as_str()));

        debug!(
            "Moving temporary partition file from {} to {}",
            from.display(),
            target_path.display()
        );

        let result = rename(&from, &target_path).await;

        Some(if let Err(e) = result {
            // Cross-device link (can't move files between filesystems)
            // Copy and remove the old file
            if e.raw_os_error() == Some(18) {
                copy(from, target_path)
                    .and_then(|_| remove_file(from))
                    .map_err(|e| Error::Generic {
                        store: "local",
                        source: Box::new(e),
                    })
                    .await
            } else {
                Err(Error::Generic {
                    store: "local",
                    source: Box::new(e),
                })
            }
        } else {
            Ok(())
        })
    }
}

impl Display for InternalObjectStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileStorageBackend({})", self.root_uri)
    }
}

#[async_trait::async_trait]
impl ObjectStore for InternalObjectStore {
    /// Save the provided bytes to the specified location.
    async fn put(&self, location: &Path, bytes: Bytes) -> Result<()> {
        self.inner.put(location, bytes).await
    }

    async fn put_multipart(
        &self,
        location: &Path,
    ) -> Result<(MultipartId, Box<dyn AsyncWrite + Unpin + Send>)> {
        self.inner.put_multipart(location).await
    }

    async fn abort_multipart(
        &self,
        location: &Path,
        multipart_id: &MultipartId,
    ) -> Result<()> {
        self.inner.abort_multipart(location, multipart_id).await
    }

    /// Return the bytes that are stored at the specified location.
    async fn get(&self, location: &Path) -> Result<GetResult> {
        self.inner.get(location).await
    }

    /// Return the bytes that are stored at the specified location
    /// in the given byte range
    async fn get_range(&self, location: &Path, range: Range<usize>) -> Result<Bytes> {
        self.inner.get_range(location, range).await
    }

    /// Return the metadata for the specified location
    async fn head(&self, location: &Path) -> Result<ObjectMeta> {
        self.inner.head(location).await
    }

    /// Delete the object at the specified location.
    async fn delete(&self, location: &Path) -> Result<()> {
        self.inner.delete(location).await
    }

    /// List all the objects with the given prefix.
    ///
    /// Prefixes are evaluated on a path segment basis, i.e. `foo/bar/` is a prefix of `foo/bar/x` but not of
    /// `foo/bar_baz/x`.
    async fn list(
        &self,
        prefix: Option<&Path>,
    ) -> Result<BoxStream<'_, Result<ObjectMeta>>> {
        self.inner.list(prefix).await
    }

    /// List objects with the given prefix and an implementation specific
    /// delimiter. Returns common prefixes (directories) in addition to object
    /// metadata.
    ///
    /// Prefixes are evaluated on a path segment basis, i.e. `foo/bar/` is a prefix of `foo/bar/x` but not of
    /// `foo/bar_baz/x`.
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    /// Copy an object from one path to another in the same object store.
    ///
    /// If there exists an object at the destination, it will be overwritten.
    async fn copy(&self, from: &Path, to: &Path) -> Result<()> {
        self.inner.copy(from, to).await
    }

    /// Copy an object from one path to another, only if destination is empty.
    ///
    /// Will return an error if the destination already has an object.
    async fn copy_if_not_exists(&self, from: &Path, to: &Path) -> Result<()> {
        self.inner.copy_if_not_exists(from, to).await
    }

    /// Move an object from one path to another in the same object store.
    ///
    /// Will return an error if the destination already has an object.
    async fn rename_if_not_exists(&self, from: &Path, to: &Path) -> Result<()> {
        self.inner.rename_if_not_exists(from, to).await
    }
}
