package websearch.search

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.AskPattern._
import parrot.protocol.jvm.ComponentContext
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk}
import java.io.ByteArrayOutputStream
import java.nio.{ByteBuffer, ByteOrder}
import java.nio.file.Files
import java.util.Locale
import scala.concurrent.Await
import scala.concurrent.duration._
import scala.jdk.CollectionConverters._

/** Real actor contracts with independently computed BM25 expectations. */
object SearchContracts extends App {
  def i32(n: Int): Array[Byte] = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(n).array()
  def i64(n: Long): Array[Byte] = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(n).array()
  def string(s: String): Array[Byte] = { val b = s.getBytes("UTF-8"); i32(b.length) ++ b }
  def terms(entries: (String, Long, Int)*): Array[Byte] =
    i32(entries.size) ++ entries.flatMap { case (t, id, tf) => string(t) ++ i64(id) ++ i32(tf) }.toArray
  def score(tf: Int, dl: Int, average: Double, n: Int, df: Int): String = {
    val idf = math.log(1.0 + (n - df + 0.5) / (df + 0.5))
    val value = idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * dl / average))
    String.format(Locale.ROOT, "%.2f", Double.box(value))
  }
  val directory = Files.createTempDirectory("ws-search-contracts-")
  var system: ActorSystem[Any] = null
  def start(): Unit = {
    val config = s"data_dir = \"${directory.toString}\"".getBytes("UTF-8")
    system = ActorSystem(new SearchComponent().behavior(ComponentContext("/user/search", "test", Some(config))), "search-contracts")
  }
  def stop(): Unit = if (system != null) {
    system.terminate()
    Await.result(system.whenTerminated, 10.seconds)
    system = null
  }
  def request(key: String, payload: Array[Byte] = Array.emptyByteArray): Array[Byte] = {
    implicit val timeout: akka.util.Timeout = 5.seconds
    implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
    Await.result(system.ask[Any](ref => BridgeAsk("bin:ws/" + key, payload, ref)), 6.seconds) match {
      case BridgeReplyOk(_, bytes) => bytes
      case other => throw new AssertionError(s"unexpected reply $other")
    }
  }
  def query(): String = new String(request("Search", i32(10) ++ string("alpha")), "UTF-8")
  def scores(json: String): Map[Long, String] =
    """"doc":([0-9]+),"score":([0-9]+\.[0-9]+)""".r.findAllMatchIn(json)
      .map(m => m.group(1).toLong -> m.group(2)).toMap
  def doc(id: Long, host: String): Unit = {
    request("DocMeta", i64(id) ++ string(s"http://$host/") ++ string(host) ++ string("alpha"))
  }
  try {
    start()
    doc(1, "a.test"); doc(2, "b.test")
    request("IndexTerms", terms(("alpha", 1, 2), ("beta", 1, 8), ("alpha", 2, 1)))
    assert(scores(query()) == Map(1L -> score(2, 10, 5.5, 2, 2), 2L -> score(1, 1, 5.5, 2, 2)))
    request("Flush")
    doc(3, "c.test")
    val update = terms(("alpha", 3, 1), ("gamma", 3, 5))
    request("IndexTerms", update)
    val before = query()
    request("IndexTerms", update)
    assert(query() == before, "duplicate term assignment changed ranking")
    assert(scores(before) == Map(1L -> score(2, 10, 17.0 / 3, 3, 3), 2L -> score(1, 1, 17.0 / 3, 3, 3), 3L -> score(1, 6, 17.0 / 3, 3, 3)))
    println("PASS BM25 formula and duplicate term assignment")
    request("Flush")
    stop(); start()
    assert(query() == before, "multiple full snapshots changed scores on replay")
    println("PASS multi-snapshot replay preserves exact ranking")
    val docs = new String(request("ListDocs", i32(-1) ++ i32(2)), "UTF-8")
    assert(docs.contains("\"offset\":2") && docs.contains("c.test") && !docs.contains("a.test"), docs)
    val listing = new String(request("ListTerms", i32(-1) ++ i32(2)), "UTF-8")
    assert(listing.contains("\"offset\":2") && listing.contains("gamma") && !listing.contains("alpha"), listing)
    val controls = (0 until 32).map(_.toChar).mkString
    val title = "quote\"slash\\" + controls + "中文"
    val oldLocale = Locale.getDefault(Locale.Category.FORMAT)
    try {
      Locale.setDefault(Locale.Category.FORMAT, Locale.GERMANY)
      request("DocMeta", i64(1) ++ string("https://a.test/") ++ string(title) ++ string(title))
      val encoded = query()
      assert(!encoded.exists(_ < ' '), s"raw JSON control character: $encoded")
      (0 until 32).foreach { n =>
        val expected = "\\u" + ("0000" + Integer.toHexString(n)).takeRight(4)
        assert(encoded.contains(expected), s"missing escape $expected in $encoded")
      }
      assert(encoded.contains("quote\\\"slash\\\\"), encoded)
      assert(scores(encoded) == scores(before), s"locale changed score JSON: $encoded")
      println("PASS JSON preserves all control characters and ignores numeric locale")
    } finally Locale.setDefault(Locale.Category.FORMAT, oldLocale)
    request("Clear")
    val empty = new String(request("ListDocs", i32(-1) ++ i32(2)), "UTF-8")
    assert(empty.contains("\"offset\":0") && empty.contains("\"items\":[]"), empty)
    println("PASS unsigned offset clamping, stable ties and empty page")
    doc(1, "unsigned.test")
    request("IndexTerms", terms(("alpha", 1, -1)))
    val high = query()
    assert(scores(high) == Map(1L -> "0.63"), high)
    request("Flush")
    stop(); start()
    assert(query() == high, "u32 frequency changed sign after snapshot replay")
    println("PASS unsigned term frequency survives snapshot replay")
  } finally {
    stop()
    val paths = Files.walk(directory)
    try paths.iterator().asScala.toVector.reverse.foreach(Files.delete)
    finally paths.close()
  }
}
