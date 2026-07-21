//! Pre-built package-export databases for tiered, R-free export resolution.
//!
//! This module owns everything that lets Raven resolve package **export names**
//! without an installed package or a running R: one serializable record
//! ([`model::PackageRecord`]) and two on-disk encodings that decode back into the
//! existing [`crate::package_library::PackageInfo`]:
//!
//! - **Tier 2** ([`json_db`]): a committed, diff-friendly `.raven/packages.json`
//!   the user generates locally (`raven packages freeze`). "Frozen Tier 1".
//! - **Tier 3** ([`binary_db`]): a Raven-bundled, memory-mapped `names.db` built
//!   from r-universe latest. Export-only; the R-free floor.
//!
//! Consumers query these through the [`PackageMetadataProvider`] seam, which
//! `PackageLibrary` consults in tier order **after** the installed (Tier 1) path
//! misses. Providers feed *export resolution* only; they never affect
//! install-status (the missing-package diagnostic), which stays Tier-1-only.

pub mod binary_db;
pub mod embedded_base;
pub mod json_db;
pub mod merge;
pub mod model;
pub mod renv_lock;
pub mod runiverse;

#[cfg(test)]
use std::cell::RefCell;
use std::fs::File;
use std::io;
use std::path::PathBuf;

use crate::package_library::PackageInfo;

#[cfg(not(windows))]
const USER_DATA_APP_DIR_UNIX: &str = "raven";
#[cfg(windows)]
const USER_DATA_APP_DIR_WINDOWS: &str = "Raven";

/// A source of pre-built package metadata, consulted in tier order when the
/// installed (Tier 1) path does not resolve a package.
///
/// Implementations are pure, synchronous reads of pre-built data (an in-memory
/// map for Tier 2, a memory-mapped + lazily-decoded payload for Tier 3). They
/// MUST NOT block or perform I/O beyond a memory-mapped read, because the async
/// resolution path that calls them must stay cheap.
pub trait PackageMetadataProvider: Send + Sync {
    /// Return this source's `PackageInfo` for `name`, or `None` if it does not
    /// know the package.
    fn lookup(&self, name: &str) -> Option<PackageInfo>;
}

/// The result of loading a small metadata file through an already-open handle.
///
/// Keeping this seam here lets both on-disk provider formats share the same
/// replacement-race contract without coupling either parser to platform file
/// identity APIs.
#[derive(Debug)]
pub(crate) enum ThinFileLoadError<E> {
    Absent,
    Load(E),
    Io(io::Error),
    DeadlineExpired,
    ConcurrentModification,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThinFileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    readonly: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanoseconds: i64,
    #[cfg(windows)]
    volume_serial_number: u64,
    #[cfg(windows)]
    file_index: u64,
    #[cfg(windows)]
    file_attributes: u64,
    #[cfg(windows)]
    creation_time: Option<u64>,
    #[cfg(windows)]
    last_write_time: Option<u64>,
}

/// Capture identity from an open file so Windows can use stable by-handle APIs
/// for the volume serial number and file index.
fn thin_file_identity(file: &File) -> io::Result<ThinFileIdentity> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    #[cfg(windows)]
    let windows_identity = winapi_util::file::information(file)?;

    Ok(ThinFileIdentity {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        readonly: metadata.permissions().readonly(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        mode: metadata.mode(),
        #[cfg(unix)]
        change_seconds: metadata.ctime(),
        #[cfg(unix)]
        change_nanoseconds: metadata.ctime_nsec(),
        #[cfg(windows)]
        volume_serial_number: windows_identity.volume_serial_number(),
        #[cfg(windows)]
        file_index: windows_identity.file_index(),
        #[cfg(windows)]
        file_attributes: windows_identity.file_attributes(),
        #[cfg(windows)]
        creation_time: windows_identity.creation_time(),
        #[cfg(windows)]
        last_write_time: windows_identity.last_write_time(),
    })
}

