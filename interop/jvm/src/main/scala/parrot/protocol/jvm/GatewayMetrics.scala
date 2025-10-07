package parrot.protocol.jvm

import java.util.concurrent.atomic.{AtomicInteger, AtomicLong}

/** 观测五件套：JVM 网关级指标计数器（transport 层逐帧打点）。
  *
  * 全链路共用单例（listen + 多连接）；快照经 admin-v2 MetricsReport 拉取。
  */
object GatewayMetrics {
  val startedAtMs: Long = System.currentTimeMillis()

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
}
