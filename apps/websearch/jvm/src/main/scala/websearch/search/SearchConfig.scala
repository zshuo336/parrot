package websearch.search

import java.nio.ByteBuffer
import java.nio.charset.{CodingErrorAction, StandardCharsets}
import java.nio.file.Paths
import scala.jdk.CollectionConverters._

/** Explicit component configuration must never silently choose shared storage. */
private[search] object SearchConfig {
  def directory(config: Option[Array[Byte]], environment: Option[String]): String = {
    val bytes = config.getOrElse(Array.emptyByteArray)
    require(bytes.length <= 32768, "search configuration exceeds 32768 bytes")
    val text = StandardCharsets.UTF_8.newDecoder()
      .onMalformedInput(CodingErrorAction.REPORT)
      .onUnmappableCharacter(CodingErrorAction.REPORT)
      .decode(ByteBuffer.wrap(bytes)).toString
    val table = org.tomlj.Toml.parse(text)
    require(!table.hasErrors(), "invalid search TOML: " + table.errors().asScala.mkString("; "))
    require(table.keySet().asScala.forall(_ == "data_dir"), "unknown search configuration field")
    if (!table.contains("data_dir")) environment.getOrElse("./data")
    else {
      val value = table.getString("data_dir")
      require(value != null && value.getBytes(StandardCharsets.UTF_8).length <= 4096 &&
        !value.contains('\u0000') && Paths.get(value).isAbsolute,
        "data_dir must be an absolute gateway path of at most 4096 UTF-8 bytes without NUL")
      value
    }
  }
}
