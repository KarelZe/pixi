use super::{extract_executable_from_script, EnvironmentName, ExposedName, Mapping};
use ahash::HashSet;
use console::StyledObject;
use fancy_display::FancyDisplay;
use fs_err as fs;
use fs_err::tokio as tokio_fs;
use indexmap::{IndexMap, IndexSet};
use is_executable::IsExecutable;
use itertools::Itertools;
use miette::{Context, IntoDiagnostic};
use pixi_config::home_path;
use pixi_manifest::PrioritizedChannel;
use pixi_utils::executable_from_path;
use rattler_conda_types::{
    Channel, ChannelConfig, NamedChannelOrUrl, PackageName, PackageRecord, PrefixRecord,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::str::FromStr;
use std::{
    io::Read,
    path::{Path, PathBuf},
};
use url::Url;

/// Global binaries directory, default to `$HOME/.pixi/bin`
#[derive(Debug, Clone)]
pub struct BinDir(PathBuf);

impl BinDir {
    /// Create the binary executable directory from path
    #[cfg(test)]
    pub fn new(root: PathBuf) -> miette::Result<Self> {
        let path = root.join("bin");
        std::fs::create_dir_all(&path).into_diagnostic()?;
        Ok(Self(path))
    }

    /// Create the binary executable directory from environment variables
    pub async fn from_env() -> miette::Result<Self> {
        let bin_dir = home_path()
            .map(|path| path.join("bin"))
            .ok_or(miette::miette!(
                "Couldn't determine global binary executable directory"
            ))?;
        tokio_fs::create_dir_all(&bin_dir).await.into_diagnostic()?;
        Ok(Self(bin_dir))
    }

    /// Asynchronously retrieves all files in the binary executable directory.
    ///
    /// This function reads the directory specified by `self.0` and collects all
    /// file paths into a vector. It returns a `miette::Result` containing the
    /// vector of file paths or an error if the directory can't be read.
    pub(crate) async fn files(&self) -> miette::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        let mut entries = tokio_fs::read_dir(&self.0).await.into_diagnostic()?;

        while let Some(entry) = entries.next_entry().await.into_diagnostic()? {
            let path = entry.path();
            if path.is_file() && path.is_executable() && is_text(&path)? {
                files.push(path);
            }
        }

        Ok(files)
    }

    /// Returns the path to the binary directory
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Returns the path to the executable script for the given exposed name.
    ///
    /// This function constructs the path to the executable script by joining the
    /// `bin_dir` with the provided `exposed_name`. If the target platform is
    /// Windows, it sets the file extension to `.bat`.
    pub(crate) fn executable_script_path(&self, exposed_name: &ExposedName) -> PathBuf {
        // Add .bat to the windows executable
        let exposed_name = if cfg!(windows) {
            // Not using `.set_extension()` because it will break the `.` in the name for cases like `python3.9.1`
            format!("{}.bat", exposed_name)
        } else {
            exposed_name.to_string()
        };
        self.path().join(exposed_name)
    }
}

/// Global environments directory, default to `$HOME/.pixi/envs`
#[derive(Debug, Clone)]
pub struct EnvRoot(PathBuf);

impl EnvRoot {
    /// Create the environment root directory
    #[cfg(test)]
    pub fn new(root: PathBuf) -> miette::Result<Self> {
        let path = root.join("envs");
        std::fs::create_dir_all(&path).into_diagnostic()?;
        Ok(Self(path))
    }

    /// Create the environment root directory from environment variables
    pub(crate) async fn from_env() -> miette::Result<Self> {
        let path = home_path()
            .map(|path| path.join("envs"))
            .ok_or_else(|| miette::miette!("Couldn't get home path"))?;
        tokio_fs::create_dir_all(&path).await.into_diagnostic()?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Get all directories in the env root
    pub(crate) async fn directories(&self) -> miette::Result<Vec<PathBuf>> {
        let mut directories = Vec::new();
        let mut entries = tokio_fs::read_dir(&self.path()).await.into_diagnostic()?;

        while let Some(entry) = entries.next_entry().await.into_diagnostic()? {
            let path = entry.path();
            if path.is_dir() {
                directories.push(path);
            }
        }

        Ok(directories)
    }
}

/// A global environment directory
pub(crate) struct EnvDir {
    pub(crate) path: PathBuf,
}

impl EnvDir {
    // Create EnvDir from path
    pub(crate) fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// Create a global environment directory based on passed global environment root
    pub(crate) async fn from_env_root(
        env_root: EnvRoot,
        environment_name: &EnvironmentName,
    ) -> miette::Result<Self> {
        let path = env_root.path().join(environment_name.as_str());
        tokio_fs::create_dir_all(&path).await.into_diagnostic()?;

        Ok(Self { path })
    }

