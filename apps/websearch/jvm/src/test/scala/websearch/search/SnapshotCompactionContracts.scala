package websearch.search

import java.io.{ByteArrayOutputStream, DataOutputStream, IOException}
import java.nio.ByteBuffer
import java.nio.file.{Files, Path}
import java.security.MessageDigest
import java.util.zip.CRC32
import scala.collection.mutable

object SnapshotCompactionContracts extends App {
  val root = Files.createTempDirectory("ws-index-compaction-contracts-")
  println("Index compaction evidence: " + root)
  def directory(name: String): Path = Files.createDirectories(root.resolve(name).resolve("index"))
  def segment(n: Long): String = f"seg-$n%020d.segment"
  def hash(bytes: Array[Byte]): Vector[Byte] = MessageDigest.getInstance("SHA-256").digest(bytes).toVector
  def crc(content: Array[Byte]): Array[Byte] = {
    val checksum = new CRC32; checksum.update(content)
    content ++ ByteBuffer.allocate(4).putInt(checksum.getValue.toInt).array()
  }
  def awaitCondition(label: String)(condition: => Boolean): Unit = {
    val deadline = System.nanoTime() + 10000000000L
    while (!condition && System.nanoTime() < deadline) Thread.sleep(5)
    assert(condition, label)
  }
  def rejected(label: String)(body: => Any): Unit = {
    try { body; throw new AssertionError("accepted " + label) }
    catch { case _: IOException => () }
  }
  // Hold verification at a known boundary; no filesystem timing assumption is
  // needed to prove pending progress, cancellation, join and failure delivery.
  val entered = new java.util.concurrent.CountDownLatch(1)
  val finished = new java.util.concurrent.atomic.AtomicBoolean(false)
  val preparing = new SnapshotPreparation((cancelled, verified) => {
    verified.set(42); entered.countDown()
    while (!cancelled.get()) Thread.sleep(1)
    finished.set(true)
    throw new java.io.InterruptedIOException("test verification cancelled")
  })
  assert(entered.await(5, java.util.concurrent.TimeUnit.SECONDS))
  assert(preparing.progress == ((false, 42L)) && preparing.result.isEmpty)
  preparing.close()
  assert(finished.get(), "close returned before verifier stopped")
  rejected("cancelled preparation result")(preparing.result)
  val failure = new SnapshotPreparation((_, _) => throw new IOException("bad ancestor"))
  try rejected("failed preparation") {
    awaitCondition("failed preparation never completed")(failure.progress._1)
  } finally failure.close()
  val success = new SnapshotPreparation((_, verified) => { verified.set(1); Vector(7L -> Vector.fill[Byte](32)(3)) })
  try {
    awaitCondition("successful preparation never completed")(success.progress._1)
    assert(success.progress == ((true, 1L)))
    assert(success.result.get == Vector(7L -> Vector.fill[Byte](32)(3)))
  } finally success.close()
  println("PASS preparation exposes pending progress, propagates failure, and joins cancelled verification")
  val docs = mutable.LongMap(1L -> SearchBehavior.Doc("https://compact.test/one", "one", "alpha"))
  val postings = mutable.Map("alpha" -> mutable.LongMap(1L -> 1))
  val payloadFile = root.resolve("version2-payload.saved")
  SnapshotStore.write(payloadFile.toFile, docs, postings)
  val body = Files.readAllBytes(payloadFile).drop(8).dropRight(4)
  val epoch = Vector.fill[Byte](16)(7)
  def legacy(index: Path, count: Int): Vector[(Long, Vector[Byte])] = {
    var previous = Vector.fill[Byte](32)(0)
    (0 until count).map { n =>
      val bytes = crc("PRWSIDX4".getBytes("US-ASCII") ++ epoch.toArray ++
        ByteBuffer.allocate(8).putLong(n.toLong - 1).array() ++ previous.toArray ++ body)
      Files.write(index.resolve(segment(n)), bytes)
      previous = hash(bytes); n.toLong -> previous
    }.toVector
  }
  def compact(generation: Long, ancestors: Vector[(Long, Vector[Byte])]): Array[Byte] = {
    val bytes = new ByteArrayOutputStream
    val out = new DataOutputStream(bytes)
    out.write("PRWSIDX5".getBytes("US-ASCII")); out.write(epoch.toArray)
    out.writeLong(generation); out.writeInt(ancestors.size)
    ancestors.foreach { case (g, digest) => out.writeLong(g); out.write(digest.toArray) }
    out.write(body); out.close(); crc(bytes.toByteArray)
  }

