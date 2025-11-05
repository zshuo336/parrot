package parrot.protocol.jvm

import org.scalatest.funsuite.AnyFunSuite
import scala.io.Source
import scala.util.Using

/** golden vectors 逐字节断言（读 docs/vectors/wire1.json——Rust 冻结件）。 */
class WireFrameSpec extends AnyFunSuite {

  test("shared error payload corpus") {
    val path = java.nio.file.Paths.get(sys.props.getOrElse("parrot.repo", "../.."), "docs/vectors/error-payloads.tsv")
    val rows = Using.resource(Source.fromFile(path.toFile))(_.getLines().filterNot(_.startsWith("#")).toVector)
    assert(rows.size == 30)
    def bytes(hex: String): Array[Byte] = hex.grouped(2).map(Integer.parseInt(_, 16).toByte).toArray
    rows.foreach { row =>
      val fields = row.split("\t", -1)
      assert(fields.length == 5)
      val payload = bytes(fields(2))
      if (fields(1) == "reject") intercept[IllegalArgumentException] { WireFrame.decodeErrPayload(payload) }
      else {
        assert(fields(1) == "accept")
        val (code, detail) = WireFrame.decodeErrPayload(payload)
        assert(code == fields(3).toInt, fields(0))
        assert(detail.getBytes(java.nio.charset.StandardCharsets.UTF_8).sameElements(bytes(fields(4))), fields(0))
        assert(WireFrame.encodeErrPayload(code, detail).sameElements(payload), fields(0))
      }
    }
  }

  test("error encoder rejects unknown codes and invalid Unicode") {
    Seq(0, 14, -1, 65536).foreach(code => intercept[IllegalArgumentException] { WireFrame.encodeErrPayload(code, "error") })
    intercept[IllegalArgumentException] { WireFrame.encodeErrPayload(3, "\ud800") }
  }

  private def loadVectors(): List[SharedWireVectors.Vector] = SharedWireVectors.load()

  test("shared golden vectors retain the original four names") {
    val vs = loadVectors()
    assert(vs.size >= 4, s"missing frozen vectors: ${vs.map(_.name)}")
    assert(Set("ask-basic", "tell-basic", "reply-err-stopped", "heartbeat").subsetOf(vs.map(_.name).toSet))
  }

  test("every shared vector encodes and decodes all independently stated fields") {
    val vectors = loadVectors()
    vectors.foreach { v =>
      withClue(v.name + ": ") {
        val expected = new WireFrame.Frame(1, v.frameType, v.flags, v.correlationId,
          v.hopCount, v.hopLimit, v.path, v.typeKey, hex(v.payloadHex), null, v.sequence)
        val bytes = hex(v.bytesHex)
        assert(expected.encode().sameElements(bytes), "independent field encoding differs from shared bytes")
        val decoded = WireFrame.decode(bytes)
        assert(decoded.nonEmpty, "shared complete frame must decode")
        val (actual, consumed) = decoded.get
        assert(consumed == bytes.length, "shared vector has trailing bytes")
        assert(actual == expected, "decoded fields differ from independent expectations")
      }
    }
    println(s"JVM_SHARED_WIRE_JSON_OK vectors=${vectors.size}")
  }

  test("every shared vector split preserves complete header and following frame") {
    loadVectors().foreach { vector =>
      val bytes = hex(vector.bytesHex)
      val expected = new WireFrame.Frame(1, vector.frameType, vector.flags, vector.correlationId,
        vector.hopCount, vector.hopLimit, vector.path, vector.typeKey, hex(vector.payloadHex), null, vector.sequence)
      (0 until bytes.length).foreach { cut =>
        withClue(s"${vector.name} cut=$cut: ") {
          val prefix = bytes.take(cut)
          val preserved = prefix.clone()
          assert(WireFrame.decode(prefix).isEmpty)
          assert(prefix.sameElements(preserved), "partial input mutated")
          val combined = prefix ++ bytes.drop(cut) ++ bytes
          val (actual, consumed) = WireFrame.decode(combined).get
          assert(consumed == bytes.length && actual == expected)
          assert(combined.drop(consumed).sameElements(bytes), "following frame changed")
        }
      }
    }
  }

  test("shared vector pipeline preserves order and all independently stated fields") {
    val vectors = loadVectors()
    var remaining = vectors.flatMap(v => hex(v.bytesHex)).toArray
    vectors.foreach { vector =>
      val expected = new WireFrame.Frame(1, vector.frameType, vector.flags, vector.correlationId,
        vector.hopCount, vector.hopLimit, vector.path, vector.typeKey, hex(vector.payloadHex), null, vector.sequence)
      val (actual, consumed) = WireFrame.decode(remaining).get
      assert(actual == expected, vector.name)
      assert(consumed == hex(vector.bytesHex).length, vector.name)
      remaining = remaining.drop(consumed)
    }
    assert(remaining.isEmpty)
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
    // ACK 专有 chosen_codec=bin（tag 9；tag 8 为 direct_addr）
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
