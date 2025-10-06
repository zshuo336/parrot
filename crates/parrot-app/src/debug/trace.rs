//! F1（DEV_09 §3.6）：trace span 树聚合。
//!
//! 数据源：PARROT_TRACE=frame 输出的 trace_line（TX/RX 每帧一行）。
//! 本模块：逐行解析 → 按 trace_id（TRACING 位帧 cid）聚合 →
//! hop/path 展开为 span 树（`parrot app trace <app>` 的渲染数据面）。

use std::collections::BTreeMap;

/// 单帧事件（trace_line 解析产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanEvent {
    pub trace_id: u64,
    pub frame_type: u8,
    pub hop_count: u8,
    pub path: String,
    pub type_key: String,
    pub len: usize,
    /// 解析原始行（审计保留）。
    pub raw: String,
}

/// span 树节点（路径聚合——同 path 多帧 = 多事件）。
#[derive(Debug, Clone, Default)]
pub struct SpanNode {
    pub path: String,
    pub events: Vec<SpanEvent>,
    /// 子节点（hop+1 的相邻路径）。
    pub children: BTreeMap<String, SpanNode>,
}

impl SpanNode {
    /// 本节点总帧数。
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// 子树帧数（含自身）。
    pub fn total_events(&self) -> usize {
        self.events.len()
            + self.children.values().map(SpanNode::total_events).sum::<usize>()
    }
}

/// trace_id → span 树。
#[derive(Debug, Clone, Default)]
pub struct SpanTree {
    pub trace_id: u64,
    pub root: SpanNode,
}

impl SpanTree {
    /// 总帧数。
    pub fn total_events(&self) -> usize {
        self.root.total_events()
    }
}

/// trace_line 解析（容错：非 frame 行跳过——trace 输出可含其它行）。
///
/// 行形态（frame.rs `trace_line`）：
/// `frame ft=0x10 cid=1 hop=0/8 flags=0x0002 trace_id=1 path="/x" key="bin:t::M" len=1`
pub fn parse_trace_line(line: &str) -> Option<SpanEvent> {
    let line = line.trim();
    if !line.starts_with("frame ") {
        return None;
    }
    let mut fields = BTreeMap::new();
    for kv in line.split_whitespace().skip(1) {
        if let Some((k, v)) = kv.split_once('=') {
            fields.insert(k, v);
        }
    }
    // 仅 TRACING 帧（有 trace_id 字段）参与聚合
    let trace_id: u64 = fields.get("trace_id")?.parse().ok()?;
    let frame_type = u8::from_str_radix(fields.get("ft")?.trim_start_matches("0x"), 16).ok()?;
    let hop_count = fields
        .get("hop")?
        .split('/')
        .next()?
        .parse()
        .ok()?;
    let path = fields.get("path")?.trim_matches('"').to_string();
    let type_key = fields.get("key")?.trim_matches('"').to_string();
    let len: usize = fields.get("len")?.parse().ok()?;
    Some(SpanEvent {
        trace_id,
        frame_type,
        hop_count,
        path,
        type_key,
        len,
        raw: line.to_string(),
    })
}

/// 多行聚合 → trace_id → SpanTree（根节点 path = 最小 hop 首帧路径）。
pub fn aggregate(lines: impl Iterator<Item = String>) -> BTreeMap<u64, SpanTree> {
    let mut trees: BTreeMap<u64, SpanTree> = BTreeMap::new();
    for line in lines {
        if let Some(e) = parse_trace_line(&line) {
            let tree = trees
                .entry(e.trace_id)
                .or_insert_with(|| SpanTree {
                    trace_id: e.trace_id,
                    root: SpanNode::default(),
                });
            // 根 path 空时以首事件 path 立
            if tree.root.path.is_empty() {
                tree.root.path = e.path.clone();
            }
            insert_event(&mut tree.root, e);
        }
    }
    trees
}

