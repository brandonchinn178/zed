use std::{collections::HashMap, path::PathBuf, sync::Arc};

use fs::Fs;
use serde::Deserialize;
use serde_json_lenient::Value;

use crate::{
    devcontainer_api::DevContainerError,
    devcontainer_json::{FeatureOptions, MountDefinition},
    safe_id_upper,
};

/// Parsed components of an OCI feature reference such as
/// `ghcr.io/devcontainers/features/aws-cli:1`.
///
/// Mirrors the CLI's `OCIRef` in `containerCollectionsOCI.ts`.
#[derive(Debug, Clone)]
pub(crate) struct OciFeatureRef {
    /// Registry hostname, e.g. `ghcr.io`
    pub registry: String,
    /// Full repository path within the registry, e.g. `devcontainers/features/aws-cli`
    pub path: String,
    /// Version tag, digest, or `latest`
    pub version: String,
}

/// Minimal representation of a `devcontainer-feature.json` file, used to
/// extract option default values after the feature tarball is downloaded.
///
/// See: https://containers.dev/implementors/features/#devcontainer-featurejson-properties
#[derive(Debug, Deserialize, Eq, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevContainerFeatureJson {
    #[serde(rename = "id")]
    pub(crate) _id: Option<String>,
    #[serde(default)]
    pub(crate) options: HashMap<String, FeatureOptionDefinition>,
    pub(crate) mounts: Option<Vec<MountDefinition>>,
    pub(crate) privileged: Option<bool>,
    pub(crate) entrypoint: Option<String>,
}

/// A single option definition inside `devcontainer-feature.json`.
/// We only need the `default` field to populate env variables.
#[derive(Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct FeatureOptionDefinition {
    pub(crate) default: Option<Value>,
}

impl FeatureOptionDefinition {
    fn serialize_default(&self) -> Option<String> {
        self.default.as_ref().map(|some_value| match some_value {
            Value::Bool(b) => b.to_string(),
            Value::String(s) => s.to_string(),
            Value::Number(n) => n.to_string(),
            other => other.to_string(),
        })
    }
}

#[derive(Debug, Eq, PartialEq, Default)]
pub(crate) struct FeatureManifest {
    file_path: PathBuf,
    feature_json: DevContainerFeatureJson,
}

impl FeatureManifest {
    pub(crate) fn new(file_path: PathBuf, feature_json: DevContainerFeatureJson) -> Self {
        Self {
            file_path,
            feature_json,
        }
    }

    /// Merges user options from devcontainer.json with default options defined in this feature manifest
    pub(crate) fn genereate_merged_env(&self, options: &FeatureOptions) -> HashMap<String, String> {
        let mut merged: HashMap<String, String> = self
            .feature_json
            .options
            .iter()
            .filter_map(|(k, v)| {
                v.serialize_default()
                    .map(|v_some| (safe_id_upper(k), v_some))
            })
            .collect();

        match options {
            FeatureOptions::Bool(_) => {}
            FeatureOptions::String(version) => {
                merged.insert("VERSION".to_string(), version.clone());
            }
            FeatureOptions::Options(map) => {
                for (key, value) in map {
                    merged.insert(safe_id_upper(key), value.to_string());
                }
            }
        }
        merged
    }

    pub(crate) async fn write_feature_env(
        &self,
        fs: &Arc<dyn Fs>,
        options: &FeatureOptions,
    ) -> Result<String, DevContainerError> {
        let merged_env = self.genereate_merged_env(options);

        let env_vars: Vec<String> = merged_env
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let env_file_content = env_vars.join("\n");

        fs.write(
            &self.file_path.join("devcontainer-features.env"),
            env_file_content.as_bytes(),
        )
        .await
        .map_err(|e| {
            log::error!("error writing devcontainer feature environment: {e}");
            DevContainerError::FilesystemError
        })?;

        Ok(env_file_content)
    }

    pub(crate) fn mounts(&self) -> Vec<MountDefinition> {
        if let Some(mounts) = &self.feature_json.mounts {
            mounts.clone()
        } else {
            vec![]
        }
    }

    pub(crate) fn privileged(&self) -> bool {
        self.feature_json.privileged.unwrap_or(false)
    }

    pub(crate) fn entrypoint(&self) -> Option<String> {
        self.feature_json.entrypoint.clone()
    }

    pub(crate) fn file_path(&self) -> PathBuf {
        self.file_path.clone()
    }
}

/// Parses an OCI feature reference string into its components.
///
/// Handles formats like:
/// - `ghcr.io/devcontainers/features/aws-cli:1`
/// - `ghcr.io/user/repo/go`  (implicitly `:latest`)
/// - `ghcr.io/devcontainers/features/rust@sha256:abc123`
///
/// Returns `None` for local paths (`./…`) and direct tarball URIs (`https://…`).
pub(crate) fn parse_oci_feature_ref(input: &str) -> Option<OciFeatureRef> {
    if input.starts_with('.')
        || input.starts_with('/')
        || input.starts_with("https://")
        || input.starts_with("http://")
    {
        return None;
    }

    let input_lower = input.to_lowercase();

    let (resource, version) = if let Some(at_idx) = input_lower.rfind('@') {
        // Digest-based: ghcr.io/foo/bar@sha256:abc
        (
            input_lower[..at_idx].to_string(),
            input_lower[at_idx + 1..].to_string(),
        )
    } else {
        let last_slash = input_lower.rfind('/');
        let last_colon = input_lower.rfind(':');
        match (last_slash, last_colon) {
            (Some(slash), Some(colon)) if colon > slash => (
                input_lower[..colon].to_string(),
                input_lower[colon + 1..].to_string(),
            ),
            _ => (input_lower.clone(), "latest".to_string()),
        }
    };

    let parts: Vec<&str> = resource.split('/').collect();
    if parts.len() < 3 {
        return None;
    }

    let registry = parts[0].to_string();
    let path = parts[1..].join("/");

    Some(OciFeatureRef {
        registry,
        path,
        version,
    })
}
