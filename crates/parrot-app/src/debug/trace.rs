//! F1（DEV_09 §3.6）：trace span 树聚合。
//!
//! 数据源：PARROT_TRACE=frame 输出的 trace_line（TX/RX 每帧一行）。
//! 本模块：逐行解析 → 按 trace_id（TRACING 位帧 cid）聚合 →
//! 按 path 分组、选择最小 hop 路径为根。当前帧不含 parent span 或连接命名空间，
//! 此投影不能证明跨路径因果关系，也不能安全合并 cid 复用的独立会话。

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
    /// 其它路径分组；当前格式不能证明它们的因果父节点。
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
            + self
                .children
                .values()
                .map(SpanNode::total_events)
                .sum::<usize>()
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
    let raw = line.trim();
    // Accept only the envelopes emitted by the transport, not an arbitrary
    // embedded "frame" substring in unrelated application output.
    let body = raw
        .strip_prefix("[parrot-trace] TX ")
        .or_else(|| raw.strip_prefix("[parrot-trace] RX "))
        .unwrap_or(raw)
        .strip_prefix("frame ")?;
    let fields = trace_fields(body)?;
    let trace_id: u64 = decimal(fields.get("trace_id")?)?;
    let cid: u64 = decimal(fields.get("cid")?)?;
    let flags = u16::from_str_radix(hexadecimal(fields.get("flags")?, 4)?, 16).ok()?;
    if flags & parrot_remote::frame::flags::TRACING == 0 || trace_id != cid {
        return None;
    }
    let frame_type = u8::from_str_radix(hexadecimal(fields.get("ft")?, 2)?, 16).ok()?;
    let (count, limit) = fields.get("hop")?.split_once('/')?;
    let hop_count = decimal(count)?;
    let _: u8 = decimal(limit)?;
    // This is a diagnostic parser, not wire admission: even an exhausted hop
    // limit can be useful in a log. Both numbers must still be well formed.
    let path = debug_string(fields.get("path")?)?;
    let type_key = debug_string(fields.get("key")?)?;
    let len = decimal(fields.get("len")?)?;
    Some(SpanEvent {
        trace_id,
        frame_type,
        hop_count,
        path,
        type_key,
        len,
        raw: raw.to_string(),
    })
}

fn decimal<T: std::str::FromStr>(value: &str) -> Option<T> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn hexadecimal(value: &str, max_digits: usize) -> Option<&str> {
    let value = value.strip_prefix("0x")?;
    (!value.is_empty() && value.len() <= max_digits && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(value)
}

/// Split fields without interpreting whitespace or '=' inside Debug strings.
/// Reject duplicates instead of silently replacing correlation metadata.
fn trace_fields(mut input: &str) -> Option<BTreeMap<&str, &str>> {
    let mut fields = BTreeMap::new();
    while !input.is_empty() {
        let (key, rest) = input.split_once('=')?;
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        let end = if rest.starts_with('"') {
            let mut escaped = false;
            let mut closing = None;
            for (i, ch) in rest.char_indices().skip(1) {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    closing = Some(i + 1);
                    break;
                }
            }
            closing?
        } else {
            rest.find(char::is_whitespace).unwrap_or(rest.len())
        };
        let (value, tail) = rest.split_at(end);
        if value.is_empty()
            || (!tail.is_empty() && !tail.starts_with(char::is_whitespace))
            || fields.insert(key, value).is_some()
        {
            return None;
        }
        input = tail.trim_start();
    }
    Some(fields)
}

/// Rust Debug string syntax differs from JSON (notably \0 and \u{...}).
fn debug_string(value: &str) -> Option<String> {
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = value.chars();
    let mut decoded = String::with_capacity(value.len());
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => decoded.push(match chars.next()? {
                '\\' => '\\',
                '"' => '"',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '0' => '\0',
                'u' => {
                    if chars.next()? != '{' {
                        return None;
                    }
                    let mut scalar = 0;
                    let mut digits = 0;
                    loop {
                        let ch = chars.next()?;
                        if ch == '}' {
                            break;
                        }
                        if !ch.is_ascii_hexdigit() || digits == 6 {
                            return None;
                        }
                        scalar = scalar * 16 + ch.to_digit(16)?;
                        digits += 1;
                    }
                    if digits == 0 {
                        return None;
                    }
                    char::from_u32(scalar)?
                }
                _ => return None,
            }),
            '"' => return None,
            ch if ch.is_control() => return None,
            ch => decoded.push(ch),
        }
    }
    Some(decoded)
}

/// Group by trace_id and path, selecting the lowest-hop path as the root.
/// Equal-hop ties use path order, so log arrival order cannot change the root.
/// Children are path groups, not inferred parent/child spans: the current
/// frame format contains no parent span identifier or connection namespace.
/// Keep each observation (including TX/RX) in arrival order within its path.
/// Callers must supply a correlation scope where cids are not reused.
pub fn aggregate(lines: impl Iterator<Item = String>) -> BTreeMap<u64, SpanTree> {
    let mut groups: BTreeMap<u64, BTreeMap<String, SpanNode>> = BTreeMap::new();
    for line in lines {
        if let Some(event) = parse_trace_line(&line) {
            groups
                .entry(event.trace_id)
                .or_default()
                .entry(event.path.clone())
                .or_insert_with(|| SpanNode {
                    path: event.path.clone(),
                    ..Default::default()
                })
                .events
                .push(event);
        }
    }
    groups
        .into_iter()
        .map(|(trace_id, mut paths)| {
            // Every group has at least one event; include path in the key to
            // make ties independent of input order, including the empty path.
            let root_path = paths
                .values()
                .flat_map(|node| &node.events)
                .min_by_key(|event| (event.hop_count, &event.path))
                .expect("nonempty trace group")
                .path
                .clone();
            let mut root = paths.remove(&root_path).expect("root path exists");
            root.children = paths;
            (trace_id, SpanTree { trace_id, root })
        })
        .collect()
}

/// 人类可读路径投影；路径用 Debug 转义，防止控制字符改变终端输出。
pub fn render_tree(tree: &SpanTree) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "trace {} ({} frames)\n",
        tree.trace_id,
        tree.total_events()
    ));
    render_node(&tree.root, &mut out, 0);
    out
}

fn render_node(n: &SpanNode, out: &mut String, depth: usize) {
    let pad = "  ".repeat(depth);
    out.push_str(&format!(
        "{pad}{} ×{} frames\n",
        n.path.escape_debug(),
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
