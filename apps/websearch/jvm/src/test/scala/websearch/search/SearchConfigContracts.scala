package websearch.search

/** Storage selection is a schema contract, never a best-effort regex hint. */
object SearchConfigContracts extends App {
  def read(text: String): String = SearchConfig.directory(Some(text.getBytes("UTF-8")), Some("/fallback"))
  assert(SearchConfig.directory(None, None) == "./data")
  assert(SearchConfig.directory(None, Some("/legacy")) == "/legacy")
  assert(read("# empty table\n") == "/fallback")
  val expected = "/remote/quote\" back\\slash 中文"
  assert(read("data_dir = '" + expected + "'\n") == expected)
  assert(read("data_dir = \"/remote/quote\\\" back\\\\slash 中文\" # comment\n") == expected)
  assert(read("data_dir = \"\"\"\n/remote/中文\"\"\"\n") == "/remote/中文")
  assert(read("\"data_dir\" = '/remote/#?%'\n") == "/remote/#?%")
  assert(read("data_dir = \"/remote/\\U0001F600\"\n") == "/remote/😀")
  for (bad <- Vector("data_dir = 1", "data_dir = []", "data_dir = {}", "data_dir = false",
      "data_dir = ''", "data_dir = 'relative'", "data_dri = '/other'", "data_dir = '/a'\nother = 1",
      "data_dir = '/a'\ndata_dir = '/b'", "data_dir = \"unterminated", "[config]\ndata_dir = '/a'",
      "data_dir = \"/null\\u0000\"", "data_dir = '" + "/" + ("a" * 4096) + "'", "#" * 32769)) {
    var rejected = false
    try read(bad) catch { case _: Exception => rejected = true }
    assert(rejected, "invalid config fell back or was accepted: " + bad.take(120))
  }
  var rejectedUtf8 = false
  try SearchConfig.directory(Some(Array(0xff.toByte)), Some("/fallback"))
  catch { case _: java.nio.charset.CharacterCodingException => rejectedUtf8 = true }
  assert(rejectedUtf8, "malformed UTF-8 must not select fallback storage")
  println("PASS search config: TOML strings, legacy fallback, schema, size, UTF-8 and path validation")
}
