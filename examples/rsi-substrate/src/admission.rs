//! Example admission gate: attestation + eval record + artifact cross-check.
//!
//! `PluginSourcePolicy::verify` intentionally only checks id/semver/revision/
//! hash/ABI. The RSI example adds the deployment-side check the ADR requires:
//! the eval record must describe the same artifact and variant revision, and
//! the manifest's attestation must bind the eval record bytes.

use cordis_host::source::{manifest_path, sha256_file};
use serde_json::Value;
use std::path::Path;

pub const REVIEWED_PIPELINES: &[&str] = &["rsi-pipeline@1"];

pub fn verify(artifact: &Path, eval_record: &Path, spec: &Path) -> Result<(), String> {
    let manifest: Value = read_json(&manifest_path(artifact))?;
    let record: Value = read_json(eval_record)?;
    let actual = sha256_file(artifact)?;

    if manifest["sha256"].as_str() != Some(actual.as_str()) {
        return Err("manifest.sha256 does not match the artifact".to_string());
    }
    let iface = manifest["iface_spec_sha256"]
        .as_str()
        .ok_or_else(|| "manifest.iface_spec_sha256 is missing".to_string())?;
    if iface.len() != 64 || !iface.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("manifest.iface_spec_sha256 is not a SHA-256 hex string".to_string());
    }
    let actual_spec = sha256_file(spec)?;
    if iface != actual_spec {
        return Err("manifest.iface_spec_sha256 does not match the interface spec".to_string());
    }
    if record["artifact_sha256"].as_str() != Some(actual.as_str()) {
        return Err("eval_record.artifact_sha256 does not match the artifact".to_string());
    }
    if record["variant_revision"].as_str() != manifest["source_revision"].as_str() {
        return Err("eval_record.variant_revision does not match the manifest".to_string());
    }
    let eval_sha = sha256_file(eval_record)?;
    let attestation = &manifest["attestation"];
    if attestation["eval_record_sha256"].as_str() != Some(eval_sha.as_str()) {
        return Err("attestation.eval_record_sha256 does not match the eval record".to_string());
    }
    let pipeline = attestation["pipeline_revision"]
        .as_str()
        .ok_or_else(|| "attestation.pipeline_revision is missing".to_string())?;
    if !REVIEWED_PIPELINES.contains(&pipeline) {
        return Err(format!("pipeline revision {pipeline:?} is not reviewed"));
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("parse {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tampered_eval_record_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "rsi-admission-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join("solver.dll");
        std::fs::write(&artifact, b"artifact-bytes").unwrap();
        let artifact_sha = sha256_file(&artifact).unwrap();
        let eval = dir.join("eval.json");
        let spec = dir.join("spec.json");
        std::fs::write(&spec, br#"{"methods":["solve","hold"]}"#).unwrap();
        let spec_sha = sha256_file(&spec).unwrap();
        let eval_body = json!({
            "pipeline_id": "rsi-evolve",
            "pipeline_revision": "rsi-pipeline@1",
            "variant_revision": "rsi:v1",
            "artifact_sha256": artifact_sha,
            "cases": [1, 2, 3, 4, 5],
        });
        std::fs::write(
            &eval,
            format!("{}\n", serde_json::to_string_pretty(&eval_body).unwrap()),
        )
        .unwrap();
        let eval_sha = sha256_file(&eval).unwrap();
        let manifest = json!({
            "id": "rsi-solver",
            "version": "0.1.0",
            "source_revision": "rsi:v1",
            "abi_version": 1,
            "sha256": artifact_sha,
            "provides": { "solver@1": "1.0.0" },
            "requires": {},
            "iface_spec_sha256": spec_sha,
            "attestation": {
                "pipeline_id": "rsi-evolve",
                "pipeline_revision": "rsi-pipeline@1",
                "eval_record_sha256": eval_sha,
            },
        });
        std::fs::write(
            manifest_path(&artifact),
            format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
        )
        .unwrap();
        assert!(verify(&artifact, &eval, &spec).is_ok());

        // Keep the eval record valid JSON: only the attestation SHA can catch
        // this tamper.
        let mut record: Value = read_json(&eval).unwrap();
        record["cases"] = json!([9]);
        std::fs::write(
            &eval,
            format!("{}\n", serde_json::to_string_pretty(&record).unwrap()),
        )
        .unwrap();
        assert!(
            verify(&artifact, &eval, &spec).is_err(),
            "a semantically tampered eval record must fail admission"
        );

        // Restore the eval record and tamper the interface spec instead.
        std::fs::write(
            &eval,
            format!("{}\n", serde_json::to_string_pretty(&eval_body).unwrap()),
        )
        .unwrap();
        std::fs::write(&spec, br#"{"methods":["answer","hold"]}"#).unwrap();
        assert!(
            verify(&artifact, &eval, &spec).is_err(),
            "a mismatched interface spec must fail admission"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
