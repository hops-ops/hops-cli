use clap::{Args, Subcommand, ValueEnum};
use distributed_tooling::{
    generate_service_scaffold, BusTarget, FileMode, GeneratedFile, GithubRepo, GitopsPromoteTarget,
    PostCreateAction, ServiceScaffoldSpec, ServiceTransport, StoreTarget,
};
use serde::Deserialize;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const DISTRIBUTED_MANIFEST_SCHEMA_VERSION: u64 = 1;

#[derive(Args, Debug)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommands,
}

#[derive(Subcommand, Debug)]
pub enum ServiceCommands {
    /// Scaffold a new Distributed microservice crate
    #[command(alias = "create")]
    Scaffold(ScaffoldArgs),
    /// Print a service's Distributed project manifest as JSON
    Describe(DescribeArgs),
    /// Render SQL schema artifacts from a service manifest
    Schema(SchemaArgs),
}

#[derive(Args, Debug)]
pub struct ScaffoldArgs {
    /// Service/package name to scaffold
    pub name: String,
    /// Output directory. Defaults to ./<name>.
    #[arg(long)]
    pub path: Option<PathBuf>,
    /// Service framework to scaffold
    #[arg(long, value_enum, default_value = "distributed")]
    pub framework: Framework,
    /// Compatibility alias for scaffold kind, e.g. distributed-microsvc.
    #[arg(long)]
    pub kind: Option<String>,
    /// Runtime transport to scaffold
    #[arg(long, value_enum, default_value = "http")]
    pub transport: Transport,
    /// Compatibility shortcut for --transport http.
    #[arg(long)]
    pub http: bool,
    /// Compatibility shortcut for --transport knative.
    #[arg(long)]
    pub knative: bool,
    /// Model aggregate to scaffold. May be repeated.
    #[arg(long)]
    pub model: Vec<String>,
    /// Generate placeholder read-model modules and register them in distributed_manifest().
    #[arg(long)]
    pub read_models: bool,
    /// Command handler to scaffold. May be repeated.
    #[arg(long)]
    pub command: Vec<String>,
    /// Event handler to scaffold. May be repeated.
    #[arg(long)]
    pub event: Vec<String>,
    /// Message bus backend to scaffold.
    #[arg(long, value_enum)]
    pub bus: Option<Bus>,
    /// Generate a Helm deploy chart under .gitops/deploy.
    #[arg(long)]
    pub gitops: bool,
    /// Generate a GitOps promotion chart for Argo CD or Flux.
    #[arg(long, value_enum)]
    pub gitops_promote: Option<GitopsPromote>,
    /// GitHub repository to create and configure with release workflows.
    #[arg(long, value_name = "OWNER/REPO")]
    pub github: Option<String>,
    /// GitOps preview environment repository to promote pull-request previews into.
    #[arg(long, value_name = "OWNER/REPO")]
    pub github_preview: Option<String>,
    /// GitOps permanent environment repository to promote version tags into.
    #[arg(long, value_name = "OWNER/REPO")]
    pub github_promote: Option<String>,
    /// Read-model/schema storage target
    #[arg(
        long,
        alias = "storage",
        visible_alias = "storage",
        visible_alias = "read-model",
        value_enum,
        default_value = "postgres"
    )]
    pub store: Store,
    /// Path to the local Distributed crate.
    #[arg(long)]
    pub distributed_path: Option<PathBuf>,
    /// Overwrite generated files in an existing directory.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct DescribeArgs {
    /// Service project directory. Defaults to the current directory.
    #[arg(long, default_value = ".")]
    pub path: PathBuf,
    /// Cargo.toml for the target service. Overrides --path.
    #[arg(long)]
    pub manifest_path: Option<PathBuf>,
    /// Cargo package to inspect when the manifest belongs to a workspace.
    #[arg(long)]
    pub package: Option<String>,
    /// Comma-delimited feature list for the target service.
    #[arg(long, value_delimiter = ',')]
    pub features: Vec<String>,
    /// Disable default features on the target service dependency.
    #[arg(long)]
    pub no_default_features: bool,
    /// Manifest function to call. Defaults to <crate>::distributed_manifest.
    #[arg(long)]
    pub entrypoint: Option<String>,
    /// Output format.
    #[arg(long, value_enum, default_value = "json")]
    pub format: ManifestFormat,
    /// Path to the local Distributed crate.
    #[arg(long)]
    pub distributed_path: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct SchemaArgs {
    /// Service project directory. Defaults to the current directory.
    #[arg(long, default_value = ".")]
    pub path: PathBuf,
    /// Cargo.toml for the target service. Overrides --path.
    #[arg(long)]
    pub manifest_path: Option<PathBuf>,
    /// Cargo package to inspect when the manifest belongs to a workspace.
    #[arg(long)]
    pub package: Option<String>,
    /// Comma-delimited feature list for the target service.
    #[arg(long, value_delimiter = ',')]
    pub features: Vec<String>,
    /// Disable default features on the target service dependency.
    #[arg(long)]
    pub no_default_features: bool,
    /// Manifest function to call. Defaults to <crate>::distributed_manifest.
    #[arg(long)]
    pub entrypoint: Option<String>,
    /// SQL dialect to render.
    #[arg(long, value_enum, default_value = "postgres")]
    pub dialect: SchemaDialect,
    /// Output file. Defaults to stdout.
    #[arg(long, alias = "output", visible_alias = "output")]
    pub out: Option<PathBuf>,
    /// Path to the local Distributed crate.
    #[arg(long)]
    pub distributed_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Framework {
    Distributed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    Http,
    Knative,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum GitopsPromote {
    Argo,
    Flux,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Bus {
    Rabbitmq,
    Kafka,
    Psql,
    Nats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Store {
    Postgres,
    Sqlite,
    InMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ManifestFormat {
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SchemaDialect {
    Postgres,
    Sqlite,
}

// Map the CLI's clap enums onto the scaffold-tooling spec enums. These exist so
// `--help` / value parsing stay owned by the CLI while generation stays owned by
// `distributed_tooling`.
impl From<Transport> for ServiceTransport {
    fn from(transport: Transport) -> Self {
        match transport {
            Transport::Http => ServiceTransport::Http,
            Transport::Knative => ServiceTransport::Knative,
        }
    }
}

impl From<Store> for StoreTarget {
    fn from(store: Store) -> Self {
        match store {
            Store::Postgres => StoreTarget::Postgres,
            Store::Sqlite => StoreTarget::Sqlite,
            Store::InMemory => StoreTarget::InMemory,
        }
    }
}

impl From<Bus> for BusTarget {
    fn from(bus: Bus) -> Self {
        match bus {
            Bus::Rabbitmq => BusTarget::Rabbitmq,
            Bus::Kafka => BusTarget::Kafka,
            Bus::Psql => BusTarget::Psql,
            Bus::Nats => BusTarget::Nats,
        }
    }
}

impl From<GitopsPromote> for GitopsPromoteTarget {
    fn from(promote: GitopsPromote) -> Self {
        match promote {
            GitopsPromote::Argo => GitopsPromoteTarget::Argo,
            GitopsPromote::Flux => GitopsPromoteTarget::Flux,
        }
    }
}

pub fn run(args: &ServiceArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        ServiceCommands::Scaffold(scaffold) => run_scaffold(scaffold),
        ServiceCommands::Describe(describe) => run_describe(describe),
        ServiceCommands::Schema(schema) => run_schema(schema),
    }
}

fn run_scaffold(args: &ScaffoldArgs) -> Result<(), Box<dyn Error>> {
    validate_scaffold_kind(args.framework, args.kind.as_deref())?;
    let transport = if args.http && args.knative {
        return Err("--http and --knative cannot be used together".into());
    } else if args.http {
        Transport::Http
    } else if args.knative {
        Transport::Knative
    } else {
        args.transport
    };

    let github = parse_optional_github_repo(args.github.as_deref(), "--github")?;
    let github_preview =
        parse_optional_github_repo(args.github_preview.as_deref(), "--github-preview")?;
    let github_promote =
        parse_optional_github_repo(args.github_promote.as_deref(), "--github-promote")?;

    // The default output directory uses the normalized package name, so derive it
    // (and fail fast on an invalid name) before creating any directory.
    let package_name = distributed_tooling::package_name(&args.name)?;
    let output_dir = args
        .path
        .clone()
        .unwrap_or_else(|| PathBuf::from(&package_name));
    let output_dir = absolute_path(&output_dir)?;
    ensure_output_dir(&output_dir, args.force)?;

    let distributed_path = resolve_distributed_path(args.distributed_path.as_deref(), &output_dir)?;
    let distributed_dependency_path = path_for_toml(&relative_path(&output_dir, &distributed_path));

    let spec = ServiceScaffoldSpec {
        name: args.name.clone(),
        transport: transport.into(),
        store: args.store.into(),
        bus: args.bus.map(Into::into),
        models: args.model.clone(),
        read_models: args.read_models,
        commands: args.command.clone(),
        events: args.event.clone(),
        distributed_dependency_path,
        gitops: args.gitops,
        gitops_promote: args.gitops_promote.map(Into::into),
        github,
        github_preview,
        github_promote,
    };

    let project = generate_service_scaffold(spec)?;
    for file in &project.files {
        write_generated_file(&output_dir, file)?;
    }
    for warning in &project.warnings {
        eprintln!("warning: {warning}");
    }
    for action in &project.post_create_actions {
        match action {
            PostCreateAction::EnsureGithubRepository { repo } => ensure_github_repo(repo)?,
        }
    }

    println!("Scaffolded Distributed service at {}", output_dir.display());
    Ok(())
}

fn run_describe(args: &DescribeArgs) -> Result<(), Box<dyn Error>> {
    match args.format {
        ManifestFormat::Json => {
            let json = run_manifest_harness(
                &HarnessOptions {
                    path: args.path.clone(),
                    manifest_path: args.manifest_path.clone(),
                    package: args.package.clone(),
                    features: args.features.clone(),
                    no_default_features: args.no_default_features,
                    entrypoint: args.entrypoint.clone(),
                    distributed_path: args.distributed_path.clone(),
                },
                HarnessMode::DescribeJson,
            )?;
            let envelope: serde_json::Value = serde_json::from_str(&json)?;
            validate_manifest_json(&envelope)?;
            println!("{}", serde_json::to_string_pretty(&envelope)?);
            Ok(())
        }
    }
}

fn run_schema(args: &SchemaArgs) -> Result<(), Box<dyn Error>> {
    let sql = run_manifest_harness(
        &HarnessOptions {
            path: args.path.clone(),
            manifest_path: args.manifest_path.clone(),
            package: args.package.clone(),
            features: args.features.clone(),
            no_default_features: args.no_default_features,
            entrypoint: args.entrypoint.clone(),
            distributed_path: args.distributed_path.clone(),
        },
        HarnessMode::SchemaSql(args.dialect),
    )?;

    if let Some(out) = &args.out {
        if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(out, sql)?;
    } else {
        print!("{sql}");
    }
    Ok(())
}

fn validate_scaffold_kind(framework: Framework, kind: Option<&str>) -> Result<(), Box<dyn Error>> {
    if framework != Framework::Distributed {
        return Err("only --framework distributed is supported".into());
    }

    if let Some(kind) = kind {
        match kind {
            "distributed-microsvc" | "distributed" => {}
            _ => {
                return Err(format!(
                    "unsupported service kind `{kind}`; expected distributed-microsvc"
                )
                .into());
            }
        }
    }

    Ok(())
}

fn ensure_output_dir(path: &Path, force: bool) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        if !path.is_dir() {
            return Err(format!("{} exists and is not a directory", path.display()).into());
        }
        if !force && fs::read_dir(path)?.next().is_some() {
            return Err(format!(
                "{} already exists and is not empty; pass --force to overwrite generated files",
                path.display()
            )
            .into());
        }
    }
    fs::create_dir_all(path)?;
    Ok(())
}

/// Write one generated file under `output_dir`, creating parent directories and
/// honoring the optional executable mode hint.
fn write_generated_file(output_dir: &Path, file: &GeneratedFile) -> Result<(), Box<dyn Error>> {
    let path = output_dir.join(&file.path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, &file.contents)?;
    if file.mode == Some(FileMode::Executable) {
        set_executable(&path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o111);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), Box<dyn Error>> {
    Ok(())
}

fn parse_optional_github_repo(
    raw: Option<&str>,
    flag: &str,
) -> Result<Option<GithubRepo>, Box<dyn Error>> {
    raw.map(|value| {
        GithubRepo::parse(value)
            .map_err(|err| -> Box<dyn Error> { format!("{flag}: {err}").into() })
    })
    .transpose()
}

fn ensure_github_repo(repo: &GithubRepo) -> Result<(), Box<dyn Error>> {
    let slug = repo.slug();
    let view_output = Command::new("gh")
        .args(["repo", "view", &slug, "--json", "nameWithOwner"])
        .output();

    match view_output {
        Ok(output) if output.status.success() => {
            println!("GitHub repository {slug} already exists");
            return Ok(());
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(
                "GitHub CLI (`gh`) is not installed or not in PATH. Install it before using --github."
                    .into(),
            );
        }
        Err(err) => return Err(Box::new(err)),
    }

    let output = Command::new("gh")
        .args(github_repo_create_args(&slug))
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh repo create failed: {stderr}").into());
    }
    println!("Created GitHub repository {slug}");
    Ok(())
}

fn github_repo_create_args(slug: &str) -> Vec<&str> {
    vec!["repo", "create", slug, "--private"]
}

fn validate_manifest_json(envelope: &serde_json::Value) -> Result<(), Box<dyn Error>> {
    let Some(schema_version) = envelope
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    else {
        return Err("manifest JSON is missing numeric schema_version".into());
    };
    if schema_version != DISTRIBUTED_MANIFEST_SCHEMA_VERSION {
        return Err(format!(
            "unsupported Distributed manifest schema version {schema_version}; expected {DISTRIBUTED_MANIFEST_SCHEMA_VERSION}"
        )
        .into());
    }
    if envelope.get("project").is_none() {
        return Err("manifest JSON is missing project".into());
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct HarnessOptions {
    path: PathBuf,
    manifest_path: Option<PathBuf>,
    package: Option<String>,
    features: Vec<String>,
    no_default_features: bool,
    entrypoint: Option<String>,
    distributed_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
enum HarnessMode {
    DescribeJson,
    SchemaSql(SchemaDialect),
}

impl HarnessMode {
    fn cache_key(self) -> &'static str {
        match self {
            HarnessMode::DescribeJson => "describe-json",
            HarnessMode::SchemaSql(SchemaDialect::Postgres) => "schema-postgres",
            HarnessMode::SchemaSql(SchemaDialect::Sqlite) => "schema-sqlite",
        }
    }
}

fn run_manifest_harness(
    options: &HarnessOptions,
    mode: HarnessMode,
) -> Result<String, Box<dyn Error>> {
    let manifest_path =
        resolve_target_manifest_path(&options.path, options.manifest_path.as_deref())?;
    let package = cargo_package(&manifest_path, options.package.as_deref())?;
    let distributed_path =
        resolve_distributed_path(options.distributed_path.as_deref(), &package.directory)?;
    let crate_ident = package.name.replace('-', "_");
    let entrypoint = options
        .entrypoint
        .clone()
        .map(|entrypoint| qualify_entrypoint(&crate_ident, &entrypoint))
        .unwrap_or_else(|| Ok(format!("{crate_ident}::distributed_manifest")))?;
    validate_rust_path(&entrypoint)?;

    let harness_root = package
        .directory
        .join("target/hops-service-manifest-harness");
    let harness_dir = harness_root.join(mode.cache_key());
    fs::create_dir_all(harness_dir.join("src"))?;
    fs::write(
        harness_dir.join("Cargo.toml"),
        harness_cargo_toml(
            &format!("hops-service-manifest-harness-{}", mode.cache_key()),
            &crate_ident,
            &package.name,
            &package.directory,
            &distributed_path,
            &options.features,
            options.no_default_features,
        ),
    )?;
    fs::write(
        harness_dir.join("src/main.rs"),
        harness_main_rs(&entrypoint, mode),
    )?;

    let manifest_path = harness_dir.join("Cargo.toml");
    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "--manifest-path",
            manifest_path.to_string_lossy().as_ref(),
        ])
        .env("CARGO_TARGET_DIR", harness_root.join("target"))
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("manifest harness failed: {stderr}").into());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn harness_cargo_toml(
    harness_package_name: &str,
    crate_ident: &str,
    package_name: &str,
    package_dir: &Path,
    distributed_path: &Path,
    features: &[String],
    no_default_features: bool,
) -> String {
    let features = features
        .iter()
        .map(toml_string)
        .collect::<Vec<_>>()
        .join(", ");
    let default_features = if no_default_features {
        ", default-features = false"
    } else {
        ""
    };

    format!(
        r#"[package]
name = {harness_package_name}
version = "0.1.0"
edition = "2021"

[workspace]

[dependencies]
distributed = {{ path = {distributed_path} }}
serde_json = "1"
{crate_ident} = {{ package = {package_name}, path = {package_dir}{default_features}, features = [{features}] }}
"#,
        harness_package_name = toml_string(harness_package_name),
        distributed_path = toml_string(path_for_toml(distributed_path)),
        package_name = toml_string(package_name),
        package_dir = toml_string(path_for_toml(package_dir)),
    )
}

fn harness_main_rs(entrypoint: &str, mode: HarnessMode) -> String {
    match mode {
        HarnessMode::DescribeJson => format!(
            r#"fn main() {{
    let manifest = {entrypoint}();
    let envelope = distributed::DistributedManifestEnvelope::new(manifest);
    println!("{{}}", serde_json::to_string_pretty(&envelope).expect("manifest should serialize"));
}}
"#
        ),
        HarnessMode::SchemaSql(dialect) => {
            let dialect = match dialect {
                SchemaDialect::Postgres => "Postgres",
                SchemaDialect::Sqlite => "Sqlite",
            };
            format!(
                r#"fn main() {{
    let manifest = {entrypoint}();
    let envelope = distributed::DistributedManifestEnvelope::new(manifest);
    let statements = envelope
        .project
        .sql_statements(distributed::TableSqlDialect::{dialect})
        .expect("manifest SQL should render");
    if !statements.is_empty() {{
        println!("{{}}", statements.join("\n\n"));
    }}
}}
"#
            )
        }
    }
}

