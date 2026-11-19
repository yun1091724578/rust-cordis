//! One test: the scenario installs a process-global source policy and must not
//! race another test in the same binary.

#[tokio::test]
async fn rsi_substrate_e2e() {
    rsi_substrate::scenario::run().await.expect("RSI e2e");
}
