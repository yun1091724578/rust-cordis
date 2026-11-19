//! Build and stage the real cdylibs used by the e2e.

use cordis_host::source::{sha256_file, AllowedPlugin, PluginSourcePolicy};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SOLVER_VARIANTS: [&str; 5] = ["v1", "v2", "v3", "panic", "crash"];

#[derive(Clone, Debug)]
pub struct Artifact {
    pub path: PathBuf,
    pub manifest: PathBuf,
    pub spec: PathBuf,
    pub sha256: String,
    pub revision: &'static str,
}

pub struct Fixtures {
    pub dir: TempDir,
    pub pipeline: Artifact,
    pub solver: BTreeMap<&'static str, Artifact>,
    pub eval_records: BTreeMap<&'static str, PathBuf>,
    pub policy: PluginSourcePolicy,
    target: PathBuf,
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        if !self.dir.keep && !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.target);
        }
    }
}

pub struct TempDir {
    path: PathBuf,
    keep: bool,
}

impl TempDir {
    fn create() -> Self {
        let path = std::env::temp_dir().join(format!(
            "rsi-substrate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create run dir");
        Self {
            path,
            keep: std::env::var_os("CORDIS_RSI_KEEP").is_some(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.keep && !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

impl Fixtures {
    /// Build all artifacts before creating the App (the loader's watcher needs
    /// the current runtime, and builds are blocking).
    pub fn build() -> Result<Self, String> {
        let root = workspace_root();
        let base = option_env!("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
            .unwrap_or_else(std::env::temp_dir);
        let base = if base.is_absolute() {
            base
        } else {
            root.join(base)
        };
        // Per-process target under the shared tmp dir: dependencies still
        // reuse the outer target cache, but the six feature builds cannot
        // race another test binary. It is removed on successful completion.
        let target = base.join(format!("rsi-substrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).map_err(|err| err.to_string())?;
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let dir = TempDir::create();

        let pipeline_spec = write_spec(
            &dir.path,
            "pipeline.spec.json",
            r#"{"service":"pipeline@1","methods":["run","run_hold"]}"#,
        )?;
        build(&cargo, &root, &target, &["-p", "rsi-pipeline"])?;
        let pipeline = stage(
            &target,
            &dir.path,
            Stage {
                stem: "rsi_pipeline",
                name: "rsi-pipeline.dll",
                id: "rsi-pipeline",
                spec: &pipeline_spec,
                revision: "rsi:pipeline",
                eval_record: None,
            },
        )?;

        let mut solver = BTreeMap::new();
        let mut eval_records = BTreeMap::new();
        for variant in SOLVER_VARIANTS {
            build(
                &cargo,
                &root,
                &target,
                &[
                    "-p",
                    "rsi-solver",
                    "--no-default-features",
                    "--features",
                    variant,
                ],
            )?;
            let name = format!("solver-{variant}.dll");
            let revision: &'static str = match variant {
                "v1" => "rsi:v1",
                "v2" => "rsi:v2",
                "v3" => "rsi:v3",
                "panic" => "rsi:panic",
                _ => "rsi:crash",
            };
            let eval_record = dir.path.join(format!("eval-record-{variant}.json"));
            let spec = if variant == "v3" {
                r#"{"service":"solver@1","methods":["answer","hold"]}"#
            } else {
                r#"{"service":"solver@1","methods":["solve","hold"]}"#
            };
            let iface_spec = write_spec(&dir.path, &format!("solver-{variant}.spec.json"), spec)?;
            solver.insert(
                variant,
                stage(
                    &target,
                    &dir.path,
                    Stage {
                        stem: "rsi_solver",
                        name: &name,
                        id: "rsi-solver",
                        spec: &iface_spec,
                        revision,
                        eval_record: Some(&eval_record),
                    },
                )?,
            );
            eval_records.insert(variant, eval_record);
        }

        let mut allowed = Vec::new();
        for artifact in std::iter::once(&pipeline).chain(solver.values()) {
            allowed.push(AllowedPlugin {
                id: manifest_id(&artifact.path),
                version: "0.1.0".to_string(),
                source_revision: artifact.revision.to_string(),
                abi_version: cordis_abi::ABI_VERSION,
                sha256: artifact.sha256.clone(),
            });
        }
        let allow_list = json!({
            "allowed": allowed.iter().map(|row| json!({
                "id": &row.id,
                "version": &row.version,
                "source_revision": &row.source_revision,
                "abi_version": row.abi_version,
                "sha256": &row.sha256,
            })).collect::<Vec<_>>()
        });
        std::fs::write(
            dir.path.join("allow-list.json"),
            format!("{}\n", serde_json::to_string_pretty(&allow_list).unwrap()),
        )
        .map_err(|err| err.to_string())?;
        let policy = PluginSourcePolicy::new(allowed);

        Ok(Self {
            dir,
            pipeline,
            solver,
            eval_records,
            policy,
            target,
        })
    }

    pub fn canary(&self) -> PathBuf {
        self.dir.path().join("solver.dll")
    }

    pub fn baseline(&self) -> PathBuf {
        self.dir.path().join("solver-baseline.dll")
    }

    /// Publish a variant as the canary: manifest first, artifact second, both
    /// via temp+rename so HMR sees one fingerprint change.
    pub fn publish(&self, variant: &str) -> Result<(), String> {
        self.publish_to(&self.canary(), variant)
    }

    /// Publish to the currently monitored source path.
    pub fn publish_to(&self, target: &Path, variant: &str) -> Result<(), String> {
        let artifact = self
            .solver
            .get(variant)
            .ok_or_else(|| format!("unknown solver variant {variant}"))?;
        atomic_copy(&artifact.manifest, &manifest_path(target))?;
        atomic_copy(&artifact.path, target)
    }

    pub fn install_baseline(&self) -> Result<(), String> {
        let artifact = self.solver.get("v1").expect("v1 exists");
        let target = self.baseline();
        std::fs::copy(&artifact.manifest, manifest_path(&target)).map_err(|err| err.to_string())?;
        atomic_copy(&artifact.path, &target)
    }

    /// Install pipeline and the initial canary. The YAML points at these
    /// stable names; later publishes only replace the canary file.
    pub fn install_initial(&self) -> Result<(), String> {
        std::fs::copy(
            &self.pipeline.manifest,
            manifest_path(&self.pipeline_path()),
        )
        .map_err(|err| err.to_string())?;
        atomic_copy(&self.pipeline.path, &self.pipeline_path())?;
        self.install_baseline()?;
        self.publish("v1")
    }

    pub fn pipeline_path(&self) -> PathBuf {
        self.dir.path().join("pipeline.dll")
    }

    pub fn write_config(&self, solver_name: &str) -> Result<PathBuf, String> {
        let path = self.dir.path().join("cordis.yaml");
        let tmp = path.with_extension("tmp-yaml");
        std::fs::write(
            &tmp,
            format!("- id: pipeline\n  name: pipeline.dll\n- id: solver\n  name: {solver_name}\n"),
        )
        .map_err(|err| err.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|err| err.to_string())?;
        Ok(path)
    }
}

fn build(cargo: &str, root: &Path, target: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new(cargo)
        .current_dir(root)
        .arg("build")
        .args(args)
        .arg("--offline")
        .arg("--locked")
        .arg("--target-dir")
        .arg(target)
        .status()
        .map_err(|err| format!("启动 cargo build 失败: {err}"))?;
    if !status.success() {
        return Err(format!("cargo build {args:?} 失败: {status}"));
    }
    Ok(())
}

struct Stage<'a> {
    stem: &'a str,
    name: &'a str,
    id: &'a str,
    spec: &'a Path,
    revision: &'static str,
    eval_record: Option<&'a Path>,
}

fn stage(target: &Path, dir: &Path, spec: Stage<'_>) -> Result<Artifact, String> {
    let built = target.join("debug").join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        spec.stem,
        std::env::consts::DLL_SUFFIX
    ));
    let path = dir.join(spec.name);
    atomic_copy(&built, &path)?;
    let sha256 = sha256_file(&path)?;
    let iface_spec = sha256_file(spec.spec)?;
    let manifest = manifest_path(&path);
    let mut body = json!({
        "id": spec.id,
        "version": "0.1.0",
        "source_revision": spec.revision,
        "abi_version": cordis_abi::ABI_VERSION,
        "sha256": sha256,
        "provides": if spec.id == "rsi-solver" { json!({"solver@1": "1.0.0"}) } else { json!({"pipeline@1": "1.0.0"}) },
        "requires": if spec.id == "rsi-solver" { json!({}) } else { json!({"solver@1": "^1.0"}) },
        "iface_spec_sha256": iface_spec,
    });
    if let Some(eval_record) = spec.eval_record {
        let record = json!({
            "pipeline_id": "rsi-evolve",
            "pipeline_revision": "rsi-pipeline@1",
            "variant_revision": spec.revision,
            "artifact_sha256": sha256,
            "cases": [1, 2, 3, 4, 5],
        });
        let text = format!("{}\n", serde_json::to_string_pretty(&record).unwrap());
        std::fs::write(eval_record, &text).map_err(|err| err.to_string())?;
        let eval_sha = sha256_file(eval_record)?;
        body["attestation"] = json!({
            "pipeline_id": "rsi-evolve",
            "pipeline_revision": "rsi-pipeline@1",
            "eval_record_sha256": eval_sha,
        });
    }
    std::fs::write(
        &manifest,
        format!("{}\n", serde_json::to_string_pretty(&body).unwrap()),
    )
    .map_err(|err| err.to_string())?;
    Ok(Artifact {
        path,
        manifest,
        spec: spec.spec.to_path_buf(),
        sha256,
        revision: spec.revision,
    })
}

fn manifest_id(path: &Path) -> String {
    let text = std::fs::read_to_string(manifest_path(path)).unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn manifest_path(artifact: &Path) -> PathBuf {
    cordis_host::source::manifest_path(artifact)
}

fn atomic_copy(source: &Path, target: &Path) -> Result<(), String> {
    let tmp = target.with_extension("tmp-copy");
    std::fs::copy(source, &tmp).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp, target).map_err(|err| err.to_string())
}

fn write_spec(dir: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    let path = dir.join(name);
    std::fs::write(&path, text).map_err(|err| err.to_string())?;
    Ok(path)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("examples/rsi-substrate has a workspace root")
        .to_path_buf()
}
