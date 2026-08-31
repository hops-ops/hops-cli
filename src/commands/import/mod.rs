//! `hops import` adds the Hops GitOps delivery contract to an existing app
//! repository without changing its application source.

mod templates;

use clap::Args;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PLACEHOLDERS: &[&str] = &[
    "__APP_NAME__",
    "__CHART_NAME__",
    "__IMAGE_REPOSITORY__",
    "__PORT__",
    "__SOURCE_REPOSITORY__",
    "__STAGING_REPOSITORY__",
    "__PREVIEW_REPOSITORY__",
    "__PROJECT__",
    "__DOCKERFILES_JSON__",
];

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Existing Git repository to import. Defaults to the current directory.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Kubernetes application name. Defaults to the GitHub repository name.
    #[arg(long)]
    pub name: Option<String>,

    /// Source GitHub repository in OWNER/REPO form. Defaults to origin.
    #[arg(long)]
    pub repository: Option<String>,

    /// Staging environment repository. Defaults to OWNER/OWNER-staging-env.
    #[arg(long)]
    pub staging_repository: Option<String>,

    /// Preview environment repository. Defaults to OWNER/OWNER-preview-envs.
    #[arg(long)]
    pub preview_repository: Option<String>,

    /// Argo CD project used for generated applications.
    #[arg(long, default_value = "default")]
    pub project: String,

    /// Container and Service target port.
    #[arg(long, default_value_t = 3000)]
    pub port: u16,

    /// Dockerfile path relative to the repository. Auto-detects ./Dockerfile.
    #[arg(long)]
    pub dockerfile: Option<PathBuf>,

    /// Replace files owned by the importer when they already exist.
    #[arg(long)]
    pub force: bool,

    /// Do not configure the vNext DEPLOY_KEY repository secret and deploy key.
    #[arg(long)]
    pub skip_deploy_key: bool,
}

#[derive(Debug)]
struct ImportError(String);

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ImportError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubRepository {
    owner: String,
    name: String,
}

