//! Actual crawler-lab registration process contracts.
#[path = "../../test-support/registry.rs"]
mod support;

#[tokio::test]
async fn gossip_addresses_and_other_connections_do_not_register_gateways() {
    support::gossip_is_not_registration(env!("CARGO_BIN_EXE_crawler-lab"), "crawler-lab", false)
        .await;
}
#[tokio::test]
async fn all_three_registered_gateways_allow_the_scenario_to_begin() {
    support::direct_registration(env!("CARGO_BIN_EXE_crawler-lab"), "crawler-lab", false).await;
}
