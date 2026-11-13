//! Public plugin identity: version is optional metadata, never ABI.
//!
//! A plugin that does not declare a version must keep working. The loader and
//! host may use this for diagnostics; it is not an authorization or ABI gate.

use cordis_core::{BoxFuture, Config, Ctx, Inject, Plugin, PluginKey, Result};
use std::sync::Arc;

struct Versioned;

impl Plugin for Versioned {
    fn name(&self) -> &str {
        "versioned"
    }

    fn key(&self) -> PluginKey {
        Arc::from("versioned")
    }

    fn inject(&self) -> Inject {
        Inject::default()
    }

    fn version(&self) -> Option<&str> {
        Some("1.2.3")
    }

    fn apply(&self, _ctx: Ctx, _config: Config) -> BoxFuture<Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn plugin_version_is_optional_and_explicit() {
    let versioned = Versioned;
    assert_eq!(versioned.version(), Some("1.2.3"));

    let anonymous = cordis_core::plugin_fn("anonymous", Inject::default(), |_, _| {
        Box::pin(async { Ok(()) })
    });
    assert_eq!(anonymous.version(), None);

    let wrapped = cordis_core::with_provision(Arc::new(Versioned), Vec::new());
    assert_eq!(
        wrapped.version(),
        Some("1.2.3"),
        "wrapper must preserve version"
    );
}