impl GithubRepository {
    fn parse(raw: &str) -> Result<Self, ImportError> {
        let raw = raw.trim().trim_end_matches(".git");
        let Some((owner, name)) = raw.split_once('/') else {
            return Err(ImportError(
                "repository must be in GitHub OWNER/REPO form".to_string(),
            ));
        };
        let valid = !owner.is_empty()
            && !name.is_empty()
            && !name.contains('/')
            && [owner, name].iter().all(|part| {
                part.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            });
        if !valid {
            return Err(ImportError(
                "repository must be in GitHub OWNER/REPO form".to_string(),
            ));
        }
        Ok(Self {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildStrategy {
    Dockerfile(String),
    Railpack,
}

#[derive(Debug)]
struct ImportPlan {
    root: PathBuf,
    repository: GithubRepository,
    app_name: String,
    build_strategy: BuildStrategy,
    files: Vec<GeneratedFile>,
}

#[derive(Debug)]
struct GeneratedFile {
    path: &'static str,
    contents: String,
}

pub fn run(args: &ImportArgs) -> Result<(), Box<dyn Error>> {
    let plan = build_plan(args)?;

    if !args.skip_deploy_key {
        require_command("vnext", "install vnext before configuring the deploy key")?;
        require_command(
            "gh",
            "install and authenticate gh before configuring the deploy key",
        )?;
    }

    let collisions = existing_generated_paths(&plan);
    if !collisions.is_empty() && !args.force {
        return Err(Box::new(ImportError(format!(
            "refusing to replace existing generated files:\n  {}\nrerun with --force to replace only these importer-owned paths",
            collisions.join("\n  ")
        ))));
    }

    write_plan(&plan)?;
    log::info!(
        "Imported {} from {} with {} delivery files ({})",
        plan.app_name,
        plan.repository.slug(),
        plan.files.len(),
        match &plan.build_strategy {
            BuildStrategy::Dockerfile(path) => format!("Dockerfile: {path}"),
            BuildStrategy::Railpack => "Railpack fallback".to_string(),
        }
    );

    if !args.skip_deploy_key {
        configure_deploy_key(&plan)?;
    } else {
        log::info!("Skipped vNext deploy-key setup (--skip-deploy-key)");
    }

    log::info!("Next: review the generated files and commit them to the application repository");
    Ok(())
}

fn build_plan(args: &ImportArgs) -> Result<ImportPlan, ImportError> {
    if args.port == 0 {
        return Err(ImportError("--port must be greater than zero".to_string()));
    }
    validate_project(&args.project)?;

    let root = git_root(&args.path)?;
    let repository = match &args.repository {
        Some(raw) => GithubRepository::parse(raw)?,
        None => repository_from_origin(&root)?,
    };
    let staging = match &args.staging_repository {
        Some(raw) => GithubRepository::parse(raw)?,
        None => GithubRepository::parse(&format!("{0}/{0}-staging-env", repository.owner))?,
    };
    let preview = match &args.preview_repository {
        Some(raw) => GithubRepository::parse(raw)?,
        None => GithubRepository::parse(&format!("{0}/{0}-preview-envs", repository.owner))?,
    };
    let app_name = kubernetes_name(args.name.as_deref().unwrap_or(&repository.name))?;
    let build_strategy = resolve_build_strategy(&root, args.dockerfile.as_deref())?;
    let image_repository = format!(
        "ghcr.io/{}/{}",
        repository.owner.to_ascii_lowercase(),
        repository.name.to_ascii_lowercase()
    );

    let replacements = [
        ("__APP_NAME__", app_name.clone()),
        ("__CHART_NAME__", app_name.clone()),
        ("__IMAGE_REPOSITORY__", image_repository),
        ("__PORT__", args.port.to_string()),
        ("__SOURCE_REPOSITORY__", repository.slug()),
        ("__STAGING_REPOSITORY__", staging.slug()),
        ("__PREVIEW_REPOSITORY__", preview.slug()),
        ("__PROJECT__", args.project.clone()),
        (
            "__DOCKERFILES_JSON__",
            dockerfiles_json(&build_strategy).map_err(|error| ImportError(error.to_string()))?,
        ),
    ];

    let files = generated_files(&build_strategy)
        .into_iter()
        .map(|(path, template)| {
            Ok(GeneratedFile {
                path,
                contents: render(template, &replacements)?,
            })
        })
        .collect::<Result<Vec<_>, ImportError>>()?;

    Ok(ImportPlan {
        root,
        repository,
        app_name,
        build_strategy,
        files,
    })
}

fn generated_files(strategy: &BuildStrategy) -> Vec<(&'static str, &'static str)> {
    vec![
        (".gitops/local/Chart.yaml", templates::LOCAL_CHART),
        (".gitops/local/values.yaml", templates::LOCAL_VALUES),
        (
            ".gitops/local/templates/deployment.yaml",
            templates::DEPLOYMENT,
        ),
        (".gitops/local/templates/service.yaml", templates::SERVICE),
        (".gitops/deploy/Chart.yaml", templates::DEPLOY_CHART),
        (".gitops/deploy/values.yaml", templates::DEPLOY_VALUES),
        (
            ".gitops/deploy/templates/deployment.yaml",
            templates::DEPLOYMENT,
        ),
        (".gitops/deploy/templates/service.yaml", templates::SERVICE),
        (".gitops/promote/Chart.yaml", templates::PROMOTE_CHART),
        (".gitops/promote/values.yaml", templates::PROMOTE_VALUES),
        (
            ".gitops/promote/templates/application.yaml",
            templates::PROMOTE_APPLICATION,
        ),
        (
            ".github/workflows/on-push-main-version-and-tag.yaml",
            templates::VERSION_WORKFLOW,
        ),
        (
            ".github/workflows/publish-image.yaml",
            match strategy {
                BuildStrategy::Dockerfile(_) => templates::DOCKER_PUBLISH_WORKFLOW,
                BuildStrategy::Railpack => templates::RAILPACK_PUBLISH_WORKFLOW,
            },
        ),
        (
            ".github/workflows/on-v-tag.yaml",
            templates::RELEASE_WORKFLOW,
        ),
        (
            ".github/workflows/on-pr-preview.yaml",
            templates::PREVIEW_WORKFLOW,
        ),
    ]
}

fn git_root(path: &Path) -> Result<PathBuf, ImportError> {
    if !path.is_dir() {
        return Err(ImportError(format!(
            "repository path is not a directory: {}",
            path.display()
        )));
    }
    let output = command_output(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--show-toplevel"]),
    )
    .map_err(|error| ImportError(format!("failed to inspect Git repository: {error}")))?;
    if !output.status.success() {
        return Err(ImportError(format!(
            "{} is not inside a Git repository",
            path.display()
        )));
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return Err(ImportError(
            "git returned an empty repository root".to_string(),
        ));
    }
    Ok(PathBuf::from(root))
}

fn repository_from_origin(root: &Path) -> Result<GithubRepository, ImportError> {
    let output = command_output(Command::new("git").arg("-C").arg(root).args([
        "config",
        "--get",
        "remote.origin.url",
    ]))
    .map_err(|error| ImportError(format!("failed to inspect origin: {error}")))?;
    if !output.status.success() {
        return Err(ImportError(
            "origin is missing; pass --repository OWNER/REPO".to_string(),
        ));
    }
    let remote = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_github_remote(&remote).ok_or_else(|| {
        ImportError(format!(
            "origin is not a supported GitHub URL: {remote}; pass --repository OWNER/REPO"
        ))
    })
}

fn parse_github_remote(remote: &str) -> Option<GithubRepository> {
    let path = if let Some(path) = remote.strip_prefix("git@github.com:") {
        path
    } else if let Some(path) = remote.strip_prefix("ssh://git@github.com/") {
        path
    } else if let Some(path) = remote.strip_prefix("https://github.com/") {
        path
    } else if let Some(path) = remote.strip_prefix("http://github.com/") {
        path
    } else {
        return None;
    };
    GithubRepository::parse(path).ok()
}

fn resolve_build_strategy(
    root: &Path,
    requested: Option<&Path>,
) -> Result<BuildStrategy, ImportError> {
    let candidate = requested
        .map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            }
        })
        .or_else(|| {
            let default = root.join("Dockerfile");
            default.is_file().then_some(default)
        });

    let Some(candidate) = candidate else {
        return Ok(BuildStrategy::Railpack);
    };
    if !candidate.is_file() {
        return Err(ImportError(format!(
            "Dockerfile does not exist: {}",
            candidate.display()
        )));
    }
    let canonical_root = root.canonicalize().map_err(|error| {
        ImportError(format!(
            "failed to resolve repository root {}: {error}",
            root.display()
        ))
    })?;
    let canonical_candidate = candidate.canonicalize().map_err(|error| {
        ImportError(format!(
            "failed to resolve Dockerfile {}: {error}",
            candidate.display()
        ))
    })?;
    let relative = canonical_candidate
        .strip_prefix(&canonical_root)
        .map_err(|_| ImportError("Dockerfile must be inside the repository".to_string()))?;
    let relative = relative.to_string_lossy().replace('\\', "/");
    Ok(BuildStrategy::Dockerfile(format!("./{relative}")))
}

fn dockerfiles_json(strategy: &BuildStrategy) -> Result<String, serde_json::Error> {
    match strategy {
        BuildStrategy::Dockerfile(path) => serde_json::to_string(&serde_json::json!([{
            "dockerfile": path,
            "context": ".",
            "prefix": "",
            "postfix": ""
        }])),
        BuildStrategy::Railpack => Ok(String::new()),
    }
}

fn kubernetes_name(raw: &str) -> Result<String, ImportError> {
    let mut normalized = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while normalized.contains("--") {
        normalized = normalized.replace("--", "-");
    }
    normalized = normalized.trim_matches('-').to_string();
    if normalized.len() > 63 {
        normalized.truncate(63);
        normalized = normalized.trim_end_matches('-').to_string();
    }
    if normalized.is_empty() {
        return Err(ImportError(
            "application name cannot be converted to a Kubernetes name".to_string(),
        ));
    }
    Ok(normalized)
}

fn validate_project(project: &str) -> Result<(), ImportError> {
    if project.is_empty()
        || project.len() > 63
        || project.starts_with('-')
        || project.ends_with('-')
        || !project.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        return Err(ImportError(
            "--project must be a lowercase Kubernetes name".to_string(),
        ));
    }
    Ok(())
}

