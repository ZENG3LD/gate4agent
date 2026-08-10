//! Native, bounded history discovery/load/cache authority.
//!
//! Host paths and SQLite handles stay behind opaque candidate IDs. The crate
//! owns filesystem effects, while `gate4agent-adapters` remains the only owner
//! of provider transcript semantics.

mod discovery;
mod load;
mod sqlite;

use gate4agent_adapters::{
    history_source_variants, HistoryDocument, HistorySourceLayout, HISTORY_DOCUMENT_MAX_BYTES,
    HISTORY_METADATA_MAX_BYTES,
};
use gate4agent_provider_ports::{
    HistoryAuthority, HistoryCandidate, HistoryCandidateId, HistoryDiscoveryRequest,
    HistoryLoadRequest,
};
use gate4agent_types::{
    AdapterBinding, AdapterId, AgentId, ProviderSessionIdentity, ProviderSessionKey,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

pub const HISTORY_ROOTS_MAX: usize = 64;
pub const HISTORY_CANDIDATES_MAX: usize = 1_024;
pub const HISTORY_CACHE_ENTRIES_MAX: usize = 256;
pub const HISTORY_WALK_ENTRIES_MAX: usize = 50_000;
pub const HISTORY_WALK_DEPTH_MAX: usize = 16;
pub const HISTORY_SIBLING_FILES_MAX: usize = 1_024;
pub const HISTORY_AUXILIARY_INDEX_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeHistoryLimits {
    pub max_candidates: usize,
    pub max_cache_entries: usize,
    pub max_walk_entries: usize,
    pub max_walk_depth: usize,
    pub max_sibling_files: usize,
}

impl Default for NativeHistoryLimits {
    fn default() -> Self {
        Self {
            max_candidates: HISTORY_CANDIDATES_MAX,
            max_cache_entries: HISTORY_CACHE_ENTRIES_MAX,
            max_walk_entries: HISTORY_WALK_ENTRIES_MAX,
            max_walk_depth: HISTORY_WALK_DEPTH_MAX,
            max_sibling_files: HISTORY_SIBLING_FILES_MAX,
        }
    }
}

impl NativeHistoryLimits {
    fn validate(self) -> Result<Self, NativeHistoryConfigError> {
        if self.max_candidates == 0 || self.max_candidates > HISTORY_CANDIDATES_MAX {
            return Err(NativeHistoryConfigError::InvalidLimit("max_candidates"));
        }
        if self.max_cache_entries == 0 || self.max_cache_entries > HISTORY_CACHE_ENTRIES_MAX {
            return Err(NativeHistoryConfigError::InvalidLimit("max_cache_entries"));
        }
        if self.max_walk_entries == 0 || self.max_walk_entries > HISTORY_WALK_ENTRIES_MAX {
            return Err(NativeHistoryConfigError::InvalidLimit("max_walk_entries"));
        }
        if self.max_walk_depth == 0 || self.max_walk_depth > HISTORY_WALK_DEPTH_MAX {
            return Err(NativeHistoryConfigError::InvalidLimit("max_walk_depth"));
        }
        if self.max_sibling_files == 0 || self.max_sibling_files > HISTORY_SIBLING_FILES_MAX {
            return Err(NativeHistoryConfigError::InvalidLimit("max_sibling_files"));
        }
        Ok(self)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct NativeHistoryRoot {
    adapter_id: AdapterId,
    layout: HistorySourceLayout,
    path: PathBuf,
}

impl NativeHistoryRoot {
    pub fn new(
        adapter_id: AdapterId,
        layout: HistorySourceLayout,
        path: impl Into<PathBuf>,
    ) -> Result<Self, NativeHistoryConfigError> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(NativeHistoryConfigError::RootMustBeAbsolute);
        }
        let supported = history_source_variants(&adapter_id)
            .map_err(|_| NativeHistoryConfigError::UnsupportedAdapter(adapter_id.clone()))?;
        if !supported.iter().any(|variant| variant.layout == layout) {
            return Err(NativeHistoryConfigError::UnsupportedLayout { adapter_id, layout });
        }
        Ok(Self {
            adapter_id,
            layout,
            path,
        })
    }

    pub fn adapter_id(&self) -> &AdapterId {
        &self.adapter_id
    }

    pub fn layout(&self) -> HistorySourceLayout {
        self.layout
    }
}

impl fmt::Debug for NativeHistoryRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeHistoryRoot")
            .field("adapter_id", &self.adapter_id)
            .field("layout", &self.layout)
            .field("path", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHistoryConfig {
    roots: Vec<NativeHistoryRoot>,
    limits: NativeHistoryLimits,
}

impl NativeHistoryConfig {
    pub fn new(roots: Vec<NativeHistoryRoot>) -> Result<Self, NativeHistoryConfigError> {
        Self::with_limits(roots, NativeHistoryLimits::default())
    }

    pub fn with_limits(
        roots: Vec<NativeHistoryRoot>,
        limits: NativeHistoryLimits,
    ) -> Result<Self, NativeHistoryConfigError> {
        if roots.len() > HISTORY_ROOTS_MAX {
            return Err(NativeHistoryConfigError::TooManyRoots);
        }
        let limits = limits.validate()?;
        let mut unique = HashSet::new();
        for root in &roots {
            if !unique.insert((root.adapter_id.clone(), root.layout, root.path.clone())) {
                return Err(NativeHistoryConfigError::DuplicateRoot);
            }
        }
        Ok(Self { roots, limits })
    }

    pub fn roots(&self) -> &[NativeHistoryRoot] {
        &self.roots
    }

    pub fn limits(&self) -> NativeHistoryLimits {
        self.limits
    }
}

/// Pinned Orca-compatible roots beneath an explicitly supplied native home.
///
/// Environment overrides and WSL/remote homes are intentionally not read here;
/// an owning shell may append explicit roots after applying its own policy.
pub fn orca_home_roots(
    home: impl AsRef<Path>,
) -> Result<Vec<NativeHistoryRoot>, NativeHistoryConfigError> {
    let home = home.as_ref();
    if !home.is_absolute() {
        return Err(NativeHistoryConfigError::RootMustBeAbsolute);
    }
    let mut roots = Vec::new();
    let mut push = |id: &str, layout, path: PathBuf| -> Result<(), NativeHistoryConfigError> {
        let adapter_id = AdapterId::new(id)
            .map_err(|_| NativeHistoryConfigError::UnsupportedAdapterValue(id.to_owned()))?;
        roots.push(NativeHistoryRoot::new(adapter_id, layout, path)?);
        Ok(())
    };

    push(
        "claude-code",
        HistorySourceLayout::SingleNdjson,
        home.join(".claude").join("projects"),
    )?;
    push(
        "codex",
        HistorySourceLayout::NdjsonWithOptionalIndex,
        home.join(".codex").join("sessions"),
    )?;
    push(
        "gemini",
        HistorySourceLayout::JsonOrNdjson,
        home.join(".gemini").join("tmp"),
    )?;
    push(
        "antigravity",
        HistorySourceLayout::SingleNdjson,
        home.join(".gemini").join("antigravity-cli").join("brain"),
    )?;
    push(
        "copilot",
        HistorySourceLayout::SingleNdjson,
        home.join(".copilot").join("session-state"),
    )?;
    push(
        "cursor",
        HistorySourceLayout::SingleNdjson,
        home.join(".cursor").join("projects"),
    )?;
    let opencode = home.join(".local").join("share").join("opencode");
    push(
        "opencode",
        HistorySourceLayout::SessionJsonWithSiblingMessageJson,
        opencode.join("storage"),
    )?;
    push(
        "opencode",
        HistorySourceLayout::ReadOnlySqliteProjection,
        opencode,
    )?;
    push(
        "grok",
        HistorySourceLayout::SummaryJsonWithSiblingNdjson,
        home.join(".grok").join("sessions"),
    )?;
    push(
        "hermes",
        HistorySourceLayout::SingleJson,
        home.join(".hermes").join("sessions"),
    )?;
    push(
        "rovo",
        HistorySourceLayout::MetadataJsonWithSiblingJson,
        home.join(".rovodev").join("sessions"),
    )?;
    for state_root in [home.join(".openclaw"), home.join(".clawdbot")] {
        push(
            "openclaw",
            HistorySourceLayout::SingleNdjson,
            state_root.join("agents"),
        )?;
    }
    push(
        "pi",
        HistorySourceLayout::SingleNdjson,
        home.join(".pi").join("agent").join("sessions"),
    )?;
    push(
        "omp",
        HistorySourceLayout::SingleNdjson,
        home.join(".omp").join("agent").join("sessions"),
    )?;
    push(
        "devin",
        HistorySourceLayout::SingleJson,
        home.join(".local")
            .join("share")
            .join("devin")
            .join("cli")
            .join("transcripts"),
    )?;
    for droid_root in [
        home.join(".factory").join("sessions"),
        home.join(".factory").join("projects"),
    ] {
        push("droid", HistorySourceLayout::SingleNdjson, droid_root)?;
    }
    push(
        "kimi",
        HistorySourceLayout::StateJsonWithIndexAndSiblingNdjson,
        home.join(".kimi-code").join("sessions"),
    )?;
    push(
        "qwen-code",
        HistorySourceLayout::SingleNdjson,
        home.join(".qwen").join("projects"),
    )?;
    Ok(roots)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SourceIdentity {
    agent_id: AgentId,
    binding: AdapterBinding,
}

impl SourceIdentity {
    fn from_request(request: &HistoryDiscoveryRequest) -> Self {
        Self {
            agent_id: request.agent_id().clone(),
            binding: request.binding().clone(),
        }
    }

    fn matches_load(&self, request: &HistoryLoadRequest) -> bool {
        &self.agent_id == request.agent_id() && &self.binding == request.binding()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum CandidateLocator {
    File {
        root: PathBuf,
        primary: PathBuf,
        layout: HistorySourceLayout,
    },
    Sqlite {
        database: PathBuf,
        session_id: String,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CandidateKey {
    source: SourceIdentity,
    locator: CandidateLocator,
}

#[derive(Clone, Debug)]
struct DiscoveredCandidate {
    locator: CandidateLocator,
    session_id_hint: String,
    modified_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug)]
struct CandidateRecord {
    key: CandidateKey,
    session_id_hint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ComponentSignature {
    path: PathBuf,
    present: bool,
    directory: bool,
    len: u64,
    modified_nanos: u128,
}

#[derive(Clone, Debug)]
struct LoadedDocument {
    document: HistoryDocument,
    signatures: Vec<ComponentSignature>,
}

#[derive(Clone, Debug)]
struct CacheEntry {
    loaded: LoadedDocument,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeHistoryCacheStats {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHistoryDiscoveryIssue {
    pub root_slot: usize,
    pub adapter_id: AdapterId,
    pub kind: NativeHistoryDiscoveryIssueKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeHistoryDiscoveryIssueKind {
    Inaccessible,
    EntryLimitReached,
    InvalidDatabase,
}

pub struct NativeHistoryAuthority {
    config: NativeHistoryConfig,
    candidates: HashMap<String, CandidateRecord>,
    ids_by_key: HashMap<CandidateKey, String>,
    source_order: VecDeque<SourceIdentity>,
    cache: HashMap<String, CacheEntry>,
    cache_order: VecDeque<String>,
    cache_hits: u64,
    cache_misses: u64,
    issues: Vec<NativeHistoryDiscoveryIssue>,
}

impl NativeHistoryAuthority {
    pub fn new(config: NativeHistoryConfig) -> Self {
        Self {
            config,
            candidates: HashMap::new(),
            ids_by_key: HashMap::new(),
            source_order: VecDeque::new(),
            cache: HashMap::new(),
            cache_order: VecDeque::new(),
            cache_hits: 0,
            cache_misses: 0,
            issues: Vec::new(),
        }
    }

    pub fn cache_stats(&self) -> NativeHistoryCacheStats {
        NativeHistoryCacheStats {
            entries: self.cache.len(),
            hits: self.cache_hits,
            misses: self.cache_misses,
        }
    }

    pub fn active_candidate_count(&self) -> usize {
        self.candidates.len()
    }

    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.cache_order.clear();
    }

    pub fn take_discovery_issues(&mut self) -> Vec<NativeHistoryDiscoveryIssue> {
        std::mem::take(&mut self.issues)
    }

    fn discover_candidates(
        &mut self,
        request: &HistoryDiscoveryRequest,
    ) -> Result<Vec<HistoryCandidate>, NativeHistoryError> {
        self.issues.clear();
        let source = SourceIdentity::from_request(request);
        let mut discovered = Vec::new();
        for (root_slot, root) in self.config.roots.iter().enumerate() {
            if root.adapter_id != request.binding().id {
                continue;
            }
            let result = discovery::discover_root(
                root,
                root_slot,
                self.config.limits,
                usize::from(request.limit()),
            );
            discovered.extend(result.candidates);
            self.issues.extend(result.issues);
        }
        discovery::dedupe_and_sort(&request.binding().id, &mut discovered);
        discovered.truncate(usize::from(request.limit()).min(self.config.limits.max_candidates));

        let current_locators = discovered
            .iter()
            .map(|candidate| candidate.locator.clone())
            .collect::<HashSet<_>>();
        let stale_ids = self
            .candidates
            .iter()
            .filter(|(_, record)| {
                record.key.source == source && !current_locators.contains(&record.key.locator)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in stale_ids {
            self.remove_candidate(&id);
        }
        self.evict_sources_for(&source, discovered.len())?;

        let mut candidates = Vec::with_capacity(discovered.len());
        for discovered in discovered {
            let key = CandidateKey {
                source: source.clone(),
                locator: discovered.locator.clone(),
            };
            let id = self
                .ids_by_key
                .get(&key)
                .cloned()
                .unwrap_or_else(|| self.fresh_candidate_id());
            let candidate_id = HistoryCandidateId::new(id.clone())
                .map_err(|_| NativeHistoryError::InvalidCandidate)?;
            let Ok(candidate) = HistoryCandidate::new(
                request,
                candidate_id,
                discovered.session_id_hint.clone(),
                discovered.modified_at_unix_ms,
            ) else {
                continue;
            };
            let record = CandidateRecord {
                key: key.clone(),
                session_id_hint: discovered.session_id_hint,
            };
            self.ids_by_key.insert(key, id.clone());
            self.candidates.insert(id, record);
            candidates.push(candidate);
        }
        if candidates.is_empty() {
            self.source_order.retain(|candidate| candidate != &source);
        } else {
            self.touch_source(&source);
        }
        Ok(candidates)
    }

    pub fn resume_provider_session(
        &self,
        request: &HistoryLoadRequest,
        session_id: impl Into<String>,
    ) -> Result<ProviderSessionIdentity, NativeHistoryError> {
        let id = request.candidate().id().as_str();
        let record = self
            .candidates
            .get(id)
            .ok_or(NativeHistoryError::CandidateExpired)?;
        if !record.key.source.matches_load(request)
            || record.session_id_hint != request.candidate().session_id_hint()
        {
            return Err(NativeHistoryError::CandidateSourceMismatch);
        }
        let transcript_path = if request.binding().id.as_str() == "pi" {
            let CandidateLocator::File { root, primary, .. } = &record.key.locator else {
                return Err(NativeHistoryError::CandidateSourceMismatch);
            };
            Some(
                load::validated_resume_file(root, primary)?
                    .to_str()
                    .ok_or(NativeHistoryError::InvalidUtf8)?
                    .to_owned(),
            )
        } else {
            None
        };
        Ok(ProviderSessionIdentity {
            key: if request.binding().id.as_str() == "antigravity" {
                ProviderSessionKey::ConversationId
            } else {
                ProviderSessionKey::SessionId
            },
            id: session_id.into(),
            transcript_path,
        })
    }

    fn load_candidate(
        &mut self,
        request: &HistoryLoadRequest,
    ) -> Result<HistoryDocument, NativeHistoryError> {
        let id = request.candidate().id().as_str();
        let record = self
            .candidates
            .get(id)
            .ok_or(NativeHistoryError::CandidateExpired)?
            .clone();
        if !record.key.source.matches_load(request)
            || record.session_id_hint != request.candidate().session_id_hint()
        {
            return Err(NativeHistoryError::CandidateSourceMismatch);
        }
        self.touch_source(&record.key.source);

        if let Some(entry) = self.cache.get(id).cloned() {
            if load::signatures_are_current(&entry.loaded.signatures) {
                self.cache_hits = self.cache_hits.saturating_add(1);
                self.touch_cache(id);
                return Ok(entry.loaded.document);
            }
            self.cache.remove(id);
            self.cache_order.retain(|candidate| candidate != id);
        }
        self.cache_misses = self.cache_misses.saturating_add(1);
        let loaded = load::load_document(&record, self.config.limits)?;
        let document = loaded.document.clone();
        self.cache.insert(id.to_owned(), CacheEntry { loaded });
        self.touch_cache(id);
        while self.cache.len() > self.config.limits.max_cache_entries {
            if let Some(expired) = self.cache_order.pop_front() {
                self.cache.remove(&expired);
            }
        }
        Ok(document)
    }

    fn touch_cache(&mut self, id: &str) {
        self.cache_order.retain(|candidate| candidate != id);
        self.cache_order.push_back(id.to_owned());
    }

    fn touch_source(&mut self, source: &SourceIdentity) {
        self.source_order.retain(|candidate| candidate != source);
        self.source_order.push_back(source.clone());
    }

    fn evict_sources_for(
        &mut self,
        current: &SourceIdentity,
        current_count: usize,
    ) -> Result<(), NativeHistoryError> {
        while self
            .candidates
            .values()
            .filter(|record| record.key.source != *current)
            .count()
            .saturating_add(current_count)
            > self.config.limits.max_candidates
        {
            let evicted = self
                .source_order
                .iter()
                .find(|source| *source != current)
                .cloned()
                .or_else(|| {
                    self.candidates
                        .values()
                        .find(|record| record.key.source != *current)
                        .map(|record| record.key.source.clone())
                })
                .ok_or(NativeHistoryError::CandidateCapacity)?;
            self.remove_source(&evicted);
        }
        Ok(())
    }

    fn remove_source(&mut self, source: &SourceIdentity) {
        let ids = self
            .candidates
            .iter()
            .filter(|(_, record)| record.key.source == *source)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.remove_candidate(&id);
        }
        self.source_order.retain(|candidate| candidate != source);
    }

    fn fresh_candidate_id(&self) -> String {
        loop {
            let id = format!("hist_{}", Uuid::new_v4().simple());
            if !self.candidates.contains_key(&id) {
                return id;
            }
        }
    }

    fn remove_candidate(&mut self, id: &str) {
        if let Some(record) = self.candidates.remove(id) {
            self.ids_by_key.remove(&record.key);
        }
        self.cache.remove(id);
        self.cache_order.retain(|candidate| candidate != id);
    }
}

impl HistoryAuthority for NativeHistoryAuthority {
    type Error = NativeHistoryError;

    fn discover(
        &mut self,
        request: &HistoryDiscoveryRequest,
    ) -> Result<Vec<HistoryCandidate>, Self::Error> {
        self.discover_candidates(request)
    }

    fn load(&mut self, request: &HistoryLoadRequest) -> Result<HistoryDocument, Self::Error> {
        self.load_candidate(request)
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NativeHistoryConfigError {
    #[error("history roots must be absolute")]
    RootMustBeAbsolute,
    #[error("history adapter is unsupported: {0}")]
    UnsupportedAdapter(AdapterId),
    #[error("history adapter ID is unsupported: {0}")]
    UnsupportedAdapterValue(String),
    #[error("history layout {layout:?} is not declared by adapter {adapter_id}")]
    UnsupportedLayout {
        adapter_id: AdapterId,
        layout: HistorySourceLayout,
    },
    #[error("history root count exceeds the hard bound")]
    TooManyRoots,
    #[error("duplicate history root")]
    DuplicateRoot,
    #[error("history limit cannot be zero or exceed its hard bound: {0}")]
    InvalidLimit(&'static str),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NativeHistoryError {
    #[error("history candidate capacity is exhausted")]
    CandidateCapacity,
    #[error("history candidate is invalid")]
    InvalidCandidate,
    #[error("history candidate is expired or unknown")]
    CandidateExpired,
    #[error("history candidate source does not match the request")]
    CandidateSourceMismatch,
    #[error("history source changed or escaped its authorized root")]
    SourceChanged,
    #[error("history source could not be read")]
    ReadFailed,
    #[error("history source is not valid UTF-8")]
    InvalidUtf8,
    #[error("history source exceeds the {max}-byte bound")]
    FileTooLarge { max: usize },
    #[error("history SQLite source is unavailable")]
    DatabaseUnavailable,
    #[error("history SQLite source has an unsupported schema")]
    DatabaseSchema,
}

fn transcript_limit() -> usize {
    HISTORY_DOCUMENT_MAX_BYTES
}

fn metadata_limit() -> usize {
    HISTORY_METADATA_MAX_BYTES
}
