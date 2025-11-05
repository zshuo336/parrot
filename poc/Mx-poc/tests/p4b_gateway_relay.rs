//! JVM -> Rust hub -> OTP, with separate callback namespaces per TCP link.
#[path = "support/gateway.rs"]
mod gateway;
#[path = "support/interop.rs"]
mod interop;
use remote_poc::*;
use std::sync::Arc;

#[tokio::test]
async fn gateway_ask_and_tell_relay_via_parrot_hub() {
    interop::install();
    let erlang = gateway::Gateway::start("erlang", None).await;
    let erl_ep = spawn_endpoint(
        tcp_connect(([127, 0, 0, 1], erlang.port).into())
            .await
            .unwrap(),
    )
    .await;
    let jvm = gateway::Gateway::start("jvm", Some("/erl/user/erlang_service")).await;
    let router = Arc::new(PrefixRouter::new());
    router.add("/erl", erl_ep.sender(), erl_ep.callbacks.clone());
    let ep = spawn_endpoint_routed(
        tcp_connect(([127, 0, 0, 1], jvm.port).into())
            .await
            .unwrap(),
        router,
    )
    .await;
    interop::exercise(&ep, "/user/akka_service", 1, 222, 2).await;
    let remote = ep.remote_ref("/user/akka_service");
    assert_eq!(
        *interop::ask(&remote, Box::new(interop::Relay))
            .await
            .downcast_ref::<u64>()
            .unwrap(),
        103
    );
    assert_eq!(
        *interop::ask(&remote, Box::new(interop::RelayTell))
            .await
            .downcast_ref::<u64>()
            .unwrap(),
        1
    );
    assert_eq!(
        *interop::ask(&remote, Box::new(interop::RelayTell))
            .await
            .downcast_ref::<u64>()
            .unwrap(),
        2
    );
    // Independent destination query proves the TELL reached OTP, not JVM.
    let destination = erl_ep.remote_ref("/user/erlang_service");
    assert_eq!(
        *interop::ask(&destination, Box::new(interop::TellCount))
            .await
            .downcast_ref::<u64>()
            .unwrap(),
        2
    );
    assert_eq!(ep.callbacks.pending_count(), 0);
    assert_eq!(erl_ep.callbacks.pending_count(), 0);
    jvm.close().await;
    erlang.close().await;
}