fn render(template: &str, replacements: &[(&str, String)]) -> Result<String, ImportError> {
    let mut rendered = template.to_string();
    for (placeholder, value) in replacements {
        rendered = rendered.replace(placeholder, value);
    }
    let unresolved = PLACEHOLDERS
        .iter()
        .filter(|placeholder| rendered.contains(**placeholder))
        .copied()
        .collect::<Vec<_>>();
    if !unresolved.is_empty() {
        return Err(ImportError(format!(
            "internal template has unresolved placeholders: {}",
            unresolved.join(", ")
        )));
    }
    Ok(rendered)
}

fn existing_generated_paths(plan: &ImportPlan) -> Vec<String> {
    plan.files
        .iter()
        .filter(|file| plan.root.join(file.path).exists())
        .map(|file| file.path.to_string())
        .collect()
}

fn write_plan(plan: &ImportPlan) -> Result<(), ImportError> {
    for file in &plan.files {
        let destination = plan.root.join(file.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                ImportError(format!("failed to create {}: {error}", parent.display()))
            })?;
        }
        fs::write(&destination, &file.contents).map_err(|error| {
            ImportError(format!(
                "failed to write {}: {error}",
                destination.display()
            ))
        })?;
        log::info!("  write  {}", file.path);
    }
    Ok(())
}

