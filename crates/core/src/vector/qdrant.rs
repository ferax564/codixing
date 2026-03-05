//! Optional Qdrant vector backend.
//!
//! Enable with `--features qdrant` at build time.  Requires a running Qdrant
//! instance reachable at the URL stored in the `QDRANT_URL` environment
//! variable (default: `http://localhost:6334`).
//!
//! The collection name defaults to `codixing`; override with
//! `QDRANT_COLLECTION`.
//!
//! # Example
//!
//! ```bash
//! QDRANT_URL=http://localhost:6334 cargo run --features qdrant -- search "query"
//! ```

#[cfg(feature = "qdrant")]
pub use self::inner::QdrantVectorIndex;

#[cfg(feature = "qdrant")]
mod inner {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;

    use qdrant_client::qdrant::{
        CreateCollectionBuilder, DeletePointsBuilder, Distance, PointStruct, PointsIdsList,
        SearchPointsBuilder, UpsertPointsBuilder, Value, VectorParamsBuilder,
    };
    use qdrant_client::{Payload, Qdrant};

    use crate::error::{CodixingError, Result};
    use crate::vector::VectorBackend;

    /// Qdrant-backed vector index.
    ///
    /// Vectors are stored in a remote (or local) Qdrant collection.  A local
    /// `file_chunks` mirror is kept in memory so that [`remove_file`] and
    /// [`file_chunks_owned`] do not require a round-trip to the server.
    ///
    /// # Note on blocking
    ///
    /// The [`VectorBackend`] trait is synchronous.  Each method builds a
    /// single-threaded `tokio` runtime to block on the async Qdrant calls.
    /// This is intentional: the engine uses `rayon` for parallelism and calls
    /// vector operations sequentially; creating a tiny per-call runtime avoids
    /// introducing an async executor dependency into the core library.
    pub struct QdrantVectorIndex {
        client: Qdrant,
        collection: String,
        /// Vector dimensionality — stored for diagnostics and potential future use.
        #[allow(dead_code)]
        dims: usize,
        /// In-memory mirror of file path → chunk IDs.
        file_chunks: Mutex<HashMap<String, Vec<u64>>>,
    }

    impl QdrantVectorIndex {
        /// Connect to Qdrant and ensure the target collection exists.
        ///
        /// Reads connection parameters from environment variables:
        /// - `QDRANT_URL` — default `http://localhost:6334`
        /// - `QDRANT_COLLECTION` — default `codixing`
        /// - `QDRANT_API_KEY` — optional API key
        pub fn new(dims: usize) -> Result<Self> {
            let url =
                std::env::var("QDRANT_URL").unwrap_or_else(|_| "http://localhost:6334".to_string());
            let collection =
                std::env::var("QDRANT_COLLECTION").unwrap_or_else(|_| "codixing".to_string());

            let rt = build_rt()?;

            let mut builder = Qdrant::from_url(&url);
            if let Ok(key) = std::env::var("QDRANT_API_KEY") {
                builder = builder.api_key(key);
            }
            let client = builder
                .build()
                .map_err(|e| CodixingError::VectorIndex(format!("qdrant connect: {e}")))?;

            // Create the collection if it does not already exist.
            rt.block_on(async {
                let exists = client.collection_exists(&collection).await.map_err(|e| {
                    CodixingError::VectorIndex(format!("qdrant collection_exists: {e}"))
                })?;

                if !exists {
                    client
                        .create_collection(
                            CreateCollectionBuilder::new(&collection).vectors_config(
                                VectorParamsBuilder::new(dims as u64, Distance::Cosine),
                            ),
                        )
                        .await
                        .map_err(|e| {
                            CodixingError::VectorIndex(format!("qdrant create_collection: {e}"))
                        })?;
                }
                Ok::<_, CodixingError>(())
            })?;

            Ok(Self {
                client,
                collection,
                dims,
                file_chunks: Mutex::new(HashMap::new()),
            })
        }