/// Load a thin provider file from one opened handle and verify that the path
/// still names those exact bytes before accepting the parsed value.
///
/// A replace-in-place race is retried once from a new handle. If the second
/// attempt also races, callers receive a typed concurrent-modification result
/// rather than accidentally publishing either stale or mixed metadata. Both
/// attempts share the physical provider generation's original deadline; expiry
/// suppresses the retry and is returned distinctly.
pub(crate) fn load_thin_file_with_retry<T, E>(
    path: &std::path::Path,
    deadline: Option<std::time::Instant>,
    mut load: impl FnMut(&File) -> Result<T, E>,
) -> Result<T, ThinFileLoadError<E>> {
    for attempt in 0..2 {
        if deadline.is_some_and(|deadline| deadline <= std::time::Instant::now()) {
            return Err(ThinFileLoadError::DeadlineExpired);
        }
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ThinFileLoadError::Absent);
            }
            Err(error) => return Err(ThinFileLoadError::Io(error)),
        };
        let before = thin_file_identity(&file).map_err(ThinFileLoadError::Io)?;
        let loaded = load(&file);
        let handle_after = thin_file_identity(&file);
        let path_after = File::open(path).and_then(|file| thin_file_identity(&file));
        let unchanged = matches!(
            (&handle_after, &path_after),
            (Ok(handle), Ok(current)) if handle == &before && current == &before
        );
        if unchanged {
            return loaded.map_err(ThinFileLoadError::Load);
        }
        if attempt == 1 {
            return Err(ThinFileLoadError::ConcurrentModification);
        }
    }
    unreachable!("the thin-file loader performs exactly two attempts")
}

/// Resolve ordered `names.db` sidecar candidates.
pub fn locate_shipped_db_candidates() -> Vec<PathBuf> {
    locate_shipped_db_candidates_from(&capture_shipped_db_candidate_inputs())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ShippedDbCandidateInputs {
    override_path: Option<std::ffi::OsString>,
    user_data_dir: Option<PathBuf>,
    current_exe: Option<PathBuf>,
}

pub(crate) fn capture_shipped_db_candidate_inputs() -> ShippedDbCandidateInputs {
    ShippedDbCandidateInputs {
        override_path: std::env::var_os("RAVEN_NAMES_DB"),
        user_data_dir: user_data_dir(),
        current_exe: std::env::current_exe().ok(),
    }
}

pub(crate) fn locate_shipped_db_candidates_from(inputs: &ShippedDbCandidateInputs) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(path) = inputs
        .override_path
        .as_ref()
        .filter(|path| !path.is_empty())
    {
        out.push(PathBuf::from(path));
    }
    if let Some(dir) = inputs.user_data_dir.as_ref() {
        push_unique(&mut out, dir.join("names.db"));
    }
    if let Some(dir) = inputs.current_exe.as_ref().and_then(|path| path.parent()) {
        push_unique(&mut out, dir.join("names.db"));
    }
    out
}

pub fn user_data_sidecar_path(file_name: &str) -> Option<PathBuf> {
    user_data_dir().map(|dir| dir.join(file_name))
}

fn push_unique(out: &mut Vec<PathBuf>, path: PathBuf) {
    if !out.iter().any(|existing| existing == &path) {
        out.push(path);
    }
}

/// The Raven per-user data directory: `%LOCALAPPDATA%\Raven` on Windows,
/// `$XDG_DATA_HOME/raven` (or `$HOME/.local/share/raven`) elsewhere.
///
/// Hand-rolled rather than via the `xdg` crate on purpose: `xdg` is a
/// unix-only dependency, so using it would cover only the non-Windows arm and
/// split this one cfg-unified resolver into two mechanisms. The unix rule is
/// factored into [`unix_user_data_dir`] so it can be unit-tested with injected
/// env values without touching the process environment.
fn user_data_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(dir) = TEST_USER_DATA_DIR.with(|cell| cell.borrow().clone()) {
        return Some(dir);
    }

    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .and_then(absolute_non_empty_path)
            .map(|p| p.join(USER_DATA_APP_DIR_WINDOWS))
    }

    #[cfg(not(windows))]
    {
        unix_user_data_dir(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
    }
}

