use clap::{Args, Subcommand, ValueEnum};
use serde::Deserialize;
use std::collections::BTreeSet;
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

impl Bus {
    fn kind(self) -> &'static str {
        match self {
            Bus::Rabbitmq => "rabbitmq",
            Bus::Kafka => "kafka",
            Bus::Psql => "psql",
            Bus::Nats => "nats",
        }
    }
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

pub fn run(args: &ServiceArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        ServiceCommands::Scaffold(scaffold) => run_scaffold(scaffold),
        ServiceCommands::Describe(describe) => run_describe(describe),
        ServiceCommands::Schema(schema) => run_schema(schema),
    }
}

fn run_scaffold(args: &ScaffoldArgs) -> Result<(), Box<dyn Error>> {
    validate_scaffold_kind(args.framework, args.kind.as_deref())?;
    let include_read_models = args.read_models;
    let transport = if args.http && args.knative {
        return Err("--http and --knative cannot be used together".into());
    } else if args.http {
        Transport::Http
    } else if args.knative {
        Transport::Knative
    } else {
        args.transport
    };
    let names = ScaffoldNames::new(&args.name)?;
    let models = model_scaffolds(&args.model)?;
    let read_models = if include_read_models {
        if models.is_empty() {
            vec![ModelScaffold::new(&names.package_name)?]
        } else {
            models.clone()
        }
    } else {
        Vec::new()
    };
    let mut module_idents = BTreeSet::new();
    let commands = message_handlers_with_modules(
        if args.command.is_empty() {
            vec![default_command_name(&names, &models)]
        } else {
            args.command.clone()
        },
        "command",
        &mut module_idents,
    )?;
    let events = message_handlers_with_modules(args.event.clone(), "event", &mut module_idents)?;
    let github = parse_optional_github_repo(args.github.as_deref(), "--github")?;
    let github_preview =
        parse_optional_github_repo(args.github_preview.as_deref(), "--github-preview")?;
    let github_promote =
        parse_optional_github_repo(args.github_promote.as_deref(), "--github-promote")?;
    let output_dir = args
        .path
        .clone()
        .unwrap_or_else(|| PathBuf::from(&names.package_name));
    let output_dir = absolute_path(&output_dir)?;
    ensure_output_dir(&output_dir, args.force)?;

    let distributed_path = resolve_distributed_path(args.distributed_path.as_deref(), &output_dir)?;
    let distributed_dependency_path = relative_path(&output_dir, &distributed_path);
    let scaffold = Scaffold {
        names,
        output_dir,
        distributed_dependency_path,
        transport,
        store: args.store,
        bus: args.bus,
        include_read_models,
        gitops: args.gitops
            || args.gitops_promote.is_some()
            || github.is_some()
            || github_preview.is_some()
            || github_promote.is_some(),
        gitops_promote: args.gitops_promote,
        github,
        github_preview,
        github_promote,
        models,
        read_models,
        commands,
        events,
    };

    scaffold.write()?;
    if let Some(repo) = &scaffold.github {
        ensure_github_repo(repo)?;
    }
    println!(
        "Scaffolded Distributed service at {}",
        scaffold.output_dir.display()
    );
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

struct Scaffold {
    names: ScaffoldNames,
    output_dir: PathBuf,
    distributed_dependency_path: PathBuf,
    transport: Transport,
    store: Store,
    bus: Option<Bus>,
    include_read_models: bool,
    gitops: bool,
    gitops_promote: Option<GitopsPromote>,
    github: Option<GithubRepo>,
    github_preview: Option<GithubRepo>,
    github_promote: Option<GithubRepo>,
    models: Vec<ModelScaffold>,
    read_models: Vec<ModelScaffold>,
    commands: Vec<MessageHandler>,
    events: Vec<MessageHandler>,
}

impl Scaffold {
    fn write(&self) -> Result<(), Box<dyn Error>> {
        self.write_file("Cargo.toml", self.cargo_toml())?;
        self.write_file("src/lib.rs", self.lib_rs())?;
        self.write_file("src/main.rs", self.main_rs())?;
        self.write_file("src/manifest.rs", self.manifest_rs())?;
        self.write_file("src/service.rs", self.service_rs())?;
        if !self.models.is_empty() {
            self.write_file("src/models/mod.rs", self.models_mod_rs())?;
            for model in &self.models {
                self.write_file(
                    &format!("src/models/{}.rs", model.module_ident),
                    self.model_rs(model),
                )?;
            }
        }
        self.write_file("src/handlers/mod.rs", self.handlers_mod_rs())?;
        for command in &self.commands {
            self.write_file(
                &format!("src/handlers/{}.rs", command.module_ident),
                self.command_handler_rs(command),
            )?;
        }
        for event in &self.events {
            self.write_file(
                &format!("src/handlers/{}.rs", event.module_ident),
                self.event_handler_rs(event),
            )?;
        }
        if self.include_read_models {
            self.write_file("src/read_models/mod.rs", self.read_models_mod_rs())?;
        }
        if self.gitops {
            self.write_gitops_deploy()?;
        }
        if let Some(promote) = self.gitops_promote {
            self.write_gitops_promote(promote)?;
        }
        if self.github.is_some() {
            self.write_github_release_workflows()?;
        }
        if self.github_preview.is_some() {
            self.write_github_preview_workflow()?;
            self.write_github_promotion_chart(".gitops/preview/helm")?;
        }
        if self.github_promote.is_some() {
            self.write_github_promote_workflow()?;
            self.write_github_promotion_chart(".gitops/promote/helm")?;
        }
        Ok(())
    }

    fn write_file(&self, relative: &str, contents: String) -> Result<(), Box<dyn Error>> {
        let path = self.output_dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        Ok(())
    }

    fn cargo_toml(&self) -> String {
        let distributed_path = toml_string(&path_for_toml(&self.distributed_dependency_path));
        let features = self
            .distributed_features()
            .into_iter()
            .map(toml_string)
            .collect::<Vec<_>>()
            .join(", ");
        let axum = if self.transport == Transport::Knative {
            "axum = \"0.7\"\n"
        } else {
            ""
        };

        format!(
            r#"[package]
name = {package_name}
version = "0.1.0"
edition = "2021"

[workspace]

[dependencies]
distributed = {{ path = {distributed_path}, features = [{features}] }}
{axum}serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
tokio = {{ version = "1", features = ["macros", "net", "rt-multi-thread"] }}
"#,
            package_name = toml_string(&self.names.package_name),
            axum = axum,
        )
    }

    fn distributed_features(&self) -> Vec<&'static str> {
        let mut features = Vec::new();
        match self.transport {
            Transport::Http => features.push("http"),
            Transport::Knative => features.push("http"),
        }
        match self.store {
            Store::Postgres => features.push("postgres"),
            Store::Sqlite => features.push("sqlite"),
            Store::InMemory => {}
        }
        features
    }

    fn lib_rs(&self) -> String {
        let models = if !self.models.is_empty() {
            "pub mod models;\n"
        } else {
            ""
        };
        let read_models = if self.include_read_models {
            "pub mod read_models;\n"
        } else {
            ""
        };
        format!(
            r#"pub mod handlers;
pub mod manifest;
{models}{read_models}pub mod service;

pub use manifest::distributed_manifest;
"#
        )
    }

    fn main_rs(&self) -> String {
        match self.transport {
            Transport::Http => format!(
                r#"#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {{
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let service = {crate_ident}::service::in_memory();
    distributed::microsvc::serve(service, &addr).await?;
    Ok(())
}}
"#,
                crate_ident = self.names.crate_ident,
            ),
            Transport::Knative => format!(
                r#"#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {{
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let service = {crate_ident}::service::in_memory();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let app = distributed::microsvc::cloud_events_router(service);
    axum::serve(listener, app).await?;
    Ok(())
}}
"#,
                crate_ident = self.names.crate_ident,
            ),
        }
    }

    fn manifest_rs(&self) -> String {
        let read_model_import = if self.include_read_models && !self.read_models.is_empty() {
            format!(
                "use crate::read_models::{{{}}};\n\n",
                self.read_models
                    .iter()
                    .map(|model| model.view_ident.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            String::new()
        };
        let read_model_registration = self
            .read_models
            .iter()
            .map(|model| format!("        .read_model::<{}>()\n", model.view_ident))
            .collect::<String>();
        format!(
            r#"use distributed::{{
    DistributedProjectManifest, ServiceManifest,
}};

{read_model_import}pub fn distributed_manifest() -> DistributedProjectManifest {{
    DistributedProjectManifest::new({project_name})
{read_model_registration}        .service(crate::service::manifest())
}}

pub fn service_manifest() -> ServiceManifest {{
    crate::service::manifest()
}}
"#,
            read_model_import = read_model_import,
            project_name = rust_string(&self.names.package_name),
            read_model_registration = read_model_registration,
        )
    }

    fn service_rs(&self) -> String {
        let registrations = self
            .commands
            .iter()
            .map(|handler| format!("        command handlers::{},\n", handler.module_ident))
            .chain(
                self.events
                    .iter()
                    .map(|handler| format!("        event handlers::{},\n", handler.module_ident)),
            )
            .collect::<String>();
        let manifest_commands = self
            .commands
            .iter()
            .map(|handler| {
                format!(
                    "        .command(handlers::{}::COMMAND)\n",
                    handler.module_ident
                )
            })
            .collect::<String>();
        let manifest_events = self
            .events
            .iter()
            .map(|handler| {
                format!(
                    "        .event(handlers::{}::EVENT)\n",
                    handler.module_ident
                )
            })
            .collect::<String>();
        let transport = match self.transport {
            Transport::Http => "http",
            Transport::Knative => "knative",
        };

        format!(
            r#"use std::sync::Arc;

use distributed::{{microsvc::Service, HashMapRepository, ServiceManifest}};

use crate::handlers;

pub type ServiceRepo = HashMapRepository;

pub fn in_memory() -> Arc<Service<ServiceRepo>> {{
    build(HashMapRepository::new())
}}

pub fn build(repo: ServiceRepo) -> Arc<Service<ServiceRepo>> {{
    Arc::new(distributed::register_handlers!(
        Service::with_repo(repo),
{registrations}    ))
}}

pub fn manifest() -> ServiceManifest {{
    ServiceManifest::new({service_name})
{manifest_commands}{manifest_events}        .transport({transport})
}}
"#,
            registrations = registrations,
            service_name = rust_string(&self.names.package_name),
            manifest_commands = manifest_commands,
            manifest_events = manifest_events,
            transport = rust_string(transport),
        )
    }

    fn models_mod_rs(&self) -> String {
        let modules = self
            .models
            .iter()
            .map(|model| {
                format!(
                    "pub mod {module_ident};\npub use {module_ident}::{type_ident};\n",
                    module_ident = model.module_ident,
                    type_ident = model.type_ident,
                )
            })
            .collect::<Vec<_>>()
            .join("");

        format!(
            r#"{modules}
use serde::{{Deserialize, Serialize}};


#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandInput {{
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}}
"#,
            modules = modules,
        )
    }

    fn model_rs(&self, model: &ModelScaffold) -> String {
        format!(
            r#"use distributed::{{sourced, Entity, Snapshot}};

#[derive(Default, Snapshot)]
pub struct {model_struct} {{
    pub entity: Entity,
    pub name: Option<String>,
    pub status: String,
}}

#[sourced(entity)]
impl {model_struct} {{
    #[event({command_recorded_event})]
    pub fn record_command(&mut self, command: String, id: String, name: Option<String>) {{
        self.entity.set_id(&id);
        if let Some(name) = name {{
            self.name = Some(name);
        }}
        self.status = command;
    }}
}}
"#,
            model_struct = model.type_ident,
            command_recorded_event =
                rust_string(&format!("{}.command_recorded", model.message_prefix)),
        )
    }

    fn handlers_mod_rs(&self) -> String {
        self.commands
            .iter()
            .chain(self.events.iter())
            .map(|handler| format!("pub mod {};\n", handler.module_ident))
            .collect()
    }

    fn command_handler_rs(&self, handler: &MessageHandler) -> String {
        if let Some(model) = self.command_model(handler) {
            format!(
                r#"use distributed::{{
    microsvc::{{Context, HandlerError}}, Aggregate, CommitBatch, StreamIdentity, StreamWrite,
    TransactionalCommit,
}};
use serde_json::{{json, Value}};

use crate::models::{{CommandInput, {model_type}}};
use crate::service::ServiceRepo;

pub const COMMAND: &str = {message_name};
pub const MODEL: &str = {model_name};

pub fn guard(ctx: &Context<ServiceRepo>) -> bool {{
    ctx.has_fields(&["id"])
}}

pub async fn handle(ctx: &Context<'_, ServiceRepo>) -> Result<Value, HandlerError> {{
    let input = ctx.input::<CommandInput>()?;
    let mut aggregate = {model_type}::default();
    aggregate.record_command(COMMAND.to_string(), input.id.clone(), input.name.clone())?;
    let identity = StreamIdentity::new({model_type}::aggregate_type(), aggregate.entity().id())?;
    let stream = StreamWrite::new(identity, aggregate.entity_mut());
    ctx.repo().commit_batch(CommitBatch::new(vec![stream])).await?;
    Ok(json!({{ "command": COMMAND, "id": input.id, "model": MODEL, "name": input.name }}))
}}
"#,
                model_type = model.type_ident,
                message_name = rust_string(&handler.message_name),
                model_name = rust_string(&model.name),
            )
        } else {
            format!(
                r#"use distributed::microsvc::{{Context, HandlerError}};
use serde_json::{{json, Value}};

use crate::service::ServiceRepo;

pub const COMMAND: &str = {message_name};

pub fn guard(_ctx: &Context<ServiceRepo>) -> bool {{
    true
}}

pub async fn handle(ctx: &Context<'_, ServiceRepo>) -> Result<Value, HandlerError> {{
    let input = ctx.input::<Value>()?;
    Ok(json!({{ "command": COMMAND, "input": input }}))
}}
"#,
                message_name = rust_string(&handler.message_name),
            )
        }
    }

    fn event_handler_rs(&self, handler: &MessageHandler) -> String {
        format!(
            r#"use distributed::microsvc::{{Context, HandlerError}};
use serde_json::{{json, Value}};

use crate::service::ServiceRepo;

pub const EVENT: &str = {message_name};

pub fn guard(_ctx: &Context<ServiceRepo>) -> bool {{
    true
}}

pub async fn handle(ctx: &Context<'_, ServiceRepo>) -> Result<Value, HandlerError> {{
    let input = ctx.input::<Value>()?;
    Ok(json!({{ "event": EVENT, "input": input }}))
}}
"#,
            message_name = rust_string(&handler.message_name),
        )
    }

    fn read_models_mod_rs(&self) -> String {
        let views = self
            .read_models
            .iter()
            .map(|model| {
                format!(
                    r#"#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ReadModel)]
#[table({table_name})]
pub struct {view_struct} {{
    #[id("id")]
    pub id: String,
    pub name: String,
    pub status: String,
}}
"#,
                    table_name = rust_string(&model.table_name),
                    view_struct = model.view_ident,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            r#"use distributed::ReadModel;
use serde::{{Deserialize, Serialize}};

{views}
"#,
            views = views,
        )
    }

    fn write_gitops_deploy(&self) -> Result<(), Box<dyn Error>> {
        self.write_file(".gitops/deploy/Chart.yaml", self.gitops_deploy_chart_yaml())?;
        self.write_file(
            ".gitops/deploy/values.yaml",
            self.gitops_deploy_values_yaml(),
        )?;
        match self.transport {
            Transport::Http => {
                self.write_file(
                    ".gitops/deploy/templates/deployment.yaml",
                    self.gitops_http_deployment_yaml(),
                )?;
                self.write_file(
                    ".gitops/deploy/templates/service.yaml",
                    self.gitops_http_service_yaml(),
                )?;
            }
            Transport::Knative => {
                self.write_file(
                    ".gitops/deploy/templates/knative-service.yaml",
                    self.gitops_knative_service_yaml(),
                )?;
                self.write_file(
                    ".gitops/deploy/templates/knative-brokers.yaml",
                    self.gitops_knative_brokers_yaml(),
                )?;
                self.write_file(
                    ".gitops/deploy/templates/knative-triggers.yaml",
                    self.gitops_knative_triggers_yaml(),
                )?;
            }
        }
        Ok(())
    }

    fn write_gitops_promote(&self, promote: GitopsPromote) -> Result<(), Box<dyn Error>> {
        self.write_file(
            ".gitops/promote/Chart.yaml",
            self.gitops_promote_chart_yaml(promote),
        )?;
        self.write_file(
            ".gitops/promote/values.yaml",
            self.gitops_promote_values_yaml(),
        )?;
        match promote {
            GitopsPromote::Argo => self.write_file(
                ".gitops/promote/templates/application.yaml",
                self.gitops_argo_application_yaml(),
            )?,
            GitopsPromote::Flux => self.write_file(
                ".gitops/promote/templates/helmrelease.yaml",
                self.gitops_flux_helmrelease_yaml(),
            )?,
        }
        Ok(())
    }

    fn write_github_release_workflows(&self) -> Result<(), Box<dyn Error>> {
        self.write_file(
            ".github/workflows/version.yaml",
            self.github_version_workflow_yaml(),
        )?;
        self.write_file(
            ".github/workflows/release.yaml",
            self.github_release_workflow_yaml(),
        )?;
        Ok(())
    }

    fn write_github_preview_workflow(&self) -> Result<(), Box<dyn Error>> {
        self.write_file(
            ".github/workflows/preview.yaml",
            self.github_preview_workflow_yaml(),
        )
    }

    fn write_github_promote_workflow(&self) -> Result<(), Box<dyn Error>> {
        self.write_file(
            ".github/workflows/promote.yaml",
            self.github_promote_workflow_yaml(),
        )
    }

    fn write_github_promotion_chart(&self, path: &str) -> Result<(), Box<dyn Error>> {
        self.write_file(
            &format!("{path}/Chart.yaml"),
            self.github_promotion_chart_yaml(),
        )?;
        self.write_file(
            &format!("{path}/values.yaml"),
            self.github_promotion_values_yaml(),
        )?;
        self.write_file(
            &format!("{path}/templates/application.yaml"),
            self.github_promotion_application_yaml(),
        )?;
        Ok(())
    }

    fn gitops_deploy_chart_yaml(&self) -> String {
        format!(
            r#"apiVersion: v2
name: {chart_name}
description: Deploy chart for {service_name}
type: application
version: 0.1.0
appVersion: "0.1.0"
"#,
            chart_name = k8s_name(&format!("{}-deploy", self.names.package_name)),
            service_name = self.names.package_name,
        )
    }

    fn gitops_deploy_values_yaml(&self) -> String {
        let bus = self
            .bus
            .map(|bus| format!("bus:\n  kind: {}\n", bus.kind()))
            .unwrap_or_default();
        format!(
            r#"image:
  repository: {image_repository}
  tag: latest
service:
  port: 3000
{bus}
"#,
            image_repository = self.image_repository(),
            bus = bus,
        )
    }

    fn gitops_http_deployment_yaml(&self) -> String {
        let name = k8s_name(&self.names.package_name);
        let bus_env = self.bus_env_yaml();
        format!(
            r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: {name}
  labels:
    app.kubernetes.io/name: {name}
spec:
  replicas: 1
  selector:
    matchLabels:
      app.kubernetes.io/name: {name}
  template:
    metadata:
      labels:
        app.kubernetes.io/name: {name}
    spec:
      containers:
        - name: {name}
          image: {image_repository}:latest
          ports:
            - containerPort: 3000
          env:
            - name: BIND_ADDR
              value: 0.0.0.0:3000
{bus_env}
"#,
            name = name,
            image_repository = self.image_repository(),
            bus_env = bus_env,
        )
    }

    fn gitops_http_service_yaml(&self) -> String {
        let name = k8s_name(&self.names.package_name);
        format!(
            r#"apiVersion: v1
kind: Service
metadata:
  name: {name}
  labels:
    app.kubernetes.io/name: {name}
spec:
  selector:
    app.kubernetes.io/name: {name}
  ports:
    - name: http
      port: 80
      targetPort: 3000
"#,
            name = name,
        )
    }

    fn gitops_knative_service_yaml(&self) -> String {
        let name = k8s_name(&self.names.package_name);
        let bus_env = self.bus_env_yaml();
        format!(
            r#"apiVersion: serving.knative.dev/v1
kind: Service
metadata:
  name: {name}
  labels:
    app.kubernetes.io/name: {name}
spec:
  template:
    metadata:
      annotations:
        autoscaling.knative.dev/min-scale: "0"
    spec:
      containers:
        - image: {image_repository}:latest
          ports:
            - containerPort: 3000
          env:
            - name: BIND_ADDR
              value: 0.0.0.0:3000
{bus_env}
"#,
            name = name,
            image_repository = self.image_repository(),
            bus_env = bus_env,
        )
    }

    fn gitops_knative_brokers_yaml(&self) -> String {
        self.knative_broker_names()
            .into_iter()
            .map(|broker| {
                format!(
                    r#"apiVersion: eventing.knative.dev/v1
kind: Broker
metadata:
  name: {broker}
"#,
                    broker = broker,
                )
            })
            .collect::<Vec<_>>()
            .join("---\n")
    }

    fn gitops_knative_triggers_yaml(&self) -> String {
        self.knative_triggers()
            .into_iter()
            .map(|trigger| {
                format!(
                    r#"apiVersion: eventing.knative.dev/v1
kind: Trigger
metadata:
  name: {name}
spec:
  broker: {broker}
  filter:
    attributes:
      type: {event_type}
  subscriber:
    ref:
      apiVersion: serving.knative.dev/v1
      kind: Service
      name: {service_name}
    uri: /cloudevent/{event_type}
"#,
                    name = trigger.name,
                    broker = trigger.broker,
                    event_type = trigger.event_type,
                    service_name = k8s_name(&self.names.package_name),
                )
            })
            .collect::<Vec<_>>()
            .join("---\n")
    }

    fn gitops_promote_chart_yaml(&self, promote: GitopsPromote) -> String {
        let suffix = match promote {
            GitopsPromote::Argo => "argo",
            GitopsPromote::Flux => "flux",
        };
        format!(
            r#"apiVersion: v2
name: {chart_name}
description: GitOps promotion chart for {service_name}
type: application
version: 0.1.0
appVersion: "0.1.0"
"#,
            chart_name = k8s_name(&format!("{}-{}-promote", self.names.package_name, suffix)),
            service_name = self.names.package_name,
        )
    }

    fn gitops_promote_values_yaml(&self) -> String {
        r#"repoUrl: https://example.invalid/repo.git
targetRevision: HEAD
destinationNamespace: default
"#
        .to_string()
    }

    fn gitops_argo_application_yaml(&self) -> String {
        let name = k8s_name(&self.names.package_name);
        format!(
            r#"apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: {name}
spec:
  project: default
  source:
    repoURL: https://example.invalid/repo.git
    targetRevision: HEAD
    path: .gitops/deploy
  destination:
    server: https://kubernetes.default.svc
    namespace: default
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
"#,
            name = name,
        )
    }

    fn gitops_flux_helmrelease_yaml(&self) -> String {
        let name = k8s_name(&self.names.package_name);
        format!(
            r#"apiVersion: source.toolkit.fluxcd.io/v1
kind: GitRepository
metadata:
  name: {name}-gitops
spec:
  interval: 1m
  url: https://example.invalid/repo.git
  ref:
    branch: main
---
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: {name}
spec:
  interval: 5m
  chart:
    spec:
      chart: .gitops/deploy
      sourceRef:
        kind: GitRepository
        name: {name}-gitops
      interval: 1m
  targetNamespace: default
"#,
            name = name,
        )
    }

    fn github_version_workflow_yaml(&self) -> String {
        r#"name: Version and Tag

on:
  push:
    branches:
      - main

permissions:
  contents: write

jobs:
  version-and-tag:
    name: Version and Tag
    uses: unbounded-tech/workflow-vnext-tag/.github/workflows/workflow.yaml@v1.21.3
    secrets: inherit
    with:
      useDeployKey: true
      rust: true
      yqPatches: |
        patches:
          - filePath: .gitops/deploy/values.yaml
            selector: .image.tag
            valuePrefix: v
          - filePath: .gitops/deploy/Chart.yaml
            selector: .version
            valuePrefix: ""
          - filePath: .gitops/deploy/Chart.yaml
            selector: .appVersion
            valuePrefix: v
"#
        .to_string()
    }

    fn github_release_workflow_yaml(&self) -> String {
        r#"name: Release

on:
  push:
    tags:
      - "v*.*.*"

permissions:
  contents: write

jobs:
  release:
    name: GitHub Release
    uses: unbounded-tech/workflow-simple-release/.github/workflows/workflow.yaml@v2.1.3
    with:
      tag: ${{ github.ref_name }}
      name: ${{ github.ref_name }}
"#
        .to_string()
    }

    fn github_preview_workflow_yaml(&self) -> String {
        let preview_repo = self
            .github_preview
            .as_ref()
            .expect("preview workflow requires preview repo");
        let environment_name = preview_repo.environment_name();
        format!(
            r#"name: Preview

on:
  pull_request:
    branches:
      - main
    types:
      - labeled
      - opened
      - reopened
      - synchronize

permissions:
  contents: write
  issues: write
  packages: write
  pull-requests: write

jobs:
  preview:
    name: Preview Promotion PR
    if: contains(github.event.pull_request.labels.*.name, 'preview')
    uses: unbounded-tech/workflows-gitops/.github/workflows/argocd-promote-helm.yaml@v1
    secrets:
      GH_PAT: ${{{{ secrets.GH_ORG_ACTIONS_REPO_WRITE_PACKAGES }}}}
    with:
      promotion_chart_path: .gitops/preview/helm
      environment_repository: {environment_repository}
      environment_name: {environment_name}
      project: {environment_name}
      name: ${{{{ github.event.repository.name }}}}
      preview: true
      promotion_pr: true
      values: |
        image:
          repository: {image_repository}
      comment: |
        Preview promoted for `${{{{ github.event.repository.name }}}}`.
        The current tag is: `pr-${{{{ github.event.pull_request.number }}}}-${{{{ github.event.pull_request.head.sha }}}}`
"#,
            environment_repository = preview_repo.full_name,
            environment_name = environment_name,
            image_repository = self.image_repository(),
        )
    }

    fn github_promote_workflow_yaml(&self) -> String {
        let promote_repo = self
            .github_promote
            .as_ref()
            .expect("promote workflow requires promote repo");
        let environment_name = promote_repo.environment_name();
        format!(
            r#"name: Promote

on:
  push:
    tags:
      - "v*.*.*"

permissions:
  contents: write
  issues: write
  packages: write
  pull-requests: write

jobs:
  promote:
    name: Release Promotion
    uses: unbounded-tech/workflows-gitops/.github/workflows/argocd-promote-helm.yaml@v1
    secrets:
      GH_PAT: ${{{{ secrets.GH_ORG_ACTIONS_REPO_WRITE_PACKAGES }}}}
    with:
      promotion_chart_path: .gitops/promote/helm
      destination_path: .gitops/deploy
      environment_repository: {environment_repository}
      environment_name: {environment_name}
      project: {environment_name}
      name: {application_name}
      values: |
        image:
          repository: {image_repository}
          tag: ${{{{ github.ref_name }}}}
"#,
            environment_repository = promote_repo.full_name,
            environment_name = environment_name,
            application_name = self.names.package_name,
            image_repository = self.image_repository(),
        )
    }

    fn github_promotion_chart_yaml(&self) -> String {
        format!(
            r#"apiVersion: v2
name: {chart_name}-promotion
description: Argo CD promotion chart for {service_name}
type: application
version: 0.1.0
appVersion: "0.1.0"
"#,
            chart_name = k8s_name(&self.names.package_name),
            service_name = self.names.package_name,
        )
    }

    fn github_promotion_values_yaml(&self) -> String {
        format!(
            r#"application:
  name: {service_name}
  repository: https://github.com/{github_repository}.git
  targetRevision: main
  path: .gitops/deploy
  values: ""
  destination:
    namespace: default
    server: https://kubernetes.default.svc
  image:
    tag: latest

project: default
preview: false
"#,
            service_name = self.names.package_name,
            github_repository = self
                .github
                .as_ref()
                .map(|repo| repo.full_name.as_str())
                .unwrap_or("OWNER/REPO"),
        )
    }

    fn github_promotion_application_yaml(&self) -> String {
        r#"apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: {{ .Values.project }}-{{ .Values.application.name }}
  namespace: argocd
  finalizers:
    - resources-finalizer.argocd.argoproj.io
spec:
  project: {{ .Values.project }}
  source:
    path: {{ .Values.application.path }}
    repoURL: {{ .Values.application.repository }}
    targetRevision: {{ .Values.application.targetRevision }}
    helm:
      version: v3
      values: |
        {{- if .Values.preview }}
        image:
          tag: {{ .Values.application.image.tag }}
        {{- end }}
        {{- if .Values.application.values }}
        {{ .Values.application.values | nindent 8 }}
        {{- end }}
  destination:
    namespace: {{ .Values.application.destination.namespace }}
    server: {{ .Values.application.destination.server }}
  syncPolicy:
    automated:
      selfHeal: true
      prune: true
"#
        .to_string()
    }

    fn image_repository(&self) -> String {
        self.github
            .as_ref()
            .map(GithubRepo::image_repository)
            .unwrap_or_else(|| format!("ghcr.io/hops-ops/{}", self.names.package_name))
    }

    fn command_model(&self, handler: &MessageHandler) -> Option<&ModelScaffold> {
        if self.models.is_empty() {
            return None;
        }
        let message_model = message_owner(&handler.message_name);
        self.models
            .iter()
            .find(|model| model.name == message_model)
            .or_else(|| self.models.first())
    }

    fn knative_broker_names(&self) -> Vec<String> {
        let mut brokers = BTreeSet::new();
        for model in &self.models {
            brokers.insert(model.command_broker.clone());
            brokers.insert(model.event_broker.clone());
        }
        for command in &self.commands {
            let broker = self
                .command_model(command)
                .map(|model| model.command_broker.clone())
                .unwrap_or_else(|| command_broker_for_message(&command.message_name));
            brokers.insert(broker);
        }
        for event in &self.events {
            brokers.insert(event_broker_for_message(&event.message_name));
        }
        brokers.into_iter().collect()
    }

    fn knative_triggers(&self) -> Vec<KnativeTrigger> {
        self.commands
            .iter()
            .map(|command| {
                let broker = self
                    .command_model(command)
                    .map(|model| model.command_broker.clone())
                    .unwrap_or_else(|| command_broker_for_message(&command.message_name));
                KnativeTrigger::new(&command.message_name, &broker, "command")
            })
            .chain(self.events.iter().map(|event| {
                let broker = event_broker_for_message(&event.message_name);
                KnativeTrigger::new(&event.message_name, &broker, "event")
            }))
            .collect()
    }

    fn bus_env_yaml(&self) -> String {
        self.bus
            .map(|bus| {
                format!(
                    r#"            - name: HOPS_BUS
              value: {}
"#,
                    bus.kind()
                )
            })
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GithubRepo {
    owner: String,
    repo: String,
    full_name: String,
}

impl GithubRepo {
    fn parse(raw: &str, flag: &str) -> Result<Self, Box<dyn Error>> {
        let trimmed = raw.trim();
        let Some((owner, repo)) = trimmed.split_once('/') else {
            return Err(format!("{flag} must be in OWNER/REPO form").into());
        };
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            return Err(format!("{flag} must be in OWNER/REPO form").into());
        }
        let valid = [owner, repo].into_iter().all(|part| {
            part.chars().all(|char| {
                char.is_ascii_alphanumeric() || char == '-' || char == '_' || char == '.'
            })
        });
        if !valid {
            return Err(format!("{flag} contains unsupported GitHub repository characters").into());
        }

        Ok(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            full_name: format!("{owner}/{repo}"),
        })
    }

    fn environment_name(&self) -> String {
        k8s_name(&self.repo)
    }

    fn image_repository(&self) -> String {
        format!("ghcr.io/{}", self.full_name.to_ascii_lowercase())
    }
}

fn parse_optional_github_repo(
    raw: Option<&str>,
    flag: &str,
) -> Result<Option<GithubRepo>, Box<dyn Error>> {
    raw.map(|value| GithubRepo::parse(value, flag)).transpose()
}

fn ensure_github_repo(repo: &GithubRepo) -> Result<(), Box<dyn Error>> {
    let view_output = Command::new("gh")
        .args(["repo", "view", &repo.full_name, "--json", "nameWithOwner"])
        .output();

    match view_output {
        Ok(output) if output.status.success() => {
            println!("GitHub repository {} already exists", repo.full_name);
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
        .args(github_repo_create_args(repo))
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh repo create failed: {stderr}").into());
    }
    println!("Created GitHub repository {}", repo.full_name);
    Ok(())
}

fn github_repo_create_args(repo: &GithubRepo) -> Vec<&str> {
    vec!["repo", "create", repo.full_name.as_str(), "--private"]
}

#[derive(Clone, Debug)]
struct ModelScaffold {
    name: String,
    message_prefix: String,
    module_ident: String,
    type_ident: String,
    view_ident: String,
    table_name: String,
    command_broker: String,
    event_broker: String,
}

impl ModelScaffold {
    fn new(raw_name: &str) -> Result<Self, Box<dyn Error>> {
        let name = to_kebab_case(raw_name);
        if name.is_empty() {
            return Err("model name must contain at least one ASCII letter or digit".into());
        }
        let ident = name.replace('-', "_");
        let type_ident = to_pascal_case(&name);
        let view_ident = format!("{type_ident}View");
        Ok(Self {
            name: name.clone(),
            message_prefix: name.clone(),
            module_ident: ident.clone(),
            type_ident,
            view_ident,
            table_name: format!("{ident}_views"),
            command_broker: format!("{name}-commands"),
            event_broker: format!("{name}-events"),
        })
    }
}

fn model_scaffolds(raw_models: &[String]) -> Result<Vec<ModelScaffold>, Box<dyn Error>> {
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for raw_model in raw_models {
        let model = ModelScaffold::new(&raw_model)?;
        if !seen.insert(model.name.clone()) {
            return Err(format!("duplicate model `{}`", model.name).into());
        }
        models.push(model);
    }
    Ok(models)
}

fn default_command_name(names: &ScaffoldNames, models: &[ModelScaffold]) -> String {
    models
        .first()
        .map(|model| format!("{}.create", model.name))
        .unwrap_or_else(|| names.command_name.clone())
}

#[derive(Clone, Debug)]
struct KnativeTrigger {
    name: String,
    broker: String,
    event_type: String,
}

impl KnativeTrigger {
    fn new(event_type: &str, broker: &str, suffix: &str) -> Self {
        Self {
            name: k8s_name(&format!("{}-{suffix}", event_type.replace('.', "-"))),
            broker: broker.to_string(),
            event_type: event_type.to_string(),
        }
    }
}

fn command_broker_for_message(message_name: &str) -> String {
    format!("{}-commands", message_owner(message_name))
}

fn event_broker_for_message(message_name: &str) -> String {
    let parts = message_name
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let owner = if parts.len() >= 3 {
        parts[0]
    } else {
        parts.first().copied().unwrap_or("events")
    };
    format!("{}-events", k8s_name(owner))
}

fn message_owner(message_name: &str) -> String {
    message_name
        .split('.')
        .find(|part| !part.is_empty())
        .map(k8s_name)
        .unwrap_or_else(|| "message".to_string())
}

fn k8s_name(value: &str) -> String {
    let name = to_kebab_case(value);
    if name.is_empty() {
        "generated".to_string()
    } else {
        name
    }
}

#[derive(Clone, Debug)]
struct MessageHandler {
    message_name: String,
    module_ident: String,
}

#[cfg(test)]
fn message_handlers(
    names: Vec<String>,
    fallback_prefix: &str,
) -> Result<Vec<MessageHandler>, Box<dyn Error>> {
    let mut seen_modules = BTreeSet::new();
    message_handlers_with_modules(names, fallback_prefix, &mut seen_modules)
}

fn message_handlers_with_modules(
    names: Vec<String>,
    fallback_prefix: &str,
    seen_modules: &mut BTreeSet<String>,
) -> Result<Vec<MessageHandler>, Box<dyn Error>> {
    let mut seen_names = BTreeSet::new();
    let mut handlers = Vec::new();

    for raw_name in names {
        let message_name = raw_name.trim();
        validate_message_name(message_name, fallback_prefix)?;
        if !seen_names.insert(message_name.to_string()) {
            return Err(format!("duplicate {fallback_prefix} `{message_name}`").into());
        }

        let base_module = to_rust_ident(message_name, fallback_prefix);
        let mut module_ident = base_module.clone();
        let mut suffix = 2;
        while !seen_modules.insert(module_ident.clone()) {
            module_ident = format!("{base_module}_{suffix}");
            suffix += 1;
        }

        handlers.push(MessageHandler {
            message_name: message_name.to_string(),
            module_ident,
        });
    }

    Ok(handlers)
}

fn validate_message_name(name: &str, kind: &str) -> Result<(), Box<dyn Error>> {
    if name.is_empty() {
        return Err(format!("{kind} name cannot be empty").into());
    }
    if name.chars().any(char::is_control) {
        return Err(format!("{kind} `{name}` contains a control character").into());
    }
    Ok(())
}

fn to_rust_ident(value: &str, fallback_prefix: &str) -> String {
    let mut ident = String::new();
    let mut last_was_separator = false;
    for char in value.chars() {
        if char.is_ascii_alphanumeric() {
            ident.push(char.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            ident.push('_');
            last_was_separator = true;
        }
    }
    while ident.ends_with('_') {
        ident.pop();
    }
    while ident.starts_with('_') {
        ident.remove(0);
    }
    if ident.is_empty() {
        ident = fallback_prefix.to_string();
    }
    if ident
        .chars()
        .next()
        .is_some_and(|char| char.is_ascii_digit())
        || is_rust_keyword(&ident)
    {
        ident = format!("{fallback_prefix}_{ident}");
    }
    ident
}

fn is_rust_keyword(value: &str) -> bool {
    matches!(
        value,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
    )
}

#[derive(Clone, Debug)]
struct ScaffoldNames {
    package_name: String,
    crate_ident: String,
    command_name: String,
}

impl ScaffoldNames {
    fn new(input: &str) -> Result<Self, Box<dyn Error>> {
        let package_name = to_kebab_case(input);
        if package_name.is_empty() {
            return Err("service name must contain at least one ASCII letter or digit".into());
        }
        let crate_ident = package_name.replace('-', "_");
        let command_name = format!("{crate_ident}.create");

        Ok(Self {
            package_name,
            crate_ident,
            command_name,
        })
    }
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
        .map(|feature| toml_string(feature))
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
        distributed_path = toml_string(&path_for_toml(distributed_path)),
        package_name = toml_string(package_name),
        package_dir = toml_string(&path_for_toml(package_dir)),
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

fn to_kebab_case(input: &str) -> String {
    let mut out = String::new();
    let mut last_was_separator = true;
    for char in input.chars() {
        if char.is_ascii_alphanumeric() {
            out.push(char.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            out.push('-');
            last_was_separator = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn to_pascal_case(input: &str) -> String {
    input
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            let mut out = String::new();
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
            out
        })
        .collect()
}

fn toml_string(value: impl AsRef<str>) -> String {
    serde_json::to_string(value.as_ref()).expect("string serialization should succeed")
}

fn rust_string(value: &str) -> String {
    toml_string(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_scaffold_names() {
        let names = ScaffoldNames::new("Checkout Saga").unwrap();

        assert_eq!(names.package_name, "checkout-saga");
        assert_eq!(names.crate_ident, "checkout_saga");
        assert_eq!(names.command_name, "checkout_saga.create");

        let models = model_scaffolds(&["checkout-saga".to_string()]).unwrap();
        assert_eq!(models[0].name, "checkout-saga");
        assert_eq!(models[0].module_ident, "checkout_saga");
        assert_eq!(models[0].type_ident, "CheckoutSaga");
        assert_eq!(models[0].view_ident, "CheckoutSagaView");
        assert_eq!(models[0].table_name, "checkout_saga_views");
        assert_eq!(models[0].command_broker, "checkout-saga-commands");
        assert_eq!(models[0].event_broker, "checkout-saga-events");
    }

    #[test]
    fn scaffold_is_standalone_and_omits_read_models_by_default() {
        let names = ScaffoldNames::new("Todo Model").unwrap();
        let scaffold = Scaffold {
            names,
            output_dir: PathBuf::from("/tmp/todo-model"),
            distributed_dependency_path: PathBuf::from("../distributed"),
            transport: Transport::Http,
            store: Store::Postgres,
            bus: None,
            include_read_models: false,
            gitops: false,
            gitops_promote: None,
            github: None,
            github_preview: None,
            github_promote: None,
            read_models: Vec::new(),
            models: Vec::new(),
            commands: message_handlers(vec!["todo.create".into()], "command").unwrap(),
            events: Vec::new(),
        };

        let cargo_toml = scaffold.cargo_toml();
        assert!(cargo_toml.contains("\n[workspace]\n"));

        let manifest = scaffold.manifest_rs();
        assert!(!manifest.contains(".read_model::<"));
        assert!(!manifest.contains("outbox_message_schema"));
        assert!(!manifest.contains(".table_schema("));
        assert!(!scaffold.lib_rs().contains("pub mod models;"));
        assert!(!scaffold.lib_rs().contains("pub mod read_models;"));
    }

    #[test]
    fn scaffold_registers_read_models_when_requested() {
        let names = ScaffoldNames::new("Todo Model").unwrap();
        let read_models = vec![ModelScaffold::new(&names.package_name).unwrap()];
        let scaffold = Scaffold {
            names,
            output_dir: PathBuf::from("/tmp/todo-model"),
            distributed_dependency_path: PathBuf::from("../distributed"),
            transport: Transport::Http,
            store: Store::Postgres,
            bus: None,
            include_read_models: true,
            gitops: false,
            gitops_promote: None,
            github: None,
            github_preview: None,
            github_promote: None,
            read_models,
            models: Vec::new(),
            commands: message_handlers(vec!["todo.create".into()], "command").unwrap(),
            events: Vec::new(),
        };

        let manifest = scaffold.manifest_rs();
        assert!(manifest.contains(".read_model::<TodoModelView>()"));
        assert!(!manifest.contains("outbox_message_schema"));
        assert!(!manifest.contains(".table_schema("));
        assert!(scaffold.lib_rs().contains("pub mod read_models;"));
        assert!(scaffold
            .read_models_mod_rs()
            .contains("pub struct TodoModelView"));
    }

    #[test]
    fn scaffold_registers_requested_commands_and_events() {
        let names = ScaffoldNames::new("Todo Model").unwrap();
        let scaffold = Scaffold {
            names,
            output_dir: PathBuf::from("/tmp/todo-model"),
            distributed_dependency_path: PathBuf::from("../distributed"),
            transport: Transport::Http,
            store: Store::Postgres,
            bus: None,
            include_read_models: false,
            gitops: false,
            gitops_promote: None,
            github: None,
            github_preview: None,
            github_promote: None,
            read_models: Vec::new(),
            models: Vec::new(),
            commands: message_handlers(
                vec!["todo.create".into(), "todo.complete".into()],
                "command",
            )
            .unwrap(),
            events: message_handlers(vec!["github-projects.issue.created".into()], "event")
                .unwrap(),
        };

        let handlers_mod = scaffold.handlers_mod_rs();
        assert!(handlers_mod.contains("pub mod todo_create;"));
        assert!(handlers_mod.contains("pub mod todo_complete;"));
        assert!(handlers_mod.contains("pub mod github_projects_issue_created;"));

        let service = scaffold.service_rs();
        assert!(service.contains(".command(handlers::todo_create::COMMAND)"));
        assert!(service.contains(".command(handlers::todo_complete::COMMAND)"));
        assert!(service.contains(".event(handlers::github_projects_issue_created::EVENT)"));
    }

    #[test]
    fn scaffold_generates_knative_gitops_from_models_commands_and_events() {
        let names = ScaffoldNames::new("Checkout Saga").unwrap();
        let explicit_models = vec!["todo".to_string(), "somethingelse".to_string()];
        let models = model_scaffolds(&explicit_models).unwrap();
        let scaffold = Scaffold {
            names,
            output_dir: PathBuf::from("/tmp/checkout-saga"),
            distributed_dependency_path: PathBuf::from("../distributed"),
            transport: Transport::Knative,
            store: Store::Postgres,
            bus: Some(Bus::Nats),
            include_read_models: true,
            gitops: true,
            gitops_promote: Some(GitopsPromote::Argo),
            github: None,
            github_preview: None,
            github_promote: None,
            read_models: models.clone(),
            models,
            commands: message_handlers(
                vec!["todo.create".into(), "somethingelse.complete".into()],
                "command",
            )
            .unwrap(),
            events: message_handlers(
                vec![
                    "checkout-saga.started".into(),
                    "github-projects.issue.created".into(),
                ],
                "event",
            )
            .unwrap(),
        };

        assert!(scaffold.cargo_toml().contains("axum = \"0.7\""));
        assert!(scaffold.service_rs().contains(".transport(\"knative\")"));
        assert!(scaffold.gitops_deploy_values_yaml().contains("kind: nats"));
        assert!(scaffold.gitops_knative_service_yaml().contains("HOPS_BUS"));
        assert!(scaffold
            .gitops_knative_service_yaml()
            .contains("value: nats"));
        assert!(scaffold.models_mod_rs().contains("pub mod todo;"));
        assert!(scaffold.models_mod_rs().contains("pub use todo::Todo;"));
        assert!(scaffold
            .model_rs(&scaffold.models[0])
            .contains("pub struct Todo"));
        assert!(scaffold.models_mod_rs().contains("pub mod somethingelse;"));
        assert!(scaffold
            .models_mod_rs()
            .contains("pub use somethingelse::Somethingelse;"));
        assert!(scaffold
            .model_rs(&scaffold.models[1])
            .contains("pub struct Somethingelse"));
        assert!(scaffold
            .read_models_mod_rs()
            .contains("pub struct TodoView"));
        assert!(scaffold
            .read_models_mod_rs()
            .contains("pub struct SomethingelseView"));

        let brokers = scaffold.gitops_knative_brokers_yaml();
        assert!(brokers.contains("name: todo-commands"));
        assert!(brokers.contains("name: todo-events"));
        assert!(brokers.contains("name: somethingelse-commands"));
        assert!(brokers.contains("name: somethingelse-events"));
        assert!(brokers.contains("name: checkout-saga-events"));
        assert!(brokers.contains("name: github-projects-events"));

        let triggers = scaffold.gitops_knative_triggers_yaml();
        assert!(triggers.contains("broker: todo-commands"));
        assert!(triggers.contains("type: todo.create"));
        assert!(triggers.contains("broker: somethingelse-commands"));
        assert!(triggers.contains("type: somethingelse.complete"));
        assert!(triggers.contains("broker: checkout-saga-events"));
        assert!(triggers.contains("type: checkout-saga.started"));
        assert!(triggers.contains("broker: github-projects-events"));
        assert!(triggers.contains("type: github-projects.issue.created"));

        assert!(scaffold
            .gitops_argo_application_yaml()
            .contains("path: .gitops/deploy"));
        assert!(scaffold
            .gitops_flux_helmrelease_yaml()
            .contains("chart: .gitops/deploy"));
    }

    #[test]
    fn parses_github_repositories_and_create_args() {
        let repo = GithubRepo::parse("hops-ops/test-domain", "--github").unwrap();

        assert_eq!(repo.owner, "hops-ops");
        assert_eq!(repo.repo, "test-domain");
        assert_eq!(repo.full_name, "hops-ops/test-domain");
        assert_eq!(repo.environment_name(), "test-domain");
        assert_eq!(repo.image_repository(), "ghcr.io/hops-ops/test-domain");
        assert_eq!(
            github_repo_create_args(&repo),
            vec!["repo", "create", "hops-ops/test-domain", "--private"]
        );

        assert!(GithubRepo::parse("missing-repo", "--github").is_err());
        assert!(GithubRepo::parse("owner/repo/extra", "--github").is_err());
    }

    #[test]
    fn scaffold_generates_github_workflows_and_promotion_charts() {
        let names = ScaffoldNames::new("Test Domain").unwrap();
        let scaffold = Scaffold {
            names,
            output_dir: PathBuf::from("/tmp/test-domain"),
            distributed_dependency_path: PathBuf::from("../distributed"),
            transport: Transport::Http,
            store: Store::Postgres,
            bus: Some(Bus::Rabbitmq),
            include_read_models: false,
            gitops: true,
            gitops_promote: None,
            github: Some(GithubRepo::parse("hops-ops/test-domain", "--github").unwrap()),
            github_preview: Some(
                GithubRepo::parse("hops-ops/test-previews", "--github-preview").unwrap(),
            ),
            github_promote: Some(
                GithubRepo::parse("hops-ops/test-staging", "--github-promote").unwrap(),
            ),
            read_models: Vec::new(),
            models: Vec::new(),
            commands: message_handlers(vec!["test-domain.create".into()], "command").unwrap(),
            events: Vec::new(),
        };

        let workflows = [
            scaffold.github_version_workflow_yaml(),
            scaffold.github_release_workflow_yaml(),
            scaffold.github_preview_workflow_yaml(),
            scaffold.github_promote_workflow_yaml(),
        ];
        for workflow in &workflows {
            serde_yaml::from_str::<serde_yaml::Value>(workflow).unwrap();
        }

        assert!(scaffold
            .gitops_deploy_values_yaml()
            .contains("repository: ghcr.io/hops-ops/test-domain"));
        assert!(workflows[0].contains("unbounded-tech/workflow-vnext-tag"));
        assert!(workflows[0].contains("rust: true"));
        assert!(workflows[1].contains("unbounded-tech/workflow-simple-release"));
        assert!(workflows[2].contains("environment_repository: hops-ops/test-previews"));
        assert!(workflows[2].contains("promotion_chart_path: .gitops/preview/helm"));
        assert!(workflows[2].contains("preview: true"));
        assert!(workflows[3].contains("environment_repository: hops-ops/test-staging"));
        assert!(workflows[3].contains("promotion_chart_path: .gitops/promote/helm"));
        assert!(workflows[3].contains("destination_path: .gitops/deploy"));
        assert!(scaffold
            .github_promotion_values_yaml()
            .contains("path: .gitops/deploy"));
        assert!(scaffold
            .github_promotion_application_yaml()
            .contains("{{ .Values.application.name }}"));
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