    /// Construct the path to the env directory for the environment
    /// `environment_name`.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Checks if a file is binary by reading the first 1024 bytes and checking for null bytes.
pub(crate) fn is_binary(file_path: impl AsRef<Path>) -> miette::Result<bool> {
    let mut file = fs::File::open(file_path.as_ref()).into_diagnostic()?;
    let mut buffer = [0; 1024];
    let bytes_read = file.read(&mut buffer).into_diagnostic()?;

    Ok(buffer[..bytes_read].contains(&0))
}

/// Checks if given path points to a text file by calling `is_binary`.
/// If that returns `false`, then it is a text file and vice-versa.
pub(crate) fn is_text(file_path: impl AsRef<Path>) -> miette::Result<bool> {
    Ok(!is_binary(file_path)?)
}

/// Finds the package record from the `conda-meta` directory.
pub(crate) async fn find_package_records(conda_meta: &Path) -> miette::Result<Vec<PrefixRecord>> {
    let mut read_dir = tokio_fs::read_dir(conda_meta).await.into_diagnostic()?;
    let mut records = Vec::new();

    while let Some(entry) = read_dir.next_entry().await.into_diagnostic()? {
        let path = entry.path();
        // Check if the entry is a file and has a .json extension
        if path.is_file() && path.extension().and_then(OsStr::to_str) == Some("json") {
            let prefix_record = PrefixRecord::from_path(&path)
                .into_diagnostic()
                .wrap_err_with(|| format!("Couldn't parse json from {}", path.display()))?;

            records.push(prefix_record);
        }
    }

    if records.is_empty() {
        miette::bail!("No package records found in {}", conda_meta.display());
    }

    Ok(records)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotChangedReason {
    AlreadyInstalled,
}

impl std::fmt::Display for NotChangedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotChangedReason::AlreadyInstalled => write!(f, "already installed"),
        }
    }
}

impl NotChangedReason {
    /// Returns the name of the environment.
    pub fn as_str(&self) -> &str {
        match self {
            NotChangedReason::AlreadyInstalled => "already installed",
        }
    }
}

