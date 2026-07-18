//! Authoritative immutable records for editor-owned documents.
//!
//! An open URI has exactly one record containing the raw/analysis document
//! views and every cross-file derivative computed from that same document
//! generation. Records are replaced, never mutated in place. Each successful
//! replacement receives a process-local [`AnalysisGeneration`] that is never
//! reused, including across close/reopen of the same URI.
//!
//! LSP versions, document revisions, and diagnostic lifecycle epochs remain
//! protocol/publish identities. They are deliberately not commit tokens:
//! clients may reopen at the same version and [`crate::state::Document`]
//! revisions restart at zero. Detached work captures the store-issued
//! generation and commits through a guarded replacement instead.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, Url};

use crate::cross_file::revalidation::DiagnosticsEpoch;
use crate::cross_file::scope::{ScopeArtifacts, compute_artifacts_with_metadata};
use crate::cross_file::types::CrossFileMetadata;
use crate::state::Document;

/// Process-wide source of analysis generations.
///
/// Constructing a fresh store must not permit ABA reuse of an earlier token.
static NEXT_ANALYSIS_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Never-reused identity for one installed analysis record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnalysisGeneration(u64);

/// Editor-protocol identity carried by an open analysis record.
///
/// This is projected from the record's `Document` plus the diagnostic gate's
/// epoch at installation time. It must not be used as an off-lock commit
/// token; use [`AnalysisGeneration`] for that purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenDocumentProvenance {
    pub lsp_version: Option<i32>,
    pub revision: u64,
    pub lifecycle_epoch: Option<DiagnosticsEpoch>,
}

/// Exact non-mutating identity for one open-document slot.
///
/// `Absent` participates in install CAS; `Present` carries both the
/// never-reused analysis generation and editor/lifecycle provenance so tests
/// and commit bases cannot accidentally fall back to LSP version/revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenRecordToken {
    uri: Url,
    identity: Option<(AnalysisGeneration, OpenDocumentProvenance)>,
}

impl OpenRecordToken {
    pub(crate) fn uri(&self) -> &Url {
        &self.uri
    }

    #[cfg(test)]
    pub(crate) fn absent_for_test(uri: Url) -> Self {
        Self {
            uri,
            identity: None,
        }
    }
}

/// One coherent immutable open-document analysis snapshot.
pub struct OpenDocumentRecord {
    document: Document,
    metadata: Arc<CrossFileMetadata>,
    artifacts: Arc<ScopeArtifacts>,
    generation: AnalysisGeneration,
    lifecycle_epoch: Option<DiagnosticsEpoch>,
}

impl OpenDocumentRecord {
    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn metadata(&self) -> &Arc<CrossFileMetadata> {
        &self.metadata
    }

    pub fn artifacts(&self) -> &Arc<ScopeArtifacts> {
        &self.artifacts
    }

    pub fn generation(&self) -> AnalysisGeneration {
        self.generation
    }

    pub fn provenance(&self) -> OpenDocumentProvenance {
        OpenDocumentProvenance {
            lsp_version: self.document.version,
            revision: self.document.revision,
            lifecycle_epoch: self.lifecycle_epoch,
        }
    }
}

impl std::fmt::Debug for OpenDocumentRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenDocumentRecord")
            .field("generation", &self.generation)
            .field("provenance", &self.provenance())
            .finish_non_exhaustive()
    }
}

/// A change batch rebased on one exact open-record generation.
///
/// Preparing applies sequential LSP ranges to a cloned [`Document`] and
/// rebuilds that document's masked text/tree/package fields once. Committing
/// derives metadata-dependent artifacts internally and succeeds only while the
/// captured generation is still current. A rejected batch must be discarded
/// and recomputed; its ranges cannot safely be replayed onto a newer record.
pub struct PreparedOpenDocument {
    base_generation: AnalysisGeneration,
    document: Document,
}

impl PreparedOpenDocument {
    pub fn document(&self) -> &Document {
        &self.document
    }
}

