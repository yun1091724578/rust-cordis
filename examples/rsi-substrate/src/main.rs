use rsi_substrate::scenario;

#[tokio::main]
async fn main() {
    match scenario::run().await {
        Ok(report) => {
            println!(
                "v1={} v2={} v1_again={} trace_lines={}",
                report.v1["passed"],
                report.v2["passed"],
                report.v1_again["passed"],
                report.trace.len()
            );
        }
        Err(err) => {
            eprintln!("rsi-substrate failed: {err}");
            std::process::exit(1);
        }
    }
}
