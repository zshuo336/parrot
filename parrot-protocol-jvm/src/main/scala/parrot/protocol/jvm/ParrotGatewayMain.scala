package parrot.protocol.jvm

import akka.actor.typed.scaladsl.Behaviors
import akka.actor.typed.{ActorRef, ActorSystem, Behavior}
import BridgeActor._
import scala.concurrent.duration._

/** interop-matrix 进程入口（Rust interop-matrix 驱动）：
  *
  * 起 akka 系统 + echo/cpu actor + Netty Wire 1.0 server，
  * 打印 "PARROT_JVM_PORT=<port>" 到 stdout 供 Rust 侧解析。
  *
  * 参数：args(0)=port（0=随机）；可选 args(1)=idle-seconds（默认 300——超时自动退出，防泄漏）。
  */
object ParrotGatewayMain {

  def echoAny: Behavior[Any] = Behaviors.receiveMessage {
    case BridgeAsk(_, payload, replyTo) =>
      // 回包 type_key 用 Rust 注册的 reply 键（codec 按回包 key 解码）
      replyTo ! BridgeReplyOk("bin:parrot.interop.Echoed#v1", payload)
      Behaviors.same
    case BridgeTell(_, _) => Behaviors.same
    case _                => Behaviors.same
  }

  /** cpu：payload 为 bincode varint u64（standard 配置 LEB128 无 zigzag），回 +1 同编码。 */
  def cpuAny: Behavior[Any] = Behaviors.receiveMessage {
    case BridgeAsk(_, payload, replyTo) =>
      // LEB128 解码
      var n    = 0L
      var i    = 0
      var p    = 0
      var cont = true
      while (cont && p < payload.length && i < 10) {
        val b = payload(p).toLong & 0xFF
        n |= (b & 0x7F) << (7 * i)
        i += 1; p += 1
        if ((b & 0x80) == 0) cont = false
      }
      // LEB128 编码 n+1
      var m   = n + 1
      val buf = new scala.collection.mutable.ArrayBuffer[Byte](10)
      while (m >= 0x80) { buf += ((m & 0x7F) | 0x80).toByte; m >>>= 7 }
      buf += m.toByte
      replyTo ! BridgeReplyOk("bin:parrot.interop.CpuResult#v1", buf.toArray)
      Behaviors.same
    case BridgeTell(_, _) => Behaviors.same
    case _                => Behaviors.same
  }

  def main(args: Array[String]): Unit = {
    val port = if (args.length > 0) args(0).toInt else 0
    val idle = if (args.length > 1) args(1).toInt else 300

    val guardian = Behaviors.setup[BridgeMsg] { ctx =>
      // echo/cpu 直接 spawn 在 guardian 下（/user/echo、/user/cpu）
      val echo = ctx.spawn(echoAny, "echo")
      val cpu  = ctx.spawn(cpuAny, "cpu")
      // resolve：路径名 → guardian 子 ref（用本地 map，不用 ctx.child——那是调用者自己的子）
      val targets = Map("echo" -> echo.unsafeUpcast[Any], "cpu" -> cpu.unsafeUpcast[Any])
      val bridge = ctx.spawn(BridgeActor(path => targets.get(path)), "bridge")
      // Netty server
      val ext = new ParrotTransportExtension(ctx.system, bridge, "jvm-gw-1")
      ParrotServerHandler.initSystem(ctx.system)
      ext.listen(port)
      System.out.println(s"PARROT_JVM_PORT=${ext.port}")
      System.out.flush()
      Behaviors.empty
    }
    val sys = ActorSystem(guardian, "parrot-gw")
    // idle 超时退出（防测试进程泄漏）
    sys.scheduler.scheduleOnce(idle.seconds, () => sys.terminate())(sys.executionContext)
  }
}
