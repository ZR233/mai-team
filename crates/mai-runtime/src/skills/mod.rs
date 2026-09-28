mod manager;
mod review_mode;

pub use manager::{SkillCatalogService, normalize_config};
pub(crate) use review_mode::{REVIEW_MODE_CONTENT, REVIEW_MODE_ID};