fn resolve_target_manifest_path(
    path: &Path,
    manifest_path: Option<&Path>,
) -> Result<PathBuf, Box<dyn Error>> {
    let manifest = if let Some(manifest_path) = manifest_path {
        manifest_path.to_path_buf()
    } else if path.is_dir() {
        path.join("Cargo.toml")
    } else {
        path.to_path_buf()
    };

    if !manifest.exists() {
        return Err(format!("target manifest not found: {}", manifest.display()).into());
    }
    Ok(manifest.canonicalize()?)
}

#[derive(Clone, Debug)]
struct CargoPackage {
    name: String,
    directory: PathBuf,
}

fn cargo_package(
    manifest_path: &Path,
    package_name: Option<&str>,
) -> Result<CargoPackage, Box<dyn Error>> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
            manifest_path.to_string_lossy().as_ref(),
        ])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("cargo metadata failed: {stderr}").into());
    }

    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)?;
    let selected = if let Some(package_name) = package_name {
        metadata
            .packages
            .into_iter()
            .find(|package| package.name == package_name)
            .ok_or_else(|| format!("package `{package_name}` was not found in cargo metadata"))?
    } else if metadata.packages.len() == 1 {
        metadata
            .packages
            .into_iter()
            .next()
            .expect("single package should exist")
    } else {
        let manifest_path = manifest_path.canonicalize()?;
        metadata
            .packages
            .into_iter()
            .find(|package| {
                Path::new(&package.manifest_path).canonicalize().ok() == Some(manifest_path.clone())
            })
            .ok_or("multiple packages found; pass --package to select one")?
    };
    let manifest_path = PathBuf::from(&selected.manifest_path);
    let directory = manifest_path
        .parent()
        .ok_or("cargo package manifest has no parent directory")?
        .to_path_buf();

    Ok(CargoPackage {
        name: selected.name,
        directory,
    })
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoMetadataPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataPackage {
    name: String,
    manifest_path: String,
}

