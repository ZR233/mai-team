use std::fs;
use std::path::Path;

#[test]
fn mai_provider_boundaries_do_not_rebuild_pl_model_semantics() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let conversion = production_source(&root.join("crates/mai-runtime/src/config/conversion.rs"));
    let profile = production_source(&root.join("crates/mai-runtime/src/model_profile.rs"));
    let provider_page = fs::read_to_string(
        root.join("crates/mai-server/web/src/features/providers/providers-page.tsx"),
    )
    .expect("provider page");

    for forbidden in [
        "ModelInfo::fallback",
        "ProviderInfo::",
        "ModelTransportProfile::",
        "connection_overrides",
        "ProviderConnectionMode",
        "ProviderWireProtocol",
    ] {
        assert!(
            !conversion.contains(forbidden) && !profile.contains(forbidden),
            "mai runtime production source must not rebuild PL semantic `{forbidden}`"
        );
    }
    for forbidden in [
        "connectionMode",
        "Wire protocol",
        "capabilitySource",
        "Additional/custom models",
        "chat_completions",
    ] {
        assert!(
            !provider_page.contains(forbidden),
            "provider Web editor must not infer PL semantic `{forbidden}`"
        );
    }
}

fn production_source(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
        .split("#[cfg(test)]")
        .next()
        .expect("production source")
        .to_string()
}