/// Derive the Unix user-data directory from `XDG_DATA_HOME` / `HOME` values:
/// an absolute, non-empty `XDG_DATA_HOME` wins, otherwise `HOME/.local/share`.
/// Takes the env values as parameters so both `user_data_dir` and its tests
/// exercise one copy of the rule.
#[cfg(not(windows))]
fn unix_user_data_dir(
    xdg_data_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(path) = xdg_data_home.and_then(absolute_non_empty_path) {
        return Some(path.join(USER_DATA_APP_DIR_UNIX));
    }
    home.and_then(absolute_non_empty_path).map(|home| {
        home.join(".local")
            .join("share")
            .join(USER_DATA_APP_DIR_UNIX)
    })
}

fn absolute_non_empty_path(value: std::ffi::OsString) -> Option<PathBuf> {
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
thread_local! {
    static TEST_USER_DATA_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct TestUserDataDirGuard {
    previous: Option<PathBuf>,
}

#[cfg(test)]
pub(crate) fn test_user_data_dir_guard(path: PathBuf) -> TestUserDataDirGuard {
    let previous = TEST_USER_DATA_DIR.with(|cell| cell.replace(Some(path)));
    TestUserDataDirGuard { previous }
}

#[cfg(test)]
impl Drop for TestUserDataDirGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        TEST_USER_DATA_DIR.with(|cell| {
            cell.replace(previous);
        });
    }
}

/// Serializes tests that mutate the process-global package-DB env var
/// (`RAVEN_NAMES_DB`). Without this, parallel test threads race: one test's
/// `set_var` / `remove_var` window can be observed by another's
/// `build_package_library` / `initialize` call (or `locate_shipped_db_candidates`),
/// producing spurious failures. Every test in the crate's lib test binary that
/// touches that var MUST hold this lock. An async (`tokio`) mutex is required
/// because some holders keep the guard across an `.await` on the build. Lives
/// here (not in a test submodule) so both `package_db` and `package_library`
/// tests can share the one instance.
#[cfg(test)]
pub(crate) static RAVEN_NAMES_DB_ENV_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