fn resolve_distributed_path(
    provided: Option<&Path>,
    anchor: &Path,
) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = provided {
        return validate_distributed_path(path);
    }
    if let Ok(path) = std::env::var("DISTRIBUTED_PATH") {
        return validate_distributed_path(Path::new(&path));
    }

    let mut roots = Vec::new();
    roots.extend(anchor.ancestors().map(Path::to_path_buf));
    roots.extend(std::env::current_dir()?.ancestors().map(Path::to_path_buf));

    for root in roots {
        for candidate in [root.clone(), root.join("distributed")] {
            if candidate.join("Cargo.toml").exists()
                && cargo_toml_package_name(&candidate.join("Cargo.toml")).as_deref()
                    == Some("distributed")
            {
                return Ok(candidate.canonicalize()?);
            }
        }
    }

    Err("unable to find local Distributed crate; pass --distributed-path".into())
}

fn validate_distributed_path(path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let path = path.canonicalize()?;
    let manifest = path.join("Cargo.toml");
    if !manifest.exists() {
        return Err(format!("{} does not contain Cargo.toml", path.display()).into());
    }
    if cargo_toml_package_name(&manifest).as_deref() != Some("distributed") {
        return Err(format!("{} is not the Distributed crate", path.display()).into());
    }
    Ok(path)
}

