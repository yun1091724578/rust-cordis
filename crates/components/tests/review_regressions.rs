//! Regression tests for P2 findings in `cordis-components`.

use cordis_components::registry;
use cordis_core::{App, Config, Dyn};

#[tokio::test]
async fn registry_get_and_names_return_empty_when_disposed() {
    let app = App::new();
    let fib = app.root.plugin(registry::plugin(), Config::Null);
    fib.settle().await;
    let handle = app
        .root
        .get(registry::HANDLE_KEY)
        .expect("registry service")
        .downcast::<registry::RegistryHandle>()
        .unwrap();
    let _slot = handle.put("alpha", Dyn::new(1i32)).unwrap();
    assert_eq!(handle.names(), vec!["alpha".to_string()]);
    assert!(handle.get("alpha").is_some());

    // Dispose the registry fiber; even if another strong Ctx temporarily existed,
    // once inactive `names()` and `get()` must not report active slots.
    fib.dispose().await;
    assert!(handle.names().is_empty());
    assert!(handle.get("alpha").is_none());
    app.close().await;
}