fn insert_event(root: &mut SpanNode, e: SpanEvent) {
    // 首帧（最小 hop）挂根；hop+1 挂相邻子——简化：按 hop 分层，
    // 同 hop 并列根 children（path 键）
    if e.path == root.path {
        root.events.push(e);
        return;
    }
    let child = root.children.entry(e.path.clone()).or_insert_with(|| SpanNode {
        path: e.path.clone(),
        ..Default::default()
    });
    child.events.push(e);
}

/// `parrot app trace <app>` 渲染（人类可读树——CLI 数据面）。
pub fn render_tree(tree: &SpanTree) -> String {
    let mut out = String::new();
    out.push_str(&format!("trace {} ({} frames)\n", tree.trace_id, tree.total_events()));
    render_node(&tree.root, &mut out, 0);
    out
}

fn render_node(n: &SpanNode, out: &mut String, depth: usize) {
    let pad = "  ".repeat(depth);
    out.push_str(&format!(
        "{pad}{} ×{} frames\n",
        n.path,
        n.event_count()
    ));
    for c in n.children.values() {
        render_node(c, out, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traced(ft: u8, cid: u64, hop: u8, path: &str, key: &str, len: usize) -> String {
        format!("frame ft=0x{ft:02X} cid={cid} hop={hop}/8 flags=0x0002 trace_id={cid} path=\"{path}\" key=\"{key}\" len={len}")
    }

    #[test]
    fn parse_full_line() {
        let e = parse_trace_line(&traced(0x10, 7, 0, "/frontier/x", "m::Ask", 12)).unwrap();
        assert_eq!(e.trace_id, 7);
        assert_eq!(e.frame_type, 0x10);
        assert_eq!(e.hop_count, 0);
        assert_eq!(e.path, "/frontier/x");
        assert_eq!(e.type_key, "m::Ask");
        assert_eq!(e.len, 12);
    }

    #[test]
    fn parse_non_tracing_rejected() {
        // 无 trace_id 字段（flags 无 TRACING）——不参与聚合
        let line = "frame ft=0x10 cid=1 hop=0/8 flags=0x0000 path=\"/x\" key=\"k\" len=1";
        assert!(parse_trace_line(line).is_none());
    }

    #[test]
    fn parse_non_frame_line_rejected() {
        assert!(parse_trace_line("some other log line").is_none());
        assert!(parse_trace_line("").is_none());
    }

    #[test]
    fn aggregate_groups_by_trace_id() {
        let lines = vec![
            traced(0x10, 1, 0, "/a", "k", 1),
            traced(0x10, 2, 0, "/b", "k", 2),
            traced(0x20, 1, 1, "/c", "k", 3),
        ];
        let trees = aggregate(lines.into_iter());
        assert_eq!(trees.len(), 2);
        assert_eq!(trees[&1].total_events(), 2);
        assert_eq!(trees[&2].total_events(), 1);
    }

    #[test]
    fn tree_children_by_path() {
        let lines = vec![
            traced(0x10, 9, 0, "/src", "k", 1),
            traced(0x10, 9, 1, "/mid", "k", 1),
            traced(0x10, 9, 2, "/dst", "k", 1),
            traced(0x10, 9, 1, "/alt", "k", 1),
        ];
        let trees = aggregate(lines.into_iter());
        let t = &trees[&9];
        assert_eq!(t.root.path, "/src");
        assert_eq!(t.root.event_count(), 1);
        assert_eq!(t.root.children.len(), 3);
        assert!(t.root.children.contains_key("/mid"));
        assert!(t.root.children.contains_key("/dst"));
        assert!(t.root.children.contains_key("/alt"));
        assert_eq!(t.total_events(), 4);
    }

    #[test]
    fn render_readable() {
        let lines = vec![
            traced(0x10, 5, 0, "/src", "k", 1),
            traced(0x10, 5, 1, "/dst", "k", 2),
        ];
        let trees = aggregate(lines.into_iter());
        let s = render_tree(&trees[&5]);
        assert!(s.contains("trace 5 (2 frames)"));
        assert!(s.contains("/src ×1"));
        assert!(s.contains("/dst ×1"));
    }
}