/// RAII guard for the process-global `RAVEN_NAMES_DB` var in tests: sets it on
/// construction and restores the prior value (or unsets it) on drop, so a
/// panicking assertion can't leak the var to a sibling test. Callers MUST hold
/// [`RAVEN_NAMES_DB_ENV_LOCK`] for the guard's whole lifetime — that lock is
/// what makes the `set_var`/`remove_var` sound, by serializing every test in
/// this binary that reads or writes the var. This concentrates the one
/// `unsafe` env mutation (edition 2024 made `set_var`/`remove_var` unsafe) in a
/// single audited place instead of repeating it at each call site.
#[cfg(test)]
pub(crate) struct NamesDbEnvGuard {
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl NamesDbEnvGuard {
    pub(crate) fn set(value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os("RAVEN_NAMES_DB");
        // SAFETY: the caller holds `RAVEN_NAMES_DB_ENV_LOCK` (see type doc), so
        // no other thread reads or writes the environment concurrently.
        unsafe { std::env::set_var("RAVEN_NAMES_DB", value) };
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for NamesDbEnvGuard {
    fn drop(&mut self) {
        // SAFETY: mirrors `set`; the lock is still held for the guard's lifetime.
        unsafe {
            match self.previous.take() {
                Some(prev) => std::env::set_var("RAVEN_NAMES_DB", prev),
                None => std::env::remove_var("RAVEN_NAMES_DB"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[tokio::test]
    async fn sidecar_candidates_prefer_env_then_user_data_then_exe_relative() {
        let _env_guard = RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let user_data = dir.path().join("data");
        let custom = dir.path().join("custom.db");

        let _db_env = NamesDbEnvGuard::set(&custom);
        let _user_data_guard = test_user_data_dir_guard(user_data.clone());
        let candidates = locate_shipped_db_candidates();
        let exe_relative = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join("names.db");

        assert_eq!(candidates[0], custom);
        assert_eq!(candidates[1], user_data.join("names.db"));
        assert!(candidates[2..].contains(&exe_relative));
    }

    #[tokio::test]
    async fn empty_env_does_not_shadow_user_data_sidecar() {
        let _env_guard = RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let user_data = dir.path().join("data");

        let _db_env = NamesDbEnvGuard::set("");
        let _user_data_guard = test_user_data_dir_guard(user_data.clone());
        let first = locate_shipped_db_candidates().remove(0);

        assert_eq!(first, user_data.join("names.db"));
    }

    #[cfg(not(windows))]
    #[test]
    fn user_data_roots_ignore_empty_and_relative_values() {
        assert_eq!(
            unix_user_data_dir(Some("".into()), Some("/home/me".into())),
            Some(PathBuf::from("/home/me/.local/share/raven"))
        );
        assert_eq!(
            unix_user_data_dir(Some("relative".into()), Some("/home/me".into())),
            Some(PathBuf::from("/home/me/.local/share/raven"))
        );
        assert_eq!(unix_user_data_dir(None, Some("relative-home".into())), None);
        assert_eq!(
            unix_user_data_dir(Some("/xdg".into()), Some("/home/me".into())),
            Some(PathBuf::from("/xdg/raven"))
        );
        assert_eq!(
            unix_user_data_dir(None, Some("/home/me".into())),
            Some(PathBuf::from("/home/me/.local/share/raven"))
        );
    }

    #[test]
    fn thin_file_loader_retries_one_opened_file_identity_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("thin.db");
        std::fs::write(&path, "old").unwrap();
        let mut attempts = 0;
        let loaded = load_thin_file_with_retry(&path, None, |file| {
            attempts += 1;
            let mut value = String::new();
            let mut file = file;
            file.read_to_string(&mut value).unwrap();
            if attempts == 1 {
                std::fs::write(&path, "new-longer").unwrap();
            }
            Ok::<_, std::convert::Infallible>(value)
        })
        .unwrap();

        assert_eq!(attempts, 2);
        assert_eq!(loaded, "new-longer");
    }

    #[test]
    fn thin_file_loader_reports_typed_concurrent_modification_after_retry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("thin.db");
        std::fs::write(&path, "initial").unwrap();
        let mut attempts = 0;
        let result = load_thin_file_with_retry(&path, None, |file| {
            attempts += 1;
            let mut value = String::new();
            let mut file = file;
            file.read_to_string(&mut value).unwrap();
            std::fs::write(&path, "x".repeat(20 + attempts)).unwrap();
            Ok::<_, std::convert::Infallible>(value)
        });

        assert_eq!(attempts, 2);
        assert!(matches!(
            result,
            Err(ThinFileLoadError::ConcurrentModification)
        ));
    }

    #[test]
    fn thin_file_retry_stays_within_original_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("thin.db");
        std::fs::write(&path, "initial").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
        let mut attempts = 0;
        let result = load_thin_file_with_retry(&path, Some(deadline), |file| {
            attempts += 1;
            let mut value = String::new();
            let mut file = file;
            file.read_to_string(&mut value).unwrap();
            std::fs::write(&path, "replacement-is-longer").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(30));
            Ok::<_, std::convert::Infallible>(value)
        });

        assert_eq!(attempts, 1, "expiry prevents the identity-race retry");
        assert!(matches!(result, Err(ThinFileLoadError::DeadlineExpired)));
    }
}
