use std::{
    collections::HashMap,
    fmt::Debug,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};

use fs::Fs;
use http_client::HttpClient;
use smol::process::Command;
use util::ResultExt;

use crate::{
    DevContainerConfig, DevContainerContext,
    devcontainer_api::{DevContainerError, DevContainerUp},
    devcontainer_json::{
        DevContainer, DevContainerBuildType, FeatureOptions, MountDefinition,
        deserialize_devcontainer_json,
    },
    docker::{
        Docker, DockerComposeConfig, DockerComposeService, DockerComposeServiceBuild,
        DockerComposeVolume, DockerInspect, DockerPs, get_remote_dir_from_config,
    },
    features::{DevContainerFeatureJson, FeatureManifest, parse_oci_feature_ref},
    get_oci_token,
    oci::{TokenResponse, download_oci_tarball, get_oci_manifest},
    safe_id_lower,
};

/**
 * What's needed next:
 * - Load the features up front (and put them in that manifest)
 * - Move merged stuff into that struct
 * - Move variable expansion into that struct
 * - Continue on with lifecycle scripts
 *
 */

enum ConfigStatus {
    Deserialized(DevContainer),
    VariableParsed(DevContainer),
}

#[derive(Debug, Clone, Eq, PartialEq, Default)]
pub(crate) struct DockerComposeResources {
    files: Vec<PathBuf>,
    config: DockerComposeConfig,
}

struct DevContainerManifest {
    http_client: Arc<dyn HttpClient>,
    fs: Arc<dyn Fs>,
    docker_client: Docker,
    raw_config: String,
    config: ConfigStatus,
    local_environment: HashMap<String, String>,
    local_project_directory: PathBuf,
    config_directory: PathBuf,
    file_name: String,
    root_image: Option<DockerInspect>,
    features_build_info: Option<FeaturesBuildInfo>,
    features: Vec<FeatureManifest>,
}
const DEFAULT_REMOTE_PROJECT_DIR: &str = "/workspaces/";
impl DevContainerManifest {
    async fn new(
        context: &DevContainerContext,
        // fs: Arc<dyn Fs>,
        // http_client: Arc<dyn HttpClient>,
        environment: HashMap<String, String>,
        local_config: DevContainerConfig,
        local_project_path: Arc<&Path>,
    ) -> Result<Self, DevContainerError> {
        let config_path = local_project_path.join(local_config.config_path.clone());
        log::info!("parsing devcontainer json found in {:?}", &config_path);
        // SO basically everywhere we read_to_string, we want to do it with appropriate variable substitution
        // Let's confirm on the spec about that though
        // Actually ok - so the spec says _only_ the devcontainer.json file can have this substitution. That makes things a bit easier
        let devcontainer_contents = context.fs.load(&config_path).await.map_err(|e| {
            log::error!("Unable to read devcontainer contents: {e}");
            DevContainerError::DevContainerParseFailed
        })?;

        let devcontainer = deserialize_devcontainer_json(&devcontainer_contents)?;

        let devcontainer_directory = config_path.parent().ok_or_else(|| {
            log::error!("Dev container file should be in a directory");
            DevContainerError::NotInValidProject
        })?;
        let file_name = config_path
            .file_name()
            .and_then(|f| f.to_str())
            .ok_or_else(|| {
                log::error!("Dev container file has no file name, or is invalid unicode");
                DevContainerError::DevContainerParseFailed
            })?;

        let docker_client = if context.use_podman {
            Docker::new("podman")
        } else {
            Docker::new("docker")
        };

        Ok(Self {
            fs: context.fs.clone(),
            http_client: context.http_client.clone(),
            docker_client,
            raw_config: devcontainer_contents,
            config: ConfigStatus::Deserialized(devcontainer),
            local_project_directory: local_project_path.to_path_buf(),
            local_environment: environment,
            config_directory: devcontainer_directory.to_path_buf(),
            file_name: file_name.to_string(),
            root_image: None,
            features_build_info: None,
            features: Vec::new(),
        })
    }

    fn devcontainer_id(&self) -> String {
        let mut labels = self.identifying_labels();
        labels.sort_by_key(|(key, _)| *key);

        let mut hasher = DefaultHasher::new();
        for (key, value) in &labels {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }

        format!("{:016x}", hasher.finish())
    }

    fn identifying_labels(&self) -> Vec<(&str, String)> {
        let labels = vec![
            (
                "devcontainer.local_folder",
                (self.local_project_directory.display()).to_string(),
            ),
            (
                "devcontainer.config_file",
                (self.config_file().display()).to_string(),
            ),
        ];
        labels
    }

    fn parse_nonremote_vars(&mut self) -> Result<(), DevContainerError> {
        let mut replaced_content = self
            .raw_config
            .replace("${devcontainerId}", &self.devcontainer_id())
            .replace(
                "${containerWorkspaceFolderBasename}",
                &self.remote_workspace_base_name().unwrap_or_default(),
            )
            .replace(
                "${localWorkspaceFolderBasename}",
                &self.local_workspace_base_name()?,
            )
            .replace(
                "${containerWorkspaceFolder}",
                &self
                    .remote_workspace_folder()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            )
            .replace("${localWorkspaceFolder}", &self.local_workspace_folder());
        for (k, v) in &self.local_environment {
            let find = format!("${{localEnv:{k}}}");
            replaced_content = replaced_content.replace(&find, &v);
        }

        let parsed_config = deserialize_devcontainer_json(&replaced_content)?;

        self.config = ConfigStatus::VariableParsed(parsed_config);

        Ok(())
    }

    // Replaces the remote_env vars in devcontainer.json
    // Ok, so this only applies at the time of docker run. We can essentially inspect the container
    // that was built, pull out the env, and combine it with whatever is predefined in the json. Then this will feed
    // back into docker run with `-e ... -e...` etc. Therefore this will take whatever the data
    // type of the docker inspect Env is, and spit out a hashmap
    fn _replace_remote_env_vars(&mut self) {}

    fn validate_config(&self) -> Result<(), DevContainerError> {
        // TODO
        Ok(())
    }
    fn config_file(&self) -> PathBuf {
        self.config_directory.join(&self.file_name).clone()
    }

    fn dev_container(&self) -> &DevContainer {
        match &self.config {
            ConfigStatus::Deserialized(dev_container) => dev_container,
            ConfigStatus::VariableParsed(dev_container) => dev_container,
        }
    }

    async fn dockerfile_location(&self) -> Option<PathBuf> {
        let dev_container = self.dev_container();
        match dev_container.build_type() {
            DevContainerBuildType::Image => None,
            DevContainerBuildType::Dockerfile => dev_container
                .build
                .as_ref()
                .map(|build| self.config_directory.join(&build.dockerfile)),
            DevContainerBuildType::DockerCompose => {
                let Ok(docker_compose_manifest) = self.docker_compose_manifest().await else {
                    return None;
                };
                let Ok((_, main_service)) = find_primary_service(&docker_compose_manifest, self)
                else {
                    return None;
                };
                main_service
                    .build
                    .and_then(|b| b.dockerfile)
                    .map(|dockerfile| self.config_directory.join(dockerfile))
            }
            DevContainerBuildType::None => None,
        }
    }

    fn generate_features_image_tag(&self, dockerfile_build_path: String) -> String {
        let mut hasher = DefaultHasher::new();
        let prefix = match &self.dev_container().name {
            Some(name) => &safe_id_lower(name),
            None => "zed-dc",
        };
        let prefix = prefix.get(..6).unwrap_or(prefix);

        dockerfile_build_path.hash(&mut hasher);

        let hash = hasher.finish();
        format!("{}-{:x}-features", prefix, hash)
    }

    /// Gets the base image from the devcontainer with the following precedence:
    /// - The devcontainer image if an image is specified
    /// - The image sourced in the Dockerfile if a Dockerfile is specified
    /// - The image sourced in the docker-compose main service, if one is specified
    /// - The image sourced in the docker-compose main service dockerfile, if one is specified
    /// If no such image is available, return an error
    async fn get_base_image_from_config(&self) -> Result<String, DevContainerError> {
        if let Some(image) = &self.dev_container().image {
            return Ok(image.to_string());
        }
        if let Some(dockerfile) = self.dev_container().build.as_ref().map(|b| &b.dockerfile) {
            let dockerfile_contents = self
                .fs
                .load(&self.config_directory.join(dockerfile))
                .await
                .map_err(|e| {
                    log::error!("Error reading dockerfile: {e}");
                    DevContainerError::DevContainerParseFailed
                })?;
            return image_from_dockerfile(self, dockerfile_contents);
        }
        if self.dev_container().docker_compose_file.is_some() {
            let docker_compose_manifest = self.docker_compose_manifest().await?;
            let (_, main_service) = find_primary_service(&docker_compose_manifest, &self)?;

            if let Some(dockerfile) = main_service
                .build
                .as_ref()
                .and_then(|b| b.dockerfile.as_ref())
            {
                let dockerfile_contents = self
                    .fs
                    .load(&self.config_directory.join(dockerfile))
                    .await
                    .map_err(|e| {
                        log::error!("Error reading dockerfile: {e}");
                        DevContainerError::DevContainerParseFailed
                    })?;
                return image_from_dockerfile(self, dockerfile_contents);
            }
            if let Some(image) = &main_service.image {
                return Ok(image.to_string());
            }

            log::error!("No valid base image found in docker-compose configuration");
            return Err(DevContainerError::DevContainerParseFailed);
        }
        log::error!("No valid base image found in dev container configuration");
        Err(DevContainerError::DevContainerParseFailed)
    }

    async fn download_feature_and_dockerfile_resources(&mut self) -> Result<(), DevContainerError> {
        let dev_container = match &self.config {
            ConfigStatus::Deserialized(_) => {
                log::error!(
                    "Dev container has not yet been parsed for variable expansion. Cannot yet download resources"
                );
                return Err(DevContainerError::DevContainerParseFailed);
            }
            ConfigStatus::VariableParsed(dev_container) => dev_container,
        };
        let root_image_tag = self.get_base_image_from_config().await?;

        let root_image = self.docker_client.inspect_image(&root_image_tag).await?;

        if dev_container.build_type() == DevContainerBuildType::Image
            && !dev_container.has_features()
        {
            log::info!("No resources to download. Proceeding with just the image");
            return Ok(());
        }

        let temp_base = std::env::temp_dir().join("devcontainer-zed");
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        let features_content_dir = temp_base.join(format!("container-features-{}", timestamp));

        let empty_context_dir = temp_base.join("empty-folder");

        self.fs
            .create_dir(&features_content_dir)
            .await
            .map_err(|e| {
                log::error!("Failed to create features content dir: {e}");
                DevContainerError::FilesystemError
            })?;

        self.fs.create_dir(&empty_context_dir).await.map_err(|e| {
            log::error!("Failed to create empty context dir: {e}");
            DevContainerError::FilesystemError
        })?;

        let dockerfile_path = features_content_dir.join("Dockerfile.extended");

        let image_tag =
            self.generate_features_image_tag(dockerfile_path.clone().display().to_string());

        let mut build_info = FeaturesBuildInfo {
            dockerfile_path,
            dockerfile_no_buildkit_path: None,
            features_content_dir,
            empty_context_dir,
            build_image: dev_container.image.clone(),
            image_tag,
        };

        let features = match &dev_container.features {
            Some(features) => features,
            None => &HashMap::new(),
        };

        let container_user = get_container_user_from_config(&root_image, self)?;
        let remote_user = get_remote_user_from_config(&root_image, self)?;

        let builtin_env_content = format!(
            "_CONTAINER_USER={}\n_REMOTE_USER={}\n",
            container_user, remote_user
        );

        let builtin_env_path = build_info
            .features_content_dir
            .join("devcontainer-features.builtin.env");

        self.fs
            .write(&builtin_env_path, &builtin_env_content.as_bytes())
            .await
            .map_err(|e| {
                log::error!("Failed to write builtin env file: {e}");
                DevContainerError::FilesystemError
            })?;

        let ordered_features =
            resolve_feature_order(features, &dev_container.override_feature_install_order);

        let mut feature_layers = String::new();

        for (index, (feature_ref, options)) in ordered_features.iter().enumerate() {
            if matches!(options, FeatureOptions::Bool(false)) {
                log::info!(
                    "Feature '{}' is disabled (set to false), skipping",
                    feature_ref
                );
                continue;
            }

            let feature_id = extract_feature_id(feature_ref);
            let consecutive_id = format!("{}_{}", feature_id, index);
            let feature_dir = build_info.features_content_dir.join(&consecutive_id);

            self.fs.create_dir(&feature_dir).await.map_err(|e| {
                log::error!(
                    "Failed to create feature directory for {}: {e}",
                    feature_ref
                );
                DevContainerError::FilesystemError
            })?;

            // --- Download the feature's OCI tarball first, so we can read
            // devcontainer-feature.json for option defaults before writing the
            // env file.
            let oci_ref = parse_oci_feature_ref(feature_ref).ok_or_else(|| {
                log::error!(
                    "Feature '{}' is not a supported OCI feature reference",
                    feature_ref
                );
                DevContainerError::DevContainerParseFailed
            })?;
            let TokenResponse { token } =
                get_oci_token(&oci_ref.registry, &oci_ref.path, &self.http_client)
                    .await
                    .map_err(|e| {
                        log::error!("Failed to get OCI token for feature '{}': {e}", feature_ref);
                        DevContainerError::ResourceFetchFailed
                    })?;
            let manifest = get_oci_manifest(
                &oci_ref.registry,
                &oci_ref.path,
                &token,
                &self.http_client,
                &oci_ref.version,
                None,
            )
            .await
            .map_err(|e| {
                log::error!(
                    "Failed to fetch OCI manifest for feature '{}': {e}",
                    feature_ref
                );
                DevContainerError::ResourceFetchFailed
            })?;
            let digest = &manifest
                .layers
                .first()
                .ok_or_else(|| {
                    log::error!(
                        "OCI manifest for feature '{}' contains no layers",
                        feature_ref
                    );
                    DevContainerError::ResourceFetchFailed
                })?
                .digest;
            download_oci_tarball(
                &token,
                &oci_ref.registry,
                &oci_ref.path,
                digest,
                "application/vnd.devcontainers.layer.v1+tar",
                &feature_dir,
                &self.http_client,
                &self.fs,
                None,
            )
            .await?;

            let feature_json_path = &feature_dir.join("devcontainer-feature.json");
            if !feature_json_path.exists() {
                let message = format!(
                    "No devcontainer-feature.json found in {:?}, no defaults to apply",
                    feature_json_path
                );
                log::error!("{}", &message);
                return Err(DevContainerError::ResourceFetchFailed);
            }

            let contents = self.fs.load(&feature_json_path).await.map_err(|e| {
                log::error!("error reading devcontainer-feature.json: {:?}", e);
                DevContainerError::FilesystemError
            })?;

            let feature_json: DevContainerFeatureJson = serde_json_lenient::from_str(&contents)
                .map_err(|e| {
                    log::error!("Failed to parse devcontainer-feature.json: {e}");
                    DevContainerError::ResourceFetchFailed
                })?;

            let feature_manifest = FeatureManifest::new(feature_dir, feature_json);

            log::info!("Downloaded OCI feature content for '{}'", feature_ref);

            let env_content = feature_manifest
                .write_feature_env(&self.fs, options)
                .await?;

            let wrapper_content = generate_install_wrapper(feature_ref, feature_id, &env_content);

            self.fs
                .write(
                    &feature_manifest
                        .file_path()
                        .join("devcontainer-features-install.sh"),
                    &wrapper_content.as_bytes(),
                )
                .await
                .map_err(|e| {
                    log::error!("Failed to write install wrapper for {}: {e}", feature_ref);
                    DevContainerError::FilesystemError
                })?;

            feature_layers.push_str(&generate_feature_layer(&consecutive_id));

            self.features.push(feature_manifest);
        }

        let dockerfile_base_content = if let Some(location) = &self.dockerfile_location().await {
            self.fs.load(location).await.log_err()
        } else {
            None
        };

        let dockerfile_base_content_for_no_bk = dockerfile_base_content.clone();

        let dockerfile_content = generate_dockerfile_extended(
            &feature_layers,
            &container_user,
            &remote_user,
            dockerfile_base_content,
        );

        self.fs
            .write(&build_info.dockerfile_path, &dockerfile_content.as_bytes())
            .await
            .map_err(|e| {
                log::error!("Failed to write Dockerfile.extended: {e}");
                DevContainerError::FilesystemError
            })?;

        let is_compose = dev_container.build_type() == DevContainerBuildType::DockerCompose;
        if self.docker_client.is_podman() && is_compose {
            let mut no_buildkit_feature_layers = String::new();
            let ordered_features_for_no_bk =
                resolve_feature_order(features, &dev_container.override_feature_install_order);
            for (index, (feature_ref, options)) in ordered_features_for_no_bk.iter().enumerate() {
                if matches!(options, FeatureOptions::Bool(false)) {
                    continue;
                }
                let feature_id = extract_feature_id(feature_ref);
                let consecutive_id = format!("{}_{}", feature_id, index);
                no_buildkit_feature_layers
                    .push_str(&generate_feature_layer_no_buildkit(&consecutive_id));
            }

            let no_buildkit_dockerfile_content = generate_dockerfile_extended_no_buildkit(
                &no_buildkit_feature_layers,
                &container_user,
                &remote_user,
                dockerfile_base_content_for_no_bk,
            );

            let no_buildkit_path = build_info
                .features_content_dir
                .join("Dockerfile.extended.no-buildkit");

            self.fs
                .write(&no_buildkit_path, no_buildkit_dockerfile_content.as_bytes())
                .await
                .map_err(|e| {
                    log::error!("Failed to write non-BuildKit Dockerfile: {e}");
                    DevContainerError::FilesystemError
                })?;

            build_info.dockerfile_no_buildkit_path = Some(no_buildkit_path);
        }

        log::info!(
            "Features build resources written to {:?}",
            build_info.features_content_dir
        );

        self.root_image = Some(root_image);
        self.features_build_info = Some(build_info);

        Ok(())
    }

