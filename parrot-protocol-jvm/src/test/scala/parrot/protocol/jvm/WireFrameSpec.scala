package parrot.protocol.jvm

import org.scalatest.funsuite.AnyFunSuite
import scala.io.Source
import scala.util.Using

/** golden vectors 逐字节断言（读 docs/vectors/wire1.json——Rust 冻结件）。 */
class WireFrameSpec extends AnyFunSuite {

  case class Vector(name: String, frameType: String, path: String, typeKey: String, payloadHex: String, bytesHex: String)

  private def loadVectors(): List[Vector] = {
    // 多候选路径：模块根 ../.. / 环境变量 / 系统属性（surefire cwd 差异兜底）
    val candidates = List(
      sys.env.get("PARROT_REPO").map(_ + "/docs/vectors/wire1.json"),
      sys.props.get("parrot.repo").map(_ + "/docs/vectors/wire1.json"),
      Some("../..").map(_ + "/docs/vectors/wire1.json"),
      Some("../../docs/vectors/wire1.json"),
      Some("docs/vectors/wire1.json"),
      Some("/Users/biluochun/work/error.d/library/parrot/docs/vectors/wire1.json")
    ).flatten
    val path = candidates.find(p => new java.io.File(p).exists())
      .getOrElse(fail(s"wire1.json not found in any of: ${candidates.mkString(", ")}"))
    val json = Using.resource(Source.fromFile(path))(_.mkString)
    // 最小 JSON 解析（无外部依赖、无正则）：按 '{' 切块，块内按名取字段（不依赖顺序）
    def field(obj: String, key: String): Option[String] = {
      val k = "\"" + key + "\":"
      val i = obj.indexOf(k)
      if (i < 0) return None
      var p = i + k.length
      while (p < obj.length && obj(p) != '"') p += 1
      if (p >= obj.length) return None
      val sb = new StringBuilder
      p += 1
      var esc = false
      var done = false
      while (p < obj.length && !done) {
        val c = obj(p)
        if (esc) { sb += c; esc = false }
        else if (c == '\\') esc = true
        else if (c == '"') done = true
        else sb += c
        p += 1
      }
      Some(sb.toString)
    }
    json.split('{').flatMap { chunk =>
      if (!chunk.contains("\"bytes_hex\"")) None
      else
        for {
          name <- field(chunk, "name")
          ft   <- field(chunk, "frame_type")
          p    <- field(chunk, "path")
          tk   <- field(chunk, "type_key")
          ph   <- field(chunk, "payload_hex")
          bh   <- field(chunk, "bytes_hex")
        } yield Vector(name, ft, p, tk, ph, bh)
    }.toList
  }

  test("golden vectors load and are frozen set of 4") {
    val vs = loadVectors()
    assert(vs.size == 4, s"expected 4 frozen vectors, got ${vs.size}: ${vs.map(_.name)}")
    assert(vs.map(_.name).toSet == Set("ask-basic", "tell-basic", "reply-err-stopped", "heartbeat"))
  }

  test("ask-basic byte-level identity with Rust frozen vector") {
    val v = loadVectors().find(_.name == "ask-basic").getOrElse(fail("ask-basic missing"))
    val expect = hex(v.bytesHex)
    // Rust 侧 Frame::ask(1, "/x", "bin:t::M", [0xAB], None)
    // payload = [u32 0][0xAB]（reply_to 前缀）
    val f = WireFrame.Frame(
      1, WireFrame.FrameType.ASK.toByte, 0, 1L, 0, 8,
      "/x", "bin:t::M", hex(v.payloadHex)
    )
    val got = f.encode()
    assert(got.sameElements(expect), s"ask-basic mismatch:\n got  ${hexOf(got)}\n want ${v.bytesHex}")
  }

  test("tell-basic byte-level identity") {
    val v = loadVectors().find(_.name == "tell-basic").getOrElse(fail("tell-basic missing"))
    val f = WireFrame.Frame(
      1, WireFrame.FrameType.TELL.toByte, 0, 0, 0, 8,
      v.path, v.typeKey, hex(v.payloadHex)
    )
    assert(f.encode().sameElements(hex(v.bytesHex)), "tell-basic mismatch")
  }

  test("heartbeat byte-level identity") {
    val v = loadVectors().find(_.name == "heartbeat").getOrElse(fail("heartbeat missing"))
    val f = WireFrame.Frame(1, WireFrame.FrameType.HEARTBEAT.toByte, 0, 0, 0, 8, "", "", Array.emptyByteArray)
    assert(f.encode().sameElements(hex(v.bytesHex)), "heartbeat mismatch")
  }

  test("decode half-frame does not consume (parity with Rust Frame::decode)") {
    val v = loadVectors().find(_.name == "tell-basic").get
    val full = hex(v.bytesHex)
    // 逐字节前缀：半包返回 None
    (1 until full.length).foreach { n =>
      val prefix = java.util.Arrays.copyOf(full, n)
      assert(WireFrame.decode(prefix).isEmpty, s"prefix len=$n must be undecodable")
    }
    val (f, consumed) = WireFrame.decode(full).get
    assert(consumed == full.length)
    assert(f.path == v.path)
  }

  test("err payload roundtrip parity") {
    val enc = WireFrame.encodeErrPayload(WireFrame.ErrCode.Stopped, "actor stopped")
    val (code, detail) = WireFrame.decodeErrPayload(enc)
    assert(code == WireFrame.ErrCode.Stopped)
    assert(detail == "actor stopped")
  }

  test("handshake tlv parity") {
    val body = WireFrame.handshakeBody("jvm-gw-1")
    val tlvs = WireFrame.parseTlv(body)
    // 必含 5 项：node_id/capabilities/max_frame_len/topology_role/hop_limit
    assert(tlvs.size == 5, s"handshake body must carry 5 TLVs, got ${tlvs.size}")
    val nodeIds = tlvs.collect { case (t, v) if t == WireFrame.TlvTag.NODE_ID => v }
    assert(nodeIds.nonEmpty && nodeIds.head.sameElements("jvm-gw-1".getBytes("UTF-8")))
    // capabilities = bin 位（0x01）
    val caps = tlvs.collectFirst { case (t, v) if t == WireFrame.TlvTag.CAPABILITIES => v }
    assert(caps.exists(_.sameElements(Array(1.toByte, 0.toByte, 0.toByte, 0.toByte))))
    // ACK 专有 chosen_codec=bin（tag 8）
    val ack = WireFrame.handshakeAckBody("jvm-gw-1")
    val ackTlvs = WireFrame.parseTlv(ack)
    assert(ackTlvs.size == 6)
    val cc = ackTlvs.collectFirst { case (t, v) if t == WireFrame.TlvTag.CHOSEN_CODEC => v }
    assert(cc.exists(_.sameElements("bin".getBytes("UTF-8"))))
  }

  private def hex(s: String): Array[Byte] =
    s.sliding(2, 2).map(Integer.parseInt(_, 16).toByte).toArray

  private def hexOf(b: Array[Byte]): String =
    b.map("%02x".format(_)).mkString
}
