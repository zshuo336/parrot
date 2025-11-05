package parrot.protocol.jvm

import com.fasterxml.jackson.databind.ObjectMapper
import com.fasterxml.jackson.databind.node.ObjectNode
import java.math.BigInteger
import java.nio.charset.StandardCharsets.UTF_8
import java.nio.file.{Files, Path}
import java.util.Comparator
import org.scalatest.funsuite.AnyFunSuite
import scala.jdk.CollectionConverters._

class SharedWireVectorsSpec extends AnyFunSuite {
  private val mapper = new ObjectMapper()
  private val base = """{"format":"parrot-wire-1.0","frozen":true,"vectors":[{"name":"data-only","frame_type":"0x11","correlation_id":1,"path":"/x","type_key":"k","payload_hex":"00ff","bytes_hex":"00"}]}"""

  private def document(edit: ObjectNode => Unit): String = {
    val root = mapper.readTree(base).asInstanceOf[ObjectNode]
    edit(root)
    mapper.writeValueAsString(root)
  }

  private def row(edit: ObjectNode => Unit): String = document { root =>
    edit(root.get("vectors").get(0).asInstanceOf[ObjectNode])
  }

  private def rejected(json: String): Unit = {
    intercept[IllegalArgumentException] { SharedWireVectors.parse(json) }
  }

  private def temporary(check: Path => Unit): Unit = {
    val path = Files.createTempDirectory("parrot-shared-vector-")
    try check(path)
    finally {
      val files = Files.walk(path)
      try files.sorted(Comparator.reverseOrder[Path]()).iterator().asScala.foreach(p => Files.delete(p))
      finally files.close()
    }
  }

  private def fixture(root: Path): Path = {
    val path = root.resolve("docs/vectors/wire1.json")
    Files.createDirectories(path.getParent)
    Files.writeString(path, base, UTF_8)
    path.toAbsolutePath.normalize()
  }

  test("actual shared document retains original names and every row") {
    val vectors = SharedWireVectors.load()
    assert(vectors.size >= 4)
    assert(Set("ask-basic", "tell-basic", "reply-err-stopped", "heartbeat").subsetOf(vectors.map(_.name).toSet))
    assert(vectors.map(_.name).distinct.size == vectors.size)
  }

  test("parser handles valid JSON whitespace field order Unicode and escaping") {
    val json = document { root =>
      root.get("vectors").get(0).asInstanceOf[ObjectNode].put("path", "\ufeff\u96ea\"\\\n{path}")
    }
    val pretty = mapper.writerWithDefaultPrettyPrinter().writeValueAsString(mapper.readTree(json))
    val vector = SharedWireVectors.parse(pretty).head
    assert(vector.path == "\ufeff\u96ea\"\\\n{path}")
    assert(vector.payloadHex == "00ff")
  }

  test("unsigned u64 and all header maxima retain exact bit patterns") {
    val json = row { node =>
      node.put("correlation_id", new BigInteger("18446744073709551615"))
      node.put("flags", 65535).put("hop_count", 128).put("hop_limit", 255).put("sequence", 4294967295L)
    }
    val vector = SharedWireVectors.parse(json).head
    assert(vector.correlationId == -1L)
    assert(vector.flags == 65535)
    assert((vector.hopCount & 255) == 128 && (vector.hopLimit & 255) == 255)
    assert(vector.sequence == 4294967295L)
  }

  test("wrong document format or nonboolean frozen marker fails") {
    rejected(document(_.put("format", "other")))
    rejected(document(_.put("frozen", false)))
    rejected(document(_.put("frozen", 1)))
    rejected(document(_.putNull("frozen")))
  }

  test("empty nonarray or malformed vector rows fail") {
    rejected(document(_.putArray("vectors")))
    rejected(document(_.putNull("vectors")))
    rejected(document(_.put("vectors", "[]")))
    rejected(document(_.putArray("vectors").addNull()))
    rejected(row(_.put("name", "")))
  }

