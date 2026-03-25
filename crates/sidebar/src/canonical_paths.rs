//! The sidebar groups threads by a canonical path list.
//!
//! Threads have a path list associated with them, but this is the absolute path
//! of whatever worktrees they were associated with. In the sidebar, we want to
//! group all threads by their main worktree, and then we add a worktree chip to
//! the sidebar entry when that thread is in another worktree.
//!
//! This module is provides the functions and structures necessary to do this
//! lookup and mapping.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::App;
use project::git_store::RepositorySnapshot;
use workspace::Workspace;

pub struct PathCanonicalizer {
    /// Maps git repositories' work_directory_abs_path to their original_repo_abs_path
    directory_mappings: HashMap<PathBuf, PathBuf>,
}

impl PathCanonicalizer {
    pub fn new() -> Self {
        Self {
            directory_mappings: HashMap::new(),
        }
    }

    fn add_snapshot_mapping(&mut self, snapshot: &RepositorySnapshot) {
        let old = self.directory_mappings.insert(
            PathBuf::from(snapshot.work_directory_abs_path.as_ref()),
            PathBuf::from(snapshot.original_repo_abs_path.as_ref()),
        );
        if let Some(old) = old {
            debug_assert_eq!(
                old.as_path(),
                snapshot.original_repo_abs_path.as_ref(),
                "all worktrees should map to the same main worktree"
            );
        }
    }

    pub fn add_workspace_mappings(&mut self, workspace: &Workspace, cx: &App) {
        for (_, repo) in workspace.project().read(cx).repositories(cx) {
            let snapshot = repo.read(cx).snapshot();
            self.add_snapshot_mapping(&snapshot);

            // TODO: Also add mappings for any known linked worktrees.
        }
    }

    pub fn canonicalize_path(&self, path: &Path) -> Option<&Path> {
        self.directory_mappings.get(path).map(|p| p.as_path())
    }
}
