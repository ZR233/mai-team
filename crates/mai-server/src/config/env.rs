/// Provides configuration values without coupling callers to process-global
/// environment mutation.
pub(crate) trait Env {
    fn var(&self, key: &str) -> Option<String>;
}

#[derive(Debug, Default)]
pub(crate) struct StdEnv;

impl Env for StdEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::{Env, StdEnv};

    #[test]
    fn std_env_returns_none_for_missing_values() {
        let env = StdEnv;

        assert_eq!(env.var("MAI_TEST_VALUE_THAT_SHOULD_NOT_EXIST"), None);
    }
}