  val index = directory("many-commits")
  val owner = SnapshotStore.open(index.toFile)
  val checkpoints = mutable.ArrayBuffer.empty[(Vector[Byte], Long, Vector[Byte])]
  try {
    for (n <- 0 until 128) {
      owner.commit(docs, postings)
      checkpoints += ((owner.crawlState._1, owner.crawlState._2, owner.crawlDigest))
      if (n == 0) Files.createLink(root.resolve("first-generation.saved"), index.resolve(segment(0)))
    }
    awaitCondition("full snapshots not reclaimed")(owner.segmentCount == 1)
    assert(SnapshotStore.files(index.toFile).size == 1)
    rejected("second live owner")(SnapshotStore.open(index.toFile))
    checkpoints.foreach { case (e, g, h) => assert(owner.containsCheckpoint(e, g, h)) }
  } finally owner.close()
  val restarted = SnapshotStore.open(index.toFile)
  try {
    checkpoints.foreach { case (e, g, h) => assert(restarted.containsCheckpoint(e, g, h)) }
    assert(restarted.recovered._1 == docs && restarted.recovered._2 == postings)
    assert(hash(Files.readAllBytes(root.resolve("first-generation.saved"))) == checkpoints.head._3)
  } finally restarted.close()
  println("PASS compact history preserves all 128 checkpoints, exact index, hardlink bytes and lease")

  val migration = directory("legacy-migration")
  val old = legacy(migration, 300)
  val migrating = SnapshotStore.open(migration.toFile)
  try {
    awaitCondition("legacy preparation did not finish")(migrating.prepareHistory()._1)
    migrating.commit(migrating.recovered._1, migrating.recovered._2)
    old.foreach { case (g, h) => assert(migrating.containsCheckpoint(epoch, g, h)) }
  } finally migrating.close() // stop may intentionally leave some obsolete files
  assert(!Thread.getAllStackTraces.keySet().toArray.exists(t => t.asInstanceOf[Thread].getName.startsWith("websearch-index-")))
  val resumed = SnapshotStore.open(migration.toFile)
  try {
    awaitCondition("interrupted cleanup not resumed")(resumed.segmentCount == 1)
    old.foreach { case (g, h) => assert(resumed.containsCheckpoint(epoch, g, h)) }
  } finally resumed.close()
  println("PASS verified legacy ancestry survives migration; worker joins before lease release and cleanup resumes")

  val corrupt = directory("bad-legacy-chain")
  legacy(corrupt, 3)
  val changed = Files.readAllBytes(corrupt.resolve(segment(0))).dropRight(4)
  changed(changed.length - 1) = 2
  Files.write(corrupt.resolve(segment(0)), crc(changed))
  val before = SnapshotStore.files(corrupt.toFile).map { case (g, f) => g -> hash(Files.readAllBytes(f.toPath)) }
  val bad = SnapshotStore.open(corrupt.toFile)
  try rejected("compaction of invalid legacy chain")(awaitCondition("invalid preparation stalled")(bad.prepareHistory()._1)) finally bad.close()
  assert(SnapshotStore.files(corrupt.toFile).map { case (g, f) => g -> hash(Files.readAllBytes(f.toPath)) } == before)
  println("PASS valid-CRC corrupted ancestor prevents migration without deleting or replacing files")

