package parrot.protocol.jvm

import org.scalatest.funsuite.AnyFunSuite
import AdminV2Codec._

class MeasuredSpec extends AnyFunSuite {
  private def hex(s: String): Array[Byte] = s.grouped(2).map(Integer.parseInt(_, 16).toByte).toArray
  private val vectors = scala.util.Using.resource(scala.io.Source.fromFile("../../docs/vectors/measured_metrics.json")) { in =>
    "\"payload_hex\"\\s*:\\s*\"([0-9a-f]+)\"".r.findAllMatchIn(in.mkString).map(m => hex(m.group(1))).toVector
  }
  test("independent Rust measured request is byte-exact") {
    assert(vectors.length == 2)
    val cmd = decodeCommand(vectors.head)
    assert(cmd.reqId == 42)
    assert(encodeCommand(cmd).sameElements(vectors.head))
  }
  test("independent Rust measured known-zero reply is byte-exact") {
    val reply = decodeReply(vectors(1))
    assert(reply.reqId == 42)
    assert(encodeReply(reply).sameElements(vectors(1)))
  }

  test("all measured vector truncations and trailing bytes are rejected") {
    for (bytes <- vectors) {
      def decode(value: Array[Byte]): Any = if (bytes(0) == 3) decodeCommand(value) else decodeReply(value)
      for (end <- bytes.indices) intercept[CodecException] { decode(bytes.take(end)) }
      intercept[CodecException] { decode(bytes :+ 0.toByte) }
    }
  }

  test("measured metadata rejects unknown masks missing units and oversized identities") {
    val valid = decodeReply(vectors(1)).asInstanceOf[AdminReplyV2.Measured].metrics
    for (invalid <- Seq(valid.copy(available = 1L << 17), valid.copy(available = 1L << 14),
      valid.copy(epoch = "界" * 43), valid.copy(processKind = "x" * 129), valid.copy(processKind = "\ud800")))
      intercept[CodecException] { encodeReply(AdminReplyV2.Measured(42, invalid)) }
    val raw = vectors(1)
    val at = raw.length - 10 // frozen available=32 + epoch length=7 + owner-a + empty unit
    assert(raw(at) == 32)
    for (mask <- Seq("fc00000200", "fb0040"))
      intercept[CodecException] { decodeReply(raw.take(at) ++ hex(mask) ++ raw.drop(at + 1)) }
    intercept[CodecException] { decodeReply(raw.take(at + 1) ++ Array[Byte](129.toByte) ++ Array.fill[Byte](129)(97) ++ Array[Byte](0)) }
  }

  test("RSS sampling is bounded closes streams and distinguishes real zero from unavailable") {
    def check(content: String, expected: Option[Long]): Unit = {
      var closed = false
      val input = new java.io.ByteArrayInputStream(content.getBytes("US-ASCII")) {
        override def close(): Unit = { closed = true; super.close() }
      }
      assert(GatewayMetrics.residentBytes(() => input) == expected)
      assert(closed)
    }
    check("Name:\ttest\nVmRSS:\t0 kB\n", Some(0))
    check("VmRSS: 12 kB\n", Some(12288))
    check("Name: test\n", None)
    check("VmRSS: -1 kB\n", None)
    check("VmRSS: 12 MB\n", None)
    check("VmRSS: 9223372036854775807 kB\n", None)
    check("VmRSS: 1 kB\n" + ("x" * 65536), None)
    assert(GatewayMetrics.residentBytes(() => throw new java.io.IOException("missing")) == None)
    var closed = false
    val failing = new java.io.InputStream {
      override def read(): Int = throw new java.io.IOException("injected read failure")
      override def close(): Unit = closed = true
    }
    assert(GatewayMetrics.residentBytes(() => failing) == None)
    assert(closed)
  }

  test("process sampling has stable epoch explicit units and independent component count") {
    val states = Vector(ComponentState("/a", "running", "1"), ComponentState("/b", "running", "1"))
    val first = GatewayMetrics.measured(1, states)
    val second = GatewayMetrics.measured(1, states)
    assert(first.epoch == second.epoch && first.epoch == GatewayMetrics.epoch)
    java.util.UUID.fromString(first.epoch)
    assert(first.processKind == "jvm_threads" && first.snapshot.processes > 0)
    assert((first.available & (1L << 14)) != 0)
    assert(first.snapshot.components == 1 && first.snapshot.componentStates.size == 2)
    assert(first.snapshot.uptimeStartMs == second.snapshot.uptimeStartMs)
    assert((first.available & ~0x1ffffL) == 0)
    assert(encodeReply(AdminReplyV2.Measured(1, first)).sameElements(
      encodeReply(decodeReply(encodeReply(AdminReplyV2.Measured(1, first))))))
  }

  test("generic handshake helpers do not advertise a management port they do not own") {
    val ordinary = WireFrame.decodeHandshake(WireFrame.handshakeBody("client"), acknowledgement = false)
    assert((ordinary.capabilities & WireFrame.CAPS_MEASURED) == 0)
  }

}
