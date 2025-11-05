//! Static checks for the crawler manifest template. These do not load its
//! artifacts, execute wiring or compare CID traces.
//! Real component business and golden comparisons run in test-crawler-business;
//! actual manifest scenarios run in run_manifest_integration.py.

use parrot_app::manifest::AppManifest;
use parrot_app::planner::{plan, LocalTopology};

fn manifest() -> AppManifest {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/crawler-lab/crawler.app.toml");
    AppManifest::from_file(&p).expect("crawler.app.toml 解析")
}

#[test]
fn manifest_parses_and_validates() {
    let m = manifest();
    assert_eq!(m.name, "crawler-lab");
    assert_eq!(m.version, "2.0.0"); // template version; not proof of deployment
    parrot_app::manifest::validate(&m).expect("manifest 合法");
}

#[test]
fn declared_dependency_order_is_frontier_index_search_crawler() {
    let m = manifest();
    let p = plan(&m, &LocalTopology).expect("plan");
    let order: Vec<&str> = p.order.iter().map(|c| c.spec.name.as_str()).collect();
    assert_eq!(order, vec!["frontier", "index", "search", "crawler"]);
}

#[test]
fn wiring_endpoints_resolve() {
    let m = manifest();
    let names: Vec<&str> = m.components.iter().map(|c| c.name.as_str()).collect();
    for w in &m.wiring {
        let from = w.from.split(':').next().expect("from 组件名");
        let to = w.to.split(':').next().expect("to 组件名");
        assert!(names.contains(&from), "wiring from {from} 未声明");
        assert!(names.contains(&to), "wiring to {to} 未声明");
    }
    assert_eq!(m.wiring.len(), 3, "三连接：hub→三网关");
}

#[test]
fn business_settings_are_not_framework_overlay() {
    let m = manifest();
    let config = m.component("crawler").unwrap().config.as_ref().unwrap();
    let settings =
        crawler_lab_core::Config::from_json(config["settings"].as_str().unwrap().as_bytes())
            .unwrap();
    assert_eq!(
        (
            settings.pages,
            settings.depth,
            settings.fanout,
            settings.batch
        ),
        (60, 2, 3, 32)
    );
    parrot_app::assemble::effective_config(&m, &Default::default()).unwrap();
}

#[test]
fn engines_match_topology() {
    let m = manifest();
    let get = |n: &str| {
        m.component(n)
            .unwrap_or_else(|| panic!("{n} 缺失"))
            .engine
            .as_str()
    };
    assert_eq!(get("frontier"), "erlang");
    assert_eq!(get("index"), "ray");
    assert_eq!(get("search"), "akka");
    assert_eq!(get("crawler"), "parrot");
}

/// Pin the entire historical fixture. Semantic comparison to actual runs is
/// separately performed by regression.compare; this alone proves no business.
#[test]
fn golden_file_is_byte_for_byte_preserved() {
    use sha2::{Digest, Sha256};
    let bytes = include_bytes!("../golden/pre_migration_output.txt");
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes)),
        "1537c00a3d93cb787b36e3c6abc3e00f55916f0197edbd3841fcaf0545dbe67c"
    );
}