    // TODO move entrypoint into this
    fn build_merged_resources(
        &self,
        base_image: DockerInspect,
    ) -> Result<DockerBuildResources, DevContainerError> {
        let dev_container = match &self.config {
            ConfigStatus::Deserialized(_) => {
                log::error!(
                    "Dev container has not yet been parsed for variable expansion. Cannot yet merge resources"
                );
                return Err(DevContainerError::DevContainerParseFailed);
            }
            ConfigStatus::VariableParsed(dev_container) => dev_container,
        };
        let mut mounts = dev_container.mounts.clone().unwrap_or(Vec::new());

        let mut feature_mounts = self
            .features
            .iter()
            .flat_map(|f| f.mounts().clone())
            .collect();

        mounts.append(&mut feature_mounts);

        let privileged = dev_container.privileged.unwrap_or(false)
            || self.features.iter().any(|f| f.privileged());

        let entrypoints = self
            .features
            .iter()
            .filter_map(|f| f.entrypoint())
            .collect();

        Ok(DockerBuildResources {
            image: base_image,
            additional_mounts: mounts,
            privileged,
            entrypoints,
        })
    }

    async fn build_resources(&self) -> Result<DevContainerBuildResources, DevContainerError> {
        // TODO this probably shouldn't proceed until parsed either
        let dev_container = self.dev_container();
        match dev_container.build_type() {
            DevContainerBuildType::Image | DevContainerBuildType::Dockerfile => {
                let built_docker_image = self.build_docker_image().await?;
                let built_docker_image = self
                    .update_remote_user_uid(built_docker_image, None)
                    .await?;

                let resources = self.build_merged_resources(built_docker_image)?;
                Ok(DevContainerBuildResources::Docker(resources))
            }
            DevContainerBuildType::DockerCompose => {
                log::info!("Using docker compose. Building extended compose files");
                let docker_compose_resources = self.build_and_extend_compose_files().await?;

                return Ok(DevContainerBuildResources::DockerCompose(
                    docker_compose_resources,
                ));
            }
            DevContainerBuildType::None => {
                return Err(DevContainerError::DevContainerParseFailed);
            }
        }
    }

    async fn run_dev_container(
        &self,
        build_resources: DevContainerBuildResources,
    ) -> Result<DevContainerUp, DevContainerError> {
        let ConfigStatus::VariableParsed(_) = &self.config else {
            log::error!(
                "Variables have not been parsed; cannot proceed with running the dev container"
            );
            return Err(DevContainerError::DevContainerParseFailed);
        };
        let running_container = match build_resources {
            DevContainerBuildResources::DockerCompose(resources) => {
                dbg!(&resources);
                self.run_docker_compose(resources).await?
            }
            DevContainerBuildResources::Docker(resources) => {
                dbg!(&resources);
                self.run_docker_image(resources).await?
            }
        };

        dbg!(&running_container);
        let remote_user = get_remote_user_from_config(&running_container, self)?;
        let remote_workspace_folder = get_remote_dir_from_config(
            &running_container,
            (&self.local_project_directory.display()).to_string(),
        )?;

        Ok(DevContainerUp {
            _outcome: "todo".to_string(),
            container_id: running_container.id,
            remote_user,
            remote_workspace_folder,
            extension_ids: self.extension_ids(),
        })
    }

    // TODO this could be done earlier in the process
    async fn docker_compose_manifest(&self) -> Result<DockerComposeResources, DevContainerError> {
        // TODO this probably shouldn't proceed until parsed either
        let dev_container = self.dev_container();
        let Some(docker_compose_files) = dev_container.docker_compose_file.clone() else {
            return Err(DevContainerError::DevContainerParseFailed);
        };
        let docker_compose_full_paths = docker_compose_files
            .iter()
            .map(|relative| self.config_directory.join(relative))
            .collect::<Vec<PathBuf>>();

        let Some(config) = self
            .docker_client
            .get_docker_compose_config(&docker_compose_full_paths)
            .await?
        else {
            log::error!("Output could not deserialize into DockerComposeConfig");
            return Err(DevContainerError::DevContainerParseFailed);
        };
        Ok(DockerComposeResources {
            files: docker_compose_full_paths,
            config,
        })
    }

    async fn build_and_extend_compose_files(
        &self,
    ) -> Result<DockerComposeResources, DevContainerError> {
        // TODO this probably shouldn't proceed until parsed either
        let dev_container = self.dev_container();

        let Some(features_build_info) = &self.features_build_info else {
            log::error!(
                "Cannot build and extend compose files: features build info is not yet constructed"
            );
            return Err(DevContainerError::DevContainerParseFailed);
        };
        let mut docker_compose_resources = self.docker_compose_manifest().await?;

        let (main_service_name, main_service) =
            find_primary_service(&docker_compose_resources, self)?;
        let built_service_image = if let Some(image) = &main_service.image {
            if dev_container
                .features
                .as_ref()
                .is_none_or(|features| features.is_empty())
            {
                self.docker_client.inspect_image(image).await?
            } else {
                let is_podman = self.docker_client.is_podman();

                if is_podman {
                    self.build_feature_content_image().await?;
                }

                let dockerfile_path = if is_podman {
                    features_build_info
                        .dockerfile_no_buildkit_path
                        .as_ref()
                        .unwrap_or(&features_build_info.dockerfile_path)
                } else {
                    &features_build_info.dockerfile_path
                };

                let build_args = if is_podman {
                    HashMap::from([
                        ("_DEV_CONTAINERS_BASE_IMAGE".to_string(), image.clone()),
                        ("_DEV_CONTAINERS_IMAGE_USER".to_string(), "root".to_string()),
                    ])
                } else {
                    HashMap::from([
                        ("BUILDKIT_INLINE_CACHE".to_string(), "1".to_string()),
                        ("_DEV_CONTAINERS_BASE_IMAGE".to_string(), image.clone()),
                        ("_DEV_CONTAINERS_IMAGE_USER".to_string(), "root".to_string()),
                    ])
                };

                let additional_contexts = if is_podman {
                    None
                } else {
                    Some(HashMap::from([(
                        "dev_containers_feature_content_source".to_string(),
                        features_build_info
                            .features_content_dir
                            .display()
                            .to_string(),
                    )]))
                };

                let build_override = DockerComposeConfig {
                    name: None,
                    services: HashMap::from([(
                        main_service_name.clone(),
                        DockerComposeService {
                            image: Some(features_build_info.image_tag.clone()),
                            entrypoint: None,
                            cap_add: None,
                            security_opt: None,
                            labels: None,
                            build: Some(DockerComposeServiceBuild {
                                context: Some(
                                    features_build_info.empty_context_dir.display().to_string(),
                                ),
                                dockerfile: Some(dockerfile_path.display().to_string()),
                                args: Some(build_args),
                                additional_contexts,
                            }),
                            volumes: Vec::new(),
                            ..Default::default()
                        },
                    )]),
                    volumes: HashMap::new(),
                };

                let temp_base = std::env::temp_dir().join("devcontainer-zed");
                let config_location = temp_base.join("docker_compose_build.json");

                let config_json = serde_json_lenient::to_string(&build_override).map_err(|e| {
                    log::error!("Error serializing docker compose runtime override: {e}");
                    DevContainerError::DevContainerParseFailed
                })?;

                self.fs
                    .write(&config_location, config_json.as_bytes())
                    .await
                    .map_err(|e| {
                        log::error!("Error writing the runtime override file: {e}");
                        DevContainerError::FilesystemError
                    })?;

                docker_compose_resources.files.push(config_location);

                // TODO project name how
                self.docker_client
                    .docker_compose_build(
                        &docker_compose_resources.files,
                        "rustwebstarter_devcontainer",
                    )
                    .await?;

                self.docker_client
                    .inspect_image(&features_build_info.image_tag)
                    .await?
            }
        } else if main_service // TODO this has to be reversed, I think?
            .build
            .as_ref()
            .map(|b| b.dockerfile.as_ref())
            .is_some()
        {
            let is_podman = self.docker_client.is_podman();

            if is_podman {
                self.build_feature_content_image().await?;
            }

            let dockerfile_path = if is_podman {
                features_build_info
                    .dockerfile_no_buildkit_path
                    .as_ref()
                    .unwrap_or(&features_build_info.dockerfile_path)
            } else {
                &features_build_info.dockerfile_path
            };

            let build_args = if is_podman {
                HashMap::from([
                    (
                        "_DEV_CONTAINERS_BASE_IMAGE".to_string(),
                        "dev_container_auto_added_stage_label".to_string(),
                    ),
                    ("_DEV_CONTAINERS_IMAGE_USER".to_string(), "root".to_string()),
                ])
            } else {
                HashMap::from([
                    ("BUILDKIT_INLINE_CACHE".to_string(), "1".to_string()),
                    (
                        "_DEV_CONTAINERS_BASE_IMAGE".to_string(),
                        "dev_container_auto_added_stage_label".to_string(),
                    ),
                    ("_DEV_CONTAINERS_IMAGE_USER".to_string(), "root".to_string()),
                ])
            };

            let additional_contexts = if is_podman {
                None
            } else {
                Some(HashMap::from([(
                    "dev_containers_feature_content_source".to_string(),
                    features_build_info
                        .features_content_dir
                        .display()
                        .to_string(),
                )]))
            };

            let build_override = DockerComposeConfig {
                name: None,
                services: HashMap::from([(
                    main_service_name.clone(),
                    DockerComposeService {
                        image: Some(features_build_info.image_tag.clone()),
                        entrypoint: None,
                        cap_add: None,
                        security_opt: None,
                        labels: None,
                        build: Some(DockerComposeServiceBuild {
                            context: Some(
                                features_build_info.empty_context_dir.display().to_string(),
                            ),
                            dockerfile: Some(dockerfile_path.display().to_string()),
                            args: Some(build_args),
                            additional_contexts,
                        }),
                        volumes: Vec::new(),
                        ..Default::default()
                    },
                )]),
                volumes: HashMap::new(),
            };

            let temp_base = std::env::temp_dir().join("devcontainer-zed");
            let config_location = temp_base.join("docker_compose_build.json");

            let config_json = serde_json_lenient::to_string(&build_override).map_err(|e| {
                log::error!("Error serializing docker compose runtime override: {e}");
                DevContainerError::DevContainerParseFailed
            })?;

            self.fs
                .write(&config_location, config_json.as_bytes())
                .await
                .map_err(|e| {
                    log::error!("Error writing the runtime override file: {e}");
                    DevContainerError::FilesystemError
                })?;

            docker_compose_resources.files.push(config_location);

            // TODO project name how
            self.docker_client
                .docker_compose_build(
                    &docker_compose_resources.files,
                    "rustwebstarter_devcontainer",
                )
                .await?;
            self.docker_client
                .inspect_image(&features_build_info.image_tag)
                .await?
        } else {
            log::error!("Docker compose must have either image or dockerfile defined");
            return Err(DevContainerError::DevContainerParseFailed);
        };

        let built_service_image = self
            .update_remote_user_uid(built_service_image, Some(&features_build_info.image_tag))
            .await?;

        let resources = self.build_merged_resources(built_service_image)?;

        let runtime_override_file = self
            .write_runtime_override_file(&main_service_name, resources)
            .await?;

        dbg!(&runtime_override_file);

        docker_compose_resources.files.push(runtime_override_file);

        Ok(docker_compose_resources)
    }

    async fn write_runtime_override_file(
        &self,
        main_service_name: &str,
        resources: DockerBuildResources,
    ) -> Result<PathBuf, DevContainerError> {
        let config = self.build_runtime_override(main_service_name, resources)?;
        let temp_base = std::env::temp_dir().join("devcontainer-zed");
        let config_location = temp_base.join("docker_compose_runtime.json");

        let config_json = serde_json_lenient::to_string(&config).map_err(|e| {
            log::error!("Error serializing docker compose runtime override: {e}");
            DevContainerError::DevContainerParseFailed
        })?;

        self.fs
            .write(&config_location, config_json.as_bytes())
            .await
            .map_err(|e| {
                log::error!("Error writing the runtime override file: {e}");
                DevContainerError::FilesystemError
            })?;

        Ok(config_location)
    }

    fn build_runtime_override(
        &self,
        main_service_name: &str,
        resources: DockerBuildResources,
    ) -> Result<DockerComposeConfig, DevContainerError> {
        let mut runtime_labels = vec![];

        if let Some(metadata) = &resources.image.config.labels.metadata {
            let serialized_metadata = serde_json_lenient::to_string(metadata).map_err(|e| {
                log::error!("Error serializing docker image metadata: {e}");
                DevContainerError::ContainerNotValid(resources.image.id.clone())
            })?;

            runtime_labels.push(format!(
                "{}={}",
                "devcontainer.metadata", serialized_metadata
            ));
        }

        for (k, v) in self.identifying_labels() {
            runtime_labels.push(format!("{}={}", k, v));
        }

        let config_volumes: HashMap<String, DockerComposeVolume> = resources
            .additional_mounts
            .iter()
            .filter_map(|mount| {
                if let Some(mount_type) = &mount.mount_type
                    && mount_type.to_lowercase() == "volume"
                {
                    Some((
                        mount
                            .source
                            .clone()
                            .replace("${devcontainerId}", "devcontainer123"),
                        DockerComposeVolume {
                            name: mount
                                .source
                                .clone()
                                .replace("${devcontainerId}", "devcontainer123"),
                        },
                    ))
                } else {
                    None
                }
            })
            .collect();
        // TODO Probably worth its own method
        let mut entrypoint_script_lines = vec![
            "echo Container started".to_string(),
            "trap \"exit 0\" 15".to_string(),
        ];
        for entrypoint in resources.entrypoints {
            entrypoint_script_lines.push(entrypoint.clone());
        }
        entrypoint_script_lines.append(&mut vec![
            "exec \"$@\"".to_string(),
            "while sleep 1 & wait $!; do :; done".to_string(),
        ]);

        let volumes: Vec<MountDefinition> = resources
            .additional_mounts
            .iter()
            .map(|v| MountDefinition {
                source: v
                    .source
                    .clone()
                    .replace("${devcontainerId}", "devcontainer123"),
                target: v
                    .target
                    .clone()
                    .replace("${devcontainerId}", "devcontainer123"),
                mount_type: v.mount_type.clone(),
            })
            .collect();

        let new_docker_compose_config = DockerComposeConfig {
            name: None,
            services: HashMap::from([(
                main_service_name.to_string(),
                DockerComposeService {
                    entrypoint: Some(vec![
                        "/bin/sh".to_string(),
                        "-c".to_string(),
                        entrypoint_script_lines.join("\n").trim().to_string(),
                        "-".to_string(),
                    ]),
                    cap_add: Some(vec!["SYS_PTRACE".to_string()]),
                    security_opt: Some(vec!["seccomp=unconfined".to_string()]),
                    labels: Some(runtime_labels),
                    volumes,
                    privileged: Some(resources.privileged),
                    ..Default::default()
                },
            )]),
            volumes: config_volumes,
            ..Default::default()
        };

        Ok(new_docker_compose_config)
    }