fn cargo_toml_package_name(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let mut in_package = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed == "[package]" {
            in_package = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_package = false;
        }
        if in_package {
            if let Some(value) = trimmed.strip_prefix("name") {
                let value = value.trim_start();
                if let Some(value) = value.strip_prefix('=') {
                    return value.trim().trim_matches('"').to_string().into();
                }
            }
        }
    }
    None
}

fn qualify_entrypoint(crate_ident: &str, entrypoint: &str) -> Result<String, Box<dyn Error>> {
    let entrypoint = entrypoint.trim();
    if entrypoint.is_empty() {
        return Err("entrypoint cannot be empty".into());
    }
    if entrypoint.contains("::") {
        Ok(entrypoint.to_string())
    } else {
        Ok(format!("{crate_ident}::{entrypoint}"))
    }
}

fn validate_rust_path(path: &str) -> Result<(), Box<dyn Error>> {
    let valid = path
        .split("::")
        .all(|segment| !segment.is_empty() && is_rust_ident(segment));
    if valid {
        Ok(())
    } else {
        Err(format!("invalid Rust entrypoint path `{path}`").into())
    }
}

fn is_rust_ident(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|char| char == '_' || char.is_ascii_alphanumeric())
}

fn absolute_path(path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn relative_path(from_dir: &Path, to: &Path) -> PathBuf {
    let from = path_components(from_dir);
    let to = path_components(to);
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..from.len() {
        relative.push("..");
    }
    for component in &to[common..] {
        relative.push(component);
    }
    if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative
    }
}

