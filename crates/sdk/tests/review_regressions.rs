//! Regression tests for P1/P2 findings in `cordis-sdk` and `cordis-macros`.

use cordis::core::Dyn;

#[test]
fn decode_ret_rejects_non_json_dyn_instead_of_ok_none() {
    struct NativeOnly;
    let ret = Some(Dyn::new(NativeOnly));
    let decoded: Result<Option<String>, _> = cordis::typed::events::decode_ret(ret);
    assert!(
        decoded.is_err(),
        "non-JSON Dyn return value must return Err, not Ok(None)"
    );
}

#[cordis::config]
#[derive(Debug, PartialEq)]
struct RawIdentConfig {
    pub r#type: u32,
}

#[test]
fn config_macro_probes_raw_ident_field() {
    let err = RawIdentConfig::parse(serde_json::json!({ "type": "not-a-number" })).unwrap_err();
    assert!(
        err.issues.iter().any(|i| i.starts_with("type:")),
        "expected field-level error for `type`, got {:?}",
        err.issues
    );
}

#[cordis::service(name = "raw.ident", version = 1)]
trait RawIdentSvc {
    async fn echo(&self, r#type: String) -> Result<String, String>;
}

struct RawIdentImpl;

#[cordis::service_impl]
impl RawIdentSvc for RawIdentImpl {
    async fn echo(&self, r#type: String) -> Result<String, String> {
        Ok(r#type)
    }
}

#[tokio::test]
async fn service_macro_supports_raw_ident_param() {
    let svc = RawIdentImpl;
    assert_eq!(svc.echo("ok".to_string()).await.unwrap(), "ok");
}
