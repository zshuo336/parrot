//! G1/M1（DEV_09 §3.7）：crawler-lab Manifest 迁移验证。
//!
//! crawler.app.toml 与手写装配（apps/crawler-lab/src/main.rs）行为等价：
//! 1. Manifest 解析 + 校验通过（拓扑即依赖序）；
//! 2. 拓扑序与 main.rs 装配序一致（frontier→index→search→crawler——
//!    三网关先起，hub 最后织入）；
//! 3. wiring 连接可解析（from/to 组件均在声明内）；
//! 4. 配置 overlay 与 golden 录制参数一致（pages=60 depth=2 fanout=3
//!    batch=32——行为等价的输入面）；
//! 5. golden 关键断言锚定（迁移不触碰语义——本测试防回归漂移）。
//!
//! 四网关全链回归（cid 轨迹 vs golden/pre_migration_output.txt）由
//! `deploy/crawler-lab/run-lab.sh --pages 60 --skip-ts` 承载——本测试
//! 为其静态前置门禁。

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
    assert_eq!(m.version, "2.0.0"); // R5 新形态（uri/alt_artifact 制品直发）
    parrot_app::manifest::validate(&m).expect("manifest 合法");
}

#[test]
fn topo_order_matches_manual_assembly() {
    // main.rs 装配序：先三网关（frontier/index/search）后本体 hub（crawler）
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
fn overlay_matches_golden_recording_params() {
    let m = manifest();
    let o = m.config_overlay.expect("overlay 存在");
    // golden 录制参数（README：pages=60 depth=2 fanout=3 batch=32）
    assert_eq!(o.get("pages").and_then(|v| v.as_integer()), Some(60));
    assert_eq!(o.get("depth").and_then(|v| v.as_integer()), Some(2));
    assert_eq!(o.get("fanout").and_then(|v| v.as_integer()), Some(3));
    assert_eq!(o.get("batch").and_then(|v| v.as_integer()), Some(32));
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

/// golden 关键断言锚定（G1 迁移后 run-lab 输出必须重现——本测试保证
/// golden 文件本身不被篡改，防"改基线凑通过"）。
#[test]
fn golden_file_anchored() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/crawler-lab/golden/pre_migration_output.txt");
    let t = std::fs::read_to_string(&p).expect("golden 存在");
    // README 声明的关键断言（字节级）
    assert!(t.contains("fetched=60（去重后）pushed_total=99"));
    assert!(t.contains("terms=33 postings=1746"));
    assert!(t.contains("\"terms\":33,\"postings\":1746,\"queries\":0"));
    assert!(t.contains("crawler-lab PASS（四运行时全链集成）"));
}

/// 编排 ≤50 行门禁（DEV_09 G1：crawler.app.toml 编排 ≤50 行）。
#[test]
fn manifest_under_50_lines() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/crawler-lab/crawler.app.toml");
    let t = std::fs::read_to_string(&p).expect("manifest 存在");
    let non_comment = t
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .count();
    assert!(
        non_comment <= 50,
        "编排声明 {non_comment} 行超 50 行门禁（注释不计）"
    );
}
