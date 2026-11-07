use cordis_host::source::{AllowedPlugin, PluginSourcePolicy};
use cordis_host::LoadedPlugin;
use std::path::{Path, PathBuf};

const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const SHA_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn fixture(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cordis-source-policy-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("plugin.dll");
    std::fs::write(&path, bytes).unwrap();
    path
}

fn write_manifest(path: &Path, hash: &str, revision: &str) {
    let manifest = path.with_extension("dll.manifest.json");
    std::fs::write(
        manifest,
        serde_json::json!({
            "id": "example.plugin",
            "version": "0.1.0",
            "source_revision": revision,
            "sha256": hash,
        })
        .to_string(),
    )
    .unwrap();
}

fn allowed(hash: &str) -> AllowedPlugin {
    AllowedPlugin {
        id: "example.plugin".to_string(),
        version: "0.1.0".to_string(),
        source_revision: "abc123".to_string(),
        sha256: hash.to_string(),
    }
}

#[test]
fn verifies_a_manifest_matching_the_artifact_and_allow_list() {
    let path = fixture("ok", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);

    let verified = policy.verify(&path).expect("verified source");
    assert_eq!(verified.manifest.id, "example.plugin");
    assert_eq!(verified.manifest.source_revision, "abc123");
    assert_eq!(verified.sha256, SHA_ABC);
}

#[test]
fn rejects_missing_manifest() {
    let path = fixture("missing-manifest", b"abc");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = policy.verify(&path).unwrap_err();
    assert!(error.contains("manifest"), "{error}");
}

#[test]
fn rejects_a_tampered_artifact() {
    let path = fixture("tampered", b"abc");
    write_manifest(&path, SHA_EMPTY, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]);
    let error = policy.verify(&path).unwrap_err();
    assert!(error.contains("SHA256"), "{error}");
}

#[test]
fn rejects_an_artifact_that_is_not_in_the_allow_list() {
    let path = fixture("not-allowed", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]);
    let error = policy.verify(&path).unwrap_err();
    assert!(error.contains("allow-list"), "{error}");
}

#[test]
fn rejects_a_manifest_without_source_revision() {
    let path = fixture("no-revision", b"abc");
    let manifest = path.with_extension("dll.manifest.json");
    std::fs::write(
        manifest,
        serde_json::json!({
            "id": "example.plugin",
            "version": "0.1.0",
            "source_revision": "",
            "sha256": SHA_ABC,
        })
        .to_string(),
    )
    .unwrap();
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = policy.verify(&path).unwrap_err();
    assert!(error.contains("source_revision"), "{error}");
}

#[test]
fn open_verified_rejects_a_tampered_artifact_before_dlopen() {
    let path = fixture("open-verified", b"not a dynamic library");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = match LoadedPlugin::open_verified(&path, &policy) {
        Ok(_) => panic!("tampered artifact must not load"),
        Err(error) => error,
    };
    let text = error.to_string();
    assert!(text.contains("SHA256"), "{text}");
    assert!(
        !text.contains("LoadLibrary") && !text.contains("dlopen"),
        "{text}"
    );
}

#[test]
fn rejects_an_allow_list_identity_mismatch() {
    let path = fixture("identity-mismatch", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![AllowedPlugin {
        id: "other.plugin".to_string(),
        ..allowed(SHA_ABC)
    }]);
    let error = policy.verify(&path).unwrap_err();
    assert!(error.contains("id"), "{error}");
}
