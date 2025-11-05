//! Real TCP business receipts; historical JVM fixture is pure JDK, not Akka.
#[path = "support/gateway.rs"]
mod gateway;
#[path = "support/interop.rs"]
mod interop;
use remote_poc::*;

#[tokio::test]
async fn rust_jvm_business_contract() {
    interop::install();
    let owner = gateway::Gateway::start("jvm", None).await;
    let link = tcp_connect(([127, 0, 0, 1], owner.port).into())
        .await
        .unwrap();
    let ep = spawn_endpoint(link).await;
    interop::exercise(&ep, "/user/akka_service", 1, 222, 2).await;
    owner.close().await;
}
