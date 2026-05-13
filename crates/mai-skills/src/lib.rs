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
pub use error::{Result, SkillError};
pub use injection::{LoadedSkill, SkillInjections, SkillInput, SkillSelection};
pub use manager::SkillsManager;
pub use mentions::extract_skill_mentions;
pub use render::render_available_response;

#[cfg(test)]
mod tests;
