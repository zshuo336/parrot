package websearch.search

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.AskPattern._
import parrot.protocol.jvm.ComponentContext
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk, BridgeReplyErr}
import java.nio.{ByteBuffer, ByteOrder}
import java.nio.file.Files
import scala.concurrent.Await
import scala.concurrent.duration._

object CrawlCheckpointContracts extends App {
  val directory = Files.createTempDirectory("ws-crawl-checkpoint-")
  println("Crawl checkpoint evidence: " + directory)
  var system: ActorSystem[Any] = null
  var committed: Array[Byte] = null
  def start(): Unit = {
    system = ActorSystem(new SearchComponent().behavior(ComponentContext("/user/search", "checkpoint",
      Some(s"data_dir = \"$directory\"".getBytes("UTF-8")))), "crawl-checkpoint")
  }
  def stop(): Unit = if (system != null) { system.terminate(); Await.result(system.whenTerminated, 10.seconds); system = null }
  def exchange(key: String, bytes: Array[Byte] = Array.emptyByteArray): Any = {
    implicit val timeout: akka.util.Timeout = 5.seconds
    implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
    Await.result(system.ask[Any](reply => BridgeAsk("bin:ws/"+key, bytes, reply)), 6.seconds)
  }
  def request(key: String, bytes: Array[Byte] = Array.emptyByteArray): Array[Byte] = exchange(key,bytes) match {
    case BridgeReplyOk(kind, body) =>
      assert(kind == "bin:ws/" + (if (key == "CrawlState") "CrawlStateR" else key + "Ack")); body
    case other => throw new AssertionError(other)
  }
  def generation(bytes: Array[Byte]): Long = { assert(bytes.length==24); ByteBuffer.wrap(bytes,16,8).order(ByteOrder.LITTLE_ENDIAN).getLong }
  try {
    start()
    exchange("PrepareIndex", Array[Byte](1)) match { case _: BridgeReplyErr => (); case other => throw new AssertionError(other) }
    exchange("PrepareIndex") match {
      case BridgeReplyOk("bin:ws/PrepareIndexR", body) => assert(body.sameElements(Array[Byte](1) ++ Array.fill[Byte](8)(0)))
      case other => throw new AssertionError(other)
    }
    exchange("Checkpoint",Array.fill[Byte](16)(0)) match { case _: BridgeReplyErr => (); case other => throw new AssertionError(other) }
    val initial = request("CrawlState"); assert(generation(initial)==0)
    assert(request("CrawlState").sameElements(initial), "state read must not add snapshots")
    stop(); start(); assert(request("CrawlState").sameElements(initial), "bootstrap identity not durable")
    println("PASS bootstrap epoch and generation survive restart; reads do not write extra snapshots")
    val epoch = initial.take(16)
    def i32(n: Int): Array[Byte] = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(n).array()
    def i64(n: Long): Array[Byte] = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(n).array()
    def str(s: String): Array[Byte] = i32(s.getBytes("UTF-8").length) ++ s.getBytes("UTF-8")
    exchange("DocMeta", i64(7) ++ str("https://checkpoint.test/page") ++ str("checkpoint title") ++ str("alpha")) match {
      case BridgeReplyOk("bin:ws/DocAck", body) => assert(body.sameElements(i32(1)))
      case other => throw new AssertionError(other)
    }
    exchange("IndexTerms", i32(1) ++ str("alpha") ++ i64(7) ++ i32(3)) match {
      case BridgeReplyOk("bin:ws/IndexAck", body) => assert(body.sameElements(i32(1)))
      case other => throw new AssertionError(other)
    }
    val next = request("Checkpoint",epoch)
    committed = Files.readAllBytes(directory.resolve("index/seg-00000000000000000001.segment"))
    Files.write(directory.resolve("acknowledged-snapshot.saved"), committed)
    assert(next.take(16).sameElements(epoch) && generation(next)==1)
    stop(); start(); assert(request("CrawlState").sameElements(next), "checkpoint identity not recovered")
    exchange("Search",i32(10) ++ str("alpha")) match {
      case BridgeReplyOk("bin:ws/SearchResult", body) => assert(new String(body,"UTF-8").contains("https://checkpoint.test/page"), "checkpoint acknowledged without persisting document and postings")
      case other => throw new AssertionError(other)
    }
    for (bad <- (0 until 16).map(epoch.take) ++ Vector(epoch ++ Array[Byte](0), Array.fill[Byte](16)(0))) {
      exchange("Checkpoint",bad) match { case _: BridgeReplyErr => (); case other => throw new AssertionError(other) }
      assert(request("CrawlState").sameElements(next), "rejected checkpoint wrote a generation")
    }
    exchange("CrawlState",Array[Byte](0)) match { case _: BridgeReplyErr => (); case other => throw new AssertionError(other) }
    assert(request("CrawlState").sameElements(next))
    println("PASS checkpoint commits once and rejects every truncation, tail and foreign epoch without mutation")
    request("Clear")
    val cleared = request("CrawlState")
    assert(!cleared.take(16).sameElements(epoch) && generation(cleared)==2)
    stop(); start(); assert(request("CrawlState").sameElements(cleared))
    exchange("Checkpoint",epoch) match { case _: BridgeReplyErr => (); case other => throw new AssertionError(other) }
    request("Flush")
    val flushed = request("CrawlState")
    assert(flushed.take(16).sameElements(cleared.take(16)) && generation(flushed)==3)
    println("PASS Clear rotates the durable epoch; ordinary Flush preserves it; stale owner epoch rejected")
  } finally stop()

  assert(new String(committed.take(8),"US-ASCII")=="PRWSIDX5")
  val badFile = Files.createTempFile(directory,"invalid-epoch-snapshot-",".bin")
  def rejected(bytes: Array[Byte]): Unit = {
    Files.write(badFile,bytes)
    try { SnapshotStore.read(badFile.toFile); throw new AssertionError("invalid version5 snapshot accepted") }
    catch { case _: java.io.IOException => () }
  }
  (0 until committed.length).foreach(n => rejected(committed.take(n)))
  val corruptEpoch=committed.clone();corruptEpoch(8)=(corruptEpoch(8)^1).toByte;rejected(corruptEpoch)
  rejected(committed ++ Array[Byte](0))
  println("PASS version5 truncation at every byte, changed epoch checksum and trailing bytes are rejected")

  val legacyDir=Files.createTempDirectory("ws-legacy-epoch-")
  val index=Files.createDirectory(legacyDir.resolve("index"))
  val docs=scala.collection.mutable.LongMap(9L -> SearchBehavior.Doc("https://legacy.test/", "legacy", "body"))
  val postings=scala.collection.mutable.Map("legacy" -> scala.collection.mutable.LongMap(9L -> 2))
  SnapshotStore.write(index.resolve("seg-00000.segment").toFile,docs,postings)
  val owner=SnapshotStore.open(index.toFile)
  val durable=try {
    assert(owner.recovered._1(9).url=="https://legacy.test/")
    owner.ensureCrawlState(owner.recovered._1,owner.recovered._2)
    assert(owner.crawlState._2==1 && owner.crawlState._1.length==16)
    owner.crawlState
  } finally owner.close()
  val recovered=SnapshotStore.open(index.toFile)
  try {
    assert(recovered.crawlState==durable)
    assert(recovered.recovered._1(9)==docs(9) && recovered.recovered._2("legacy")(9)==2)
  } finally recovered.close()
  println("PASS version2 migration commits epoch without losing existing metadata or postings")
}
