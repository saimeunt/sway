use crate::{
    error::{DirectoryError, DocumentError, LanguageServerError},
    utils::document::{get_path_from_url, get_url_from_path, get_url_from_span},
};
use dashmap::DashMap;
use forc_pkg::manifest::{GenericManifestFile, ManifestFile};
use forc_pkg::PackageManifestFile;
use lsp_types::Url;
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use parking_lot::RwLock;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use sway_types::{SourceEngine, Span};
use sway_utils::{
    constants::{LOCK_FILE_NAME, MANIFEST_FILE_NAME},
    SWAY_EXTENSION,
};
use tempfile::Builder;
use tokio::task::JoinHandle;
use tracing::error;

#[derive(Debug, Eq, PartialEq, Hash)]
pub enum Directory {
    Manifest,
    Temp,
}

#[derive(Debug)]
pub struct SyncWorkspace {
    pub directories: DashMap<Directory, PathBuf>,
    pub notify_join_handle: RwLock<Option<JoinHandle<()>>>,
}

impl SyncWorkspace {
    pub const LSP_TEMP_PREFIX: &'static str = "SWAY_LSP_TEMP_DIR";

    pub(crate) fn new() -> Self {
        Self {
            directories: DashMap::new(),
            notify_join_handle: RwLock::new(None),
        }
    }

    /// Overwrite the contents of the tmp/folder with everything in
    /// the current workspace.
    pub fn resync(&self) -> Result<(), LanguageServerError> {
        self.clone_manifest_dir_to_temp()?;
        if let (Ok(manifest_dir), Some(manifest_path), Some(temp_manifest_path)) = (
            self.manifest_dir(),
            self.manifest_path(),
            self.temp_manifest_path(),
        ) {
            edit_manifest_dependency_paths(&manifest_dir, &manifest_path, &temp_manifest_path)?;
        }
        Ok(())
    }

    /// Clean up the temp directory that was created once the
    /// server closes down.
    pub(crate) fn remove_temp_dir(&self) {
        if let Ok(dir) = self.temp_dir() {
            dir.parent().map(fs::remove_dir);
        }
    }

    pub(crate) fn create_temp_dir_from_workspace(
        &self,
        manifest_dir: &Path,
    ) -> Result<(), LanguageServerError> {
        let manifest = PackageManifestFile::from_dir(manifest_dir).map_err(|_| {
            DocumentError::ManifestFileNotFound {
                dir: manifest_dir.to_string_lossy().to_string(),
            }
        })?;

        // strip Forc.toml from the path to get the manifest directory
        let manifest_dir = manifest
            .path()
            .parent()
            .ok_or(DirectoryError::ManifestDirNotFound)?;

        // extract the project name from the path
        let project_name = manifest_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(DirectoryError::CantExtractProjectName {
                dir: manifest_dir.to_string_lossy().to_string(),
            })?;

        // Create a new temporary directory that we can clone the current workspace into.
        let temp_dir = Builder::new()
            .prefix(SyncWorkspace::LSP_TEMP_PREFIX)
            .tempdir()
            .map_err(|_| DirectoryError::TempDirFailed)?;

        let temp_path = temp_dir
            .into_path()
            .canonicalize()
            .map_err(|_| DirectoryError::CanonicalizeFailed)?
            .join(project_name);

        self.directories
            .insert(Directory::Manifest, manifest_dir.to_path_buf());
        self.directories.insert(Directory::Temp, temp_path);

        Ok(())
    }

    pub(crate) fn clone_manifest_dir_to_temp(&self) -> Result<(), DirectoryError> {
        copy_dir_contents(self.manifest_dir()?, self.temp_dir()?)
            .map_err(|_| DirectoryError::CopyContentsFailed)?;

        Ok(())
    }

    /// Convert the Url path from the client to point to the same file in our temp folder
    pub(crate) fn workspace_to_temp_url(&self, uri: &Url) -> Result<Url, DirectoryError> {
        convert_url(uri, &self.temp_dir()?, &self.manifest_dir()?)
    }

    /// Convert the [Url] path from the temp folder to point to the same file in the users workspace.
    pub(crate) fn temp_to_workspace_url(&self, uri: &Url) -> Result<Url, DirectoryError> {
        convert_url(uri, &self.manifest_dir()?, &self.temp_dir()?)
    }

