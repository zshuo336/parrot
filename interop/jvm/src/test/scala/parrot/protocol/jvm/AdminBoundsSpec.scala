package parrot.protocol.jvm

import org.scalatest.funsuite.AnyFunSuite
import AdminV2Codec._

class AdminBoundsSpec extends AnyFunSuite {
  private def hex(s: String): Array[Byte] = s.grouped(2).map(Integer.parseInt(_, 16).toByte).toArray

  for (length <- Seq("fc00000080", "fd0000000001000000", "fdffffffffffffffff")) {
    test(s"command string length rejects unsigned truncation $length") {
      intercept[CodecException] { decodeCommand(hex("030201" + length)) }
    }
    test(s"reply vector count rejects unsigned truncation $length") {
      intercept[CodecException] { decodeReply(hex("040001" + length)) }
      intercept[CodecException] { decodeReply(hex("040301" + length)) }
    }
  }

  test("malformed UTF-8 cannot name a different management target") {
    for (value <- Seq("ff", "c080", "eda080", "f4908080")) {
      val bytes = hex(value)
      intercept[CodecException] { decodeCommand(Array[Byte](3, 2, 1, bytes.length.toByte) ++ bytes) }
      intercept[CodecException] { decodeReply(Array[Byte](4, 4, 1, 1, bytes.length.toByte) ++ bytes) }
    }
  }

  test("outbound malformed Unicode is rejected rather than rewritten") {
    intercept[CodecException] { encodeCommand(AdminCommandV2.Stop(1, "\ud800")) }
    intercept[CodecException] { encodeReply(AdminReplyV2.Failed(1, 1, "\udfff")) }
  }

  test("Failed.code is u16 in both directions") {
    for (code <- Seq(-1, 65536, Int.MaxValue))
      intercept[CodecException] { encodeReply(AdminReplyV2.Failed(1, code, "")) }
    for (encoded <- Seq("fc00000100", "fd0100000001000000", "fdffffffffffffffff"))
      intercept[CodecException] { decodeReply(hex("040401" + encoded + "00")) }
    val max = AdminReplyV2.Failed(1, 65535, "valid")
    assert(decodeReply(encodeReply(max)) == max)
  }

  test("Dylib.abi is u32 in both directions") {
    def command(abi: Long) = AdminCommandV2.Deploy(1,
      ComponentDeploy("", "", ArtifactRef.Dylib("", "", abi), InstancePolicy.Singleton, None))
    for (abi <- Seq(-1L, 0x100000000L))
      intercept[CodecException] { encodeCommand(command(abi)) }
    for (encoded <- Seq("fd0000000001000000", "fdffffffffffffffff"))
      intercept[CodecException] { decodeCommand(hex("0300010000050000" + encoded + "0000")) }
    assert(decodeCommand(encodeCommand(command(0xffffffffL))) == command(0xffffffffL))
  }

  test("u64 identifiers and counters preserve all 64 bits") {
    val request = AdminCommandV2.Drain(-1L, "", Long.MinValue)
    val expected = hex("0301fdffffffffffffffff00fd0000000000000080")
    assert(encodeCommand(request).sameElements(expected))
    assert(decodeCommand(expected) == request)
    val reply = AdminReplyV2.Drained(Long.MinValue, -1L, Long.MaxValue)
    assert(decodeReply(encodeReply(reply)) == reply)
  }

  test("all independent frozen vectors reject every truncation and trailing byte") {
    val source = scala.util.Using.resource(scala.io.Source.fromFile("../../docs/vectors/admin_v2.json"))(_.mkString)
    val pattern = "\"payload_hex\"\\s*:\\s*\"([0-9a-f]+)\"".r
    val vectors = pattern.findAllMatchIn(source).map(m => hex(m.group(1))).toVector
    assert(vectors.length == 6)
    for (bytes <- vectors) {
      def decode(value: Array[Byte]): Any =
        if (bytes(0) == 3) decodeCommand(value) else decodeReply(value)
      for (end <- bytes.indices) intercept[CodecException] { decode(bytes.take(end)) }
      intercept[CodecException] { decode(bytes :+ 0.toByte) }
      val encoded = if (bytes(0) == 3) encodeCommand(decodeCommand(bytes)) else encodeReply(decodeReply(bytes))
      assert(encoded.sameElements(bytes))
    }
  }

  test("typed varint fields reject encodings wider than their Rust integer type") {
    // A u16 cannot use the u32 prefix even when its value would fit.
    intercept[CodecException] { decodeReply(hex("040401fc0100000000")) }
    // The command discriminant is u32, not u64.
    intercept[CodecException] { decodeCommand(hex("03fd02000000000000000100")) }
  }

  test("optional config rejects invalid tags oversized lengths and all truncations") {
    val command = AdminCommandV2.Deploy(1, ComponentDeploy("a", "v", ArtifactRef.Props("factory"),
      InstancePolicy.Singleton, Some(Array[Byte](1, 2, 3))))
    val bytes = encodeCommand(command)
    for (end <- bytes.indices) intercept[CodecException] { decodeCommand(bytes.take(end)) }
    val invalidTag = bytes.clone()
    invalidTag(invalidTag.length - 5) = 2
    intercept[CodecException] { decodeCommand(invalidTag) }
    val hugeConfig = bytes.dropRight(4) ++ hex("fd0300000001000000010203")
    intercept[CodecException] { decodeCommand(hugeConfig) }
  }

}