impl FancyDisplay for NotChangedReason {
    fn fancy_display(&self) -> StyledObject<&str> {
        console::style(self.as_str()).cyan()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnvState {
    Installed,
    NotChanged(NotChangedReason),
}

impl EnvState {
    pub fn as_str(&self) -> &str {
        match self {
            EnvState::Installed => "installed",
            EnvState::NotChanged(reason) => reason.as_str(),
        }
    }
}

impl FancyDisplay for EnvState {
    fn fancy_display(&self) -> StyledObject<&str> {
        match self {
            EnvState::Installed => console::style(self.as_str()).green(),
            EnvState::NotChanged(ref reason) => reason.fancy_display(),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct EnvChanges {
    pub changes: HashMap<EnvironmentName, EnvState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub(crate) enum StateChange {
    AddedExposed(ExposedName),
    RemovedExposed(ExposedName),
    UpdatedExposed(ExposedName),
    AddedPackage(PackageRecord),
    AddedEnvironment,
    RemovedEnvironment,
    UpdatedEnvironment,
}

#[must_use]
#[derive(Debug, Default)]
pub(crate) struct StateChanges {
    changes: HashMap<EnvironmentName, Vec<StateChange>>,
}

impl StateChanges {
    /// Creates a new `StateChanges` instance with a single environment name and an empty vector as its value.
    pub(crate) fn new_with_env(env_name: EnvironmentName) -> Self {
        Self {
            changes: HashMap::from([(env_name, Vec::new())]),
        }
    }

    /// Checks if there are any changes in the state.
    pub(crate) fn has_changed(&self) -> bool {
        !self.changes.values().all(Vec::is_empty)
    }

    pub(crate) fn insert_change(&mut self, env_name: &EnvironmentName, change: StateChange) {
        if let Some(entry) = self.changes.get_mut(env_name) {
            entry.push(change);
        } else {
            self.changes.insert(env_name.clone(), Vec::from([change]));
        }
    }

    pub(crate) fn push_changes(
        &mut self,
        env_name: &EnvironmentName,
        changes: impl IntoIterator<Item = StateChange>,
    ) {
        if let Some(entry) = self.changes.get_mut(env_name) {
            entry.extend(changes);
        } else {
            self.changes
                .insert(env_name.clone(), changes.into_iter().collect());
        }
    }

    #[cfg(test)]
    pub fn changes(self) -> HashMap<EnvironmentName, Vec<StateChange>> {
        self.changes
    }

    /// Remove changes that cancel each other out
    fn prune(&mut self) {
        self.changes = self
            .changes
            .iter()
            .map(|(env, changes_for_env)| {
                // Remove changes if the environment is removed afterwards
                let mut pruned_changes: Vec<StateChange> = Vec::new();
                for change in changes_for_env {
                    if let StateChange::RemovedEnvironment = change {
                        pruned_changes.clear();
                    }
                    pruned_changes.push(change.clone());
                }
                (env.clone(), pruned_changes)
            })
            .collect()
    }

    pub(crate) fn report(&mut self) {
        self.prune();

        for (env_name, changes_for_env) in &self.changes {
            // If there are no changes for the environment, skip it
            if changes_for_env.is_empty() {
                continue;
            }

            let mut iter = changes_for_env.iter().peekable();

            while let Some(change) = iter.next() {
                match change {
                    StateChange::AddedExposed(exposed) => {
                        let mut exposed_names = vec![exposed.clone()];
                        while let Some(StateChange::AddedExposed(next_exposed)) = iter.peek() {
                            exposed_names.push(next_exposed.clone());
                            iter.next();
                        }
                        if exposed_names.len() == 1 {
                            eprintln!(
                                "{}Exposed executable {} from environment {}.",
                                console::style(console::Emoji("✔ ", "")).green(),
                                exposed_names[0].fancy_display(),
                                env_name.fancy_display()
                            );
                        } else {
                            eprintln!(
                                "{}Exposed executables from environment {}:",
                                console::style(console::Emoji("✔ ", "")).green(),
                                env_name.fancy_display()
                            );
                            for exposed_name in exposed_names {
                                eprintln!("   - {}", exposed_name.fancy_display());
                            }
                        }
                    }
                    StateChange::RemovedExposed(exposed) => {
                        eprintln!(
                            "{}Removed exposed executable {} from environment {}.",
                            console::style(console::Emoji("✔ ", "")).green(),
                            exposed.fancy_display(),
                            env_name.fancy_display()
                        );
                    }
                    StateChange::UpdatedExposed(exposed) => {
                        let mut exposed_names = vec![exposed.clone()];
                        while let Some(StateChange::AddedExposed(next_exposed)) = iter.peek() {
                            exposed_names.push(next_exposed.clone());
                            iter.next();
                        }
                        if exposed_names.len() == 1 {
                            eprintln!(
                                "{}Updated executable {} of environment {}.",
                                console::style(console::Emoji("✔ ", "")).green(),
                                exposed_names[0].fancy_display(),
                                env_name.fancy_display()
                            );
                        } else {
                            eprintln!(
                                "{}Updated executables of environment {}:",
                                console::style(console::Emoji("✔ ", "")).green(),
                                env_name.fancy_display()
                            );
                            for exposed_name in exposed_names {
                                eprintln!("   - {}", exposed_name.fancy_display());
                            }
                        }
                    }
                    StateChange::AddedPackage(pkg) => {
                        eprintln!(
                            "{}Added package {}={} to environment {}.",
                            console::style(console::Emoji("✔ ", "")).green(),
                            console::style(pkg.name.as_normalized()).green(),
                            console::style(&pkg.version).blue(),
                            env_name.fancy_display()
                        );
                    }
                    StateChange::AddedEnvironment => {
                        eprintln!(
                            "{}Added environment {}.",
                            console::style(console::Emoji("✔ ", "")).green(),
                            env_name.fancy_display()
                        );
                    }
                    StateChange::RemovedEnvironment => {
                        eprintln!(
                            "{}Removed environment {}.",
                            console::style(console::Emoji("✔ ", "")).green(),
                            env_name.fancy_display()
                        );
                    }
                    StateChange::UpdatedEnvironment => {
                        eprintln!(
                            "{}Updated environment {}.",
                            console::style(console::Emoji("✔ ", "")).green(),
                            env_name.fancy_display()
                        );
                    }
                }
            }
        }
    }
}

impl std::ops::BitOrAssign for StateChanges {
    fn bitor_assign(&mut self, rhs: Self) {
        for (env_name, changes_for_env) in rhs.changes {
            self.changes
                .entry(env_name)
                .or_default()
                .extend(changes_for_env);
        }
    }
}

/// converts a channel url string to a PrioritizedChannel
pub(crate) fn channel_url_to_prioritized_channel(
    channel: &str,
    channel_config: &ChannelConfig,
) -> miette::Result<PrioritizedChannel> {
    // If channel url contains channel config alias as a substring, don't use it as a URL
    if channel.contains(channel_config.channel_alias.as_str()) {
        // Create channel from URL for parsing
        let channel = Channel::from_url(Url::from_str(channel).expect("channel should be url"));
        // If it has a name return as named channel
        if let Some(name) = channel.name {
            // If the channel has a name, use it as the channel
            return Ok(NamedChannelOrUrl::from_str(&name).into_diagnostic()?.into());
        }
    }
    // If channel doesn't contain the alias or has no name, use it as a URL
    Ok(NamedChannelOrUrl::from_str(channel)
        .into_diagnostic()?
        .into())
}

/// Figures out what the status is of the exposed binaries of the environment.
///
/// Returns a tuple of the exposed binaries to remove and the exposed binaries to add.
pub(crate) async fn get_expose_scripts_sync_status(
    bin_dir: &BinDir,
    env_dir: &EnvDir,
    mappings: &IndexSet<Mapping>,
) -> miette::Result<(IndexSet<PathBuf>, IndexSet<ExposedName>)> {
    // Get all paths to the binaries from the scripts in the bin directory.
    let locally_exposed = bin_dir.files().await?;
    let executable_paths = futures::future::join_all(locally_exposed.iter().map(|path| {
        let path = path.clone();
        async move {
            extract_executable_from_script(&path)
                .await
                .ok()
                .map(|exec| (path, exec))
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect_vec();

    // Filter out all binaries that are related to the environment
    let related = executable_paths
        .into_iter()
        .filter(|(_, exec)| exec.starts_with(env_dir.path()))
        .collect_vec();

    fn match_mapping(mapping: &Mapping, exposed: &Path, executable: &Path) -> bool {
        executable_from_path(exposed) == mapping.exposed_name().to_string()
            && executable_from_path(executable) == mapping.executable_name()
    }

    // Get all related expose scripts not required by the environment manifest
    let to_remove = related
        .iter()
        .filter_map(|(exposed, executable)| {
            if mappings
                .iter()
                .any(|mapping| match_mapping(mapping, exposed, executable))
            {
                None
            } else {
                Some(exposed)
            }
        })
        .cloned()
        .collect::<IndexSet<PathBuf>>();

    // Get all required exposed binaries that are not yet exposed
    let to_add = mappings
        .iter()
        .filter_map(|mapping| {
            if related
                .iter()
                .any(|(exposed, executable)| match_mapping(mapping, exposed, executable))
            {
                None
            } else {
                Some(mapping.exposed_name().clone())
            }
        })
        .collect::<IndexSet<ExposedName>>();

    Ok((to_remove, to_add))
}

/// Check if all binaries were exposed, or if the user selected a subset of them.
pub fn check_all_exposed(
    env_binaries: &IndexMap<PackageName, Vec<(String, PathBuf)>>,
    exposed_mapping_binaries: &IndexSet<Mapping>,
) -> bool {
    let mut env_binaries_names_iter = env_binaries.values().flatten().map(|(name, _)| name);

    let exposed_binaries_names: HashSet<&str> = exposed_mapping_binaries
        .iter()
        .map(|mapping| mapping.executable_name())
        .collect();

    let auto_exposed =
        env_binaries_names_iter.all(|name| exposed_binaries_names.contains(&name.as_str()));

    auto_exposed
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_create() {
        // Create a temporary directory
        let temp_dir = tempdir().unwrap();

        // Set the env root to the temporary directory
        let env_root = EnvRoot::new(temp_dir.path().to_owned()).unwrap();

        // Define a test environment name
        let environment_name = &EnvironmentName::from_str("test-env").unwrap();

        // Create a new binary env dir
        let bin_env_dir = EnvDir::from_env_root(env_root, environment_name)
            .await
            .unwrap();

        // Verify that the directory was created
        assert!(bin_env_dir.path().exists());
        assert!(bin_env_dir.path().is_dir());
    }

    #[tokio::test]
    async fn test_find_package_record() {
        // Get meta file from test data folder relative to the current file
        let dummy_conda_meta_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("global")
            .join("test_data")
            .join("conda-meta");
        // Find the package record
        let records = find_package_records(&dummy_conda_meta_path).await.unwrap();

        // Verify that the package record was found
        assert!(records
            .iter()
            .any(|rec| rec.repodata_record.package_record.name.as_normalized() == "python"));
    }

    #[test]
    fn test_channel_url_to_prioritized_channel() {
        let channel_config = ChannelConfig {
            channel_alias: Url::from_str("https://conda.anaconda.org").unwrap(),
            root_dir: PathBuf::from("/tmp"),
        };
        // Same host as alias
        let channel = "https://conda.anaconda.org/conda-forge";
        let prioritized_channel =
            channel_url_to_prioritized_channel(channel, &channel_config).unwrap();
        assert_eq!(
            PrioritizedChannel::from(NamedChannelOrUrl::from_str("conda-forge").unwrap()),
            prioritized_channel
        );

        // Different host
        let channel = "https://prefix.dev/conda-forge";
        let prioritized_channel =
            channel_url_to_prioritized_channel(channel, &channel_config).unwrap();
        assert_eq!(
            PrioritizedChannel::from(
                NamedChannelOrUrl::from_str("https://prefix.dev/conda-forge").unwrap()
            ),
            prioritized_channel
        );

        // File URL
        let channel = "file:///C:/Users/user/channel/output";
        let prioritized_channel =
            channel_url_to_prioritized_channel(channel, &channel_config).unwrap();
        assert_eq!(
            PrioritizedChannel::from(
                NamedChannelOrUrl::from_str("file:///C:/Users/user/channel/output").unwrap()
            ),
            prioritized_channel
        );
    }

    #[rstest]
    #[case("python3.9.1")]
    #[case("python3.9")]
    #[case("python3")]
    #[case("python")]
    fn test_executable_script_path(#[case] exposed_name: &str) {
        let path = PathBuf::from("/home/user/.pixi/bin");
        let bin_dir = BinDir(path.clone());
        let exposed_name = ExposedName::from_str(exposed_name).unwrap();
        let executable_script_path = bin_dir.executable_script_path(&exposed_name);

        if cfg!(windows) {
            let expected = format!("{}.bat", exposed_name);
            assert_eq!(executable_script_path, path.join(expected));
        } else {
            assert_eq!(executable_script_path, path.join(exposed_name.to_string()));
        }
    }

    #[tokio::test]
    async fn test_get_expose_scripts_sync_status() {
        let tmp_home_dir = tempfile::tempdir().unwrap();
        let tmp_home_dir_path = tmp_home_dir.path().to_path_buf();
        let env_root = EnvRoot::new(tmp_home_dir_path.clone()).unwrap();
        let env_name = EnvironmentName::from_str("test").unwrap();
        let env_dir = EnvDir::from_env_root(env_root, &env_name).await.unwrap();
        let bin_dir = BinDir::new(tmp_home_dir_path.clone()).unwrap();

        // Test empty
        let exposed = IndexSet::new();
        let (to_remove, to_add) = get_expose_scripts_sync_status(&bin_dir, &env_dir, &exposed)
            .await
            .unwrap();
        assert!(to_remove.is_empty());
        assert!(to_add.is_empty());

        // Test with exposed
        let mut exposed = IndexSet::new();
        exposed.insert(Mapping::new(
            ExposedName::from_str("test").unwrap(),
            "test".to_string(),
        ));
        let (to_remove, to_add) = get_expose_scripts_sync_status(&bin_dir, &env_dir, &exposed)
            .await
            .unwrap();
        assert!(to_remove.is_empty());
        assert_eq!(to_add.len(), 1);

        // Add a script to the bin directory
        let script_path = if cfg!(windows) {
            bin_dir.path().join("test.bat")
        } else {
            bin_dir.path().join("test")
        };

        #[cfg(windows)]
        {
            let script = format!(
                r#"
            @"{}" %*
            "#,
                env_dir
                    .path()
                    .join("bin")
                    .join("test.exe")
                    .to_string_lossy()
            );
            tokio_fs::write(&script_path, script).await.unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let script = format!(
                r#"#!/bin/sh
            "{}" "$@"
            "#,
                env_dir.path().join("bin").join("test").to_string_lossy()
            );
            tokio_fs::write(&script_path, script).await.unwrap();
            // Set the file permissions to make it executable
            let metadata = tokio_fs::metadata(&script_path).await.unwrap();
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o755); // rwxr-xr-x
            tokio_fs::set_permissions(&script_path, permissions)
                .await
                .unwrap();
        };

        let (to_remove, to_add) = get_expose_scripts_sync_status(&bin_dir, &env_dir, &exposed)
            .await
            .unwrap();
        assert!(to_remove.is_empty());
        assert!(to_add.is_empty());

        // Test to_remove
        let (to_remove, to_add) =
            get_expose_scripts_sync_status(&bin_dir, &env_dir, &IndexSet::new())
                .await
                .unwrap();
        assert_eq!(to_remove.len(), 1);
        assert!(to_add.is_empty());
    }
}