    /// If it is a path to a temp directory, convert the path in the [Span] to the same file in the user's
    /// workspace. Otherwise, return the span as-is.
    pub(crate) fn temp_to_workspace_span(
        &self,
        source_engine: &SourceEngine,
        span: &Span,
    ) -> Result<Span, DirectoryError> {
        let url = get_url_from_span(source_engine, span)?;
        if is_path_in_temp_workspace(&url) {
            let converted_url = convert_url(&url, &self.manifest_dir()?, &self.temp_dir()?)?;
            let converted_path = get_path_from_url(&converted_url)?;
            let source_id = source_engine.get_source_id(&converted_path);
            let converted_span = Span::new(
                span.src().clone(),
                span.start(),
                span.end(),
                Some(source_id),
            );
            match converted_span {
                Some(span) => Ok(span),
                None => Err(DirectoryError::SpanFromPathFailed {
                    path: converted_path.to_string_lossy().to_string(),
                }),
            }
        } else {
            Ok(span.clone())
        }
    }

    /// If path is part of the users workspace, then convert URL from temp to workspace dir.
    /// Otherwise, pass through if it points to a dependency path
    pub(crate) fn to_workspace_url(&self, url: Url) -> Option<Url> {
        if is_path_in_temp_workspace(&url) {
            Some(self.temp_to_workspace_url(&url).ok()?)
        } else {
            Some(url)
        }
    }

    pub(crate) fn temp_manifest_path(&self) -> Option<PathBuf> {
        self.temp_dir()
            .map(|dir| dir.join(sway_utils::constants::MANIFEST_FILE_NAME))
            .ok()
    }

    pub fn manifest_path(&self) -> Option<PathBuf> {
        self.manifest_dir()
            .map(|dir| dir.join(sway_utils::constants::MANIFEST_FILE_NAME))
            .ok()
    }

    /// Watch the manifest directory and check for any save events on Forc.toml
    pub(crate) fn watch_and_sync_manifest(&self) {
        if let (Ok(manifest_dir), Some(manifest_path), Some(temp_manifest_path)) = (
            self.manifest_dir(),
            self.manifest_path(),
            self.temp_manifest_path(),
        ) {
            if let Err(err) =
                edit_manifest_dependency_paths(&manifest_dir, &manifest_path, &temp_manifest_path)
            {
                error!("Failed to edit manifest dependency paths: {}", err);
            }

            let handle = tokio::spawn(async move {
                let (tx, mut rx) = tokio::sync::mpsc::channel(10);
                // Setup debouncer. No specific tickrate, max debounce time 500 milliseconds
                let mut debouncer = new_debouncer(Duration::from_millis(500), move |event| {
                    if let Ok(e) = event {
                        let _ = tx.blocking_send(e);
                    }
                })
                .unwrap();

                debouncer
                    .watcher()
                    .watch(&manifest_dir, RecursiveMode::NonRecursive)
                    .unwrap();
                while let Some(_events) = rx.recv().await {
                    // Rescan the Forc.toml and convert
                    // relative paths to absolute. Save into our temp directory.
                    if let Err(err) = edit_manifest_dependency_paths(
                        &manifest_dir,
                        &manifest_path,
                        &temp_manifest_path,
                    ) {
                        error!("Failed to edit manifest dependency paths: {}", err);
                    }
                }
            });

            // Store the join handle so we can clean up the thread on shutdown
            {
                let mut join_handle = self.notify_join_handle.write();
                *join_handle = Some(handle);
            }
        }
    }

    /// Return the path to the projects manifest directory.
    pub(crate) fn manifest_dir(&self) -> Result<PathBuf, DirectoryError> {
        self.directories
            .try_get(&Directory::Manifest)
            .try_unwrap()
            .map(|item| item.value().clone())
            .ok_or(DirectoryError::ManifestDirNotFound)
    }

    /// Return the path to the temporary directory that was created for the current session.
    pub(crate) fn temp_dir(&self) -> Result<PathBuf, DirectoryError> {
        self.directories
            .try_get(&Directory::Temp)
            .try_unwrap()
            .map(|item| item.value().clone())
            .ok_or(DirectoryError::TempDirNotFound)
    }
}

/// Check if the current path is part of the users workspace.
/// Returns false if the path is from a dependency
pub(crate) fn is_path_in_temp_workspace(uri: &Url) -> bool {
    uri.as_ref().contains(SyncWorkspace::LSP_TEMP_PREFIX)
}

