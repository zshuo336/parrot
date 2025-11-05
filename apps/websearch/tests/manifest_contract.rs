use parrot_app::assemble::effective_config;
use parrot_app::manifest::{validate, AppManifest};
use std::path::PathBuf;

#[test]
fn shipped_manifest_is_accepted_by_the_standard_application_loader() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("websearch.app.toml");
    let manifest = AppManifest::from_file(&path).expect("websearch manifest parses");
    validate(&manifest).expect("websearch manifest validates");
    let resolved = effective_config(&manifest, &Default::default())
        .expect("business settings must not be parsed as framework overlay keys");
    assert!(resolved.thread.shared_pool_size > 0);

    let crawler = manifest
        .component("crawler")
        .expect("crawler component is declared");
    let settings = crawler
        .config
        .as_ref()
        .and_then(|table| table.get("settings"))
        .and_then(|value| value.as_str())
        .expect("crawler business settings are carried as component config");
    assert!(settings.contains("\"sites\":100"));
}