  test("duplicate object keys and duplicate vector names fail") {
    rejected(base.replace("\"frozen\":true", "\"frozen\":true,\"frozen\":true"))
    rejected(base.replace("\"name\":\"data-only\"", "\"name\":\"data-only\",\"name\":\"data-only\""))
    rejected(document { root => root.withArray("vectors").add(root.get("vectors").get(0).deepCopy[ObjectNode]()) })
  }

  test("invalid JSON nonobject documents nonstandard numbers and trailing JSON fail") {
    Seq("{", "null", "[]", "", base + "{}", base.replace("true", "NaN"), base.replace("true", "Infinity"))
      .foreach(rejected)
  }

  test("missing required fields and wrong string types fail") {
    Seq("name", "frame_type", "correlation_id", "path", "type_key", "payload_hex", "bytes_hex").foreach { key =>
      rejected(row(_.remove(key)))
    }
    Seq("name", "path", "type_key", "payload_hex", "bytes_hex").foreach { key =>
      rejected(row(_.put(key, 7)))
      rejected(row(_.putNull(key)))
    }
  }

  test("numeric fields reject coercion negative values and overflow") {
    Seq("correlation_id" -> 64, "flags" -> 16, "hop_count" -> 8, "hop_limit" -> 8, "sequence" -> 32).foreach {
      case (key, bits) =>
        rejected(row(_.put(key, -1)))
        rejected(row(_.put(key, true)))
        rejected(row(_.put(key, "1")))
        rejected(row(_.put(key, 1.0)))
        rejected(row(_.put(key, (BigInt(1) << bits).bigInteger)))
    }
  }

  test("hex and frame type reject truncation whitespace and nonhex characters") {
    Seq("bytes_hex", "payload_hex").foreach { key =>
      Seq("0", "gg", "00 01", "00\n01").foreach(value => rejected(row(_.put(key, value))))
    }
    Seq("0x+1", "0x 1", "0x-0", "0X10", "0x1", "0x100", "0xgg").foreach { value =>
      rejected(row(_.put("frame_type", value)))
    }
    assert(SharedWireVectors.parse(row(_.put("frame_type", "0xAf"))).head.frameType == 175.toByte)
  }

  test("identities reject NUL and unpaired Unicode surrogates") {
    Seq("name", "path", "type_key").foreach { key =>
      Seq("left\u0000right", "\ud800", "\udfff").foreach(value => rejected(row(_.put(key, value))))
    }
  }

  test("appended distinct rows are parsed rather than frozen to four entries") {
    val json = document { root =>
      val extra = root.get("vectors").get(0).deepCopy[ObjectNode]()
      extra.put("name", "second-data-row").put("correlation_id", 99)
      root.withArray("vectors").add(extra)
    }
    val vectors = SharedWireVectors.parse(json)
    assert(vectors.map(_.name) == List("data-only", "second-data-row"))
    assert(vectors.last.correlationId == 99)
  }

  test("explicit missing root never falls back to a valid local or environment fixture") {
    temporary { cwd =>
      fixture(cwd)
      intercept[IllegalArgumentException] { SharedWireVectors.resolve(Some(cwd.resolve("missing").toString), Some(cwd.toString), cwd) }
      intercept[IllegalArgumentException] { SharedWireVectors.resolve(None, Some(cwd.resolve("missing").toString), cwd) }
      intercept[IllegalArgumentException] { SharedWireVectors.resolve(Some(""), Some(cwd.toString), cwd) }
    }
  }

  test("explicit property precedes environment with no personal absolute fallback") {
    temporary { cwd =>
      val property = fixture(cwd.resolve("property"))
      fixture(cwd.resolve("environment"))
      assert(SharedWireVectors.resolve(Some(cwd.resolve("property").toString), Some(cwd.resolve("environment").toString), cwd) == property)
      intercept[IllegalArgumentException] { SharedWireVectors.resolve(None, None, cwd.resolve("unrelated/deep/module")) }
    }
  }

  test("default discovery supports repository and Maven module working directories") {
    temporary { repo =>
      val expected = fixture(repo)
      val module = Files.createDirectories(repo.resolve("interop/jvm"))
      assert(SharedWireVectors.resolve(None, None, repo) == expected)
      assert(SharedWireVectors.resolve(None, None, module) == expected)
    }
  }
}
