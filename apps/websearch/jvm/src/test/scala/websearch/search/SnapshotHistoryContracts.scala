package websearch.search

import java.nio.file.{Files, Path}
import scala.collection.mutable
import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.AskPattern._
import parrot.protocol.jvm.ComponentContext
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk, BridgeReplyErr}
import scala.concurrent.Await
import scala.concurrent.duration._

object SnapshotHistoryContracts extends App {
  val root = Files.createTempDirectory("ws-snapshot-history-")
  println("Snapshot history evidence: " + root)
  def directory(name: String): Path = Files.createDirectories(root.resolve(name).resolve("index"))
  def segment(n: Long): String = f"seg-$n%020d.segment"
  val original = directory("original")
  val owner = SnapshotStore.open(original.toFile)
  val docs = mutable.LongMap(1L -> SearchBehavior.Doc("https://history.test/one", "one", "alpha"))
  val terms = mutable.Map("alpha" -> mutable.LongMap(1L -> 1))
  val (epoch, acknowledged, digest) = try {
    owner.commit(docs, terms)
    Files.copy(original.resolve(segment(0)), root.resolve("base-snapshot.saved"))
    docs.update(2L, SearchBehavior.Doc("https://history.test/two", "two", "beta"))
    terms.update("beta", mutable.LongMap(2L -> 1))
    owner.commit(docs, terms)
    Files.copy(original.resolve(segment(1)), root.resolve("acknowledged-snapshot.saved"))
    (owner.crawlState._1, owner.crawlState._2, owner.crawlDigest)
  } finally owner.close()
  assert(acknowledged == 1)
  val branch = directory("branch")
  Files.copy(root.resolve("base-snapshot.saved"), branch.resolve(segment(0)))
  val fork = SnapshotStore.open(branch.toFile)
  try {
    fork.commit(fork.recovered._1, fork.recovered._2)
    assert(fork.crawlState == (epoch -> acknowledged))
    assert(!fork.containsCheckpoint(epoch, acknowledged, digest), "equal-number fork accepted")
    fork.commit(fork.recovered._1, fork.recovered._2)
    assert(!fork.containsCheckpoint(epoch, acknowledged, digest), "higher-number fork accepted")
    assert(!fork.recovered._1.contains(2), "fork must actually lack the confirmed document")
  } finally fork.close()
  println("PASS same epoch equal/higher generation forks reject the completed checkpoint")

  // Merely finding an acknowledged file in the directory is not ancestry proof:
  // graft it onto a different branch with a later head.
  if (Files.exists(branch.resolve(segment(1)))) Files.move(branch.resolve(segment(1)), root.resolve("displaced-branch.segment"))
  Files.copy(root.resolve("acknowledged-snapshot.saved"), branch.resolve(segment(1)))
  val graft = SnapshotStore.open(branch.toFile)
  try {
    assert(!graft.containsCheckpoint(epoch, acknowledged, digest), "grafted obsolete file impersonated an ancestor")
    val deadline = System.nanoTime() + 5000000000L
    while (graft.segmentCount != 1 && System.nanoTime() < deadline) Thread.sleep(5)
    assert(SnapshotStore.files(branch.toFile).size == 1, "obsolete graft was not reclaimed")
  } finally graft.close()
  println("PASS grafted acknowledged snapshot cannot impersonate an ancestor")

  val valid = SnapshotStore.open(original.toFile)
  try {
    valid.commit(valid.recovered._1, valid.recovered._2)
    valid.commit(valid.recovered._1, valid.recovered._2)
    assert(valid.containsCheckpoint(epoch, acknowledged, digest), "lost acknowledgement must allow a newer descendant")
  } finally valid.close()
  val restarted = SnapshotStore.open(original.toFile)
  try assert(restarted.containsCheckpoint(epoch, acknowledged, digest)) finally restarted.close()
  println("PASS ancestor proof survives multiple later commits and fresh owner restart")

