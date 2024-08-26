use std::{
    collections::HashMap,
    io,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::ExitStatus,
};

use arguments::FirecrackerConfigOverride;
use async_trait::async_trait;
use installation::FirecrackerInstallation;
use jailed::JailRenamerError;
use tokio::{fs, process::Child, task::JoinError};

use crate::shell_spawner::ShellSpawner;

pub mod arguments;
pub mod command_modifier;
pub mod installation;
pub mod jailed;
pub mod unrestricted;

#[derive(Debug, thiserror::Error)]
pub enum VmmExecutorError {
    #[error("An async I/O error occurred: `{0}`")]
    IoError(io::Error),
    #[error("Forking an auxiliary shell via the spawner to force chown/mkdir failed: `{0}`")]
    ShellForkFailed(io::Error),
    #[error("A forced shell chown command exited with a non-zero exit status: `{0}`")]
    ChownExitedWithWrongStatus(ExitStatus),
    #[error("A forced shell mkdir command exited with a non-zero exit status: `{0}`")]
    MkdirExitedWithWrongStatus(ExitStatus),
    #[error("Joining on a spawned async task failed: `{0}`")]
    TaskJoinFailed(JoinError),
    #[error("Spawning an auxiliary or primary shell via the spawner failed: `{0}`")]
    ShellSpawnFailed(io::Error),
    #[error("A passed-in resource at the path `{0}` was expected but doesn't exist or isn't accessible")]
    ExpectedResourceMissing(PathBuf),
    #[error("A directory that is supposed to have a parent in the filesystem has none")]
    ExpectedDirectoryParentMissing,
    #[error("Invoking the jail renamer to produce an inner path failed: `{0}`")]
    JailRenamerFailed(JailRenamerError),
    #[error("Another error occurred: `{0}`")]
    Other(Box<dyn std::error::Error + Send>),
}

/// A VMM executor is layer 2 of FCTools: manages a VMM process by setting up the environment, correctly invoking
/// the process and cleaning up the environment. This allows modularity between different modes of VMM execution.
#[async_trait]
pub trait VmmExecutor: Send + Sync {
    /// Get the host location of the VMM socket, if one exists.
    fn get_socket_path(&self) -> Option<PathBuf>;

    /// Resolves an inner path into an outer path.
    fn inner_to_outer_path(&self, inner_path: &Path) -> PathBuf;

    // Returns a boolean determining whether this executor leaves any traces on the host filesystem after cleanup.
    fn traceless(&self) -> bool;

    /// Prepare all transient resources for the VM invocation.
    async fn prepare(
        &self,
        shell_spawner: &impl ShellSpawner,
        outer_paths: Vec<PathBuf>,
    ) -> Result<HashMap<PathBuf, PathBuf>, VmmExecutorError>;

    /// Invoke the VM on the given FirecrackerInstallation and return the spawned tokio Child.
    async fn invoke(
        &self,
        shell_spawner: &impl ShellSpawner,
        installation: &FirecrackerInstallation,
        config_override: FirecrackerConfigOverride,
    ) -> Result<Child, VmmExecutorError>;

    /// Clean up all transient resources of the VM invocation.
    async fn cleanup(&self, shell_spawner: &impl ShellSpawner) -> Result<(), VmmExecutorError>;
}

pub(crate) async fn force_chown(path: &Path, shell_spawner: &impl ShellSpawner) -> Result<(), VmmExecutorError> {
    if !shell_spawner.increases_privileges() {
        return Ok(());
    }

    // SAFETY: calling FFI libc functions that return the process UID and GID can never result in UB
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };

    let mut child = shell_spawner
        .spawn(format!("chown -f -R {uid}:{gid} {}", path.to_string_lossy()))
        .await
        .map_err(VmmExecutorError::ShellSpawnFailed)?;
    let exit_status = child.wait().await.map_err(VmmExecutorError::ShellForkFailed)?;

    // code 256 means that a concurrent chown is being called and the chown will still be applied, so this error can
    // "safely" be ignored, which is better than inducing the overhead of global locking on chown paths
    if !exit_status.success() && exit_status.into_raw() != 256 {
        return Err(VmmExecutorError::ChownExitedWithWrongStatus(exit_status));
    }

    Ok(())
}

async fn force_mkdir(path: &Path, shell_spawner: &impl ShellSpawner) -> Result<(), VmmExecutorError> {
    if !shell_spawner.increases_privileges() {
        fs::create_dir_all(path).await.map_err(VmmExecutorError::IoError)?;
        return Ok(());
    }

    let mut child = shell_spawner
        .spawn(format!("mkdir -p {}", path.to_string_lossy()))
        .await
        .map_err(VmmExecutorError::ShellSpawnFailed)?;
    let exit_status = child.wait().await.map_err(VmmExecutorError::ShellForkFailed)?;

    if !exit_status.success() {
        return Err(VmmExecutorError::MkdirExitedWithWrongStatus(exit_status));
    }

    Ok(())
}

async fn create_file_with_tree(path: impl AsRef<Path>) -> Result<(), VmmExecutorError> {
    let path = path.as_ref();

    if let Some(parent_path) = path.parent() {
        fs::create_dir_all(parent_path)
            .await
            .map_err(VmmExecutorError::IoError)?;
    }

    fs::File::create(path).await.map_err(VmmExecutorError::IoError)?;
    Ok(())
}