    async fn build_docker_image(&self) -> Result<DockerInspect, DevContainerError> {
        // TODO this probably shouldn't proceed until parsed either
        let dev_container = self.dev_container();

        match dev_container.build_type() {
            DevContainerBuildType::Image => {
                let Some(image_tag) = &dev_container.image else {
                    return Err(DevContainerError::DevContainerParseFailed);
                };
                let base_image = self.docker_client.inspect_image(image_tag).await?;
                if dev_container
                    .features
                    .as_ref()
                    .is_none_or(|features| features.is_empty())
                {
                    log::info!("No features to add. Using base image");
                    return Ok(base_image.clone());
                }
            }
            DevContainerBuildType::Dockerfile => {}
            DevContainerBuildType::DockerCompose | DevContainerBuildType::None => {
                return Err(DevContainerError::DevContainerParseFailed);
            }
        };

        let mut command = self.create_docker_build()?;

        let output = command.output().await.map_err(|e| {
            log::error!("Error building docker image: {e}");
            DevContainerError::CommandFailed(command.get_program().display().to_string())
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("docker buildx build failed: {stderr}");
            return Err(DevContainerError::CommandFailed(
                command.get_program().display().to_string(),
            ));
        }

        // After a successful build, inspect the newly tagged image to get its metadata
        let Some(features_build_info) = &self.features_build_info else {
            log::error!("Features build info expected, but not created");
            return Err(DevContainerError::DevContainerParseFailed);
        };
        let image = self
            .docker_client
            .inspect_image(&features_build_info.image_tag)
            .await?;

        Ok(image)
    }

    async fn update_remote_user_uid(
        &self,
        image: DockerInspect,
        override_tag: Option<&str>,
    ) -> Result<DockerInspect, DevContainerError> {
        let dev_container = self.dev_container();

        let Some(features_build_info) = &self.features_build_info else {
            return Ok(image);
        };

        // updateRemoteUserUID defaults to true per the devcontainers spec
        if dev_container.update_remote_user_uid == Some(false) {
            return Ok(image);
        }

        let remote_user = get_remote_user_from_config(&image, self)?;
        if remote_user == "root" || remote_user.chars().all(|c| c.is_ascii_digit()) {
            return Ok(image);
        }

        let image_user = image
            .config
            .image_user
            .as_deref()
            .unwrap_or("root")
            .to_string();

        let host_uid = Command::new("id")
            .arg("-u")
            .output()
            .await
            .map_err(|e| {
                log::error!("Failed to get host UID: {e}");
                DevContainerError::CommandFailed("id -u".to_string())
            })
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<u32>()
                    .map_err(|e| {
                        log::error!("Failed to parse host UID: {e}");
                        DevContainerError::CommandFailed("id -u".to_string())
                    })
            })?;

        let host_gid = Command::new("id")
            .arg("-g")
            .output()
            .await
            .map_err(|e| {
                log::error!("Failed to get host GID: {e}");
                DevContainerError::CommandFailed("id -g".to_string())
            })
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<u32>()
                    .map_err(|e| {
                        log::error!("Failed to parse host GID: {e}");
                        DevContainerError::CommandFailed("id -g".to_string())
                    })
            })?;

        let temp_dir = std::env::temp_dir()
            .join("devcontainer-zed")
            .join(format!("update-uid-{}", std::process::id()));

        self.fs.create_dir(&temp_dir).await.map_err(|e| {
            log::error!("Failed to create temp dir for UID update: {e}");
            DevContainerError::FilesystemError
        })?;

        let dockerfile_content = generate_update_uid_dockerfile();

        let dockerfile_path = temp_dir.join("updateUID.Dockerfile");
        self.fs
            .write(&dockerfile_path, dockerfile_content.as_bytes())
            .await
            .map_err(|e| {
                log::error!("Failed to write updateUID Dockerfile: {e}");
                DevContainerError::FilesystemError
            })?;

        let empty_context_dir = temp_dir.join("empty-context");
        self.fs.create_dir(&empty_context_dir).await.map_err(|e| {
            log::error!("Failed to create empty context dir: {e}");
            DevContainerError::FilesystemError
        })?;

        let updated_image_tag = override_tag
            .map(|t| t.to_string())
            .unwrap_or_else(|| format!("{}-uid", features_build_info.image_tag));

        let mut command = Command::new(self.docker_client.docker_cli());
        command.args(["build"]);
        command.args(["-f", &dockerfile_path.display().to_string()]);
        command.args(["-t", &updated_image_tag]);
        command.args([
            "--build-arg",
            &format!("BASE_IMAGE={}", features_build_info.image_tag),
        ]);
        command.args(["--build-arg", &format!("REMOTE_USER={}", remote_user)]);
        command.args(["--build-arg", &format!("NEW_UID={}", host_uid)]);
        command.args(["--build-arg", &format!("NEW_GID={}", host_gid)]);
        command.args(["--build-arg", &format!("IMAGE_USER={}", image_user)]);
        command.arg(empty_context_dir.display().to_string());

        dbg!(&command);

        let output = command.output().await.map_err(|e| {
            log::error!("Error building UID update image: {e}");
            DevContainerError::CommandFailed(command.get_program().display().to_string())
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("UID update build failed: {stderr}");
            return Err(DevContainerError::CommandFailed(
                command.get_program().display().to_string(),
            ));
        }

        self.docker_client.inspect_image(&updated_image_tag).await
    }

    async fn build_feature_content_image(&self) -> Result<(), DevContainerError> {
        let Some(features_build_info) = &self.features_build_info else {
            log::error!("Features build info not available for building feature content image");
            return Err(DevContainerError::DevContainerParseFailed);
        };
        let features_content_dir = &features_build_info.features_content_dir;

        let dockerfile_content = "FROM scratch\nCOPY . /tmp/build-features/\n";
        let dockerfile_path = features_content_dir.join("Dockerfile.feature-content");

        self.fs
            .write(&dockerfile_path, dockerfile_content.as_bytes())
            .await
            .map_err(|e| {
                log::error!("Failed to write feature content Dockerfile: {e}");
                DevContainerError::FilesystemError
            })?;

        let mut command = Command::new(self.docker_client.docker_cli());
        command.args([
            "build",
            "-t",
            "dev_container_feature_content_temp",
            "-f",
            &dockerfile_path.display().to_string(),
            &features_content_dir.display().to_string(),
        ]);

        let output = command.output().await.map_err(|e| {
            log::error!("Error building feature content image: {e}");
            DevContainerError::CommandFailed(self.docker_client.docker_cli())
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("Feature content image build failed: {stderr}");
            return Err(DevContainerError::CommandFailed(
                self.docker_client.docker_cli(),
            ));
        }

        Ok(())
    }

    fn create_docker_build(&self) -> Result<Command, DevContainerError> {
        // TODO this probably shouldn't proceed until parsed either
        let dev_container = self.dev_container();

        let Some(features_build_info) = &self.features_build_info else {
            log::error!(
                "Cannot create docker build command; features build info has not been constructed"
            );
            return Err(DevContainerError::DevContainerParseFailed);
        };
        let mut command = smol::process::Command::new(self.docker_client.docker_cli());

        command.args(["buildx", "build"]);

        // --load is short for --output=docker, loading the built image into the local docker images
        command.arg("--load");

        // BuildKit build context: provides the features content directory as a named context
        // that the Dockerfile.extended can COPY from via `--from=dev_containers_feature_content_source`
        command.args([
            "--build-context",
            &format!(
                "dev_containers_feature_content_source={}",
                features_build_info.features_content_dir.display()
            ),
        ]);

        // Build args matching the CLI reference implementation's `getFeaturesBuildOptions`
        if let Some(build_image) = &features_build_info.build_image {
            command.args([
                "--build-arg",
                &format!("_DEV_CONTAINERS_BASE_IMAGE={}", build_image),
            ]);
        } else {
            command.args([
                "--build-arg",
                "_DEV_CONTAINERS_BASE_IMAGE=dev_container_auto_added_stage_label",
            ]);
        }

        command.args([
            "--build-arg",
            &format!(
                "_DEV_CONTAINERS_IMAGE_USER={}",
                self.root_image
                    .as_ref()
                    .and_then(|docker_image| docker_image.config.image_user.as_ref())
                    .unwrap_or(&"root".to_string())
            ),
        ]);

        command.args([
            "--build-arg",
            "_DEV_CONTAINERS_FEATURE_CONTENT_SOURCE=dev_container_feature_content_temp",
        ]);

        if let Some(args) = dev_container.build.as_ref().and_then(|b| b.args.as_ref()) {
            for (key, value) in args {
                command.args(["--build-arg", &format!("{}={}", key, value)]);
            }
        }

        command.args(["--target", "dev_containers_target_stage"]);

        command.args([
            "-f",
            &features_build_info.dockerfile_path.display().to_string(),
        ]);

        command.args(["-t", &features_build_info.image_tag]);

        if dev_container.build_type() == DevContainerBuildType::Dockerfile {
            command.arg(self.config_directory.display().to_string());
        } else {
            // Use an empty folder as the build context to avoid pulling in unneeded files.
            // The actual feature content is supplied via the BuildKit build context above.
            command.arg(features_build_info.empty_context_dir.display().to_string());
        }

        dbg!(&command);

        Ok(command)
    }

    // TODO it would be nice if these two functions actually just created the commands and shipped the command to a devcontainer-agnostic docker interface
    async fn run_docker_compose(
        &self,
        resources: DockerComposeResources,
    ) -> Result<DockerInspect, DevContainerError> {
        let mut command = Command::new(self.docker_client.docker_cli());
        // TODO project name how
        command.args(&["compose", "--project-name", "rustwebstarter_devcontainer"]);
        for docker_compose_file in resources.files {
            command.args(&["-f", &docker_compose_file.display().to_string()]);
        }
        command.args(&["up", "-d"]);

        dbg!(&command);

        let output = command.output().await.map_err(|e| {
            log::error!("Error running docker compose up: {e}");
            DevContainerError::CommandFailed(command.get_program().display().to_string())
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("Non-success status from docker compose up: {}", stderr);
            return Err(DevContainerError::CommandFailed(
                command.get_program().display().to_string(),
            ));
        }

        if let Some(docker_ps) = self.check_for_existing_container().await? {
            log::info!("Found newly created dev container");
            return self.docker_client.inspect_image(&docker_ps.id).await;
        }

        log::error!("Could not find existing container after docker compose up");

        Err(DevContainerError::DevContainerParseFailed)
    }

    async fn run_docker_image(
        &self,
        build_resources: DockerBuildResources,
    ) -> Result<DockerInspect, DevContainerError> {
        let mut docker_run_command = self.create_docker_run_command(build_resources)?;

        let output = docker_run_command.output().await.map_err(|e| {
            log::error!("Error running docker run: {e}");
            DevContainerError::CommandFailed(docker_run_command.get_program().display().to_string())
        })?;

        if !output.status.success() {
            let std_err = String::from_utf8_lossy(&output.stderr);
            log::error!("Non-success status from docker run. StdErr: {std_err}");
            return Err(DevContainerError::CommandFailed(
                docker_run_command.get_program().display().to_string(),
            ));
        }

        log::info!("Checking for container that was started");
        let Some(docker_ps) = self.check_for_existing_container().await? else {
            log::error!("Could not locate container just created");
            return Err(DevContainerError::DevContainerParseFailed);
        };
        self.docker_client.inspect_image(&docker_ps.id).await
    }

    fn local_workspace_folder(&self) -> String {
        self.local_project_directory.display().to_string()
    }
    fn local_workspace_base_name(&self) -> Result<String, DevContainerError> {
        self.local_project_directory
            .file_name()
            .map(|f| f.display().to_string())
            .ok_or(DevContainerError::DevContainerParseFailed)
    }

    fn remote_workspace_folder(&self) -> Result<PathBuf, DevContainerError> {
        self.dev_container()
            .workspace_folder
            .as_ref()
            .map(|folder| PathBuf::from(folder))
            .or(Some(
                PathBuf::from(DEFAULT_REMOTE_PROJECT_DIR).join(self.local_workspace_base_name()?),
            ))
            .ok_or(DevContainerError::DevContainerParseFailed)
    }
    fn remote_workspace_base_name(&self) -> Result<String, DevContainerError> {
        self.remote_workspace_folder().and_then(|f| {
            f.file_name()
                .map(|file_name| file_name.display().to_string())
                .ok_or(DevContainerError::DevContainerParseFailed)
        })
    }

    fn remote_workspace_mount(&self) -> Result<PathBuf, DevContainerError> {
        if let Some(mount) = &self.dev_container().workspace_mount {
            return Ok(PathBuf::from(&mount.target));
        }
        let Some(project_directory_name) = self.local_project_directory.file_name() else {
            return Err(DevContainerError::DevContainerParseFailed);
        };

        Ok(PathBuf::from(format!(
            "/workspaces/{}",
            project_directory_name.display()
        )))
    }

    fn create_docker_run_command(
        &self,
        build_resources: DockerBuildResources,
    ) -> Result<Command, DevContainerError> {
        let remote_workspace_folder = self.remote_workspace_mount()?;

        let docker_cli = self.docker_client.docker_cli();
        let mut command = Command::new(&docker_cli);

        command.arg("run");

        if build_resources.privileged {
            command.arg("--privileged");
        }

        if &docker_cli == "podman" {
            command.args(&["--security-opt", "label=disable", "--userns=keep-id"]);
        }

        command.arg("--sig-proxy=false");
        command.arg("-d");
        command.arg("--mount");
        // TODO I think we have to grab the local_project from workspace mount if it's in place as well
        command.arg(format!(
            "type=bind,source={},target={},consistency=cached",
            self.local_project_directory.display(),
            remote_workspace_folder.display(),
        ));

        for mount in &build_resources.additional_mounts {
            command.arg("--mount");
            command.arg(
                mount
                    .to_string()
                    .replace("${devcontainerId}", "devcontainer123"), // TODO So this is what we're doing tomorrow
            );
        }

        for (key, val) in self.identifying_labels() {
            command.arg("-l");
            command.arg(format!("{}={}", key, val));
        }

        if let Some(metadata) = &build_resources.image.config.labels.metadata {
            let serialized_metadata = serde_json_lenient::to_string(metadata).map_err(|e| {
                log::error!("Problem serializing image metadata: {e}");
                DevContainerError::ContainerNotValid(build_resources.image.id.clone())
            })?;
            command.arg("-l");
            command.arg(format!(
                "{}={}",
                "devcontainer.metadata", serialized_metadata
            ));
        }

        command.arg("--entrypoint");
        command.arg("/bin/sh");
        command.arg(&build_resources.image.id);
        command.arg("-c");
        // TODO Probably worth its own method
        let mut entrypoint_script_lines = vec![
            "echo Container started".to_string(),
            "trap \"exit 0\" 15".to_string(),
        ];
        for entrypoint in build_resources.entrypoints {
            entrypoint_script_lines.push(entrypoint.clone());
        }
        entrypoint_script_lines.append(&mut vec![
            "exec \"$@\"".to_string(),
            "while sleep 1 & wait $!; do :; done".to_string(),
        ]);

        command.arg(entrypoint_script_lines.join("\n").trim());
        command.arg("-");

        dbg!(&command);

        Ok(command)
    }

    fn extension_ids(&self) -> Vec<String> {
        self.dev_container()
            .customizations
            .as_ref()
            .map(|c| c.zed.extensions.clone())
            .unwrap_or_default()
    }

    async fn build_and_run(&mut self) -> Result<DevContainerUp, DevContainerError> {
        self.run_initialize_commands().await?;

        self.download_feature_and_dockerfile_resources().await?;

        let build_resources = self.build_resources().await?;

        let devcontainer_up = self.run_dev_container(build_resources).await?;

        self.run_remote_scripts(&devcontainer_up, true).await?;

        Ok(devcontainer_up)
    }

    async fn run_remote_scripts(
        &self,
        devcontainer_up: &DevContainerUp,
        new_container: bool,
    ) -> Result<(), DevContainerError> {
        let ConfigStatus::VariableParsed(config) = &self.config else {
            log::error!("Config not yet parsed, cannot proceed with remote scripts");
            return Err(DevContainerError::DevContainerScriptsFailed);
        };
        let remote_folder = self.remote_workspace_folder()?.display().to_string();

        if new_container {
            if let Some(on_create_command) = &config.on_create_command {
                for (command_name, command) in on_create_command.script_commands() {
                    log::info!("Running on create command {command_name}");
                    // TODO remote env
                    self.docker_client
                        .run_docker_exec(
                            &devcontainer_up.container_id,
                            &remote_folder,
                            "root",
                            &HashMap::new(),
                            command,
                        )
                        .await?;
                }
            }
            if let Some(update_content_command) = &config.update_content_command {
                for (command_name, command) in update_content_command.script_commands() {
                    log::info!("Running update content command {command_name}");
                    // TODO remote env
                    self.docker_client
                        .run_docker_exec(
                            &devcontainer_up.container_id,
                            &remote_folder,
                            "root",
                            &HashMap::new(),
                            command,
                        )
                        .await?;
                }
            }

            if let Some(post_create_command) = &config.post_create_command {
                for (command_name, command) in post_create_command.script_commands() {
                    log::info!("Running post create command {command_name}");
                    // TODO remote env
                    self.docker_client
                        .run_docker_exec(
                            &devcontainer_up.container_id,
                            &remote_folder,
                            &devcontainer_up.remote_user,
                            &HashMap::new(),
                            command,
                        )
                        .await?;
                }
                // user_scripts.push(post_create_command.clone());
            }
            if let Some(post_start_command) = &config.post_start_command {
                for (command_name, command) in post_start_command.script_commands() {
                    log::info!("Running post start command {command_name}");
                    // TODO remote env
                    self.docker_client
                        .run_docker_exec(
                            &devcontainer_up.container_id,
                            &remote_folder,
                            &devcontainer_up.remote_user,
                            &HashMap::new(),
                            command,
                        )
                        .await?;
                }
                // user_scripts.push(post_start_command.clone());
            }
        }
        if let Some(post_attach_command) = &config.post_attach_command {
            for (command_name, command) in post_attach_command.script_commands() {
                log::info!("Running post attach command {command_name}");
                // TODO remote env
                self.docker_client
                    .run_docker_exec(
                        &devcontainer_up.container_id,
                        &remote_folder,
                        &devcontainer_up.remote_user,
                        &HashMap::new(),
                        command,
                    )
                    .await?;
            }
            // user_scripts.push(post_attach_command.clone());
        }

        Ok(())
    }

    async fn run_initialize_commands(&self) -> Result<(), DevContainerError> {
        let ConfigStatus::VariableParsed(config) = &self.config else {
            log::error!("Config not yet parsed, cannot proceed with initializeCommand");
            return Err(DevContainerError::DevContainerParseFailed);
        };

        if let Some(initialize_command) = &config.initialize_command {
            log::info!("Running initialize command");
            initialize_command.run(&self.local_project_directory).await
        } else {
            log::warn!("No initialize command found");
            Ok(())
        }
    }

    async fn check_for_existing_devcontainer(
        &self,
    ) -> Result<Option<DevContainerUp>, DevContainerError> {
        if let Some(docker_ps) = self.check_for_existing_container().await? {
            log::info!("Dev container already found. Proceeding with it");
            //     2. If exists and running, return it
            //
            // TODO this moves into DevContainerManifest

            let docker_inspect = self.docker_client.inspect_image(&docker_ps.id).await?;
            //     3. If exists and not running, start it
            log::info!("TODO start the container if it's not running");

            let remote_user = get_remote_user_from_config(&docker_inspect, self)?;

            let remote_folder = get_remote_dir_from_config(
                &docker_inspect,
                (&self.local_project_directory.display()).to_string(),
            )?;

            let dev_container_up = DevContainerUp {
                _outcome: "todo".to_string(),
                container_id: docker_ps.id,
                remote_user: remote_user,
                remote_workspace_folder: remote_folder,
                extension_ids: self.extension_ids(),
            };

            self.run_remote_scripts(&dev_container_up, false).await?;

            Ok(Some(dev_container_up))
        } else {
            log::info!("Existing container not found.");

            Ok(None)
        }
    }

    async fn check_for_existing_container(&self) -> Result<Option<DockerPs>, DevContainerError> {
        self.docker_client
            .find_process_by_filters(
                self.identifying_labels()
                    .iter()
                    .map(|(k, v)| format!("label={k}={v}"))
                    .collect(),
            )
            .await
    }
}

/// Holds all the information needed to construct a `docker buildx build` command
/// that extends a base image with dev container features.
///
/// This mirrors the `ImageBuildOptions` interface in the CLI reference implementation
/// (cli/src/spec-node/containerFeatures.ts).
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct FeaturesBuildInfo {
    /// Path to the generated Dockerfile.extended
    pub dockerfile_path: PathBuf,
    /// Path to the generated non-BuildKit Dockerfile (for Podman compose)
    pub dockerfile_no_buildkit_path: Option<PathBuf>,
    /// Path to the features content directory (used as a BuildKit build context)
    pub features_content_dir: PathBuf,
    /// Path to an empty directory used as the Docker build context
    pub empty_context_dir: PathBuf,
    /// The base image name (e.g. "mcr.microsoft.com/devcontainers/rust:2-1-bookworm")
    pub build_image: Option<String>,
    /// The tag to apply to the built image (e.g. "vsc-myproject-features")
    pub image_tag: String,
}

pub(crate) async fn read_devcontainer_configuration(
    config: DevContainerConfig,
    context: &DevContainerContext,
    environment: HashMap<String, String>,
) -> Result<DevContainer, DevContainerError> {
    let mut dev_container = DevContainerManifest::new(
        context,
        environment,
        config,
        Arc::new(&context.project_directory.as_ref()),
    )
    .await?;
    dev_container.parse_nonremote_vars()?;
    Ok(dev_container.dev_container().clone())
}

pub(crate) async fn spawn_dev_container(
    context: &DevContainerContext,
    environment: HashMap<String, String>,
    config: DevContainerConfig,
    local_project_path: Arc<&Path>,
) -> Result<DevContainerUp, DevContainerError> {
    let mut devcontainer_manifest =
        DevContainerManifest::new(context, environment, config, local_project_path.clone()).await?;
    // 2. ensure that object is valid
    devcontainer_manifest.validate_config()?;

    // 3. Parse built-in variables
    devcontainer_manifest.parse_nonremote_vars()?;

    log::info!("Checking for existing container");
    if let Some(devcontainer) = devcontainer_manifest
        .check_for_existing_devcontainer()
        .await?
    {
        Ok(devcontainer)
    } else {
        log::info!("Existing container not found. Building");

        devcontainer_manifest.build_and_run().await
    }
}

#[derive(Debug)]
struct DockerBuildResources {
    image: DockerInspect,
    additional_mounts: Vec<MountDefinition>,
    privileged: bool,
    entrypoints: Vec<String>,
}

#[derive(Debug)]
enum DevContainerBuildResources {
    DockerCompose(DockerComposeResources),
    Docker(DockerBuildResources),
}

fn find_primary_service(
    docker_compose: &DockerComposeResources,
    devcontainer: &DevContainerManifest,
) -> Result<(String, DockerComposeService), DevContainerError> {
    let Some(service_name) = &devcontainer.dev_container().service else {
        return Err(DevContainerError::DevContainerParseFailed);
    };

    match docker_compose.config.services.get(service_name) {
        Some(service) => Ok((service_name.clone(), service.clone())),
        None => Err(DevContainerError::DevContainerParseFailed),
    }
}

/// Destination folder inside the container where feature content is staged during build.
/// Mirrors the CLI's `FEATURES_CONTAINER_TEMP_DEST_FOLDER`.
// TODO does this need to be more generalized
const FEATURES_CONTAINER_TEMP_DEST_FOLDER: &str = "/tmp/dev-container-features";

/// Escapes single quotes for use inside shell single-quoted strings.
///
/// Ends the current single-quoted string, inserts an escaped single quote,
/// and reopens the string: `'` → `'\''`.
fn escape_single_quotes(input: &str) -> String {
    input.replace('\'', "'\\''")
}

/// Escapes regex special characters in a string.
fn escape_regex_chars(input: &str) -> String {
    let mut result = String::with_capacity(input.len() * 2);
    for c in input.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            result.push('\\');
        }
        result.push(c);
    }
    result
}

