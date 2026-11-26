//! T1 evaluator: runs the fixed five-case suite through the T2 pipeline.

use cordis_core::{BoxFuture, Config, Context, ExternService, Result};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

pub struct Evaluator {
    pub ctx: Weak<Context>,
    pub score_path: PathBuf,
}

impl ExternService for Evaluator {
    fn service(&self) -> &str {
        "evaluator"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["run_suite".to_string(), "hold".to_string()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        method: String,
        args: Config,
    ) -> BoxFuture<Result<Config>> {
        let ctx = self.ctx.upgrade();
        let score = self.score_path.clone();
        Box::pin(async move {
            let ctx = ctx.ok_or_else(|| -> cordis_core::Error {
                "evaluator provider has been unloaded".into()
            })?;
            match method.as_str() {
                "run_suite" => {
                    let cases = [1u64, 2, 3, 4, 5];
                    let mut rows = Vec::new();
                    let mut passed = 0usize;
                    for id in cases {
                        let result = ctx.call_iface("pipeline@1", 1, "run", json!([id])).await;
                        let row = match result {
                            Ok(value) if value.get("error").is_none() => {
                                passed += 1;
                                json!({ "id": id, "ok": true, "value": value })
                            }
                            Ok(value) => json!({ "id": id, "ok": false, "error": value }),
                            Err(err) => json!({ "id": id, "ok": false, "error": err.to_string() }),
                        };
                        rows.push(row);
                    }
                    let summary = json!({
                        "total": rows.len(),
                        "passed": passed,
                        "cases": rows,
                    });
                    std::fs::write(
                        &score,
                        format!("{}\n", serde_json::to_string_pretty(&summary).unwrap()),
                    )
                    .map_err(|err| -> cordis_core::Error { err.to_string().into() })?;
                    Ok(summary)
                }
                "hold" => ctx.call_iface("pipeline@1", 1, "run_hold", args).await,
                other => Err(format!("evaluator has no method {other:?}").into()),
            }
        })
    }
}

pub fn plugin_fn(score: PathBuf) -> Arc<dyn cordis_core::Plugin> {
    cordis_core::plugin_fn(
        "evaluator",
        cordis_core::Inject::of(&["pipeline@1"]),
        move |ctx, _config| {
            let score = score.clone();
            Box::pin(async move {
                let service = Arc::new(Evaluator {
                    ctx: ctx.downgrade(),
                    score_path: score,
                });
                ctx.provide_iface("evaluator@1", service)?;
                Ok(())
            })
        },
    )
}
