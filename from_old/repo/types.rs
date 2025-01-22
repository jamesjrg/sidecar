use std::{
    fmt::Display,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::SystemTime,
};

use anyhow::Context;
use serde::{de::Error, Deserialize, Deserializer, Serialize, Serializer};
use tracing::info;

use super::state::RepoError;

#[derive(Debug)]
pub struct RepoMetadata {
    // keep track of the last commit timestamp here and nothing else for now
    pub last_commit_unix_secs: Option<i64>,

    // The commit hash we indexed on
    pub commit_hash: String,
}

// Types of repo
#[derive(Serialize, Deserialize, Hash, PartialEq, Eq, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Local,
    // Github, (We don't support this yet)
}

// Repository identifier
#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub struct RepoRef {
    pub backend: Backend,
    pub name: String,
}

impl RepoRef {
    pub fn new(backend: Backend, name: &(impl AsRef<str> + ?Sized)) -> Result<Self, RepoError> {
        let path = Path::new(name.as_ref());

        if !path.is_absolute() {
            return Err(RepoError::NonAbsoluteLocal);
        }

        for component in path.components() {
            use std::path::Component::*;
            match component {
                CurDir | ParentDir => return Err(RepoError::InvalidPath),
                _ => continue,
            }
        }

        Ok(RepoRef {
            backend,
            name: name.as_ref().to_owned(),
        })
    }

    pub fn local_path(&self) -> Option<PathBuf> {
        match self.backend {
            Backend::Local => Some(PathBuf::from(&self.name)),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_local(&self) -> bool {
        matches!(self.backend, Backend::Local)
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub fn indexed_name(&self) -> String {
        match self.backend {
            Backend::Local => Path::new(&self.name)
                .file_name()
                .expect("last component is `..`")
                .to_string_lossy()
                .into(),
        }
    }
}

impl<P: AsRef<Path>> From<&P> for RepoRef {
    fn from(path: &P) -> Self {
        RepoRef {
            backend: Backend::Local,
            name: path.as_ref().to_string_lossy().to_string(),
        }
    }
}

impl Display for RepoRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.backend() {
            Backend::Local => write!(f, "local/{}", self.name()),
        }
    }
}

impl FromStr for RepoRef {
    type Err = RepoError;

    fn from_str(refstr: &str) -> Result<Self, Self::Err> {
        info!("Parsing repo ref: {}", refstr);
        match refstr.trim_start_matches('/').split_once('/') {
            // // github.com/...
            // Some(("github.com", name)) => RepoRef::new(Backend::Github, name),
            // local/...
            Some(("local", name)) => RepoRef::new(Backend::Local, name),
            _ => Err(RepoError::InvalidBackend),
        }
    }
}

impl Serialize for RepoRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for RepoRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).and_then(|s| {
            RepoRef::from_str(s.as_str()).map_err(|e| D::Error::custom(e.to_string()))
        })
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Debug, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// There was an error during last sync & index
    Error { message: String },

    /// Repository is not yet managed by bloop
    Uninitialized,

    /// The user requested cancelling the process
    Cancelling,

    /// Last sync & index cancelled by the user
    Cancelled,

    /// Queued for sync & index
    Queued,

    /// Active VCS operation in progress
    Syncing,

    /// Active indexing in progress
    Indexing,

    /// Successfully indexed
    Done,

    /// Removed from the index
    Removed,

    /// This was removed from the remote url, so yolo
    RemoteRemoved,
}

impl SyncStatus {
    pub fn indexable(&self) -> bool {
        matches!(self, Self::Done | Self::Queued | Self::Error { .. })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Repository {
    pub disk_path: PathBuf,
    pub sync_status: SyncStatus,
    pub last_commit_unix_secs: i64,
    pub last_index_unix_secs: u64,
}

impl Repository {
    /// Marks the repository for removal on the next sync
    /// Does not initiate a new sync.
    pub(crate) fn mark_removed(&mut self) {
        self.sync_status = SyncStatus::Removed;
    }

    /// Marks the repository for indexing on the next sync
    /// Does not initiate a new sync.
    pub(crate) fn mark_queued(&mut self) {
        self.sync_status = SyncStatus::Queued;
    }

    pub(crate) fn local_from(repo_ref: &RepoRef) -> Self {
        let disk_path = repo_ref.local_path().unwrap();

        // TODO(codestory): Add the last commit timestamp here because we are passing
        // 0 right now :|
        Self {
            sync_status: SyncStatus::Queued,
            last_index_unix_secs: 0,
            last_commit_unix_secs: 0,
            disk_path,
        }
    }

    pub(crate) fn sync_done_with(&mut self, metadata: Arc<RepoMetadata>) {
        self.last_index_unix_secs = get_unix_time(SystemTime::now());
        self.last_commit_unix_secs = metadata.last_commit_unix_secs.unwrap_or(0);

        self.sync_status = SyncStatus::Done;
    }

    /// Pre-scan the repository to provide supporting metadata for a
    /// new indexing operation
    pub async fn get_repo_metadata(&self) -> Arc<RepoMetadata> {
        let last_commit_unix_secs = gix::open(&self.disk_path)
            .context("failed to open git repo")
            .and_then(|repo| Ok(repo.head()?.peel_to_commit_in_place()?.time()?.seconds))
            .ok();

        // This is the commit hash which we want to use
        let commit_hash = gix::open(&self.disk_path)
            .context("failed to open git repo")
            .and_then(|repo| Ok(repo.head()?.peel_to_commit_in_place()?.id().to_string()))
            .ok();

        RepoMetadata {
            last_commit_unix_secs,
            commit_hash: commit_hash.unwrap_or("not_found".to_owned()),
        }
        .into()
    }
}

fn get_unix_time(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time error")
        .as_secs()
}