/// Metadata-dependent open-record payload derived off-lock from one exact
/// immutable record.
///
/// The document itself is not cloned until commit. Its never-reused generation
/// proves that the tree/text used to derive `artifacts` is still installed.
pub(crate) struct PreparedOpenMetadataReplacement {
    base_generation: AnalysisGeneration,
    metadata: Arc<CrossFileMetadata>,
    artifacts: Arc<ScopeArtifacts>,
}

impl PreparedOpenMetadataReplacement {
    pub(crate) fn base_generation(&self) -> AnalysisGeneration {
        self.base_generation
    }
}

/// Why a guarded record replacement did not commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenDocumentCommitError {
    Missing,
    Stale {
        expected: AnalysisGeneration,
        actual: AnalysisGeneration,
    },
}

/// The sole authority for open documents.
///
/// The map is private and stores immutable `Arc` records. Open records are
/// structurally non-evictable: the only removal operation is explicit
/// [`Self::close`]. There is intentionally no `get_mut`, LRU, memory cap,
/// update tracker, or metrics surface.
#[derive(Default)]
pub struct OpenDocumentStore {
    records: HashMap<Url, Arc<OpenDocumentRecord>>,
}

impl OpenDocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record_token(&self, uri: &Url) -> OpenRecordToken {
        OpenRecordToken {
            uri: uri.clone(),
            identity: self
                .records
                .get(uri)
                .map(|record| (record.generation(), record.provenance())),
        }
    }

    pub(crate) fn record_token_is_current(&self, token: &OpenRecordToken) -> bool {
        self.records
            .get(token.uri())
            .map(|record| (record.generation(), record.provenance()))
            == token.identity
    }

    pub(crate) fn generation_is_current(&self, uri: &Url, expected: AnalysisGeneration) -> bool {
        self.records
            .get(uri)
            .is_some_and(|record| record.generation() == expected)
    }

    fn mint_generation(&mut self) -> AnalysisGeneration {
        let generation = NEXT_ANALYSIS_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("analysis generation counter exhausted");
        AnalysisGeneration(generation)
    }

    fn artifacts_for(
        uri: &Url,
        document: &Document,
        metadata: &CrossFileMetadata,
    ) -> Arc<ScopeArtifacts> {
        let analysis_text = document.analysis_text();
        Arc::new(match document.tree.as_ref() {
            Some(tree) => {
                compute_artifacts_with_metadata(uri, tree, &analysis_text, Some(metadata))
            }
            None => ScopeArtifacts::default(),
        })
    }

    fn install(
        &mut self,
        uri: Url,
        document: Document,
        metadata: Arc<CrossFileMetadata>,
        lifecycle_epoch: Option<DiagnosticsEpoch>,
    ) -> Arc<OpenDocumentRecord> {
        let artifacts = Self::artifacts_for(&uri, &document, &metadata);
        self.install_with_artifacts(uri, document, metadata, artifacts, lifecycle_epoch)
    }

    fn install_with_artifacts(
        &mut self,
        uri: Url,
        document: Document,
        metadata: Arc<CrossFileMetadata>,
        artifacts: Arc<ScopeArtifacts>,
        lifecycle_epoch: Option<DiagnosticsEpoch>,
    ) -> Arc<OpenDocumentRecord> {
        let generation = self.mint_generation();
        let record = Arc::new(OpenDocumentRecord {
            document,
            metadata,
            artifacts,
            generation,
            lifecycle_epoch,
        });
        self.records.insert(uri, record.clone());
        record
    }

    /// Install a detached open candidate whose document, metadata, and
    /// metadata-dependent artifacts were derived from the same snapshot.
    pub(crate) fn open_prepared(
        &mut self,
        uri: Url,
        document: Document,
        metadata: Arc<CrossFileMetadata>,
        artifacts: Arc<ScopeArtifacts>,
        lifecycle_epoch: Option<DiagnosticsEpoch>,
    ) -> Arc<OpenDocumentRecord> {
        self.install_with_artifacts(uri, document, metadata, artifacts, lifecycle_epoch)
    }

    /// Install a newly opened editor document as the URI's authority.
    pub fn open(
        &mut self,
        uri: Url,
        document: Document,
        metadata: Arc<CrossFileMetadata>,
        lifecycle_epoch: Option<DiagnosticsEpoch>,
    ) -> Arc<OpenDocumentRecord> {
        self.install(uri, document, metadata, lifecycle_epoch)
    }

    /// Prepare one ordered `didChange` batch against the current generation.
    pub fn prepare_changes(
        &self,
        uri: &Url,
        changes: impl IntoIterator<Item = TextDocumentContentChangeEvent>,
        version: i32,
    ) -> Option<PreparedOpenDocument> {
        let current = self.records.get(uri)?;
        let mut document = current.document.clone();
        document.version = Some(version);
        document.apply_changes(changes);
        Some(PreparedOpenDocument {
            base_generation: current.generation,
            document,
        })
    }

    /// Commit a prepared document and its enriched metadata if its basis lives.
    ///
    /// Artifacts are derived here from the prepared document's own tree and
    /// analysis text, preventing callers from supplying an incoherent
    /// `(document, metadata, artifacts)` tuple.
    pub fn commit_prepared_if_current(
        &mut self,
        uri: &Url,
        prepared: PreparedOpenDocument,
        metadata: Arc<CrossFileMetadata>,
    ) -> Result<Arc<OpenDocumentRecord>, OpenDocumentCommitError> {
        let current = self
            .records
            .get(uri)
            .ok_or(OpenDocumentCommitError::Missing)?;
        if current.generation != prepared.base_generation {
            return Err(OpenDocumentCommitError::Stale {
                expected: prepared.base_generation,
                actual: current.generation,
            });
        }
        let lifecycle_epoch = current.lifecycle_epoch;
        Ok(self.install(uri.clone(), prepared.document, metadata, lifecycle_epoch))
    }

    /// Replace metadata-dependent derivatives while the record is current.
    pub fn replace_metadata_if_current(
        &mut self,
        uri: &Url,
        expected: AnalysisGeneration,
        metadata: Arc<CrossFileMetadata>,
    ) -> Result<Arc<OpenDocumentRecord>, OpenDocumentCommitError> {
        let current = self
            .records
            .get(uri)
            .ok_or(OpenDocumentCommitError::Missing)?;
        if current.generation != expected {
            return Err(OpenDocumentCommitError::Stale {
                expected,
                actual: current.generation,
            });
        }
        let document = current.document.clone();
        let lifecycle_epoch = current.lifecycle_epoch;
        Ok(self.install(uri.clone(), document, metadata, lifecycle_epoch))
    }

    /// Derive metadata-dependent artifacts from one captured immutable record.
    ///
    /// Callers may run this after dropping the shared state lock: `record`
    /// remains immutable and the prepared generation is revalidated at commit.
    pub(crate) fn prepare_metadata_replacement(
        uri: &Url,
        record: &OpenDocumentRecord,
        metadata: Arc<CrossFileMetadata>,
    ) -> PreparedOpenMetadataReplacement {
        PreparedOpenMetadataReplacement {
            base_generation: record.generation,
            artifacts: Self::artifacts_for(uri, &record.document, &metadata),
            metadata,
        }
    }

    /// Install an already-derived metadata/artifact pair while its exact
    /// source record remains current.
    pub(crate) fn commit_prepared_metadata_if_current(
        &mut self,
        uri: &Url,
        prepared: PreparedOpenMetadataReplacement,
    ) -> Result<Arc<OpenDocumentRecord>, OpenDocumentCommitError> {
        let current = self
            .records
            .get(uri)
            .ok_or(OpenDocumentCommitError::Missing)?;
        if current.generation != prepared.base_generation {
            return Err(OpenDocumentCommitError::Stale {
                expected: prepared.base_generation,
                actual: current.generation,
            });
        }
        let document = current.document.clone();
        let lifecycle_epoch = current.lifecycle_epoch;
        Ok(self.install_with_artifacts(
            uri.clone(),
            document,
            prepared.metadata,
            prepared.artifacts,
            lifecycle_epoch,
        ))
    }

    /// Replace only lifecycle provenance while the record is current.
    ///
    /// The analysis payload is immutable and already coherent, so this reuses
    /// its document, metadata, and artifacts while still minting a generation.
    pub fn replace_lifecycle_epoch_if_current(
        &mut self,
        uri: &Url,
        expected: AnalysisGeneration,
        lifecycle_epoch: Option<DiagnosticsEpoch>,
    ) -> Result<Arc<OpenDocumentRecord>, OpenDocumentCommitError> {
        let current = self
            .records
            .get(uri)
            .ok_or(OpenDocumentCommitError::Missing)?;
        if current.generation != expected {
            return Err(OpenDocumentCommitError::Stale {
                expected,
                actual: current.generation,
            });
        }
        let document = current.document.clone();
        let metadata = current.metadata.clone();
        let artifacts = current.artifacts.clone();
        Ok(
            self.install_with_artifacts(
                uri.clone(),
                document,
                metadata,
                artifacts,
                lifecycle_epoch,
            ),
        )
    }

    /// Remove an editor-owned URI. The generation counter is not reset.
    pub fn close(&mut self, uri: &Url) -> Option<Arc<OpenDocumentRecord>> {
        self.records.remove(uri)
    }

    pub fn get_record(&self, uri: &Url) -> Option<&Arc<OpenDocumentRecord>> {
        self.records.get(uri)
    }

    pub fn get(&self, uri: &Url) -> Option<&Document> {
        self.records.get(uri).map(|record| &record.document)
    }

    pub fn contains_key(&self, uri: &Url) -> bool {
        self.records.contains_key(uri)
    }

    pub fn keys(&self) -> impl Iterator<Item = &Url> {
        self.records.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Url, &Document)> {
        self.records
            .iter()
            .map(|(uri, record)| (uri, &record.document))
    }

    pub fn uris(&self) -> Vec<Url> {
        self.records.keys().cloned().collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.records.len()
    }

    /// Test-fixture seam preserving concise `Document::new` insertions.
    ///
    /// Production writers must use the typed open/prepare/commit APIs.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert(&mut self, uri: Url, document: Document) -> Option<Document> {
        let replaced = self.records.get(&uri).map(|record| record.document.clone());
        let analysis_text = document.analysis_text();
        let metadata = Arc::new(crate::cross_file::extract_metadata(&analysis_text));
        self.install(uri, document, metadata, None);
        replaced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_file::revalidation::CrossFileDiagnosticsGate;

    fn uri() -> Url {
        Url::parse("file:///workspace/test.R").unwrap()
    }

    fn document(version: i32, text: &str) -> Document {
        Document::new(text, Some(version))
    }

    fn metadata() -> Arc<CrossFileMetadata> {
        Arc::new(CrossFileMetadata::default())
    }

    #[test]
    fn generation_is_never_reused_across_close_reopen() {
        let uri = uri();
        let mut store = OpenDocumentStore::new();
        let first = store.open(uri.clone(), document(1, "x <- 1"), metadata(), None);
        assert_eq!(store.close(&uri).unwrap().generation(), first.generation());

        let reopened = store.open(uri, document(1, "x <- 1"), metadata(), None);
        assert!(reopened.generation() > first.generation());
        assert_eq!(reopened.provenance().lsp_version, Some(1));
        assert_eq!(reopened.provenance().revision, 0);
    }

    #[test]
    fn generation_is_never_reused_across_store_instances() {
        let uri = uri();
        let first =
            OpenDocumentStore::new().open(uri.clone(), document(1, "x <- 1"), metadata(), None);
        let second = OpenDocumentStore::new().open(uri, document(1, "x <- 1"), metadata(), None);

        assert!(second.generation() > first.generation());
    }

    #[test]
    fn every_successful_record_replacement_mints_a_generation() {
        let uri = uri();
        let mut store = OpenDocumentStore::new();
        let opened = store.open(uri.clone(), document(1, "x <- 1"), metadata(), None);
        let prepared = store
            .prepare_changes(
                &uri,
                [TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "x <- 2".to_string(),
                }],
                2,
            )
            .unwrap();
        let changed = store
            .commit_prepared_if_current(&uri, prepared, metadata())
            .unwrap();
        let replaced = store
            .replace_metadata_if_current(&uri, changed.generation(), metadata())
            .unwrap();
        let epoch = CrossFileDiagnosticsGate::new().begin_epoch(&uri);
        let lifecycle = store
            .replace_lifecycle_epoch_if_current(&uri, replaced.generation(), Some(epoch))
            .unwrap();

        assert!(changed.generation() > opened.generation());
        assert!(replaced.generation() > changed.generation());
        assert!(lifecycle.generation() > replaced.generation());
        assert_eq!(lifecycle.provenance().lifecycle_epoch, Some(epoch));

        let retired = store
            .replace_lifecycle_epoch_if_current(&uri, lifecycle.generation(), None)
            .unwrap();
        assert!(retired.generation() > lifecycle.generation());
        assert_eq!(retired.provenance().lifecycle_epoch, None);
    }

    #[test]
    fn stale_guarded_replacement_is_all_or_nothing() {
        let uri = uri();
        let mut store = OpenDocumentStore::new();
        let opened = store.open(uri.clone(), document(1, "x <- 1"), metadata(), None);
        let stale = store
            .prepare_changes(
                &uri,
                [TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "stale <- TRUE".to_string(),
                }],
                2,
            )
            .unwrap();
        let current = store
            .replace_metadata_if_current(&uri, opened.generation(), metadata())
            .unwrap();

        assert_eq!(
            store
                .commit_prepared_if_current(&uri, stale, metadata())
                .unwrap_err(),
            OpenDocumentCommitError::Stale {
                expected: opened.generation(),
                actual: current.generation(),
            }
        );
        let after = store.get_record(&uri).unwrap();
        assert_eq!(after.generation(), current.generation());
        assert_eq!(after.document().text(), "x <- 1");

        assert_eq!(
            store
                .replace_metadata_if_current(&uri, opened.generation(), metadata())
                .unwrap_err(),
            OpenDocumentCommitError::Stale {
                expected: opened.generation(),
                actual: current.generation(),
            }
        );
        assert_eq!(
            store.get_record(&uri).unwrap().generation(),
            current.generation()
        );
    }

    #[test]
    fn stale_did_open_enrichment_cannot_overwrite_change_or_reopen() {
        let uri = uri();
        let mut store = OpenDocumentStore::new();
        let opened = store.open(uri.clone(), document(1, "old <- 1"), metadata(), None);

        let changed = store
            .prepare_changes(
                &uri,
                [TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "changed <- 2".to_string(),
                }],
                2,
            )
            .unwrap();
        let changed = store
            .commit_prepared_if_current(&uri, changed, metadata())
            .unwrap();
        assert!(matches!(
            store.replace_metadata_if_current(&uri, opened.generation(), metadata()),
            Err(OpenDocumentCommitError::Stale { .. })
        ));
        assert_eq!(
            store.get_record(&uri).unwrap().document().text(),
            "changed <- 2"
        );

        store.close(&uri);
        let reopened = store.open(uri.clone(), document(1, "reopened <- 3"), metadata(), None);
        assert!(matches!(
            store.replace_metadata_if_current(&uri, changed.generation(), metadata()),
            Err(OpenDocumentCommitError::Stale { .. })
        ));
        assert_eq!(
            store.get_record(&uri).unwrap().generation(),
            reopened.generation()
        );
        assert_eq!(
            store.get_record(&uri).unwrap().document().text(),
            "reopened <- 3"
        );
    }

    #[test]
    fn open_records_are_structurally_non_evictable() {
        let mut store = OpenDocumentStore::new();
        for i in 0..5_000 {
            let uri = Url::parse(&format!("file:///workspace/{i}.R")).unwrap();
            store.open(uri, document(1, "x <- 1"), metadata(), None);
        }

        assert_eq!(store.len(), 5_000);
        assert!(store.contains_key(&Url::parse("file:///workspace/0.R").unwrap()));
        assert!(store.contains_key(&Url::parse("file:///workspace/4999.R").unwrap()));
    }
}
