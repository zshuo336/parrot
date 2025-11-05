package websearch.search

import java.io.IOException
import java.util.concurrent.atomic.{AtomicBoolean, AtomicLong}
import scala.util.control.NonFatal

/** Read-only legacy verification, cancellable between bounded file reads. */
private[search] final class SnapshotPreparation(
    verify: (AtomicBoolean, AtomicLong) => Vector[(Long, Vector[Byte])]) extends AutoCloseable {
  private val cancelled = new AtomicBoolean(false)
  private val verified = new AtomicLong(0)
  @volatile private var value: Option[Vector[(Long, Vector[Byte])]] = None
  @volatile private var failure: IOException = null
  private val thread = new Thread(() => {
    try value = Some(verify(cancelled, verified))
    catch { case NonFatal(error) => failure = new IOException("legacy index history verification failed", error) }
  }, "websearch-index-preparation")
  thread.setDaemon(true)
  thread.start()

  def result: Option[Vector[(Long, Vector[Byte])]] = {
    if (failure != null) throw failure
    value
  }
  def progress: (Boolean, Long) = (result.isDefined, verified.get())
  override def close(): Unit = {
    cancelled.set(true)
    var interrupted = false
    var done = false
    while (!done) {
      try { thread.join(); done = true }
      catch { case _: InterruptedException => interrupted = true }
    }
    if (interrupted) Thread.currentThread().interrupt()
  }
}
