//! 0.2 migration path: external consumers can construct config DTOs without
//! relying on public field literals.
//!
//! This is intentionally a constructor-only test. 0.1 still permits literals,
//! but 0.2 may make selected DTOs opaque; this file must keep compiling.

use cordis_host::loader::LoaderConfig;
use cordis_host::{DispatchPolicy, HostLimits};

#[test]
fn config_dtos_are_usable_through_constructors_only() {
    let mut config = LoaderConfig::new("cordis.yml");
    config.host_limits = Some(HostLimits::new(16, 32, 8, 4).expect("valid limits"));
    config.dispatch_policy = Some(DispatchPolicy::new(Some(100), Some(200)));

    let limits = config.host_limits.expect("limits");
    assert_eq!(limits.max_instances_per_app, 16);
    assert_eq!(limits.max_dispatches_per_instance, 32);
    assert_eq!(limits.max_pending_per_instance, 8);
    assert_eq!(limits.max_streams_per_instance, 4);

    let policy = config.dispatch_policy.expect("policy");
    assert_eq!(policy.timeout_ms, Some(100));
    assert_eq!(policy.outbound_timeout_ms, Some(200));
}
