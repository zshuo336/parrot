//! Actual websearch registration process contracts.
#[path = "../../test-support/registry.rs"]
mod support;

#[tokio::test]
async fn serve_only_rejects_gossip_address_and_unrelated_connection() {
    support::gossip_is_not_registration(env!("CARGO_BIN_EXE_websearch"), "websearch", true).await;
}
#[tokio::test]
async fn crawling_rejects_known_gateways_and_three_unrelated_connections() {
    support::gossip_is_not_registration(env!("CARGO_BIN_EXE_websearch"), "websearch", false).await;
}
#[tokio::test]
async fn serve_only_proceeds_when_the_jvm_itself_registers() {
    support::direct_registration(env!("CARGO_BIN_EXE_websearch"), "websearch", true).await;
}
#[tokio::test]
async fn crawling_proceeds_when_all_three_gateways_register() {
    support::direct_registration(env!("CARGO_BIN_EXE_websearch"), "websearch", false).await;
}
