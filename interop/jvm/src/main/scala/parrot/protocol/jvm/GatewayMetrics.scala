package parrot.protocol.jvm

import java.util.concurrent.atomic.{AtomicInteger, AtomicLong}

/** 观测五件套：JVM 网关级指标计数器（transport 层逐帧打点）。
  *
  * 全链路共用单例（listen + 多连接）；快照经 admin-v2 MetricsReport 拉取。
  */
object GatewayMetrics {
  val startedAtMs: Long = System.currentTimeMillis()
  val epoch: String = java.util.UUID.randomUUID().toString

  final class Counters(val dummy: Int = 0) {
    val connections: AtomicInteger = new AtomicInteger
    val handshakesOk: AtomicLong = new AtomicLong
    val handshakesFailed: AtomicLong = new AtomicLong
    val asksRx: AtomicLong = new AtomicLong
    val tellsRx: AtomicLong = new AtomicLong
    val repliesTx: AtomicLong = new AtomicLong
    val replyErrs: AtomicLong = new AtomicLong
    val bytesRx: AtomicLong = new AtomicLong
    val bytesTx: AtomicLong = new AtomicLong
    val heartbeatsRx: AtomicLong = new AtomicLong
  }

  private val c = new Counters

  def counters: Counters = c

  /** Atomic unsigned u64 saturation, preserving the public Long bit representation. */
  def add(counter: AtomicLong, delta: Long): Unit = {
    require(delta >= 0, "negative metric increment")
    counter.updateAndGet { current =>
      if (java.lang.Long.compareUnsigned(current, -1L - delta) > 0) -1L else current + delta
    }
    ()
  }

  /** 快照读取（AdminPort.metricsReply 用——字段打包）。 */
  final case class Snap(
      connections: Long, handshakesOk: Long, handshakesFailed: Long,
      asksRx: Long, tellsRx: Long, repliesTx: Long, replyErrs: Long,
      bytesRx: Long, bytesTx: Long, heartbeatsRx: Long
  )

  def snapshot(): Snap = Snap(
    c.connections.get(), c.handshakesOk.get(), c.handshakesFailed.get(),
    c.asksRx.get(), c.tellsRx.get(), c.repliesTx.get(), c.replyErrs.get(),
    c.bytesRx.get(), c.bytesTx.get(), c.heartbeatsRx.get()
  )
  /** The process owner supplies only measured resources, never an inferred zero.
    * Input ownership is explicit for deterministic failure/closure tests. */
  private[jvm] def residentBytes(open: () => java.io.InputStream): Option[Long] =
    scala.util.Try {
      scala.util.Using.resource(open()) { input =>
        val bytes = input.readNBytes(65537)
        if (bytes.length > 65536) None
        else {
          val text = new String(bytes, java.nio.charset.StandardCharsets.US_ASCII)
          text.linesIterator.find(_.startsWith("VmRSS:")).flatMap { line =>
            line.trim.split("\\s+").toList match {
              case "VmRSS:" :: digits :: "kB" :: Nil if digits.nonEmpty && digits.forall(c => c >= '0' && c <= '9') =>
                scala.util.Try(Math.multiplyExact(digits.toLong, 1024L)).toOption
              case _ => None
            }
          }
        }
      }
    }.toOption.flatten

  def measured(components: Long, states: Vector[AdminV2Codec.ComponentState]): AdminV2Codec.MeasuredMetrics = {
    val c = snapshot()
    val rss = residentBytes(() => java.nio.file.Files.newInputStream(java.nio.file.Path.of("/proc/self/status")))
    val threads = scala.util.Try(java.lang.management.ManagementFactory.getThreadMXBean.getThreadCount.toLong)
      .toOption.filter(_ >= 0)
    val values = AdminV2Codec.MetricsSnapshot(System.currentTimeMillis(),
      System.getProperty("java.vm.name", "jvm") + "/" + System.getProperty("java.version", "?"),
      c.connections, c.handshakesOk, c.handshakesFailed, c.asksRx, c.tellsRx,
      c.repliesTx, c.replyErrs, c.bytesRx, c.bytesTx, c.heartbeatsRx,
      components, states, threads.getOrElse(0L), rss.getOrElse(0L), startedAtMs)
    val base = 0x1ffffL & ~((1L << 14) | (1L << 15))
    AdminV2Codec.MeasuredMetrics(values, base | (if (threads.isDefined) 1L << 14 else 0L) |
      (if (rss.isDefined) 1L << 15 else 0L), epoch, if (threads.isDefined) "jvm_threads" else "")
  }

}
