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

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
        });
    }

    async fn create_fs_with_main_and_worktree(cx: &mut TestAppContext) -> Arc<FakeFs> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            serde_json::json!({
                ".git": {
                    "worktrees": {
                        "feature-a": {
                            "commondir": "../../",
                            "HEAD": "ref: refs/heads/feature-a",
                        },
                    },
                },
                "src": {},
            }),
        )
        .await;
        fs.insert_tree(
            "/wt/feature-a",
            serde_json::json!({
                ".git": "gitdir: /project/.git/worktrees/feature-a",
                "src": {},
            }),
        )
        .await;
        fs.with_git_state(std::path::Path::new("/project/.git"), false, |state| {
            state.worktrees.push(git::repository::Worktree {
                path: std::path::PathBuf::from("/wt/feature-a"),
                ref_name: Some("refs/heads/feature-a".into()),
                sha: "abc".into(),
            });
        })
        .expect("git state should be set");
        fs
    }

    #[gpui::test]
    async fn test_main_repo_maps_to_itself(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = create_fs_with_main_and_worktree(cx).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

        let project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        multi_workspace.read_with(cx, |mw, cx| {
            let mut canonicalizer = PathCanonicalizer::new();
            for workspace in mw.workspaces() {
                canonicalizer.add_workspace_mappings(workspace.read(cx), cx);
            }

            // The main repo path should canonicalize to itself.
            assert_eq!(
                canonicalizer.canonicalize_path(Path::new("/project")),
                Some(Path::new("/project")),
            );

            // An unknown path returns None.
            assert_eq!(
                canonicalizer.canonicalize_path(Path::new("/something/else")),
                None,
            );
        });
    }

    #[gpui::test]
    async fn test_worktree_checkout_canonicalizes_to_main_repo(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = create_fs_with_main_and_worktree(cx).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

        // Open the worktree checkout as its own project.
        let project = project::Project::test(fs.clone(), ["/wt/feature-a".as_ref()], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        multi_workspace.read_with(cx, |mw, cx| {
            let mut canonicalizer = PathCanonicalizer::new();
            for workspace in mw.workspaces() {
                canonicalizer.add_workspace_mappings(workspace.read(cx), cx);
            }

            // The worktree checkout path should canonicalize to the main repo.
            assert_eq!(
                canonicalizer.canonicalize_path(Path::new("/wt/feature-a")),
                Some(Path::new("/project")),
            );
        });
    }
}
