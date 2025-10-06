//! dump-vectors：GOLDEN_VECTORS 导出 JSON（stdout）——冻结件
//! `docs/vectors/wire1.json` 的事实源（只增不改；DEV_00 §6 断言用）。
//! B1（DEV_09）：追加 admin-v2 vectors（--admin-v2 开关——wire1.json 主体
//! 保持 wire 1.0 四向量不变；admin-v2 冻结件独立文档
//! `docs/vectors/admin_v2.json` 由 MG11 四方言共享）。

use parrot_remote::golden_vectors;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    if std::env::args().any(|a| a == "--admin-v2") {
        let mut items = Vec::new();
        for v in parrot_remote::admin_v2::admin_v2_vectors() {
            items.push(serde_json::json!({
                "name": v.name,
                "payload_hex": hex(&v.bytes),
            }));
        }
        let doc = serde_json::json!({
            "format": "parrot-admin-v2",
            "frozen": true,
            "note": "只增不改：修改 = 协议 break。payload = [u8 tag][bincode standard(body)]；tag 0x03=CMD_V2 0x04=REPLY_V2",
            "vectors": items,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap());
        return;
    }
    let mut items = Vec::new();
    for v in golden_vectors() {
        items.push(serde_json::json!({
            "name": v.name,
            "frame_type": format!("0x{:02X}", v.frame.header.frame_type),
            "correlation_id": v.frame.header.correlation_id,
            "hop_count": v.frame.header.hop_count,
            "hop_limit": v.frame.header.hop_limit,
            "path": v.frame.path,
            "type_key": v.frame.type_key,
            "payload_hex": hex(&v.frame.payload),
            "bytes_hex": hex(v.bytes),
        }));
    }
    let doc = serde_json::json!({
        "format": "parrot-wire-1.0",
        "frozen": true,
        "note": "只增不改：修改 = 协议 break = 走 version 协商（07 X2）",
        "vectors": items,
    });
    println!("{}", serde_json::to_string_pretty(&doc).unwrap());
}