        /// Re-connect to Qdrant (does not restore `file_chunks` from the server).
        ///
        /// This is a best-effort open: the collection must already exist.
        pub fn load(_dir: &Path, dims: usize) -> Result<Self> {
            Self::new(dims)
        }
    }

    impl VectorBackend for QdrantVectorIndex {
        fn add(&self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()> {
            // Build the payload using qdrant's own Value type to avoid the
            // serde_json→Payload TryFrom path (not available without default-features).
            let mut map: HashMap<String, Value> = HashMap::new();
            map.insert(
                "file_path".to_string(),
                Value {
                    kind: Some(qdrant_client::qdrant::value::Kind::StringValue(
                        file_path.to_string(),
                    )),
                },
            );
            let payload = Payload::from(map);
            let point = PointStruct::new(chunk_id, vector.to_vec(), payload);

            let rt = build_rt()?;
            rt.block_on(async {
                self.client
                    .upsert_points(UpsertPointsBuilder::new(&self.collection, vec![point]))
                    .await
                    .map_err(|e| CodixingError::VectorIndex(format!("qdrant upsert: {e}")))?;
                Ok::<_, CodixingError>(())
            })?;

            self.file_chunks
                .lock()
                .unwrap()
                .entry(file_path.to_string())
                .or_default()
                .push(chunk_id);

            Ok(())
        }

        fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
            use qdrant_client::qdrant::point_id::PointIdOptions;

            let rt = build_rt()?;
            let response = rt.block_on(async {
                self.client
                    .search_points(SearchPointsBuilder::new(
                        &self.collection,
                        query.to_vec(),
                        k as u64,
                    ))
                    .await
                    .map_err(|e| CodixingError::VectorIndex(format!("qdrant search: {e}")))
            })?;

            let results = response
                .result
                .into_iter()
                .filter_map(|p| {
                    let id =
                        p.id.and_then(|pid| pid.point_id_options)
                            .and_then(|opt| match opt {
                                PointIdOptions::Num(n) => Some(n),
                                PointIdOptions::Uuid(_) => None,
                            })?;
                    Some((id, p.score))
                })
                .collect();

            Ok(results)
        }

        fn remove_file(&mut self, file_path: &str) -> Result<()> {
            let ids: Vec<u64> = self
                .file_chunks
                .lock()
                .unwrap()
                .remove(file_path)
                .unwrap_or_default();

            if ids.is_empty() {
                return Ok(());
            }

            let points_selector = PointsIdsList {
                ids: ids.into_iter().map(Into::into).collect(),
            };

            let rt = build_rt()?;
            rt.block_on(async {
                self.client
                    .delete_points(
                        DeletePointsBuilder::new(&self.collection).points(points_selector),
                    )
                    .await
                    .map_err(|e| CodixingError::VectorIndex(format!("qdrant delete: {e}")))?;
                Ok::<_, CodixingError>(())
            })
        }

        fn size(&self) -> usize {
            self.file_chunks
                .lock()
                .unwrap()
                .values()
                .map(|v| v.len())
                .sum()
        }

        fn file_chunks_owned(&self) -> HashMap<String, Vec<u64>> {
            self.file_chunks.lock().unwrap().clone()
        }

        /// Qdrant persists data automatically on the server.
        ///
        /// This is a no-op; in a production implementation you would serialise
        /// `file_chunks` to `dir` so the in-memory mirror can be restored on
        /// the next [`load`] call.
        fn save(&self, _dir: &Path) -> Result<()> {
            Ok(())
        }
    }

    /// Build a single-threaded Tokio runtime for blocking on async Qdrant calls.
    fn build_rt() -> Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CodixingError::VectorIndex(format!("tokio runtime: {e}")))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn qdrant_index_implements_backend_trait() {
            // Compile-time check: QdrantVectorIndex must satisfy VectorBackend.
            fn _assert_backend<T: VectorBackend>() {}
            _assert_backend::<QdrantVectorIndex>();
        }
    }
}
