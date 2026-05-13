pub(crate) mod api;
pub(crate) mod app;
pub(crate) mod flow;
pub(crate) mod packages;
pub(crate) mod types;

pub(crate) const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com";
pub(crate) const DEFAULT_GITHUB_WEB_BASE_URL: &str = "https://github.com";
pub(crate) const TOKEN_REFRESH_SKEW_SECS: i64 = 120;