  // Exercise real wire handlers, malformed lengths and failures without writes.
  val system = ActorSystem(new SearchComponent().behavior(ComponentContext("/user/search", "history",
    Some(s"data_dir = \"${original.getParent}\"".getBytes("UTF-8")))), "history-contracts")
  implicit val timeout: akka.util.Timeout = 5.seconds
  implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
  def exchange(key: String, body: Array[Byte]): Any =
    Await.result(system.ask[Any](r => BridgeAsk("bin:ws/" + key, body, r)), 6.seconds)
  def request(key: String, body: Array[Byte] = Array.emptyByteArray): Array[Byte] = exchange(key, body) match {
    case BridgeReplyOk(kind, bytes) =>
      assert(kind == "bin:ws/" + (if (key == "CrawlStateV2") "CrawlStateV2R" else "CheckpointV2Ack"))
      assert(bytes.length == 56); bytes
    case other => throw new AssertionError(other)
  }
  try {
    val before = request("CrawlStateV2")
    val foreign = before.clone(); foreign(24) = (foreign(24) ^ 1).toByte
    for (key <- Vector("CrawlStateV2", "CheckpointV2");
         bad <- (1 until 56).map(before.take) ++ Vector(before ++ Array[Byte](0), foreign)) {
      exchange(key, bad) match { case _: BridgeReplyErr => (); case other => throw new AssertionError(other) }
      assert(request("CrawlStateV2").sameElements(before), "invalid history request mutated storage")
    }
    exchange("CheckpointV2", Array.emptyByteArray) match { case _: BridgeReplyErr => (); case other => throw new AssertionError(other) }
    assert(request("CrawlStateV2", before).sameElements(before))
    val newer = request("CheckpointV2", before)
    assert(!newer.sameElements(before))
    assert(request("CrawlStateV2", before).sameElements(newer))
    request("CheckpointV2", before) // lost local completion can replay an ancestor
  } finally { system.terminate(); Await.result(system.whenTerminated, 10.seconds) }
  println("PASS real history wire rejects truncations/tails/foreign hash without writes and accepts ancestors")

  // Independent version3 fixture from the legacy payload writer, not by
  // relabelling v5 ancestry bytes as a legacy document body.
  val payloadFile = root.resolve("version2-payload.saved")
  SnapshotStore.write(payloadFile.toFile, docs, terms)
  val v2 = Files.readAllBytes(payloadFile)
  val v3 = "PRWSIDX3".getBytes("US-ASCII") ++ epoch.toArray ++ v2.drop(8).dropRight(4)
  val crc = new java.util.zip.CRC32; crc.update(v3)
  val legacy = directory("version3")
  val completeV3 = v3 ++ java.nio.ByteBuffer.allocate(4).putInt(crc.getValue.toInt).array()
  Files.write(legacy.resolve(segment(1)), completeV3)
  val invalidV3 = root.resolve("invalid-version3.bin")
  def rejectV3(bytes: Array[Byte]): Unit = {
    Files.write(invalidV3, bytes)
    try { SnapshotStore.read(invalidV3.toFile); throw new AssertionError("invalid version3 snapshot accepted") }
    catch { case _: java.io.IOException => () }
  }
  (0 until completeV3.length).foreach(n => rejectV3(completeV3.take(n)))
  val corruptV3 = completeV3.clone(); corruptV3(8) = (corruptV3(8) ^ 1).toByte
  rejectV3(corruptV3); rejectV3(completeV3 ++ Array[Byte](0))
  println("PASS version3 still rejects every truncation, changed epoch checksum and trailing bytes")
  val migration = SnapshotStore.open(legacy.toFile)
  try {
    val old = migration.crawlDigest
    assert(migration.recovered._1.size == 2 && migration.recovered._2("beta")(2) == 1)
    val deadline = System.nanoTime() + 10000000000L
    while (!migration.prepareHistory()._1 && System.nanoTime() < deadline) Thread.sleep(5)
    assert(migration.prepareHistory()._1, "legacy preparation timeout")
    migration.commit(migration.recovered._1, migration.recovered._2)
    assert(migration.containsCheckpoint(epoch, 1, old))
  } finally migration.close()
  println("PASS version3 migration preserves documents/postings and anchors the compact successor snapshot")
}
