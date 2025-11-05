package parrot.protocol.jvm

import com.fasterxml.jackson.core.{JsonParser, JsonProcessingException}
import com.fasterxml.jackson.databind.{DeserializationFeature, JsonNode, ObjectMapper}
import java.nio.CharBuffer
import java.nio.charset.{CharacterCodingException, CodingErrorAction, StandardCharsets}
import java.nio.file.{Files, Path, Paths}
import scala.jdk.CollectionConverters._

/** Test data only. Encoding and decoding remain in the actual Wire implementation. */
private[jvm] object SharedWireVectors {
  final case class Vector(name: String, frameType: Byte, correlationId: Long, flags: Int,
      hopCount: Byte, hopLimit: Byte, sequence: Long, path: String, typeKey: String,
      payloadHex: String, bytesHex: String)

  private val mapper = new ObjectMapper()
    .enable(JsonParser.Feature.STRICT_DUPLICATE_DETECTION)
    .enable(DeserializationFeature.FAIL_ON_TRAILING_TOKENS)
  private val relative = Paths.get("docs", "vectors", "wire1.json")

  def resolve(property: Option[String], environment: Option[String], cwd: Path): Path = {
    val configured = property.orElse(environment)
    configured match {
      case Some(root) =>
        require(root.nonEmpty, "empty configured shared fixture repository")
        val path = cwd.resolve(Paths.get(root)).resolve(relative).toAbsolutePath.normalize()
        require(Files.isRegularFile(path), s"configured shared fixture is missing: $path")
        path
      case None =>
        Seq(cwd.resolve(relative), cwd.resolve("../..").resolve(relative))
          .map(_.toAbsolutePath.normalize()).distinct.find(Files.isRegularFile(_))
          .getOrElse(throw new IllegalArgumentException(s"shared wire fixture not found from $cwd"))
    }
  }

  def load(): List[Vector] = {
    val path = resolve(sys.props.get("parrot.repo"), sys.env.get("PARROT_REPO"), Paths.get("."))
    require(Files.size(path) <= 64L * 1024 * 1024, "shared fixture exceeds test input budget")
    parse(Files.readString(path, StandardCharsets.UTF_8))
  }

  private def field(row: JsonNode, key: String): JsonNode =
    Option(row.get(key)).getOrElse(throw new IllegalArgumentException(s"missing $key"))

  private def text(node: JsonNode, key: String, nonempty: Boolean = false): String = {
    require(node.isTextual, s"invalid $key")
    val value = node.textValue()
    require(!nonempty || value.nonEmpty, s"empty $key")
    require(!value.contains('\u0000'), s"NUL in $key")
    try StandardCharsets.UTF_8.newEncoder().onMalformedInput(CodingErrorAction.REPORT)
      .onUnmappableCharacter(CodingErrorAction.REPORT).encode(CharBuffer.wrap(value))
    catch { case e: CharacterCodingException => throw new IllegalArgumentException(s"invalid Unicode in $key", e) }
    value
  }

  private def uint(node: JsonNode, bits: Int, key: String): BigInt = {
    require(node.isIntegralNumber, s"invalid $key")
    val value = BigInt(node.bigIntegerValue())
    require(value >= 0 && value < (BigInt(1) << bits), s"out of range $key")
    value
  }

  private def number(row: JsonNode, key: String, bits: Int, default: Int): BigInt =
    Option(row.get(key)).map(uint(_, bits, key)).getOrElse(BigInt(default))

  private def hex(row: JsonNode, key: String): String = {
    val value = text(field(row, key), key)
    require(value.length % 2 == 0 && value.forall(c => "0123456789abcdefABCDEF".contains(c)), s"invalid $key")
    require(value.length <= 2L * (WireFrame.MAX_FRAME_LEN + 4), s"oversized $key")
    value
  }

  def parse(json: String): List[Vector] = {
    require(json.length <= 64 * 1024 * 1024, "shared fixture exceeds test input budget")
    val document = try mapper.readTree(json)
      catch { case e: JsonProcessingException => throw new IllegalArgumentException("invalid shared fixture JSON", e) }
    require(document != null && document.isObject, "shared fixture must be an object")
    require(text(field(document, "format"), "format") == "parrot-wire-1.0", "wrong shared fixture format")
    val frozen = field(document, "frozen")
    require(frozen.isBoolean && frozen.booleanValue(), "shared fixture must be frozen")
    val rows = field(document, "vectors")
    require(rows.isArray && rows.size() > 0 && rows.size() <= 4096, "invalid shared vector list")
    val vectors = rows.elements().asScala.map { row =>
      require(row.isObject, "vector must be an object")
      val ft = text(field(row, "frame_type"), "frame_type")
      require(ft.matches("0x[0-9a-fA-F]{2}"), "invalid frame_type")
      Vector(text(field(row, "name"), "name", nonempty = true), Integer.parseInt(ft.substring(2), 16).toByte,
        uint(field(row, "correlation_id"), 64, "correlation_id").longValue,
        number(row, "flags", 16, 0).toInt, number(row, "hop_count", 8, 0).toByte,
        number(row, "hop_limit", 8, 8).toByte, number(row, "sequence", 32, 0).toLong,
        text(field(row, "path"), "path"), text(field(row, "type_key"), "type_key"),
        hex(row, "payload_hex"), hex(row, "bytes_hex"))
    }.toList
    require(vectors.map(_.name).distinct.size == vectors.size, "duplicate shared vector name")
    vectors
  }
}