fn convert_url(uri: &Url, from: &Path, to: &PathBuf) -> Result<Url, DirectoryError> {
    let path = from.join(
        PathBuf::from(uri.path())
            .strip_prefix(to)
            .map_err(DirectoryError::StripPrefixError)?,
    );
    get_url_from_path(&path)
}

/// Deserialize the manifest file and loop through the dependencies.
/// Check if the dependency is specifying a 'path'.
/// If so, check if the path is relative and convert the relative path to an absolute path.
/// Edit the toml entry using toml_edit with the absolute path.
/// Save the manifest to temp_dir/Forc.toml.
pub(crate) fn edit_manifest_dependency_paths(
    manifset_dir: &Path,
    manifest_path: &Path,
    temp_manifest_path: &Path,
) -> Result<(), LanguageServerError> {
    // Read and parse the original manifest
    let manifest_content =
        std::fs::read_to_string(manifest_path).map_err(|err| DocumentError::IOError {
            path: manifest_path.to_string_lossy().to_string(),
            error: err.to_string(),
        })?;

    let mut doc = manifest_content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|err| DocumentError::IOError {
            path: manifest_path.to_string_lossy().to_string(),
            error: format!("Failed to parse TOML: {}", err),
        })?;

    let manifest =
        ManifestFile::from_file(manifest_path).map_err(|err| DocumentError::IOError {
            path: manifest_path.to_string_lossy().to_string(),
            error: err.to_string(),
        })?;

    if let ManifestFile::Package(package) = manifest {
        // Process dependencies if they exist
        if let Some(deps) = &package.dependencies {
            if let Some(deps_table) = doc.get_mut("dependencies").and_then(|v| v.as_table_mut()) {
                process_dependencies(manifset_dir, deps, deps_table)?;
            }
        }
    }

    // Write the updated manifest to the temp file
    std::fs::write(temp_manifest_path, doc.to_string()).map_err(|err| {
        DocumentError::UnableToWriteFile {
            path: temp_manifest_path.to_string_lossy().to_string(),
            err: err.to_string(),
        }
    })?;

    Ok(())
}

/// Process dependencies and convert relative paths to absolute
fn process_dependencies(
    manifest_dir: &Path,
    deps: &std::collections::BTreeMap<String, forc_pkg::manifest::Dependency>,
    deps_table: &mut toml_edit::Table,
) -> Result<(), LanguageServerError> {
    for (name, dependency) in deps {
        if let forc_pkg::manifest::Dependency::Detailed(details) = dependency {
            if let Some(rel_path) = &details.path {
                // Convert relative path to absolute
                let abs_path = manifest_dir
                    .join(rel_path)
                    .canonicalize()
                    .map_err(|_| DirectoryError::CanonicalizeFailed)?
                    .to_string_lossy()
                    .to_string();

                // Update the path in the TOML document
                if let Some(dep_item) = deps_table.get_mut(name) {
                    let path_value = toml_edit::Value::from(abs_path);
                    if let Some(table) = dep_item.as_inline_table_mut() {
                        table.insert("path", path_value);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Copies only the specified files from the source directory to the target directory.
/// This function targets files ending with `.sw`, and the specific files `Forc.toml` and `Forc.lock`.
/// It returns `Ok(true)` if any relevant files were copied over, and `Ok(false)` if no such files were found.
fn copy_dir_contents(
    src_dir: impl AsRef<Path>,
    target_dir: impl AsRef<Path>,
) -> std::io::Result<bool> {
    let mut has_relevant_files = false;
    for entry in fs::read_dir(&src_dir)? {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            // Recursively check the directory; if it has relevant files, create the target directory
            if copy_dir_contents(&path, target_dir.as_ref().join(entry.file_name()))? {
                has_relevant_files = true;
            }
        } else if let Some(file_name_os) = path.file_name() {
            if let Some(file_name) = file_name_os.to_str() {
                if file_name.ends_with(&format!(".{SWAY_EXTENSION}"))
                    || file_name == MANIFEST_FILE_NAME
                    || file_name == LOCK_FILE_NAME
                {
                    if !has_relevant_files {
                        fs::create_dir_all(&target_dir)?;
                        has_relevant_files = true;
                    }
                    fs::copy(&path, target_dir.as_ref().join(file_name))?;
                }
            }
        }
    }
    Ok(has_relevant_files)
}
