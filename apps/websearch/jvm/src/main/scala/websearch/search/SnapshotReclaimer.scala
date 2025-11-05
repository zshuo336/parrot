package websearch.search

import java.io.{File, IOException}
import java.nio.channels.FileChannel
import java.nio.file.{Files, Path, StandardOpenOption}
import java.util.concurrent.{LinkedBlockingQueue, TimeUnit}
import java.util.concurrent.atomic.{AtomicBoolean, AtomicInteger, AtomicReference}
import scala.util.control.NonFatal

/** Only receives immutable generations already superseded by a durable head.
  * Stop and join before releasing the directory lease; unfinished work is found
  * again on reopen. No live generation or outside-directory file is enqueued.
  */
private[search] final class SnapshotReclaimer(directory: Path, initialCount: Int) extends AutoCloseable {
  private val physical = new AtomicInteger(initialCount)
  private val failure = new AtomicReference[IOException]()
  private val stopping = new AtomicBoolean(false)
  private val monitor = new Object
  private val batches = new LinkedBlockingQueue[Vector[(File, Boolean)]](100001)
  private var worker: Thread = null

  def check(): Unit = {
    val error = failure.get()
    if (error != null) throw new IOException("index reclamation failed; reopen before further requests", error)
    if (stopping.get()) throw new IOException("index reclaimer is closed")
  }
  def count: Int = { check(); physical.get() }
  def published(): Unit = { physical.incrementAndGet(); () }
  def reservePublication(): Unit = monitor.synchronized {
    val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2)
    check()
    // One new head may coexist with the entire bounded legacy inventory.
    while (physical.get() > 100000) {
      check()
      val remaining = deadline - System.nanoTime()
      if (remaining <= 0) throw new IOException("index reclamation has not freed publication capacity")
      TimeUnit.NANOSECONDS.timedWait(monitor, remaining)
    }
  }
  def submit(files: Vector[File], temporary: Vector[File] = Vector.empty): Unit = {
    if (files.isEmpty && temporary.isEmpty) return
    check()
    val work = files.map(_ -> true) ++ temporary.map(_ -> false)
    if (!batches.offer(work)) throw new IOException("index reclamation queue budget exhausted")
    if (worker == null) {
      worker = new Thread(() => drain(), "websearch-index-reclaimer")
      worker.setDaemon(true)
      worker.start()
    }
  }
  private def syncDirectory(): Unit = {
    val channel = FileChannel.open(directory, StandardOpenOption.READ)
    try channel.force(true) finally channel.close()
  }
  private def drain(): Unit = {
    try {
      while (!stopping.get()) {
        val batch = batches.poll(100, TimeUnit.MILLISECONDS)
        if (batch != null) {
          var removed = false
          val iterator = batch.iterator
          while (iterator.hasNext && !stopping.get()) {
            val (file, generation) = iterator.next()
            Files.deleteIfExists(file.toPath)
            if (generation) physical.decrementAndGet()
            removed = true
            monitor.synchronized { monitor.notifyAll() }
          }
          if (removed) syncDirectory()
        }
      }
    } catch {
      case NonFatal(error) =>
        failure.compareAndSet(null, new IOException("obsolete index cleanup failed", error))
        monitor.synchronized { monitor.notifyAll() }
    }
  }
  override def close(): Unit = {
    stopping.set(true)
    monitor.synchronized { monitor.notifyAll() }
    // Never release the lease while a worker could still unlink old paths.
    var interrupted = false
    if (worker != null) {
      var done = false
      while (!done) {
        try { worker.join(); done = true }
        catch { case _: InterruptedException => interrupted = true }
      }
    }
    batches.clear()
    if (interrupted) Thread.currentThread().interrupt()
  }
}
