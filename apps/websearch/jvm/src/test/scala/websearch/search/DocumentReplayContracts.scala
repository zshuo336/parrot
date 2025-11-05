package websearch.search

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.AskPattern._
import parrot.protocol.jvm.ComponentContext
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk, BridgeReplyErr}
import java.nio.{ByteBuffer, ByteOrder}
import java.nio.file.Files
import scala.concurrent.Await
import scala.concurrent.duration._

/** Full-document replacement is the replay unit; Flush is its durability boundary. */
object DocumentReplayContracts extends App {
  def i32(n: Int): Array[Byte] = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(n).array()
  def i64(n: Long): Array[Byte] = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(n).array()
  def str(s: String): Array[Byte] = { val b=s.getBytes("UTF-8"); i32(b.length) ++ b }
  case class Document(id: Long, title: String, words: Vector[(String, Int)]) {
    def meta: Array[Byte] = i64(id) ++ str(s"https://example.test/$id") ++ str(title) ++ str(title)
    def bytes: Array[Byte] = meta ++ i32(words.size) ++ words.flatMap { case (word, tf) => str(word) ++ i32(tf) }.toArray
  }
  def batch(docs: Document*): Array[Byte] = i32(docs.size) ++ docs.flatMap(_.bytes).toArray
  val directory = Files.createTempDirectory("ws-document-replay-")
  println("Document replay evidence: " + directory)
  var system: ActorSystem[Any] = null
  def start(): Unit = {
    val config = s"data_dir = \"$directory\"".getBytes("UTF-8")
    system = ActorSystem(new SearchComponent().behavior(ComponentContext("/user/search", "replay", Some(config))), "document-replay")
  }
  def stop(): Unit = if (system != null) {
    system.terminate(); Await.result(system.whenTerminated, 10.seconds); system = null
  }
  def exchange(key: String, bytes: Array[Byte] = Array.emptyByteArray): Any = {
    implicit val timeout: akka.util.Timeout = 5.seconds
    implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
    Await.result(system.ask[Any](reply => BridgeAsk("bin:ws/"+key, bytes, reply)), 6.seconds)
  }
  def request(key: String, bytes: Array[Byte] = Array.emptyByteArray): Array[Byte] = exchange(key, bytes) match {
    case BridgeReplyOk(_, body) => body
    case other => throw new AssertionError(s"$key: $other")
  }
  def query(term: String): String = new String(request("Search", i32(100) ++ str(term)), "UTF-8")
  def replace(docs: Document*): Unit = {
    if (args.contains("--legacy-split")) {
      docs.foreach { doc =>
        request("DocMeta", doc.meta)
        request("IndexTerms", i32(doc.words.size) ++ doc.words.flatMap { case (term, tf) => str(term) ++ i64(doc.id) ++ i32(tf) }.toArray)
      }
    } else exchange("IndexDocuments", batch(docs: _*)) match {
      case BridgeReplyOk("bin:ws/DocumentsAck", count) => assert(count.sameElements(i32(docs.size)))
      case other => throw new AssertionError(s"unexpected document receipt: $other")
    }
  }
  val old = Document(1, "old title", Vector("alpha" -> 2, "beta" -> 1))
  val other = Document(2, "other title", Vector("beta" -> 3))
  val updated = Document(1, "new title", Vector("gamma" -> 4))
  try {
    start(); replace(old, other); request("Flush")
    replace(updated)
    assert(query("alpha") == "[]", "obsolete alpha posting survived full-document replacement")
    assert(!query("beta").contains("old title") && query("beta").contains("other title"), "replacement changed an unrelated document")
    val gamma = query("gamma"); assert(gamma.contains("new title") && !gamma.contains("old title"))
    replace(updated); assert(query("gamma") == gamma, "retry changed ranking or metadata")
    println("PASS complete replacement removes obsolete terms and is idempotent")

    val valid = batch(Document(1,"bad update",Vector("alpha"->7)), Document(3,"new",Vector("beta"->1)))
    val malformed = (0 until valid.length).map(valid.take) ++ Vector(valid ++ Array[Byte](0),
      batch(updated,updated), batch(Document(1,"duplicate",Vector("gamma"->1,"gamma"->2))),
      batch(Document(1,"zero",Vector("gamma"->0))), batch(Document(1,"empty",Vector(""->1))))
    malformed.foreach { bytes =>
      exchange("IndexDocuments",bytes) match {
        case BridgeReplyErr(code, _) => assert(code == parrot.protocol.jvm.WireFrame.ErrCode.CodecError)
        case result => throw new AssertionError(s"malformed batch acknowledged: $result")
      }
      assert(query("gamma")==gamma && query("alpha")=="[]", "malformed batch partially replaced existing state")
      assert(!query("beta").contains("new"), "malformed tail inserted another document")
    }
    println("PASS every truncation, malformed tail, duplicate id/term and zero frequency is atomic")

    // A successful DocumentsAck only confirms in-memory replacement. There is
    // deliberately no Flush here; restart must restore the last complete image.
    stop(); start()
    assert(query("alpha").contains("old title") && query("gamma")=="[]", "unflushed state leaked into committed image")
    replace(updated); request("Flush"); stop(); start()
    assert(query("alpha")=="[]" && query("gamma")==gamma && query("beta").contains("other title"))
    println("PASS explicit Flush defines the durable full-document replay boundary")

    replace(Document(1,"empty page",Vector.empty)); request("Flush"); stop(); start()
    assert(query("gamma")=="[]" && query("beta").contains("other title"))
    val health = new String(request("Healthz"),"UTF-8")
    assert(health.contains("\"docs\":2") && health.contains("\"terms\":1") && health.contains("\"postings\":1"),health)
    replace(Document(2,"unsigned",Vector("omega" -> -1))); request("Flush"); stop(); start()
    assert(query("beta")=="[]" && query("omega").contains("unsigned"))
    println("PASS empty replacement removes all old terms; unsigned frequencies survive replay")

    // Old incremental messages remain supported and participate in the reverse
    // lookup, so the new replacement can remove data created by older clients.
    request("IndexTerms", i32(1) ++ str("legacyword") ++ i64(2) ++ i32(4))
    assert(query("legacyword").contains("unsigned"))
    replace(Document(2,"clean",Vector.empty))
    assert(query("legacyword")=="[]" && query("omega")=="[]")
    println("PASS replacement cleans legacy incremental entries too")
  } finally stop()
}
