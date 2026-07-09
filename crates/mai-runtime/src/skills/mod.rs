mod config;
mod constants;
mod error;
mod injection;
mod manager;
mod mentions;
mod ordering;
mod parser;
mod paths;
mod render;
mod scan;

pub use config::normalize_config;
pub use error::SkillError;
pub use injection::{SkillInjections, SkillInput, SkillSelection};
pub use manager::SkillsManager;
pub use render::render_available_response;

#[cfg(test)]
pub(crate) use injection::LoadedSkill;
#[cfg(test)]
pub(crate) use mentions::extract_skill_mentions;

#[cfg(test)]
mod tests;