fn require_command(command: &str, guidance: &str) -> Result<(), ImportError> {
    let status = Command::new(command).arg("--version").status();
    if !matches!(status, Ok(status) if status.success()) {
        return Err(ImportError(format!("{command} is required: {guidance}")));
    }
    Ok(())
}

fn configure_deploy_key(plan: &ImportPlan) -> Result<(), ImportError> {
    let status = Command::new("vnext")
        .current_dir(&plan.root)
        .args([
            "generate-deploy-key",
            "--owner",
            &plan.repository.owner,
            "--name",
            &plan.repository.name,
            "--key-name",
            "DEPLOY_KEY",
        ])
        .status()
        .map_err(|error| ImportError(format!("failed to run vnext: {error}")))?;
    if !status.success() {
        return Err(ImportError(format!(
            "GitOps files were written, but deploy-key setup failed; retry with: vnext generate-deploy-key --owner {} --name {} --key-name DEPLOY_KEY",
            plan.repository.owner, plan.repository.name
        )));
    }
    log::info!(
        "Configured the vNext DEPLOY_KEY for {}",
        plan.repository.slug()
    );
    Ok(())
}

fn command_output(command: &mut Command) -> std::io::Result<Output> {
    command.output()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct TestRepo {
        path: PathBuf,
    }

    impl TestRepo {
        fn new(remote: Option<&str>) -> Self {
            let path = std::env::temp_dir().join(format!("hops-import-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            assert!(Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(&path)
                .status()
                .unwrap()
                .success());
            if let Some(remote) = remote {
                assert!(Command::new("git")
                    .arg("-C")
                    .arg(&path)
                    .args(["remote", "add", "origin", remote])
                    .status()
                    .unwrap()
                    .success());
            }
            Self { path }
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn args(path: &Path) -> ImportArgs {
        ImportArgs {
            path: path.to_path_buf(),
            name: None,
            repository: None,
            staging_repository: None,
            preview_repository: None,
            project: "default".to_string(),
            port: 3000,
            dockerfile: None,
            force: false,
            skip_deploy_key: true,
        }
    }

    #[test]
    fn parses_supported_github_remotes() {
        for remote in [
            "git@github.com:gitkb/example.git",
            "ssh://git@github.com/gitkb/example.git",
            "https://github.com/gitkb/example.git",
            "http://github.com/gitkb/example",
        ] {
            assert_eq!(
                parse_github_remote(remote),
                Some(GithubRepository {
                    owner: "gitkb".to_string(),
                    name: "example".to_string(),
                })
            );
        }
        assert_eq!(
            parse_github_remote("git@example.com:gitkb/example.git"),
            None
        );
    }

    #[test]
    fn chooses_dockerfile_when_present() {
        let repo = TestRepo::new(Some("git@github.com:gitkb/Example_App.git"));
        fs::write(repo.path.join("Dockerfile"), "FROM scratch\n").unwrap();

        let plan = build_plan(&args(&repo.path)).unwrap();

        assert_eq!(plan.app_name, "example-app");
        assert_eq!(
            plan.build_strategy,
            BuildStrategy::Dockerfile("./Dockerfile".to_string())
        );
        let publish = plan
            .files
            .iter()
            .find(|file| file.path.ends_with("publish-image.yaml"))
            .unwrap();
        assert!(publish.contents.contains("workflows-containers"));
        assert!(
            publish.contents.contains("\\\"./Dockerfile\\\"")
                || publish.contents.contains("\"./Dockerfile\"")
        );
    }

    #[test]
    fn falls_back_to_railpack_without_dockerfile() {
        let repo = TestRepo::new(Some("https://github.com/gitkb/service.git"));

        let plan = build_plan(&args(&repo.path)).unwrap();

        assert_eq!(plan.build_strategy, BuildStrategy::Railpack);
        let publish = plan
            .files
            .iter()
            .find(|file| file.path.ends_with("publish-image.yaml"))
            .unwrap();
        assert!(publish.contents.contains("RAILPACK_VERSION"));
        assert!(publish.contents.contains("railpack prepare"));
    }

    #[test]
    fn generated_charts_and_workflows_have_expected_contract() {
        let repo = TestRepo::new(Some("git@github.com:gitkb/service.git"));
        let plan = build_plan(&args(&repo.path)).unwrap();

        assert_eq!(plan.files.len(), 15);
        for file in &plan.files {
            for placeholder in PLACEHOLDERS {
                assert!(
                    !file.contents.contains(placeholder),
                    "{} still contains {}",
                    file.path,
                    placeholder
                );
            }
            if file.path.ends_with("Chart.yaml") || file.path.ends_with("values.yaml") {
                serde_yaml::from_str::<serde_yaml::Value>(&file.contents)
                    .unwrap_or_else(|error| panic!("{} is invalid YAML: {error}", file.path));
            }
            if file.path.starts_with(".github/workflows/") {
                serde_yaml::from_str::<serde_yaml::Value>(&file.contents)
                    .unwrap_or_else(|error| panic!("{} is invalid YAML: {error}", file.path));
            }
        }

        let preview = plan
            .files
            .iter()
            .find(|file| file.path.ends_with("on-pr-preview.yaml"))
            .unwrap();
        assert!(preview.contents.contains("preview: true"));
        assert!(preview.contents.contains("gitkb/gitkb-preview-envs"));
        assert!(preview.contents.contains("auth_mode: app"));

        let release = plan
            .files
            .iter()
            .find(|file| file.path.ends_with("on-v-tag.yaml"))
            .unwrap();
        assert!(release.contents.contains("gitkb/gitkb-staging-env"));
        assert!(release.contents.contains("needs: publish-image"));
    }

    #[test]
    fn collision_check_prevents_unforced_replacement() {
        let repo = TestRepo::new(Some("git@github.com:gitkb/service.git"));
        let plan = build_plan(&args(&repo.path)).unwrap();
        write_plan(&plan).unwrap();
        fs::write(
            repo.path.join(".gitops/local/values.yaml"),
            "owned by user\n",
        )
        .unwrap();

        let collisions = existing_generated_paths(&plan);

        assert_eq!(collisions.len(), plan.files.len());
        assert!(collisions.contains(&".gitops/local/values.yaml".to_string()));
        assert_eq!(
            fs::read_to_string(repo.path.join(".gitops/local/values.yaml")).unwrap(),
            "owned by user\n"
        );
    }

    #[test]
    fn explicit_dockerfile_must_be_inside_repository() {
        let repo = TestRepo::new(Some("git@github.com:gitkb/service.git"));
        let external = std::env::temp_dir().join(format!("Dockerfile-{}", Uuid::new_v4()));
        fs::write(&external, "FROM scratch\n").unwrap();
        let mut import_args = args(&repo.path);
        import_args.dockerfile = Some(external.clone());

        let error = build_plan(&import_args).unwrap_err();

        assert!(error.to_string().contains("must be inside"));
        let _ = fs::remove_file(external);
    }
}
