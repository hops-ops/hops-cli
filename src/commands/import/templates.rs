pub const LOCAL_CHART: &str = include_str!("templates/local-chart.yaml.tmpl");
pub const DEPLOY_CHART: &str = include_str!("templates/deploy-chart.yaml.tmpl");
pub const LOCAL_DEPLOYMENT_VALUES: &str =
    include_str!("templates/local-deployment-values.yaml.tmpl");
pub const DEPLOY_DEPLOYMENT_VALUES: &str =
    include_str!("templates/deploy-deployment-values.yaml.tmpl");
pub const LOCAL_KNATIVE_VALUES: &str = include_str!("templates/local-knative-values.yaml.tmpl");
pub const DEPLOY_KNATIVE_VALUES: &str = include_str!("templates/deploy-knative-values.yaml.tmpl");
pub const DEPLOYMENT_SERVICE: &str = include_str!("templates/deployment-service.yaml.tmpl");
pub const KNATIVE_SERVICE: &str = include_str!("templates/knative-service.yaml.tmpl");
pub const PROMOTE_CHART: &str = include_str!("templates/promote-chart.yaml.tmpl");
pub const PROMOTE_VALUES: &str = include_str!("templates/promote-values.yaml.tmpl");
pub const PROMOTE_APPLICATION: &str = include_str!("templates/promote-application.yaml.tmpl");
pub const VERSION_WORKFLOW: &str = include_str!("templates/version-workflow.yaml.tmpl");
pub const DOCKER_PUBLISH_WORKFLOW: &str = include_str!("templates/publish-image-docker.yaml.tmpl");
pub const RAILPACK_PUBLISH_WORKFLOW: &str =
    include_str!("templates/publish-image-railpack.yaml.tmpl");
pub const RELEASE_WORKFLOW: &str = include_str!("templates/release-workflow.yaml.tmpl");
pub const PREVIEW_WORKFLOW: &str = include_str!("templates/preview-workflow.yaml.tmpl");
