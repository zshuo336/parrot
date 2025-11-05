package websearch.search

import java.io.{BufferedInputStream, BufferedOutputStream, DataInputStream, DataOutputStream, File, FilterInputStream, FilterOutputStream, IOException, OutputStream}
import java.nio.{ByteBuffer, CharBuffer}
import java.nio.channels.{Channels, FileChannel, FileLock, OverlappingFileLockException}
import java.nio.charset.{CodingErrorAction, StandardCharsets}
import java.nio.file.{Files, LinkOption, Path, StandardCopyOption, StandardOpenOption}
import java.util.zip.{CRC32, CheckedOutputStream}
import scala.collection.mutable
import scala.jdk.CollectionConverters._
import scala.util.control.NonFatal

/** Immutable, complete index generations. Clear publishes an empty generation.
  * A successful write includes file force, atomic rename and directory force.
  * The parent lock is held until actor termination; never unlink the lock file.
  */
private[search] object SnapshotStore {
  import SearchBehavior.Doc
  type Postings = mutable.Map[String, mutable.LongMap[Int]]
  val MaxFileBytes = 256L * 1024 * 1024
  val MaxStringBytes = 16 * 1024 * 1024
  val MaxEntries = 1000000
  val MaxPostings = 16000000
  private val Magic1 = 0x50525753 // PRWS
  private val Magic2 = 0x49445832 // IDX2
  private val Magic3 = 0x49445833 // IDX3: durable crawl epoch
  private val Magic4 = 0x49445834 // IDX4: parent generation and SHA256
  private val Magic5 = 0x49445835 // IDX5: generation and compact current-epoch ancestry
  private val MaxAncestors = 100000
  final class HistoryPreparing extends RuntimeException("index history preparation is pending; poll PrepareIndex")
  private case class History(generation: Long, entries: Vector[(Long, Vector[Byte])])
  private def validateHistory(history: History): Unit = {
    if (history.generation < 0 || history.entries.size > MaxAncestors) invalid("snapshot ancestry budget exhausted")
    var previous = -1L
    history.entries.foreach { case (generation, digest) =>
      if (generation <= previous || generation >= history.generation || digest.length != 32)
        invalid("invalid snapshot ancestry order")
      previous = generation
    }
  }

  private def invalid(message: String): Nothing = throw new IOException(message)
  private def utf8(text: String): Array[Byte] = {
    val encoder = StandardCharsets.UTF_8.newEncoder()
      .onMalformedInput(CodingErrorAction.REPORT).onUnmappableCharacter(CodingErrorAction.REPORT)
    val encoded = encoder.encode(CharBuffer.wrap(text))
    if (encoded.remaining() > MaxStringBytes) invalid("snapshot string exceeds byte budget")
    val bytes = new Array[Byte](encoded.remaining()); encoded.get(bytes); bytes
  }
  private def syncDirectory(path: Path): Unit = {
    val channel = FileChannel.open(path, StandardOpenOption.READ)
    try channel.force(true) finally channel.close()
  }
  private class LimitedOutput(out: OutputStream) extends FilterOutputStream(out) {
    private var size = 0L
    override def write(b: Int): Unit = { reserve(1); out.write(b) }
    override def write(b: Array[Byte], off: Int, len: Int): Unit = { reserve(len); out.write(b, off, len) }
    private def reserve(n: Int): Unit = {
      if (n > MaxFileBytes - size) invalid("snapshot exceeds file budget")
      size += n
    }
  }

  def write(file: File, docs: mutable.LongMap[Doc], postings: Postings): Unit =
    writeState(file, docs, postings, None)

  private def writeState(file: File, docs: mutable.LongMap[Doc], postings: Postings, epoch: Option[Vector[Byte]], parent: Option[(Long, Vector[Byte])] = None, history: Option[History] = None): Unit = {
    if (parent.exists(p => p._1 < -1 || p._2.length != 32) || (parent.isDefined && epoch.isEmpty)) invalid("invalid snapshot parent")
    if (epoch.exists(_.length != 16)) invalid("invalid crawl epoch")
    if (history.isDefined && (epoch.isEmpty || parent.isDefined)) invalid("invalid compact snapshot history")
    history.foreach(validateHistory)
    if (docs.size > MaxEntries || postings.size > MaxEntries ||
        postings.valuesIterator.map(_.size.toLong).sum > MaxPostings)
      invalid("snapshot exceeds entry budget")
    val target = file.toPath
    val directory = target.toAbsolutePath.getParent
    var temporary: Path = null
    try {
      temporary = Files.createTempFile(directory, ".snapshot-", ".tmp")
      val channel = FileChannel.open(temporary, StandardOpenOption.WRITE)
      try {
        val crc = new CRC32
        val out = new DataOutputStream(new CheckedOutputStream(
          new BufferedOutputStream(new LimitedOutput(Channels.newOutputStream(channel))), crc))
        def string(value: String): Unit = { val bytes = utf8(value); out.writeInt(bytes.length); out.write(bytes) }
        out.writeInt(Magic1); out.writeInt(if (history.isDefined) Magic5 else if (parent.isDefined) Magic4 else if (epoch.isDefined) Magic3 else Magic2)
        epoch.foreach(value => out.write(value.toArray))
        parent.foreach { case (generation, digest) => out.writeLong(generation); out.write(digest.toArray) }
        history.foreach { value =>
          out.writeLong(value.generation); out.writeInt(value.entries.size)
          value.entries.foreach { case (generation, digest) => out.writeLong(generation); out.write(digest.toArray) }
        }
        out.writeInt(docs.size)
        docs.iterator.toVector.sortBy(_._1).foreach { case (id, doc) =>
          out.writeLong(id); string(doc.url); string(doc.title); string(doc.snippet)
        }
        out.writeInt(postings.size)
        postings.iterator.toVector.sortBy(_._1).foreach { case (term, ds) =>
          string(term); out.writeInt(ds.size)
          ds.iterator.toVector.sortBy(_._1).foreach { case (id, tf) => out.writeLong(id); out.writeInt(tf) }
        }
        out.writeInt(crc.getValue.toInt)
        out.flush()
        channel.force(true)
      } finally channel.close()
      Files.move(temporary, target, StandardCopyOption.ATOMIC_MOVE)
      temporary = null
      syncDirectory(directory)
    } catch {
      case e: IOException => throw e
      case NonFatal(e) => throw new IOException("snapshot publication failed", e)
    } finally if (temporary != null) {
      // Preserve the original failure; unfinished files are never recovery inputs.
      try Files.deleteIfExists(temporary) catch { case _: IOException => () }
    }
  }

  private class Counted(in: java.io.InputStream) extends FilterInputStream(in) {
    var count = 0L
    override def read(): Int = { val b = in.read(); if (b >= 0) count += 1; b }
    override def read(b: Array[Byte], off: Int, len: Int): Int = {
      val n = in.read(b, off, len); if (n > 0) count += n; n
    }
  }
  def read(file: File): (mutable.LongMap[Doc], Postings) = {
    val (docs, postings, _, _, _) = readState(file); (docs, postings)
  }
  private def readState(file: File): (mutable.LongMap[Doc], Postings, Option[Vector[Byte]], Option[(Long, Vector[Byte])], Option[History]) = {
    val channel = FileChannel.open(file.toPath, StandardOpenOption.READ, LinkOption.NOFOLLOW_LINKS)
    try {
      if (!Files.isRegularFile(file.toPath, LinkOption.NOFOLLOW_LINKS)) invalid("snapshot is not a regular file")
      val size = channel.size()
      if (size < 8 || size > MaxFileBytes) invalid("invalid snapshot size")
      val header = new DataInputStream(Channels.newInputStream(channel))
      val version2 = header.readInt() == Magic1
      val format = if (version2) header.readInt() else 0
      if (version2) {
        if (size < 20 || (format != Magic2 && format != Magic3 && format != Magic4 && format != Magic5)) invalid("unknown snapshot format")
        if ((format == Magic3 || format == Magic4 || format == Magic5) && size < 36) invalid("truncated crawl epoch")
        // Check the complete immutable file before allocating application maps.
        channel.position(0)
        val crc = new CRC32
        val buffer = ByteBuffer.allocate(65536)
        var remaining = size - 4
        while (remaining > 0) {
          buffer.clear(); buffer.limit(math.min(remaining, buffer.capacity.toLong).toInt)
          val n = channel.read(buffer)
          if (n <= 0) invalid("truncated snapshot checksum input")
          crc.update(buffer.array(), 0, n); remaining -= n
        }
        if (java.lang.Integer.toUnsignedLong(header.readInt()) != crc.getValue) invalid("snapshot checksum mismatch")
      }
      channel.position(0)
      val counted = new Counted(new BufferedInputStream(Channels.newInputStream(channel)))
      val in = new DataInputStream(counted)
      val contentSize = size - (if (version2) 4 else 0)
      if (version2) { in.readInt(); in.readInt() }
      val epoch = if (format == Magic3 || format == Magic4 || format == Magic5) {
        val bytes = new Array[Byte](16); in.readFully(bytes); Some(bytes.toVector)
      } else None
      val parent = if (format == Magic4) {
        val generation = in.readLong()
        val bytes = new Array[Byte](32); in.readFully(bytes)
        if (generation < -1 || (generation == -1 && bytes.exists(_ != 0))) invalid("invalid snapshot parent")
        Some(generation -> bytes.toVector)
      } else None
      val history = if (format == Magic5) {
        val generation = in.readLong()
        val size = in.readInt()
        if (size < 0 || size > MaxAncestors || size.toLong > (contentSize - counted.count - 8) / 40)
          invalid("invalid snapshot ancestry count")
        val entries = Vector.fill(size) {
          val ancestor = in.readLong()
          val bytes = new Array[Byte](32); in.readFully(bytes)
          ancestor -> bytes.toVector
        }
        val value = History(generation, entries); validateHistory(value); Some(value)
      } else None
      def remaining: Long = contentSize - counted.count
      def count(minBytes: Int, max: Int = MaxEntries): Int = {
        val n = in.readInt()
        if (n < 0 || n > max || n.toLong > remaining / minBytes) invalid("invalid snapshot count")
        n
      }
      def string(): String = {
        if (!version2) {
          val value = in.readUTF()
          utf8(value) // reject unpaired UTF-16 surrogates in legacy modified UTF-8
          value
        } else {
          val n = in.readInt()
          if (n < 0 || n > MaxStringBytes || n > remaining) invalid("invalid snapshot string length")
          val bytes = new Array[Byte](n); in.readFully(bytes)
          StandardCharsets.UTF_8.newDecoder().onMalformedInput(CodingErrorAction.REPORT)
            .onUnmappableCharacter(CodingErrorAction.REPORT).decode(ByteBuffer.wrap(bytes)).toString
        }
      }
      val docs = mutable.LongMap.empty[Doc]
      val postings = mutable.Map.empty[String, mutable.LongMap[Int]]
      val nd = count(if (version2) 20 else 14)
      (0 until nd).foreach { _ =>
        val id = in.readLong()
        if (docs.contains(id)) invalid("duplicate snapshot document")
        docs.update(id, Doc(string(), string(), string()))
      }
      val nt = count(if (version2) 8 else 6)
      var totalPostings = 0L
      (0 until nt).foreach { _ =>
        val term = string()
        if (postings.contains(term)) invalid("duplicate snapshot term")
        val n = count(12, MaxPostings)
        totalPostings += n
        if (totalPostings > MaxPostings) invalid("snapshot exceeds posting budget")
        val ds = mutable.LongMap.empty[Int]
        (0 until n).foreach { _ =>
          val id = in.readLong()
          if (ds.contains(id)) invalid("duplicate snapshot posting")
          ds.update(id, in.readInt()) // preserve all u32 bits
        }
        postings.update(term, ds)
      }
      if (remaining != 0) invalid("trailing snapshot bytes")
      (docs, postings, epoch, parent, history)
    } finally channel.close()
  }

  def files(directory: File): Vector[(Long, File)] = {
    val stream = Files.newDirectoryStream(directory.toPath)
    try {
      val found = mutable.ArrayBuffer.empty[(Long, File)]
      val pattern = "seg-([0-9]+)\\.segment".r
      stream.iterator.asScala.foreach { path =>
        if (path.getFileName.toString.endsWith(".segment")) {
          // One published v5 head may coexist with a full legacy inventory if
          // the process died before reclamation. Owned validates this transition.
          if (found.size >= MaxAncestors + 1) invalid("too many index generations")
          val generation = path.getFileName.toString match {
            case pattern(value) => try value.toLong catch { case _: NumberFormatException => invalid("invalid snapshot generation") }
            case _ => invalid("unknown segment filename")
          }
          if (!Files.isRegularFile(path, LinkOption.NOFOLLOW_LINKS)) invalid("segment is not a regular file")
          found += generation -> path.toFile
        }
      }
      if (found.map(_._1).distinct.size != found.size) invalid("duplicate snapshot generation")
      found.sortBy(_._1).toVector
    } finally stream.close()
  }

  private def hashFile(file: File, cancelled: () => Boolean = () => false): Vector[Byte] = {
    val channel = FileChannel.open(file.toPath, StandardOpenOption.READ, LinkOption.NOFOLLOW_LINKS)
    try {
      if (!Files.isRegularFile(file.toPath, LinkOption.NOFOLLOW_LINKS) || channel.size() > MaxFileBytes)
        invalid("invalid snapshot history file")
      val hash = java.security.MessageDigest.getInstance("SHA-256")
      val buffer = ByteBuffer.allocate(65536)
      var remaining = channel.size()
      while (remaining > 0) {
        if (cancelled()) throw new java.io.InterruptedIOException("history preparation cancelled")
        buffer.clear(); buffer.limit(math.min(remaining, buffer.capacity.toLong).toInt)
        val n = channel.read(buffer)
        if (n <= 0) invalid("truncated snapshot history")
        hash.update(buffer.array(), 0, n); remaining -= n
      }
      hash.digest().toVector
    } finally channel.close()
  }

  final class Owned private[SnapshotStore](val directory: File, channel: FileChannel, lock: FileLock) extends AutoCloseable {
    private var generations = files(directory)
    private val loaded = generations.lastOption.map { case (_, file) => readState(file) }
      .getOrElse((mutable.LongMap.empty[Doc], mutable.Map.empty[String, mutable.LongMap[Int]], None, None, None))
    val recovered: (mutable.LongMap[Doc], Postings) = (loaded._1, loaded._2)
    private var epoch = loaded._3
    private var next = generations.lastOption.map(_._1).getOrElse(-1L)
    private var digest = generations.lastOption.map(p => hashFile(p._2)).getOrElse(Vector.fill[Byte](32)(0))
    private val reclaimer = new SnapshotReclaimer(directory.toPath, generations.size)
    private var poisoned = false
    private var ancestry = loaded._5.map(_.entries)
    private var preparation: SnapshotPreparation = null
    private val history = mutable.LongMap.from(generations)
    loaded._5.foreach(h => if (h.generation != next) invalid("snapshot generation mismatch"))
    if (generations.size > MaxAncestors && ancestry.isEmpty) invalid("too many legacy index generations")
    // Inspect reserved unpublished files only after validating the authoritative
    // head and while holding the lease. Reject links before starting any worker.
    private def abandonedFiles(): Vector[File] = {
      val files = mutable.ArrayBuffer.empty[File]
      val stream = Files.newDirectoryStream(directory.toPath, ".snapshot-*.tmp")
      try stream.iterator.asScala.foreach { path =>
        if (files.size >= MaxAncestors) invalid("too many unfinished snapshots")
        if (!Files.isRegularFile(path, LinkOption.NOFOLLOW_LINKS)) invalid("unfinished snapshot is not a regular file")
        files += path.toFile
      } finally stream.close()
      files.toVector
    }
    // A complete, validated v5 head is the sole authority after interruption.
    if (ancestry.isDefined) reclaim(abandonedFiles()) else reclaimer.submit(Vector.empty, abandonedFiles())
    generations = Vector.empty

    private def reclaim(temporary: Vector[File] = Vector.empty): Unit = {
      val obsolete = history.iterator.filter(_._1 != next).toVector.sortBy(_._1)
      reclaimer.submit(obsolete.map(_._2), temporary)
      obsolete.foreach { case (generation, _) => history.remove(generation) }
    }
    private def legacyHistory(files: Map[Long, File], head: Long, headHash: Vector[Byte], identity: Option[Vector[Byte]],
        cancelled: java.util.concurrent.atomic.AtomicBoolean, verified: java.util.concurrent.atomic.AtomicLong): Vector[(Long, Vector[Byte])] = {
      val result = mutable.ArrayBuffer.empty[(Long, Vector[Byte])]
      var cursor = head
      var expected = headHash
      var done = false
      while (cursor >= 0 && !done) {
        if (cancelled.get()) throw new java.io.InterruptedIOException("history preparation cancelled")
        val file = files.getOrElse(cursor, throw new IOException("missing snapshot history"))
        if (hashFile(file, () => cancelled.get()) != expected) invalid("snapshot history checksum mismatch")
        verified.incrementAndGet()
        val input = new DataInputStream(Files.newInputStream(file.toPath, LinkOption.NOFOLLOW_LINKS))
        try {
          if (input.readInt() != Magic1) done = true
          else {
            val format = input.readInt()
            if (format != Magic3 && format != Magic4 && format != Magic5) done = true
            else {
              val storedEpoch = new Array[Byte](16); input.readFully(storedEpoch)
              if (!identity.contains(storedEpoch.toVector)) done = true
              else {
                result += cursor -> expected
                if (result.size > MaxAncestors) invalid("snapshot ancestry budget exhausted")
                if (format == Magic3) done = true
                else if (format == Magic5) {
                  val compact = readState(file)._5.get
                  if (compact.generation != cursor) invalid("snapshot generation mismatch")
                  result ++= compact.entries.reverse
                  done = true
                } else {
                  val parent = input.readLong()
                  val bytes = new Array[Byte](32); input.readFully(bytes)
                  if (parent < -1 || parent >= cursor || (parent == -1 && bytes.exists(_ != 0)))
                    invalid("invalid snapshot history order")
                  cursor = parent; expected = bytes.toVector
                }
              }
            }
          }
        } finally input.close()
      }
      result.reverse.toVector
    }
    def prepareHistory(): (Boolean, Long) = {
      reclaimer.check()
      if (ancestry.isDefined || epoch.isEmpty) return (true, 0L)
      if (preparation == null) {
        val files = history.iterator.toMap
        val head = next; val headHash = digest; val identity = epoch
        preparation = new SnapshotPreparation((cancelled, verified) => legacyHistory(files, head, headHash, identity, cancelled, verified))
      }
      preparation.progress
    }
    private def stopPreparation(): Unit = if (preparation != null) {
      preparation.close(); preparation = null
    }
    private def preparedHistory(): Vector[(Long, Vector[Byte])] = {
      prepareHistory()
      preparation.result.getOrElse(throw new HistoryPreparing)
    }
    def crawlDigest: Vector[Byte] = digest
    def containsCheckpoint(expectedEpoch: Vector[Byte], generation: Long, expectedHash: Vector[Byte]): Boolean = {
      if (!epoch.contains(expectedEpoch) || generation < 0 || generation > next || expectedHash.length != 32) return false
      ancestry match {
        case Some(entries) =>
          if (generation == next) return digest == expectedHash
          var low = 0; var high = entries.size - 1
          while (low <= high) {
            val middle = low + (high - low) / 2
            val (candidate, hash) = entries(middle)
            if (candidate == generation) return hash == expectedHash
            if (candidate < generation) low = middle + 1 else high = middle - 1
          }
          false
        case None =>
          if (generation == next) digest == expectedHash
          else preparedHistory().exists { case (candidate, hash) => candidate == generation && hash == expectedHash }
      }
    }
    def segmentCount: Int = reclaimer.count
    def matchesEpoch(bytes: Array[Byte]): Boolean = epoch.contains(bytes.toVector)
    def crawlState: (Vector[Byte], Long) =
      (epoch.getOrElse(throw new IOException("crawl identity has not been committed")), next)
    def ensureCrawlState(docs: mutable.LongMap[Doc], postings: Postings): Unit =
      if (epoch.isEmpty) commit(docs, postings)
    def commit(docs: mutable.LongMap[Doc], postings: Postings, resetEpoch: Boolean = false): Unit = {
      if (poisoned) invalid("snapshot store failed; reopen before further writes")
      if (next == Long.MaxValue) invalid("snapshot generation budget exhausted")
      reclaimer.reservePublication()
      val generation = next + 1
      val target = new File(directory, f"seg-$generation%020d.segment")
      if (Files.exists(target.toPath, LinkOption.NOFOLLOW_LINKS)) invalid("snapshot target already exists")
      val identity = if (resetEpoch || epoch.isEmpty) {
        val uuid = java.util.UUID.randomUUID()
        ByteBuffer.allocate(16).putLong(uuid.getMostSignificantBits).putLong(uuid.getLeastSignificantBits).array().toVector
      } else epoch.get
      val ancestors = if (resetEpoch || epoch.isEmpty) Vector.empty else
        ancestry.map(_ :+ (next -> digest)).getOrElse(preparedHistory())
      val compact = History(generation, ancestors)
      validateHistory(compact)
      stopPreparation()
      try {
        writeState(target, docs, postings, Some(identity), history = Some(compact))
        digest = hashFile(target)
        history.update(generation, target)
        epoch = Some(identity)
        next = generation
        ancestry = Some(ancestors)
        reclaimer.published()
        reclaim()
      } catch {
        case NonFatal(error) => poisoned = true; throw error
      }
    }
    override def close(): Unit = {
      try stopPreparation() finally {
        try reclaimer.close() finally { try lock.release() finally channel.close() }
      }
    }
  }
  def open(directory: File): Owned = {
    val parent = directory.toPath.toAbsolutePath.getParent
    Files.createDirectories(parent)
    val channel = FileChannel.open(parent.resolve(".search.lock"), StandardOpenOption.CREATE, StandardOpenOption.WRITE)
    try {
      val lock = try channel.tryLock() catch { case _: OverlappingFileLockException => null }
      if (lock == null) invalid("search data directory already has an owner")
      Files.createDirectories(directory.toPath)
      if (!Files.isDirectory(directory.toPath, LinkOption.NOFOLLOW_LINKS)) invalid("index path is not a directory")
      syncDirectory(parent)
      new Owned(directory, channel, lock)
    } catch { case NonFatal(e) => channel.close(); throw e }
  }
}