fn path_components(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect()
}

fn path_for_toml(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn toml_string(value: impl AsRef<str>) -> String {
    serde_json::to_string(value.as_ref()).expect("string serialization should succeed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_repo_create_args_are_private() {
        assert_eq!(
            github_repo_create_args("hops-ops/test-domain"),
            vec!["repo", "create", "hops-ops/test-domain", "--private"]
        );
    }

    #[test]
    fn optional_github_repo_reports_the_flag_on_error() {
        let err = parse_optional_github_repo(Some("missing-repo"), "--github")
            .expect_err("invalid repo should error");
        assert!(err.to_string().contains("--github"));
        assert!(parse_optional_github_repo(None, "--github")
            .unwrap()
            .is_none());
        let ok = parse_optional_github_repo(Some("hops-ops/test-domain"), "--github")
            .unwrap()
            .unwrap();
        assert_eq!(ok.slug(), "hops-ops/test-domain");
    }

    #[test]
    fn harness_is_standalone_inside_cached_target_directory() {
        let cargo_toml = harness_cargo_toml(
            "hops-service-manifest-harness-schema-postgres",
            "todo_model",
            "todo-model",
            Path::new("/tmp/todo-model"),
            Path::new("/tmp/distributed"),
            &[],
            false,
        );

        assert!(cargo_toml.contains("\n[workspace]\n"));
        assert!(cargo_toml.contains("name = \"hops-service-manifest-harness-schema-postgres\""));
    }
}