  val validation = directory("format-validation")
  val testFile = root.resolve("invalid-compact.bin")
  val valid = compact(3, Vector(0L -> hash(Array[Byte](0)), 2L -> hash(Array[Byte](2))))
  def reject(bytes: Array[Byte]): Unit = {
    Files.write(testFile, bytes); rejected("invalid compact bytes")(SnapshotStore.read(testFile.toFile))
  }
  valid.indices.foreach(n => reject(valid.take(n)))
  reject(crc(valid.dropRight(4) ++ Array[Byte](0)))
  for (entries <- Vector(Vector(2L -> hash(Array[Byte](1)), 1L -> hash(Array[Byte](2))),
                         Vector(1L -> hash(Array[Byte](1)), 1L -> hash(Array[Byte](2))),
                         Vector(3L -> hash(Array[Byte](3))), Vector(-1L -> hash(Array[Byte](4)))))
    reject(compact(3, entries))
  for (count <- Vector(-1, 100001)) {
    val content = valid.dropRight(4).clone(); ByteBuffer.wrap(content).putInt(32, count); reject(crc(content))
  }
  Files.write(validation.resolve(segment(4)), valid)
  rejected("renamed compact generation")(SnapshotStore.open(validation.toFile))
  println("PASS v5 rejects every truncation, valid-CRC invalid order/count/tail, and renamed generation")

  val cleanup = directory("cleanup-authority")
  val head = cleanup.resolve(segment(3))
  Files.write(head, valid)
  val obsolete = cleanup.resolve(segment(0)); Files.write(obsolete, Files.readAllBytes(payloadFile))
  val temporary = cleanup.resolve(".snapshot-abandoned.tmp"); Files.write(temporary, Array[Byte](1, 2))
  val damaged = valid.clone(); damaged(10) = (damaged(10) ^ 1).toByte; Files.write(head, damaged)
  rejected("corrupt authoritative head")(SnapshotStore.open(cleanup.toFile))
  assert(Files.exists(obsolete) && Files.exists(temporary))
  Files.write(head, valid)
  val clean = SnapshotStore.open(cleanup.toFile)
  try awaitCondition("unpublished file was not reclaimed")(clean.segmentCount == 1 && !Files.exists(temporary)) finally clean.close()
  val outside = root.resolve("outside.saved"); Files.write(outside, Array[Byte](9))
  val link = cleanup.resolve(".snapshot-link.tmp"); Files.createSymbolicLink(link, outside)
  rejected("linked temporary path")(SnapshotStore.open(cleanup.toFile))
  assert(Files.isSymbolicLink(link) && Files.readAllBytes(outside).sameElements(Array[Byte](9)))
  Files.delete(link)
  val afterLink = SnapshotStore.open(cleanup.toFile); afterLink.close()
  println("PASS invalid head never authorizes GC; regular unpublished files reclaimed and links rejected")

  val boundary = directory("ancestry-limit")
  val entries = (0 until 100000).map(n => n.toLong -> hash(ByteBuffer.allocate(4).putInt(n).array())).toVector
  val full = compact(100000, entries)
  val target = boundary.resolve(segment(100000)); Files.write(target, full)
  val limited = SnapshotStore.open(boundary.toFile)
  try {
    rejected("ancestry growth beyond real cap")(limited.commit(limited.recovered._1, limited.recovered._2))
    assert(Files.readAllBytes(target).sameElements(full))
    limited.commit(mutable.LongMap.empty[SearchBehavior.Doc], mutable.Map.empty[String, mutable.LongMap[Int]], resetEpoch = true)
    awaitCondition("fresh epoch did not reclaim bounded history")(limited.segmentCount == 1)
    assert(!limited.containsCheckpoint(epoch, 99999, entries.last._2))
  } finally limited.close()
  val cleared = SnapshotStore.open(boundary.toFile)
  try assert(cleared.recovered._1.isEmpty && cleared.recovered._2.isEmpty) finally cleared.close()
  println("PASS real ancestry cap rejects without mutation; explicit new epoch remains possible and durable")
}