/// Extracts the short feature ID from a full feature reference string.
///
/// Examples:
/// - `ghcr.io/devcontainers/features/aws-cli:1` → `aws-cli`
/// - `ghcr.io/user/repo/go` → `go`
/// - `ghcr.io/devcontainers/features/rust@sha256:abc` → `rust`
/// - `./myFeature` → `myFeature`
fn extract_feature_id(feature_ref: &str) -> &str {
    let without_version = if let Some(at_idx) = feature_ref.rfind('@') {
        &feature_ref[..at_idx]
    } else {
        let last_slash = feature_ref.rfind('/');
        let last_colon = feature_ref.rfind(':');
        match (last_slash, last_colon) {
            (Some(slash), Some(colon)) if colon > slash => &feature_ref[..colon],
            _ => feature_ref,
        }
    };
    match without_version.rfind('/') {
        Some(idx) => &without_version[idx + 1..],
        None => without_version,
    }
}

/// Generates a shell command that looks up a user's passwd entry.
///
/// Mirrors the CLI's `getEntPasswdShellCommand` in `commonUtils.ts`.
/// Tries `getent passwd` first, then falls back to grepping `/etc/passwd`.
// TODO fairly sure this exists elsewhere, we should deduplicate
fn get_ent_passwd_shell_command(user: &str) -> String {
    let escaped_for_shell = user.replace('\\', "\\\\").replace('\'', "\\'");
    let escaped_for_regex = escape_regex_chars(user).replace('\'', "\\'");
    format!(
        " (command -v getent >/dev/null 2>&1 && getent passwd '{shell}' || grep -E '^{re}|^[^:]*:[^:]*:{re}:' /etc/passwd || true)",
        shell = escaped_for_shell,
        re = escaped_for_regex,
    )
}

/// Determines feature installation order, respecting `overrideFeatureInstallOrder`.
///
/// Features listed in the override come first (in the specified order), followed
/// by any remaining features sorted lexicographically by their full reference ID.
fn resolve_feature_order<'a>(
    features: &'a HashMap<String, FeatureOptions>,
    override_order: &Option<Vec<String>>,
) -> Vec<(&'a String, &'a FeatureOptions)> {
    if let Some(order) = override_order {
        let mut ordered: Vec<(&'a String, &'a FeatureOptions)> = Vec::new();
        for ordered_id in order {
            if let Some((key, options)) = features.get_key_value(ordered_id) {
                ordered.push((key, options));
            }
        }
        let mut remaining: Vec<_> = features
            .iter()
            .filter(|(id, _)| !order.iter().any(|o| o == *id))
            .collect();
        remaining.sort_by_key(|(id, _)| id.as_str());
        ordered.extend(remaining);
        ordered
    } else {
        let mut entries: Vec<_> = features.iter().collect();
        entries.sort_by_key(|(id, _)| id.as_str());
        entries
    }
}

/// Generates the `devcontainer-features-install.sh` wrapper script for one feature.
///
/// Mirrors the CLI's `getFeatureInstallWrapperScript` in
/// `containerFeaturesConfiguration.ts`.
fn generate_install_wrapper(feature_ref: &str, feature_id: &str, env_variables: &str) -> String {
    let escaped_id = escape_single_quotes(feature_ref);
    let escaped_name = escape_single_quotes(feature_id);
    let options_indented: String = env_variables
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| format!("    {}", l))
        .collect::<Vec<_>>()
        .join("\n");
    let escaped_options = escape_single_quotes(&options_indented);

    let mut script = String::new();
    script.push_str("#!/bin/sh\n");
    script.push_str("set -e\n");
    script.push_str("\n");
    script.push_str("on_exit () {\n");
    script.push_str("    [ $? -eq 0 ] && exit\n");
    script.push_str("    echo 'ERROR: Feature \"");
    script.push_str(&escaped_name);
    script.push_str("\" (");
    script.push_str(&escaped_id);
    script.push_str(") failed to install!'\n");
    script.push_str("}\n");
    script.push_str("\n");
    script.push_str("trap on_exit EXIT\n");
    script.push_str("\n");
    script.push_str(
        "echo ===========================================================================\n",
    );
    script.push_str("echo 'Feature       : ");
    script.push_str(&escaped_name);
    script.push_str("'\n");
    script.push_str("echo 'Id            : ");
    script.push_str(&escaped_id);
    script.push_str("'\n");
    script.push_str("echo 'Options       :'\n");
    script.push_str("echo '");
    script.push_str(&escaped_options);
    script.push_str("'\n");
    script.push_str(
        "echo ===========================================================================\n",
    );
    script.push_str("\n");
    script.push_str("set -a\n");
    script.push_str(". ../devcontainer-features.builtin.env\n");
    script.push_str(". ./devcontainer-features.env\n");
    script.push_str("set +a\n");
    script.push_str("\n");
    script.push_str("chmod +x ./install.sh\n");
    script.push_str("./install.sh\n");
    script
}

/// Generates a single Dockerfile `RUN` instruction that installs one feature
/// using a BuildKit bind mount.
///
/// Mirrors the v2 BuildKit branch of `getFeatureLayers` in
/// `containerFeaturesConfiguration.ts`.
fn generate_feature_layer(consecutive_id: &str) -> String {
    format!(
        r#"
RUN --mount=type=bind,from=dev_containers_feature_content_source,source=./{id},target=/tmp/build-features-src/{id} \
    cp -ar /tmp/build-features-src/{id} {dest} \
 && chmod -R 0755 {dest}/{id} \
 && cd {dest}/{id} \
 && chmod +x ./devcontainer-features-install.sh \
 && ./devcontainer-features-install.sh \
 && rm -rf {dest}/{id}
"#,
        id = consecutive_id,
        dest = FEATURES_CONTAINER_TEMP_DEST_FOLDER,
    )
}

fn generate_feature_layer_no_buildkit(consecutive_id: &str) -> String {
    let source = format!("/tmp/build-features/{}", consecutive_id);
    let dest = format!("{}/{}", FEATURES_CONTAINER_TEMP_DEST_FOLDER, consecutive_id);
    format!(
        r#"
COPY --chown=root:root --from=dev_containers_feature_content_source {source} {dest}
RUN chmod -R 0755 {dest} \
 && cd {dest} \
 && chmod +x ./devcontainer-features-install.sh \
 && ./devcontainer-features-install.sh

"#
    )
}

// Dockerfile actions need to be moved to their own file
fn dockerfile_alias(dockerfile_content: &str) -> Option<String> {
    dockerfile_content
        .lines()
        .find(|line| line.starts_with("FROM"))
        .and_then(|line| {
            let words: Vec<&str> = line.split(" ").collect();
            if words.len() > 2 && words[words.len() - 2].to_lowercase() == "as" {
                return Some(words[words.len() - 1].to_string());
            } else {
                return None;
            }
        })
}

