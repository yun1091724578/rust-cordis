use cordis_host::loader::{DiskPluginSource, PluginSource};
use cordis_host::patch::Entry;
use cordis_host::source::{
    active_policy, set_active_policy, set_allow_unverified, AllowedPlugin, PluginSourcePolicy,
};
use cordis_host::{probe_capabilities_in, LoadedPlugin};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

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
            "abi_version": 1,
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
        abi_version: 1,
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
            "abi_version": 1,
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
fn open_staged_verified_refuses_to_delete_the_original_artifact() {
    let path = fixture("same-path", b"not a dynamic library");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = match LoadedPlugin::open_staged_verified(&path, &path, &policy) {
        Ok(_) => panic!("original path must not be adopted as a staged copy"),
        Err(error) => error,
    };
    let text = error.to_string();
    assert!(text.contains("暂存副本"), "{text}");
}

#[test]
fn open_staged_verified_rejects_different_staged_bytes() {
    let original = fixture("staged-original", b"abc");
    write_manifest(&original, SHA_ABC, "abc123");
    let staged = fixture("staged-copy", b"different");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = match LoadedPlugin::open_staged_verified(&original, &staged, &policy) {
        Ok(_) => panic!("a staged copy with different bytes must not load"),
        Err(error) => error,
    };
    let text = error.to_string();
    assert!(text.contains("暂存副本 SHA256"), "{text}");
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

/// 进程级 active policy 会让测试互相污染：拿锁并保证 Drop 时复位为 `None`。
struct ActivePolicyGuard {
    _lock: MutexGuard<'static, ()>,
}

static POLICY_LOCK: Mutex<()> = Mutex::new(());

impl ActivePolicyGuard {
    fn new() -> Self {
        let lock = POLICY_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_active_policy(None);
        set_allow_unverified(false);
        Self { _lock: lock }
    }

    fn set(&self, policy: PluginSourcePolicy) {
        set_active_policy(Some(Arc::new(policy)));
    }
}

impl Drop for ActivePolicyGuard {
    fn drop(&mut self) {
        set_active_policy(None);
        set_allow_unverified(false);
    }
}

fn entry_for(path: &Path) -> Entry {
    Entry {
        name: path.to_string_lossy().into_owned(),
        ..Entry::default()
    }
}

fn assert_no_staged_copy(path: &Path) {
    let dir = path.parent().expect("fixture 有父目录");
    let staged: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".r1.") || name.ends_with(".r1"))
        .collect();
    assert!(staged.is_empty(), "策略拒绝时不得产生暂存副本：{staged:?}");
}

#[test]
fn disk_source_without_policy_denies_by_default() {
    let _guard = ActivePolicyGuard::new();
    assert!(active_policy().is_none());

    let path = fixture("disk-no-policy", b"not a dynamic library");
    let error = match DiskPluginSource.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("默认拒绝不得装载"),
        Err(error) => error,
    };
    assert!(error.contains("未配置插件来源策略"), "{error}");
}

#[test]
fn disk_source_with_explicit_development_mode_keeps_the_legacy_load_path() {
    let _guard = ActivePolicyGuard::new();
    set_allow_unverified(true);
    let path = fixture("disk-dev-mode", b"not a dynamic library");
    let error = match DiskPluginSource.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("假字节不能装载成功"),
        Err(error) => error,
    };
    assert!(
        !error.contains("manifest") && !error.contains("allow-list"),
        "开发模式不得走来源校验：{error}"
    );
}

#[test]
fn disk_source_with_active_policy_rejects_an_unlisted_plugin_before_staging() {
    let guard = ActivePolicyGuard::new();
    let path = fixture("disk-unlisted", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    guard.set(PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]));

    let error = match DiskPluginSource.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("未列入 allow-list 的插件必须被拒绝"),
        Err(error) => error,
    };
    assert!(error.contains("allow-list"), "{error}");
    assert!(
        !error.contains("LoadLibrary") && !error.contains("dlopen"),
        "{error}"
    );
    assert_no_staged_copy(&path);
}

#[test]
fn disk_source_with_active_policy_rejects_a_tampered_plugin_before_staging() {
    let guard = ActivePolicyGuard::new();
    let path = fixture("disk-tampered", b"abc");
    // manifest 自述 empty 的 hash，但实际文件是 "abc"。
    write_manifest(&path, SHA_EMPTY, "abc123");
    guard.set(PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]));

    let error = match DiskPluginSource.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("artifact 与 manifest hash 不一致必须被拒绝"),
        Err(error) => error,
    };
    assert!(error.contains("SHA256"), "{error}");
    assert_no_staged_copy(&path);
}

#[test]
fn probe_with_active_policy_verifies_before_opening() {
    let guard = ActivePolicyGuard::new();
    let path = fixture("probe-unlisted", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    guard.set(PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]));

    let error = probe_capabilities_in(&path, &[])
        .expect_err("probe 也必须执行策略")
        .to_string();
    assert!(error.contains("allow-list"), "{error}");
    assert!(
        !error.contains("LoadLibrary") && !error.contains("dlopen"),
        "{error}"
    );
    assert_no_staged_copy(&path);
}
