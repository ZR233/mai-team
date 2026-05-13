use std::sync::Arc;

use mai_agents::AgentProfilesManager;
use mai_docker::DockerClient;
use mai_model::ModelClient;
use mai_skills::SkillsManager;
use mai_store::ConfigStore;

use crate::GithubAppBackend;

pub(crate) struct RuntimeDeps {
    pub(crate) docker: DockerClient,
    pub(crate) model: ModelClient,
    pub(crate) store: Arc<ConfigStore>,
    pub(crate) skills: SkillsManager,
    pub(crate) agent_profiles: AgentProfilesManager,
    pub(crate) github_http: reqwest::Client,
    pub(crate) github_backend: Arc<dyn GithubAppBackend>,
}