fn dockerfile_inject_alias(dockerfile_content: &str, alias: &str) -> String {
    if dockerfile_alias(dockerfile_content).is_some() {
        dockerfile_content.to_string()
    } else {
        dockerfile_content
            .lines()
            .map(|line| {
                if line.starts_with("FROM") {
                    format!("{} AS {}", line, alias)
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<String>>()
            .join("\n")
    }
}

//////////////////////////////

/// Generates the full `Dockerfile.extended` content that extends a base image
/// with dev container features.
///
/// Mirrors the CLI's `getContainerFeaturesBaseDockerFile` combined with
/// `getFeatureLayers` (both in `containerFeaturesConfiguration.ts`), using
/// the BuildKit path (named build contexts, `--mount` bind mounts).
fn generate_dockerfile_extended(
    feature_layers: &str,
    container_user: &str,
    remote_user: &str,
    // TODO: use this to optionally include in the template
    // TODO also looks like this needs a test
    // From here, you really just need to change the docker build args to include any args from the build object, and point at the .devcontainer folder instead of the empty dir
    dockerfile_content: Option<String>,
) -> String {
    let container_home_cmd = get_ent_passwd_shell_command(container_user);
    let remote_home_cmd = get_ent_passwd_shell_command(remote_user);
    // So what happens is the reference implementation parses this content and aliases the "FROM" statement to `dev_container_auto_added_stage_label`, then using that as the _DEV_CONTAINERS_BASE_IMAGE arg
    // This is going to require actually parsing Dockerfile. Which means I probably need a docker crate. This is the worst.
    let dockerfile_content = dockerfile_content
        .map(|content| {
            if dockerfile_alias(&content).is_some() {
                content
            } else {
                dockerfile_inject_alias(&content, "dev_container_auto_added_stage_label")
            }
        })
        .unwrap_or("".to_string());

    dbg!(&dockerfile_content);

    let dest = FEATURES_CONTAINER_TEMP_DEST_FOLDER;

    format!(
        r#"ARG _DEV_CONTAINERS_BASE_IMAGE=placeholder

{dockerfile_content}

FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_feature_content_normalize
USER root
COPY --from=dev_containers_feature_content_source ./devcontainer-features.builtin.env /tmp/build-features/
RUN chmod -R 0755 /tmp/build-features/

FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage

USER root

RUN mkdir -p {dest}
COPY --from=dev_containers_feature_content_normalize /tmp/build-features/ {dest}

RUN \
echo "_CONTAINER_USER_HOME=$({container_home_cmd} | cut -d: -f6)" >> {dest}/devcontainer-features.builtin.env && \
echo "_REMOTE_USER_HOME=$({remote_home_cmd} | cut -d: -f6)" >> {dest}/devcontainer-features.builtin.env

{feature_layers}

ARG _DEV_CONTAINERS_IMAGE_USER=root
USER $_DEV_CONTAINERS_IMAGE_USER
"#
    )
}

fn generate_dockerfile_extended_no_buildkit(
    feature_layers: &str,
    container_user: &str,
    remote_user: &str,
    dockerfile_content: Option<String>,
) -> String {
    let container_home_cmd = get_ent_passwd_shell_command(container_user);
    let remote_home_cmd = get_ent_passwd_shell_command(remote_user);
    let dockerfile_content = dockerfile_content
        .map(|content| {
            if dockerfile_alias(&content).is_some() {
                content
            } else {
                dockerfile_inject_alias(&content, "dev_container_auto_added_stage_label")
            }
        })
        .unwrap_or("".to_string());

    let dest = FEATURES_CONTAINER_TEMP_DEST_FOLDER;

    format!(
        r#"ARG _DEV_CONTAINERS_BASE_IMAGE=placeholder

{dockerfile_content}

FROM dev_container_feature_content_temp as dev_containers_feature_content_source

FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_feature_content_normalize
USER root
COPY --from=dev_containers_feature_content_source /tmp/build-features/devcontainer-features.builtin.env /tmp/build-features/
RUN chmod -R 0755 /tmp/build-features/

FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage

USER root

RUN mkdir -p {dest}
COPY --from=dev_containers_feature_content_normalize /tmp/build-features/ {dest}

RUN \
echo "_CONTAINER_USER_HOME=$({container_home_cmd} | cut -d: -f6)" >> {dest}/devcontainer-features.builtin.env && \
echo "_REMOTE_USER_HOME=$({remote_home_cmd} | cut -d: -f6)" >> {dest}/devcontainer-features.builtin.env

{feature_layers}

ARG _DEV_CONTAINERS_IMAGE_USER=root
USER $_DEV_CONTAINERS_IMAGE_USER
"#
    )
}

/// TODO test
fn generate_update_uid_dockerfile() -> String {
    r#"ARG BASE_IMAGE
FROM $BASE_IMAGE

USER root

ARG REMOTE_USER
ARG NEW_UID
ARG NEW_GID
SHELL ["/bin/sh", "-c"]
RUN eval $(sed -n "s/${REMOTE_USER}:[^:]*:\([^:]*\):\([^:]*\):[^:]*:\([^:]*\).*/OLD_UID=\1;OLD_GID=\2;HOME_FOLDER=\3/p" /etc/passwd); \
	eval $(sed -n "s/\([^:]*\):[^:]*:${NEW_UID}:.*/EXISTING_USER=\1/p" /etc/passwd); \
	eval $(sed -n "s/\([^:]*\):[^:]*:${NEW_GID}:.*/EXISTING_GROUP=\1/p" /etc/group); \
	if [ -z "$OLD_UID" ]; then \
		echo "Remote user not found in /etc/passwd ($REMOTE_USER)."; \
	elif [ "$OLD_UID" = "$NEW_UID" -a "$OLD_GID" = "$NEW_GID" ]; then \
		echo "UIDs and GIDs are the same ($NEW_UID:$NEW_GID)."; \
	elif [ "$OLD_UID" != "$NEW_UID" -a -n "$EXISTING_USER" ]; then \
		echo "User with UID exists ($EXISTING_USER=$NEW_UID)."; \
	else \
		if [ "$OLD_GID" != "$NEW_GID" -a -n "$EXISTING_GROUP" ]; then \
			FREE_GID=65532; \
			while grep -q ":[^:]*:${FREE_GID}:" /etc/group; do FREE_GID=$((FREE_GID - 1)); done; \
			echo "Reassigning group $EXISTING_GROUP from GID $NEW_GID to $FREE_GID."; \
			sed -i -e "s/\(${EXISTING_GROUP}:[^:]*:\)${NEW_GID}:/\1${FREE_GID}:/" /etc/group; \
		fi; \
		echo "Updating UID:GID from $OLD_UID:$OLD_GID to $NEW_UID:$NEW_GID."; \
		sed -i -e "s/\(${REMOTE_USER}:[^:]*:\)[^:]*:[^:]*/\1${NEW_UID}:${NEW_GID}/" /etc/passwd; \
		if [ "$OLD_GID" != "$NEW_GID" ]; then \
			sed -i -e "s/\([^:]*:[^:]*:\)${OLD_GID}:/\1${NEW_GID}:/" /etc/group; \
		fi; \
		chown -R $NEW_UID:$NEW_GID $HOME_FOLDER; \
	fi;

ARG IMAGE_USER
USER $IMAGE_USER
"#.to_string()
}

fn image_from_dockerfile(
    devcontainer: &DevContainerManifest,
    dockerfile_contents: String,
) -> Result<String, DevContainerError> {
    let mut raw_contents = dockerfile_contents
        .lines()
        .find(|line| line.starts_with("FROM"))
        .and_then(|from_line| {
            from_line
                .split(' ')
                .collect::<Vec<&str>>()
                .get(1)
                .map(|s| s.to_string())
        })
        .ok_or_else(|| {
            log::error!("Could not find an image definition in dockerfile");
            DevContainerError::DevContainerParseFailed
        })?;

    for (k, v) in devcontainer
        .dev_container()
        .build
        .as_ref()
        .and_then(|b| b.args.as_ref())
        .unwrap_or(&HashMap::new())
    {
        raw_contents = raw_contents.replace(&format!("${{{}}}", k), v);
    }
    Ok(raw_contents)
}

// Container user things
// This should come from spec - see the docs
fn get_remote_user_from_config(
    docker_config: &DockerInspect,
    devcontainer: &DevContainerManifest,
) -> Result<String, DevContainerError> {
    if let DevContainer {
        remote_user: Some(user),
        ..
    } = &devcontainer.dev_container()
    {
        return Ok(user.clone());
    }
    let Some(metadata) = &docker_config.config.labels.metadata else {
        log::error!("Could not locate metadata");
        return Err(DevContainerError::ContainerNotValid(
            docker_config.id.clone(),
        ));
    };
    for metadatum in metadata {
        if let Some(remote_user) = metadatum.get("remoteUser") {
            if let Some(remote_user_str) = remote_user.as_str() {
                return Ok(remote_user_str.to_string());
            }
        }
    }
    log::error!("Could not locate the remote user");
    Err(DevContainerError::ContainerNotValid(
        docker_config.id.clone(),
    ))
}

// This should come from spec - see the docs
fn get_container_user_from_config(
    docker_config: &DockerInspect,
    devcontainer: &DevContainerManifest,
) -> Result<String, DevContainerError> {
    if let Some(user) = &devcontainer.dev_container().container_user {
        return Ok(user.to_string());
    }
    if let Some(metadata) = &docker_config.config.labels.metadata {
        for metadatum in metadata {
            if let Some(container_user) = metadatum.get("containerUser") {
                if let Some(container_user_str) = container_user.as_str() {
                    return Ok(container_user_str.to_string());
                }
            }
        }
    }
    if let Some(image_user) = &docker_config.config.image_user {
        return Ok(image_user.to_string());
    }

    Err(DevContainerError::DevContainerParseFailed)
}

#[cfg(test)]
mod test {
    use std::{collections::HashMap, path::PathBuf, sync::Arc};

    use fs::FakeFs;
    use gpui::{AppContext, TestAppContext};
    use http_client::{FakeHttpClient, HttpClient};
    use project::{
        ProjectEnvironment,
        worktree_store::{WorktreeIdCounter, WorktreeStore},
    };
    use util::paths::SanitizedPath;

    use crate::{
        DevContainerConfig, DevContainerContext,
        devcontainer_api::DevContainerError,
        docker::{DockerConfigLabels, DockerInspectConfig},
        model::{
            ConfigStatus, DevContainerManifest, DockerInspect, extract_feature_id,
            get_remote_user_from_config,
        },
    };
    const TEST_PROJECT_PATH: &str = "/path/to/local/project";

    fn fake_http_client() -> Arc<dyn HttpClient> {
        FakeHttpClient::create(|_| async move {
            Ok(http::Response::builder()
                .status(404)
                .body(http_client::AsyncBody::default())
                .unwrap())
        })
    }

    fn _build_feature_tarball(install_sh_content: &str) -> Vec<u8> {
        smol::block_on(async {
            let buffer = futures::io::Cursor::new(Vec::new());
            let mut builder = async_tar::Builder::new(buffer);

            let data = install_sh_content.as_bytes();
            let mut header = async_tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(async_tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append_data(&mut header, "install.sh", data)
                .await
                .unwrap();

            let buffer = builder.into_inner().await.unwrap();
            buffer.into_inner()
        })
    }

    fn _fake_oci_http_client() -> Arc<dyn HttpClient> {
        let tarball = Arc::new(_build_feature_tarball(
            "#!/bin/sh\nset -e\necho 'Test feature installed'\n",
        ));
        FakeHttpClient::create(move |request| {
            let tarball = tarball.clone();
            async move {
                let uri = request.uri().to_string();
                if uri.contains("/token?") {
                    let body: Vec<u8> = br#"{"token":"fake-test-token"}"#.to_vec();
                    Ok(http::Response::builder()
                        .status(200)
                        .body(body.into())
                        .unwrap())
                } else if uri.contains("/manifests/") {
                    let body: Vec<u8> = br#"{"layers":[{"digest":"sha256:deadbeef"}]}"#.to_vec();
                    Ok(http::Response::builder()
                        .status(200)
                        .body(body.into())
                        .unwrap())
                } else if uri.contains("/blobs/") {
                    let body: Vec<u8> = (*tarball).clone();
                    Ok(http::Response::builder()
                        .status(200)
                        .body(body.into())
                        .unwrap())
                } else {
                    Ok(http::Response::builder()
                        .status(404)
                        .body(http_client::AsyncBody::default())
                        .unwrap())
                }
            }
        })
    }

    fn test_project_filename() -> String {
        PathBuf::from(TEST_PROJECT_PATH)
            .file_name()
            .expect("is valid")
            .display()
            .to_string()
    }

    async fn init_devcontainer_config(
        fs: &Arc<FakeFs>,
        devcontainer_contents: &str,
    ) -> DevContainerConfig {
        fs.insert_tree(
            format!("{TEST_PROJECT_PATH}/.devcontainer"),
            serde_json::json!({"devcontainer.json": devcontainer_contents}),
        )
        .await;

        DevContainerConfig::default_config()
    }

    async fn init_devcontainer_manifest(
        cx: &mut TestAppContext,
        fs: Arc<FakeFs>,
        environment: HashMap<String, String>,
        devcontainer_contents: &str,
    ) -> Result<DevContainerManifest, DevContainerError> {
        let local_config = init_devcontainer_config(&fs, devcontainer_contents).await;
        let http_client = fake_http_client();
        let project_path = SanitizedPath::new_arc(&PathBuf::from(TEST_PROJECT_PATH));
        let worktree_store =
            cx.new(|_cx| WorktreeStore::local(false, fs.clone(), WorktreeIdCounter::default()));
        let project_environment =
            cx.new(|cx| ProjectEnvironment::new(None, worktree_store.downgrade(), None, false, cx));

        let context = DevContainerContext {
            project_directory: SanitizedPath::cast_arc(project_path),
            use_podman: false,
            fs,
            http_client,
            environment: project_environment,
        };
        DevContainerManifest::new(
            &context,
            environment,
            local_config,
            Arc::new(&PathBuf::from(TEST_PROJECT_PATH)),
        )
        .await
    }

    // Tests needed as I come across them
    // - portsAttributes should reference ports defined in forwardPorts
    //   - This can be either a specification (e.g. "db:5432"), a specific port (3000), or a port range (3000-5000)
    //   - So, we need to do a post-parsing validation there
    // - overrideFeatureInstallOrder should include only featuers listed
    // - Shutdownaction can only be none or stopContainer in the non-compose case. Can only be none or stopCompose in the compose case
    // - (docker compose) service needs to be an actually defined service in the yml file
    //   - Eh maybe this just becomes a runtime error that we handle appropriately
    //
    #[test]
    fn should_validate_incorrect_shutdown_action_for_devcontainer() {}

    #[gpui::test]
    async fn should_get_remote_user_from_devcontainer_if_available(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());

        let devcontainer_manifest = init_devcontainer_manifest(
            cx,
            fs,
            HashMap::default(),
            r#"
// These are some external comments. serde_lenient should handle them
{
    // These are some internal comments
    "image": "image",
    "remoteUser": "root",
}
            "#,
        )
        .await
        .unwrap();

        let mut metadata = HashMap::new();
        metadata.insert(
            "remoteUser".to_string(),
            serde_json_lenient::Value::String("vsCode".to_string()),
        );
        let given_docker_config = DockerInspect {
            id: "docker_id".to_string(),
            config: DockerInspectConfig {
                labels: DockerConfigLabels {
                    metadata: Some(vec![metadata]),
                },
                image_user: None,
            },
            mounts: None,
        };

        let remote_user =
            get_remote_user_from_config(&given_docker_config, &devcontainer_manifest).unwrap();

        assert_eq!(remote_user, "root".to_string())
    }

    #[gpui::test]
    async fn should_get_remote_user_from_docker_config(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let devcontainer_manifest = init_devcontainer_manifest(cx, fs, HashMap::default(), "{}")
            .await
            .unwrap();
        let mut metadata = HashMap::new();
        metadata.insert(
            "remoteUser".to_string(),
            serde_json_lenient::Value::String("vsCode".to_string()),
        );
        let given_docker_config = DockerInspect {
            id: "docker_id".to_string(),
            config: DockerInspectConfig {
                labels: DockerConfigLabels {
                    metadata: Some(vec![metadata]),
                },
                image_user: None,
            },
            mounts: None,
        };

        let remote_user = get_remote_user_from_config(&given_docker_config, &devcontainer_manifest);

        assert!(remote_user.is_ok());
        let remote_user = remote_user.expect("ok");
        assert_eq!(&remote_user, "vsCode")
    }

    // This isn't valid, because docker_build doesn't need to be created for an image-only, featureles devcontainer. The call will fail
    // #[gpui::test]
    // async fn should_create_correct_docker_build_command(cx: &mut TestAppContext) {
    //     let fs = FakeFs::new(cx.executor());
    //     let mut devcontainer_manifest = init_devcontainer_manifest(
    //         fs,
    //         HashMap::default(),
    //         r#"
    // {
    //     "image": "mcr.microsoft.com/devcontainers/rust:2-1-bookworm"
    // }
    //         "#,
    //     )
    //     .await
    //     .unwrap();

    //     let features_content_dir =
    //         PathBuf::from("/tmp/devcontainercli/container-features/0.82.0-1234567890");
    //     let dockerfile_path = features_content_dir.join("Dockerfile.extended");
    //     let empty_context_dir = PathBuf::from("/tmp/devcontainercli/empty-folder");

    //     // So these are the "prep" dependencies before we can actually start on build command
    //     devcontainer_manifest.parse_nonremote_vars().unwrap();
    //     devcontainer_manifest
    //         .download_feature_and_dockerfile_resources(&fake_http_client())
    //         .await
    //         .unwrap();

    //     let docker_build_command = devcontainer_manifest.create_docker_build().unwrap();

    //     assert_eq!(docker_build_command.get_program(), "docker");
    //     assert_eq!(
    //         docker_build_command.get_args().collect::<Vec<&OsStr>>(),
    //         vec![
    //             OsStr::new("buildx"),
    //             OsStr::new("build"),
    //             OsStr::new("--load"),
    //             OsStr::new("--build-context"),
    //             OsStr::new(&format!(
    //                 "dev_containers_feature_content_source={}",
    //                 features_content_dir.display()
    //             )),
    //             OsStr::new("--build-arg"),
    //             OsStr::new(
    //                 "_DEV_CONTAINERS_BASE_IMAGE=mcr.microsoft.com/devcontainers/rust:2-1-bookworm"
    //             ),
    //             OsStr::new("--build-arg"),
    //             OsStr::new("_DEV_CONTAINERS_IMAGE_USER=root"),
    //             OsStr::new("--build-arg"),
    //             OsStr::new(
    //                 "_DEV_CONTAINERS_FEATURE_CONTENT_SOURCE=dev_container_feature_content_temp"
    //             ),
    //             OsStr::new("--target"),
    //             OsStr::new("dev_containers_target_stage"),
    //             OsStr::new("-f"),
    //             OsStr::new(&dockerfile_path.display().to_string()),
    //             OsStr::new("-t"),
    //             OsStr::new("vsc-cli-abc123-features"),
    //             OsStr::new(&empty_context_dir.display().to_string()),
    //         ]
    //     );
    // }

    #[test]
    fn should_extract_feature_id_from_references() {
        assert_eq!(
            extract_feature_id("ghcr.io/devcontainers/features/aws-cli:1"),
            "aws-cli"
        );
        assert_eq!(
            extract_feature_id("ghcr.io/devcontainers/features/go"),
            "go"
        );
        assert_eq!(extract_feature_id("ghcr.io/user/repo/node:18.0.0"), "node");
        assert_eq!(extract_feature_id("./myFeature"), "myFeature");
        assert_eq!(
            extract_feature_id("ghcr.io/devcontainers/features/rust@sha256:abc123"),
            "rust"
        );
    }

    //     // Keeping these around since an example of this can probably translate to DevContainerManifest
    //     //
    //     // #[test]
    //     // fn should_construct_features_build_resources() {
    //     //     let client = fake_oci_http_client();
    //     //     smol::block_on(async {
    //     //         let temp_dir = std::env::temp_dir().join("devcontainer-test-features-build");
    //     //         let features_dir = temp_dir.join("features-content");
    //     //         let empty_dir = temp_dir.join("empty");
    //     //         let dockerfile_path = features_dir.join("Dockerfile.extended");

    //     //         let _ = std::fs::remove_dir_all(&temp_dir);
    //     //         std::fs::create_dir_all(&features_dir).unwrap();
    //     //         std::fs::create_dir_all(&empty_dir).unwrap();

    //     //         let build_info = FeaturesBuildInfo {
    //     //             dockerfile_path: dockerfile_path.clone(),
    //     //             features_content_dir: features_dir.clone(),
    //     //             empty_context_dir: empty_dir,
    //     //             base_image: Some("mcr.microsoft.com/devcontainers/rust:2-1-bookworm".to_string()),
    //     //             image_tag: "vsc-test-features".to_string(),
    //     //         };

    //     //         let dev_container = DevContainerManifest {
    //     //             config: DevContainer {
    //     //                 image: Some("mcr.microsoft.com/devcontainers/rust:2-1-bookworm".to_string()),
    //     //                 features: Some(HashMap::from([
    //     //                     (
    //     //                         "ghcr.io/devcontainers/features/aws-cli:1".to_string(),
    //     //                         FeatureOptions::Options(HashMap::new()),
    //     //                     ),
    //     //                     (
    //     //                         "ghcr.io/devcontainers/features/node:1".to_string(),
    //     //                         FeatureOptions::String("18".to_string()),
    //     //                     ),
    //     //                 ])),
    //     //                 remote_user: Some("vscode".to_string()),
    //     //                 ..Default::default()
    //     //             },
    //     //             ..Default::default()
    //     //         };

    //     //         let result =
    //     //             construct_features_build_resources(&dev_container, &build_info, &client, None)
    //     //                 .await;
    //     //         assert!(
    //     //             result.is_ok(),
    //     //             "construct_features_build_resources failed: {:?}",
    //     //             result
    //     //         );

    //     //         // Verify builtin env file
    //     //         let builtin_env =
    //     //             std::fs::read_to_string(features_dir.join("devcontainer-features.builtin.env"))
    //     //                 .unwrap();
    //     //         assert!(builtin_env.contains("_CONTAINER_USER=root"));
    //     //         assert!(builtin_env.contains("_REMOTE_USER=vscode"));

    //     //         // Verify Dockerfile.extended exists and contains expected structures
    //     //         let dockerfile = std::fs::read_to_string(&dockerfile_path).unwrap();
    //     //         assert!(
    //     //             dockerfile
    //     //                 .contains("FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage")
    //     //         );
    //     //         assert!(dockerfile.contains("dev_containers_feature_content_source"));
    //     //         assert!(dockerfile.contains("devcontainer-features-install.sh"));
    //     //         assert!(dockerfile.contains("_DEV_CONTAINERS_IMAGE_USER"));

    //     //         // Verify feature directories (sorted: aws-cli at index 0, node at index 1)
    //     //         assert!(features_dir.join("aws-cli_0").exists());
    //     //         assert!(features_dir.join("node_1").exists());

    //     //         // Verify aws-cli feature files — env should contain defaults from the
    //     //         // fake tarball's devcontainer-feature.json (which has none since our
    //     //         // test tarball doesn't include one), so it will be empty.
    //     //         let aws_env =
    //     //             std::fs::read_to_string(features_dir.join("aws-cli_0/devcontainer-features.env"))
    //     //                 .unwrap();
    //     //         assert!(
    //     //             aws_env.is_empty(),
    //     //             "aws-cli with empty options and no feature json defaults should produce an empty env file, got: {}",
    //     //             aws_env,
    //     //         );

    //     //         let aws_wrapper = std::fs::read_to_string(
    //     //             features_dir.join("aws-cli_0/devcontainer-features-install.sh"),
    //     //         )
    //     //         .unwrap();
    //     //         assert!(aws_wrapper.contains("#!/bin/sh"));
    //     //         assert!(aws_wrapper.contains("./install.sh"));
    //     //         assert!(aws_wrapper.contains("../devcontainer-features.builtin.env"));

    //     //         let aws_install =
    //     //             std::fs::read_to_string(features_dir.join("aws-cli_0/install.sh")).unwrap();
    //     //         assert!(
    //     //             aws_install.contains("Test feature installed"),
    //     //             "install.sh should contain content from the OCI tarball, got: {}",
    //     //             aws_install
    //     //         );

    //     //         // Verify node feature files (String("18") → VERSION="18")
    //     //         let node_env =
    //     //             std::fs::read_to_string(features_dir.join("node_1/devcontainer-features.env"))
    //     //                 .unwrap();
    //     //         assert!(
    //     //             node_env.contains("VERSION=\"18\""),
    //     //             "Expected VERSION=\"18\" in node env, got: {}",
    //     //             node_env
    //     //         );

    //     //         // Verify Dockerfile layers reference both features
    //     //         assert!(dockerfile.contains("aws-cli_0"));
    //     //         assert!(dockerfile.contains("node_1"));

    //     //         let _ = std::fs::remove_dir_all(&temp_dir);
    //     //     });
    //     // }
    //     // #[test]
    //     // fn should_construct_features_with_override_order() {
    //     //     let client = fake_oci_http_client();
    //     //     smol::block_on(async {
    //     //         let temp_dir = std::env::temp_dir().join("devcontainer-test-features-order");
    //     //         let features_dir = temp_dir.join("features-content");
    //     //         let empty_dir = temp_dir.join("empty");
    //     //         let dockerfile_path = features_dir.join("Dockerfile.extended");

    //     //         let _ = std::fs::remove_dir_all(&temp_dir);
    //     //         std::fs::create_dir_all(&features_dir).unwrap();
    //     //         std::fs::create_dir_all(&empty_dir).unwrap();

    //     //         let build_info = FeaturesBuildInfo {
    //     //             dockerfile_path: dockerfile_path.clone(),
    //     //             features_content_dir: features_dir.clone(),
    //     //             empty_context_dir: empty_dir,
    //     //             build_image: Some("mcr.microsoft.com/devcontainers/base:ubuntu".to_string()),
    //     //             image_tag: "vsc-test-order".to_string(),
    //     //         };

    //     //         let dev_container = DevContainerManifest {
    //     //             config: DevContainer {
    //     //                 image: Some("mcr.microsoft.com/devcontainers/base:ubuntu".to_string()),
    //     //                 features: Some(HashMap::from([
    //     //                     (
    //     //                         "ghcr.io/devcontainers/features/aws-cli:1".to_string(),
    //     //                         FeatureOptions::Options(HashMap::new()),
    //     //                     ),
    //     //                     (
    //     //                         "ghcr.io/devcontainers/features/node:1".to_string(),
    //     //                         FeatureOptions::Options(HashMap::from([(
    //     //                             "version".to_string(),
    //     //                             FeatureOptionValue::String("20".to_string()),
    //     //                         )])),
    //     //                     ),
    //     //                 ])),
    //     //                 override_feature_install_order: Some(vec![
    //     //                     "ghcr.io/devcontainers/features/node:1".to_string(),
    //     //                     "ghcr.io/devcontainers/features/aws-cli:1".to_string(),
    //     //                 ]),
    //     //                 ..Default::default()
    //     //             },
    //     //             ..Default::default()
    //     //         };

    //     //         let result =
    //     //             construct_features_build_resources(&dev_container, &build_info, &client, None)
    //     //                 .await;
    //     //         assert!(result.is_ok());

    //     //         // With override order: node first (index 0), aws-cli second (index 1)
    //     //         assert!(features_dir.join("node_0").exists());
    //     //         assert!(features_dir.join("aws-cli_1").exists());

    //     //         let node_env =
    //     //             std::fs::read_to_string(features_dir.join("node_0/devcontainer-features.env"))
    //     //                 .unwrap();
    //     //         assert!(
    //     //             node_env.contains("version=\"20\""),
    //     //             "Expected version=\"20\" in node env, got: {}",
    //     //             node_env
    //     //         );

    //     //         // Verify the Dockerfile layers appear in the right order
    //     //         let dockerfile = std::fs::read_to_string(&dockerfile_path).unwrap();
    //     //         let node_pos = dockerfile.find("node_0").expect("node_0 layer missing");
    //     //         let aws_pos = dockerfile
    //     //             .find("aws-cli_1")
    //     //             .expect("aws-cli_1 layer missing");
    //     //         assert!(
    //     //             node_pos < aws_pos,
    //     //             "node should appear before aws-cli in the Dockerfile"
    //     //         );

    //     //         let _ = std::fs::remove_dir_all(&temp_dir);
    //     //     });
    //     // }

    //     // #[test]
    //     // fn should_skip_disabled_features() {
    //     //     let client = fake_oci_http_client();
    //     //     smol::block_on(async {
    //     //         let temp_dir = std::env::temp_dir().join("devcontainer-test-features-disabled");
    //     //         let features_dir = temp_dir.join("features-content");
    //     //         let empty_dir = temp_dir.join("empty");
    //     //         let dockerfile_path = features_dir.join("Dockerfile.extended");

    //     //         let _ = std::fs::remove_dir_all(&temp_dir);
    //     //         std::fs::create_dir_all(&features_dir).unwrap();
    //     //         std::fs::create_dir_all(&empty_dir).unwrap();

    //     //         let build_info = FeaturesBuildInfo {
    //     //             dockerfile_path: dockerfile_path.clone(),
    //     //             features_content_dir: features_dir.clone(),
    //     //             empty_context_dir: empty_dir,
    //     //             build_image: Some("mcr.microsoft.com/devcontainers/base:ubuntu".to_string()),
    //     //             image_tag: "vsc-test-disabled".to_string(),
    //     //         };

    //     //         let dev_container = DevContainerManifest {
    //     //             config: DevContainer {
    //     //                 image: Some("mcr.microsoft.com/devcontainers/base:ubuntu".to_string()),
    //     //                 features: Some(HashMap::from([
    //     //                     (
    //     //                         "ghcr.io/devcontainers/features/aws-cli:1".to_string(),
    //     //                         FeatureOptions::Bool(false),
    //     //                     ),
    //     //                     (
    //     //                         "ghcr.io/devcontainers/features/node:1".to_string(),
    //     //                         FeatureOptions::Bool(true),
    //     //                     ),
    //     //                 ])),
    //     //                 ..Default::default()
    //     //             },
    //     //             ..Default::default()
    //     //         };

    //     //         let result =
    //     //             construct_features_build_resources(&dev_container, &build_info, &client, None)
    //     //                 .await;
    //     //         assert!(result.is_ok());

    //     //         // aws-cli is disabled (false) — its directory should not exist
    //     //         assert!(!features_dir.join("aws-cli_0").exists());
    //     //         // node is enabled (true) — its directory should exist
    //     //         assert!(features_dir.join("node_1").exists());

    //     //         let dockerfile = std::fs::read_to_string(&dockerfile_path).unwrap();
    //     //         assert!(!dockerfile.contains("aws-cli_0"));
    //     //         assert!(dockerfile.contains("node_1"));

    //     //         let _ = std::fs::remove_dir_all(&temp_dir);
    //     //     });
    //     // }

    //     // #[test]
    //     // fn should_fail_when_oci_download_fails() {
    //     //     let client = fake_http_client();
    //     //     smol::block_on(async {
    //     //         let temp_dir = std::env::temp_dir().join("devcontainer-test-features-fail");
    //     //         let features_dir = temp_dir.join("features-content");
    //     //         let empty_dir = temp_dir.join("empty");
    //     //         let dockerfile_path = features_dir.join("Dockerfile.extended");

    //     //         let _ = std::fs::remove_dir_all(&temp_dir);
    //     //         std::fs::create_dir_all(&features_dir).unwrap();
    //     //         std::fs::create_dir_all(&empty_dir).unwrap();

    //     //         let build_info = FeaturesBuildInfo {
    //     //             dockerfile_path: dockerfile_path.clone(),
    //     //             features_content_dir: features_dir.clone(),
    //     //             empty_context_dir: empty_dir,
    //     //             build_image: Some("mcr.microsoft.com/devcontainers/base:ubuntu".to_string()),
    //     //             image_tag: "vsc-test-fail".to_string(),
    //     //         };

    //     //         let dev_container = DevContainerManifest {
    //     //             config: DevContainer {
    //     //                 image: Some("mcr.microsoft.com/devcontainers/base:ubuntu".to_string()),
    //     //                 features: Some(HashMap::from([(
    //     //                     "ghcr.io/devcontainers/features/go:1".to_string(),
    //     //                     FeatureOptions::Options(HashMap::new()),
    //     //                 )])),
    //     //                 ..Default::default()
    //     //             },
    //     //             ..Default::default()
    //     //         };

    //     //         let result =
    //     //             construct_features_build_resources(&dev_container, &build_info, &client, None)
    //     //                 .await;
    //     //         assert!(
    //     //             result.is_err(),
    //     //             "Expected error when OCI download fails, but got Ok"
    //     //         );

    //     //         let _ = std::fs::remove_dir_all(&temp_dir);
    //     //     });
    //     // }

    //     #[test]
    //     fn should_create_correct_docker_run_command() {
    //         let mut metadata = HashMap::new();
    //         metadata.insert(
    //             "remoteUser".to_string(),
    //             serde_json_lenient::Value::String("vsCode".to_string()),
    //         );

    //         let devcontainer_manifest = DevContainerManifest {
    //             local_project_directory: PathBuf::from("/path/to/local/project"),
    //             config_directory: PathBuf::from("/path/to/local/project/.devcontainer"),
    //             file_name: "devcontainer.json".to_string(),
    //             ..Default::default()
    //         };

    //         let build_resources = DockerBuildResources {
    //             image: DockerInspect {
    //                 id: "mcr.microsoft.com/devcontainers/base:ubuntu".to_string(),
    //                 config: DockerInspectConfig {
    //                     labels: DockerConfigLabels { metadata: None },
    //                     image_user: None,
    //                 },
    //                 mounts: None,
    //             },
    //             additional_mounts: vec![],
    //             privileged: false,
    //             entrypoints: vec![],
    //         };
    //         let docker_run_command = devcontainer_manifest.create_docker_run_command(build_resources);

    //         assert!(docker_run_command.is_ok());
    //         let docker_run_command = docker_run_command.expect("ok");

    //         assert_eq!(docker_run_command.get_program(), "docker");
    //         assert_eq!(
    //             docker_run_command.get_args().collect::<Vec<&OsStr>>(),
    //             vec![
    //                 OsStr::new("run"),
    //                 OsStr::new("--sig-proxy=false"),
    //                 OsStr::new("-d"),
    //                 OsStr::new("--mount"),
    //                 OsStr::new(
    //                     "type=bind,source=/local/project_app,target=/workspaces/project_app,consistency=cached"
    //                 ),
    //                 OsStr::new("-l"),
    //                 OsStr::new("label1=value1"),
    //                 OsStr::new("-l"),
    //                 OsStr::new("label2=value2"),
    //                 OsStr::new("-l"),
    //                 OsStr::new(
    //                     r#"devcontainer.metadata=[{"id":"ghcr.io/devcontainers/features/common-utils:2"},{"id":"ghcr.io/devcontainers/features/git:1","customizations":{"vscode":{"settings":{"github.copilot.chat.codeGeneration.instructions":[{"text":"This dev container includes an up-to-date version of Git, built from source as needed, pre-installed and available on the `PATH`."}]}}}},{"remoteUser":"vscode"}]"#
    //                 ),
    //                 OsStr::new("--entrypoint"),
    //                 OsStr::new("/bin/sh"),
    //                 OsStr::new("mcr.microsoft.com/devcontainers/base:ubuntu"),
    //                 OsStr::new("-c"),
    //                 OsStr::new(
    //                     "
    // echo Container started
    // trap \"exit 0\" 15
    // exec \"$@\"
    // while sleep 1 & wait $!; do :; done
    //                     "
    //                     .trim()
    //                 ),
    //                 OsStr::new("-"),
    //             ]
    //         )
    //     }

    //     #[test]
    //     fn should_deserialize_docker_labels() {
    //         let given_config = r#"
    // {"Id":"fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75","Created":"2026-02-09T23:22:15.585555798Z","Path":"/bin/sh","Args":["-c","echo Container started\ntrap \"exit 0\" 15\nexec \"$@\"\nwhile sleep 1 & wait $!; do :; done","-"],"State":{"Status":"running","Running":true,"Paused":false,"Restarting":false,"OOMKilled":false,"Dead":false,"Pid":94196,"ExitCode":0,"Error":"","StartedAt":"2026-02-09T23:22:15.628810548Z","FinishedAt":"0001-01-01T00:00:00Z"},"Image":"sha256:3dcb059253b2ebb44de3936620e1cff3dadcd2c1c982d579081ca8128c1eb319","ResolvConfPath":"/var/lib/docker/containers/fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75/resolv.conf","HostnamePath":"/var/lib/docker/containers/fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75/hostname","HostsPath":"/var/lib/docker/containers/fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75/hosts","LogPath":"/var/lib/docker/containers/fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75/fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75-json.log","Name":"/magical_easley","RestartCount":0,"Driver":"overlayfs","Platform":"linux","MountLabel":"","ProcessLabel":"","AppArmorProfile":"","ExecIDs":null,"HostConfig":{"Binds":null,"ContainerIDFile":"","LogConfig":{"Type":"json-file","Config":{}},"NetworkMode":"bridge","PortBindings":{},"RestartPolicy":{"Name":"no","MaximumRetryCount":0},"AutoRemove":false,"VolumeDriver":"","VolumesFrom":null,"ConsoleSize":[0,0],"CapAdd":null,"CapDrop":null,"CgroupnsMode":"private","Dns":[],"DnsOptions":[],"DnsSearch":[],"ExtraHosts":null,"GroupAdd":null,"IpcMode":"private","Cgroup":"","Links":null,"OomScoreAdj":0,"PidMode":"","Privileged":false,"PublishAllPorts":false,"ReadonlyRootfs":false,"SecurityOpt":null,"UTSMode":"","UsernsMode":"","ShmSize":67108864,"Runtime":"runc","Isolation":"","CpuShares":0,"Memory":0,"NanoCpus":0,"CgroupParent":"","BlkioWeight":0,"BlkioWeightDevice":[],"BlkioDeviceReadBps":[],"BlkioDeviceWriteBps":[],"BlkioDeviceReadIOps":[],"BlkioDeviceWriteIOps":[],"CpuPeriod":0,"CpuQuota":0,"CpuRealtimePeriod":0,"CpuRealtimeRuntime":0,"CpusetCpus":"","CpusetMems":"","Devices":[],"DeviceCgroupRules":null,"DeviceRequests":null,"MemoryReservation":0,"MemorySwap":0,"MemorySwappiness":null,"OomKillDisable":null,"PidsLimit":null,"Ulimits":[],"CpuCount":0,"CpuPercent":0,"IOMaximumIOps":0,"IOMaximumBandwidth":0,"Mounts":[{"Type":"bind","Source":"/somepath/rustwebstarter","Target":"/workspaces/rustwebstarter","Consistency":"cached"}],"MaskedPaths":["/proc/asound","/proc/acpi","/proc/interrupts","/proc/kcore","/proc/keys","/proc/latency_stats","/proc/timer_list","/proc/timer_stats","/proc/sched_debug","/proc/scsi","/sys/firmware","/sys/devices/virtual/powercap"],"ReadonlyPaths":["/proc/bus","/proc/fs","/proc/irq","/proc/sys","/proc/sysrq-trigger"]},"GraphDriver":{"Data":null,"Name":"overlayfs"},"Mounts":[{"Type":"bind","Source":"/somepath/rustwebstarter","Destination":"/workspaces/rustwebstarter","Mode":"","RW":true,"Propagation":"rprivate"}],"Config":{"Hostname":"fca38334e88f","Domainname":"","User":"root","AttachStdin":false,"AttachStdout":false,"AttachStderr":false,"Tty":false,"OpenStdin":false,"StdinOnce":false,"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],"Cmd":["-c","echo Container started\ntrap \"exit 0\" 15\nexec \"$@\"\nwhile sleep 1 & wait $!; do :; done","-"],"Image":"mcr.microsoft.com/devcontainers/base:ubuntu","Volumes":null,"WorkingDir":"","Entrypoint":["/bin/sh"],"OnBuild":null,"Labels":{"dev.containers.features":"common","dev.containers.id":"base-ubuntu","dev.containers.release":"v0.4.24","dev.containers.source":"https://github.com/devcontainers/images","dev.containers.timestamp":"Fri, 30 Jan 2026 16:52:34 GMT","dev.containers.variant":"noble","devcontainer.config_file":".devcontainer/devcontainer.json","devcontainer.local_folder":"/somepath/rustwebstarter","devcontainer.metadata":"[ {\"id\":\"ghcr.io/devcontainers/features/common-utils:2\"}, {\"id\":\"ghcr.io/devcontainers/features/git:1\",\"customizations\":{\"vscode\":{\"settings\":{\"github.copilot.chat.codeGeneration.instructions\":[{\"text\":\"This dev container includes an up-to-date version of Git, built from source as needed, pre-installed and available on the `PATH`.\"}]}}}}, {\"remoteUser\":\"vscode\"} ]","org.opencontainers.image.ref.name":"ubuntu","org.opencontainers.image.version":"24.04","version":"2.1.6"},"StopTimeout":1},"NetworkSettings":{"Bridge":"","SandboxID":"ef2f9f610d87de6bf6061627a0cadb2b89e918bafba92e0e4e9e877d092315c7","SandboxKey":"/var/run/docker/netns/ef2f9f610d87","Ports":{},"HairpinMode":false,"LinkLocalIPv6Address":"","LinkLocalIPv6PrefixLen":0,"SecondaryIPAddresses":null,"SecondaryIPv6Addresses":null,"EndpointID":"50b3501ee308c36e212a025b4f4ddd4ffbd6aeeafa986350ea7d9fe5e16e2c8c","Gateway":"172.17.0.1","GlobalIPv6Address":"","GlobalIPv6PrefixLen":0,"IPAddress":"172.17.0.4","IPPrefixLen":16,"IPv6Gateway":"","MacAddress":"ca:02:9f:22:fd:8e","Networks":{"bridge":{"IPAMConfig":null,"Links":null,"Aliases":null,"MacAddress":"ca:02:9f:22:fd:8e","DriverOpts":null,"GwPriority":0,"NetworkID":"51bb8ccc4d1281db44f16d915963fc728619d4a68e2f90e5ea8f1cb94885063e","EndpointID":"50b3501ee308c36e212a025b4f4ddd4ffbd6aeeafa986350ea7d9fe5e16e2c8c","Gateway":"172.17.0.1","IPAddress":"172.17.0.4","IPPrefixLen":16,"IPv6Gateway":"","GlobalIPv6Address":"","GlobalIPv6PrefixLen":0,"DNSNames":null}}},"ImageManifestDescriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:39c3436527190561948236894c55b59fa58aa08d68d8867e703c8d5ab72a3593","size":2195,"platform":{"architecture":"arm64","os":"linux"}}}
    //             "#;

    //         let deserialized = serde_json_lenient::from_str::<DockerInspect>(given_config);
    //         assert!(deserialized.is_ok());
    //         let config = deserialized.unwrap();
    //         let remote_user = get_remote_user_from_config(
    //             &config,
    //             &DevContainerManifest {
    //                 config: DevContainer {
    //                     image: None,
    //                     name: None,
    //                     remote_user: None,
    //                     ..Default::default()
    //                 },
    //                 ..Default::default()
    //             },
    //         );

    //         assert!(remote_user.is_ok());
    //         assert_eq!(remote_user.unwrap(), "vscode".to_string())
    //     }

    //     #[test]
    //     fn should_inject_correct_parameters_into_dockerfile_extended() {
    //         let (feature_layers, container_user, remote_user) = (
    //             r#"RUN --mount=type=bind,from=dev_containers_feature_content_source,source=./copilot-cli_0,target=/tmp/build-features-src/copilot-cli_0 \
    //     cp -ar /tmp/build-features-src/copilot-cli_0 /tmp/dev-container-features \
    // && chmod -R 0755 /tmp/dev-container-features/copilot-cli_0 \
    // && cd /tmp/dev-container-features/copilot-cli_0 \
    // && chmod +x ./devcontainer-features-install.sh \
    // && ./devcontainer-features-install.sh \
    // && rm -rf /tmp/dev-container-features/copilot-cli_0
    //             "#.trim(),
    //             "container_user",
    //             "remote_user",
    //         );

    //         let dockerfile_extended =
    //             generate_dockerfile_extended(feature_layers, container_user, remote_user, None);

    //         assert_eq!(dockerfile_extended.trim(),
    //             r#"ARG _DEV_CONTAINERS_BASE_IMAGE=placeholder

    // FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_feature_content_normalize
    // USER root
    // COPY --from=dev_containers_feature_content_source ./devcontainer-features.builtin.env /tmp/build-features/
    // RUN chmod -R 0755 /tmp/build-features/

    // FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage

    // USER root

    // RUN mkdir -p /tmp/dev-container-features
    // COPY --from=dev_containers_feature_content_normalize /tmp/build-features/ /tmp/dev-container-features

    // RUN \
    // echo "_CONTAINER_USER_HOME=$( (command -v getent >/dev/null 2>&1 && getent passwd 'container_user' || grep -E '^container_user|^[^:]*:[^:]*:container_user:' /etc/passwd || true) | cut -d: -f6)" >> /tmp/dev-container-features/devcontainer-features.builtin.env && \
    // echo "_REMOTE_USER_HOME=$( (command -v getent >/dev/null 2>&1 && getent passwd 'remote_user' || grep -E '^remote_user|^[^:]*:[^:]*:remote_user:' /etc/passwd || true) | cut -d: -f6)" >> /tmp/dev-container-features/devcontainer-features.builtin.env

    // RUN --mount=type=bind,from=dev_containers_feature_content_source,source=./copilot-cli_0,target=/tmp/build-features-src/copilot-cli_0 \
    //     cp -ar /tmp/build-features-src/copilot-cli_0 /tmp/dev-container-features \
    // && chmod -R 0755 /tmp/dev-container-features/copilot-cli_0 \
    // && cd /tmp/dev-container-features/copilot-cli_0 \
    // && chmod +x ./devcontainer-features-install.sh \
    // && ./devcontainer-features-install.sh \
    // && rm -rf /tmp/dev-container-features/copilot-cli_0

    // ARG _DEV_CONTAINERS_IMAGE_USER=root
    // USER $_DEV_CONTAINERS_IMAGE_USER
    //             "#.trim()
    //         );

    //         let dockerfile = r#"
    // ARG VARIANT="16-bullseye"
    // FROM mcr.microsoft.com/devcontainers/typescript-node:1-${VARIANT}

    // RUN mkdir -p /workspaces && chown node:node /workspaces

    // ARG USERNAME=node
    // USER $USERNAME

    // # Save command line history
    // RUN echo "hello, world""#
    //             .trim()
    //             .to_string();

    //         let dockerfile_extended = generate_dockerfile_extended(
    //             feature_layers,
    //             container_user,
    //             remote_user,
    //             Some(dockerfile),
    //         );

    //         assert_eq!(dockerfile_extended.trim(),
    //             r#"ARG _DEV_CONTAINERS_BASE_IMAGE=placeholder

    // ARG VARIANT="16-bullseye"
    // FROM mcr.microsoft.com/devcontainers/typescript-node:1-${VARIANT} AS dev_container_auto_added_stage_label

    // RUN mkdir -p /workspaces && chown node:node /workspaces

    // ARG USERNAME=node
    // USER $USERNAME

    // # Save command line history
    // RUN echo "hello, world"

    // FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_feature_content_normalize
    // USER root
    // COPY --from=dev_containers_feature_content_source ./devcontainer-features.builtin.env /tmp/build-features/
    // RUN chmod -R 0755 /tmp/build-features/

    // FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage

    // USER root

    // RUN mkdir -p /tmp/dev-container-features
    // COPY --from=dev_containers_feature_content_normalize /tmp/build-features/ /tmp/dev-container-features

    // RUN \
    // echo "_CONTAINER_USER_HOME=$( (command -v getent >/dev/null 2>&1 && getent passwd 'container_user' || grep -E '^container_user|^[^:]*:[^:]*:container_user:' /etc/passwd || true) | cut -d: -f6)" >> /tmp/dev-container-features/devcontainer-features.builtin.env && \
    // echo "_REMOTE_USER_HOME=$( (command -v getent >/dev/null 2>&1 && getent passwd 'remote_user' || grep -E '^remote_user|^[^:]*:[^:]*:remote_user:' /etc/passwd || true) | cut -d: -f6)" >> /tmp/dev-container-features/devcontainer-features.builtin.env

    // RUN --mount=type=bind,from=dev_containers_feature_content_source,source=./copilot-cli_0,target=/tmp/build-features-src/copilot-cli_0 \
    //     cp -ar /tmp/build-features-src/copilot-cli_0 /tmp/dev-container-features \
    // && chmod -R 0755 /tmp/dev-container-features/copilot-cli_0 \
    // && cd /tmp/dev-container-features/copilot-cli_0 \
    // && chmod +x ./devcontainer-features-install.sh \
    // && ./devcontainer-features-install.sh \
    // && rm -rf /tmp/dev-container-features/copilot-cli_0

    // ARG _DEV_CONTAINERS_IMAGE_USER=root
    // USER $_DEV_CONTAINERS_IMAGE_USER
    //             "#.trim()
    //         );
    //     }

    //     #[test]
    //     fn should_create_docker_compose_command() {
    //         let docker_compose_files = vec![
    //             PathBuf::from("/var/test/docker-compose.yml"),
    //             PathBuf::from("/var/other/docker-compose2.yml"),
    //         ];

    //         let command = create_docker_compose_config_command(&docker_compose_files).unwrap();

    //         assert_eq!(command.get_program(), OsStr::new(docker_cli()));

    //         assert_eq!(
    //             command.get_args().collect::<Vec<&OsStr>>(),
    //             vec![
    //                 OsStr::new("compose"),
    //                 OsStr::new("-f"),
    //                 OsStr::new("/var/test/docker-compose.yml"),
    //                 OsStr::new("-f"),
    //                 OsStr::new("/var/other/docker-compose2.yml"),
    //                 OsStr::new("config"),
    //                 OsStr::new("--format"),
    //                 OsStr::new("json"),
    //             ]
    //         )
    //     }

    //     #[test]
    //     fn should_deserialize_docker_compose_config() {
    //         let given_config = r#"
    // {
    //     "name": "devcontainer",
    //     "networks": {
    //     "default": {
    //         "name": "devcontainer_default",
    //         "ipam": {}
    //     }
    //     },
    //     "services": {
    //         "app": {
    //             "command": [
    //             "sleep",
    //             "infinity"
    //             ],
    //             "depends_on": {
    //             "db": {
    //                 "condition": "service_started",
    //                 "restart": true,
    //                 "required": true
    //             }
    //             },
    //             "entrypoint": null,
    //             "environment": {
    //             "POSTGRES_DB": "postgres",
    //             "POSTGRES_HOSTNAME": "localhost",
    //             "POSTGRES_PASSWORD": "postgres",
    //             "POSTGRES_PORT": "5432",
    //             "POSTGRES_USER": "postgres"
    //             },
    //             "image": "mcr.microsoft.com/devcontainers/rust:2-1-bookworm",
    //             "network_mode": "service:db",
    //             "volumes": [
    //             {
    //                 "type": "bind",
    //                 "source": "/Users/kylebarton/Source",
    //                 "target": "/workspaces",
    //                 "bind": {
    //                 "create_host_path": true
    //                 }
    //             }
    //             ]
    //         },
    //         "db": {
    //             "command": null,
    //             "entrypoint": null,
    //             "environment": {
    //             "POSTGRES_DB": "postgres",
    //             "POSTGRES_HOSTNAME": "localhost",
    //             "POSTGRES_PASSWORD": "postgres",
    //             "POSTGRES_PORT": "5432",
    //             "POSTGRES_USER": "postgres"
    //             },
    //             "image": "postgres:14.1",
    //             "networks": {
    //             "default": null
    //             },
    //             "restart": "unless-stopped",
    //             "volumes": [
    //             {
    //                 "type": "volume",
    //                 "source": "postgres-data",
    //                 "target": "/var/lib/postgresql/data",
    //                 "volume": {}
    //             }
    //             ]
    //         }
    //     },
    //     "volumes": {
    //     "postgres-data": {
    //         "name": "devcontainer_postgres-data"
    //     }
    //     }
    // }
    //             "#;

    //         let docker_compose_config: DockerComposeConfig =
    //             serde_json_lenient::from_str(given_config).unwrap();

    //         let expected_config = DockerComposeConfig {
    //             name: Some("devcontainer".to_string()),
    //             services: HashMap::from([
    //                 (
    //                     "app".to_string(),
    //                     DockerComposeService {
    //                         image: Some(
    //                             "mcr.microsoft.com/devcontainers/rust:2-1-bookworm".to_string(),
    //                         ),
    //                         ..Default::default()
    //                     },
    //                 ),
    //                 (
    //                     "db".to_string(),
    //                     DockerComposeService {
    //                         image: Some("postgres:14.1".to_string()),
    //                         ..Default::default()
    //                     },
    //                 ),
    //             ]),
    //             ..Default::default()
    //         };

    //         assert_eq!(docker_compose_config, expected_config);
    //     }

    //     #[test]
    //     fn should_find_primary_service_in_docker_compose() {
    //         // State where service not defined in dev container
    //         let given_dev_container = DevContainerManifest::default();
    //         let given_docker_compose_config = DockerComposeResources {
    //             config: DockerComposeConfig {
    //                 name: Some("devcontainers".to_string()),
    //                 services: HashMap::new(),
    //                 ..Default::default()
    //             },
    //             ..Default::default()
    //         };

    //         let bad_result = find_primary_service(&given_docker_compose_config, &given_dev_container);

    //         assert!(bad_result.is_err());

    //         // State where service defined in devcontainer, not found in DockerCompose config
    //         let given_dev_container = DevContainerManifest {
    //             config: DevContainer {
    //                 service: Some("not_found_service".to_string()),
    //                 ..Default::default()
    //             },
    //             ..Default::default()
    //         };
    //         let given_docker_compose_config = DockerComposeResources {
    //             config: DockerComposeConfig {
    //                 name: Some("devcontainers".to_string()),
    //                 services: HashMap::new(),
    //                 ..Default::default()
    //             },
    //             ..Default::default()
    //         };

    //         let bad_result = find_primary_service(&given_docker_compose_config, &given_dev_container);

    //         assert!(bad_result.is_err());
    //         // State where service defined in devcontainer and in DockerCompose config
    //         let given_dev_container = DevContainerManifest {
    //             config: DevContainer {
    //                 service: Some("found_service".to_string()),
    //                 ..Default::default()
    //             },
    //             ..Default::default()
    //         };
    //         let given_docker_compose_config = DockerComposeResources {
    //             config: DockerComposeConfig {
    //                 name: Some("devcontainers".to_string()),
    //                 services: HashMap::from([(
    //                     "found_service".to_string(),
    //                     DockerComposeService {
    //                         ..Default::default()
    //                     },
    //                 )]),
    //                 ..Default::default()
    //             },
    //             ..Default::default()
    //         };

    //         let (service_name, _) =
    //             find_primary_service(&given_docker_compose_config, &given_dev_container).unwrap();

    //         assert_eq!(service_name, "found_service".to_string());
    //     }

    //     #[test]
    //     fn should_build_runtime_override() {
    //         let devcontainer_manifest = DevContainerManifest {
    //             local_project_directory: PathBuf::from("/path/to/project"),
    //             config_directory: PathBuf::from("/path/to/project/.devcontainer"),
    //             file_name: "devcontainer.json".to_string(),
    //             ..Default::default()
    //         };

    //         let docker_image = DockerInspect {
    //             id: "id".to_string(),
    //             // Todo add some labels and make this test pass
    //             config: DockerInspectConfig {
    //                 labels: DockerConfigLabels { metadata: None },
    //                 image_user: None,
    //             },
    //             mounts: None,
    //         };

    //         let resources = DockerBuildResources {
    //             image: docker_image,
    //             additional_mounts: vec![],
    //             privileged: false,
    //             entrypoints: vec![],
    //         };

    //         let runtime_override = devcontainer_manifest
    //             .build_runtime_override("app", resources)
    //             .unwrap();

    //         // ugh how are we going to do labels
    //         let expected_runtime_override = DockerComposeConfig {
    //             name: None,
    //             services: HashMap::from([(
    //                 "app".to_string(),
    //                 DockerComposeService {
    //                     entrypoint: Some(vec![
    //                         "/bin/sh".to_string(),
    //                         "-c".to_string(),
    //                         "
    // echo Container started
    // trap \"exit 0\" 15
    // exec \"$@\"
    // while sleep 1 & wait $!; do :; done"
    //                             .to_string(),
    //                         "-".to_string(),
    //                     ]),
    //                     cap_add: Some(vec!["SYS_PTRACE".to_string()]),
    //                     security_opt: Some(vec!["seccomp=unconfined".to_string()]),
    //                     labels: Some(vec!["label1=label1val".to_string()]),
    //                     ..Default::default()
    //                 },
    //             )]),
    //             ..Default::default()
    //         };

    //         assert_eq!(runtime_override, expected_runtime_override)
    //     }

    //     // TODO turn this into a merged config test more broadly
    //     #[test]
    //     fn test_privileged() {
    //         let dev_container = DevContainer::default();

    //         let feature_manifests = vec![FeatureManifest::new(
    //             PathBuf::from("/"),
    //             DevContainerFeatureJson {
    //                 _id: None,
    //                 options: HashMap::new(),
    //                 mounts: None,
    //                 privileged: Some(true),
    //                 entrypoint: None,
    //             },
    //         )];

    //         let privileged = dev_container.privileged.unwrap_or(false)
    //             || feature_manifests.iter().any(|f| f.privileged());

    //         assert!(privileged);
    //     }

    //     // Ok, let's get the docker run stuff into DevContainerManifest and then go from here
    //     #[test]
    //     fn test_remote_workspace_folder() {
    //         let devcontainer_manifest = DevContainerManifest {
    //             config: DevContainer::default(),
    //             config_directory: PathBuf::from("/path/to/local/project/.devcontainer"),
    //             local_project_directory: PathBuf::from("/path/to/local/project"),
    //             ..Default::default()
    //         };

    //         assert_eq!(
    //             devcontainer_manifest.remote_workspace_mount(),
    //             Ok(PathBuf::from("/workspaces/project")),
    //         );

    //         let devcontainer_manifest = DevContainerManifest {
    //             config: DevContainer {
    //                 workspace_mount: Some(MountDefinition {
    //                     source: "/path/to/local/project/subfolder".to_string(),
    //                     target: "/specialworkspace".to_string(),
    //                     mount_type: Some("bind".to_string()),
    //                 }),
    //                 workspace_folder: Some("/specialworkspace/subfolder".to_string()),
    //                 ..Default::default()
    //             },
    //             config_directory: PathBuf::from("/path/to/local/project/.devcontainer"),
    //             local_project_directory: PathBuf::from("/path/to/local/project"),
    //             ..Default::default()
    //         };

    //         assert_eq!(
    //             devcontainer_manifest.remote_workspace_mount(),
    //             Ok(PathBuf::from("/specialworkspace"))
    //         )
    //     }

    #[gpui::test]
    async fn test_nonremote_variable_replacement_with_default_mount(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let given_devcontainer_contents = r#"
// These are some external comments. serde_lenient should handle them
{
    // These are some internal comments
    "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
    "name": "myDevContainer-${devcontainerId}",
    "remoteUser": "root",
    "remoteEnv": {
        "DEVCONTAINER_ID": "${devcontainerId}",
        "MYVAR2": "myvarothervalue",
        "REMOTE_WORKSPACE_FOLDER_BASENAME": "${containerWorkspaceFolderBasename}",
        "LOCAL_WORKSPACE_FOLDER_BASENAME": "${localWorkspaceFolderBasename}",
        "REMOTE_WORKSPACE_FOLDER": "${containerWorkspaceFolder}",
        "LOCAL_WORKSPACE_FOLDER": "${localWorkspaceFolder}",
        "LOCAL_ENV_VAR_1": "${localEnv:local_env_1}",
        "LOCAL_ENV_VAR_2": "${localEnv:my_other_env}"

    }
}
                    "#;
        let mut devcontainer_manifest = init_devcontainer_manifest(
            cx,
            fs,
            HashMap::from([
                ("local_env_1".to_string(), "local_env_value1".to_string()),
                ("my_other_env".to_string(), "THISVALUEHERE".to_string()),
            ]),
            given_devcontainer_contents,
        )
        .await
        .unwrap();

        devcontainer_manifest.parse_nonremote_vars().unwrap();

        let ConfigStatus::VariableParsed(variable_replaced_devcontainer) =
            &devcontainer_manifest.config
        else {
            panic!("Config not parsed");
        };

        // ${devcontainerId}
        let devcontainer_id = devcontainer_manifest.devcontainer_id();
        assert_eq!(
            variable_replaced_devcontainer.name,
            Some(format!("myDevContainer-{devcontainer_id}"))
        );
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("DEVCONTAINER_ID")),
            Some(&devcontainer_id)
        );

        // ${containerWorkspaceFolderBasename}
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("REMOTE_WORKSPACE_FOLDER_BASENAME")),
            Some(&test_project_filename())
        );

        // ${localWorkspaceFolderBasename}
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("LOCAL_WORKSPACE_FOLDER_BASENAME")),
            Some(&test_project_filename())
        );

        // ${containerWorkspaceFolder}
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("REMOTE_WORKSPACE_FOLDER")),
            Some(&format!("/workspaces/{}", test_project_filename()))
        );

        // ${localWorkspaceFolder}
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("LOCAL_WORKSPACE_FOLDER")),
            Some(&TEST_PROJECT_PATH.to_string())
        );

        // ${localEnv:VARIABLE_NAME}
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("LOCAL_ENV_VAR_1")),
            Some(&"local_env_value1".to_string())
        );
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("LOCAL_ENV_VAR_2")),
            Some(&"THISVALUEHERE".to_string())
        );
    }

    #[gpui::test]
    async fn test_nonremote_variable_replacement_with_explicit_mount(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let given_devcontainer_contents = r#"
                // These are some external comments. serde_lenient should handle them
                {
                    // These are some internal comments
                    "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
                    "name": "myDevContainer-${devcontainerId}",
                    "remoteUser": "root",
                    "remoteEnv": {
                        "DEVCONTAINER_ID": "${devcontainerId}",
                        "MYVAR2": "myvarothervalue",
                        "REMOTE_WORKSPACE_FOLDER_BASENAME": "${containerWorkspaceFolderBasename}",
                        "LOCAL_WORKSPACE_FOLDER_BASENAME": "${localWorkspaceFolderBasename}",
                        "REMOTE_WORKSPACE_FOLDER": "${containerWorkspaceFolder}",
                        "LOCAL_WORKSPACE_FOLDER": "${localWorkspaceFolder}"

                    },
                    "workspaceMount": "source=/local/folder,target=/workspace/subfolder,type=bind,consistency=cached",
                    "workspaceFolder": "/workspace/customfolder"
                }
            "#;

        let mut devcontainer_manifest =
            init_devcontainer_manifest(cx, fs, HashMap::default(), given_devcontainer_contents)
                .await
                .unwrap();

        devcontainer_manifest.parse_nonremote_vars().unwrap();

        let ConfigStatus::VariableParsed(variable_replaced_devcontainer) =
            &devcontainer_manifest.config
        else {
            panic!("Config not parsed");
        };

        // ${devcontainerId}
        let devcontainer_id = devcontainer_manifest.devcontainer_id();
        assert_eq!(
            variable_replaced_devcontainer.name,
            Some(format!("myDevContainer-{devcontainer_id}"))
        );
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("DEVCONTAINER_ID")),
            Some(&devcontainer_id)
        );

        // ${containerWorkspaceFolderBasename}
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("REMOTE_WORKSPACE_FOLDER_BASENAME")),
            Some(&"customfolder".to_string())
        );

        // ${localWorkspaceFolderBasename}
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("LOCAL_WORKSPACE_FOLDER_BASENAME")),
            Some(&"project".to_string())
        );

        // ${containerWorkspaceFolder}
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("REMOTE_WORKSPACE_FOLDER")),
            Some(&"/workspace/customfolder".to_string())
        );

        // ${localWorkspaceFolder}
        assert_eq!(
            variable_replaced_devcontainer
                .remote_env
                .as_ref()
                .and_then(|env| env.get("LOCAL_WORKSPACE_FOLDER")),
            Some(&TEST_PROJECT_PATH.to_string())
        );
    }
}
