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
  * 参数：args(0)=port（0=随机）；可选 args(1)=idle-seconds（默认 300——超时自动退出，防泄漏）；
  * args 含 "parrot=h:p" 时为注册模式（主动拨号 parrot 应用——host.rs 形态）。
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

  /** CPU probe: bincode standard u64 -> checked +1; invalid inputs are CodecError. */
  def cpuAny: Behavior[Any] = Behaviors.receiveMessage {
    case BridgeAsk("bin:parrot.interop.Cpu#v1", payload, replyTo) =>
      try {
        val value = BincodeU64.decode(payload)
        replyTo ! BridgeReplyOk("bin:parrot.interop.CpuResult#v1", BincodeU64.encode(value + 1))
      } catch {
        case _: IllegalArgumentException =>
          replyTo ! BridgeReplyErr(WireFrame.ErrCode.CodecError, "invalid or overflowing bincode u64")
      }
      Behaviors.same
    case BridgeAsk(_, _, replyTo) =>
      replyTo ! BridgeReplyErr(WireFrame.ErrCode.UnknownTypeKey, "CPU probe requires bin:parrot.interop.Cpu#v1")
      Behaviors.same
    case BridgeTell(_, _) => Behaviors.same
    case _ => Behaviors.same
  }

  def main(args: Array[String]): Unit = {
    // 双模式组网（R1 自 CrawlerSearchMain 移植——业务网关删除后通用化）：
    //   [port]              —— 被动模式（listen 等 parrot 拨入）
    //   [port, parrot=h:p]  —— 注册模式（主动拨号 parrot 应用并驻留）
    //   [port, idle]        —— 被动 + idle 秒超时
    val port = if (args.length > 0) args(0).toInt else 0
    val parrotReg = args.find(_.startsWith("parrot=")).map(_.stripPrefix("parrot="))
    // R1：node_id 可注入（业务网关删除后通用化——lab 场景自报 jvm-search-1
    // 等语义名；缺省 jvm-gw-1）
    val nodeId = args.find(_.startsWith("node=")).map(_.stripPrefix("node=")).getOrElse("jvm-gw-1")
    val idle = args
      .drop(1)
      .find(_.forall(_.isDigit))
      .map(_.toInt)
      .getOrElse(300)

    val guardian = Behaviors.setup[BridgeMsg] { ctx =>
      // echo/cpu 直接 spawn 在 guardian 下（/user/echo、/user/cpu）
      val echo = ctx.spawn(CausalTrace.behavior(echoAny, ctx.system, "/user/echo"), "echo")
      val cpu  = ctx.spawn(CausalTrace.behavior(cpuAny, ctx.system, "/user/cpu"), "cpu")
      // resolve：R4——已部署组件（ComponentRoutes）优先，内置探针兜底。
      // 业务代码已迁 apps/*/jvm（ComponentSpi）；网关只剩协议骨架 + 探针。
      val targets = Map("echo" -> echo.unsafeUpcast[Any], "cpu" -> cpu.unsafeUpcast[Any])
      val routes = AdminV2Executor.ComponentRoutes(ctx.system)
      val bridge = ctx.spawn(
        BridgeActor(path =>
          routes.get(path).orElse(targets.get(path)),
          path => routes.resolveChild(path)(ctx.executionContext)
        ),
        "bridge"
      )
      // B5（DEV_09）：admin-v2 AdminPort（deploy/drain/stop/status）
      val admin = ctx.spawn(AdminPort(ctx.system), "admin-port")
      val ext = new ParrotTransportExtension(ctx.system, bridge, nodeId, Some(admin), traceContext = true)
      akka.actor.CoordinatedShutdown(ctx.system).addTask(
        akka.actor.CoordinatedShutdown.PhaseServiceUnbind, "parrot-netty-retirement") { () =>
        import scala.jdk.FutureConverters._
        ext.shutdown().asScala.map(_ => akka.Done)(ctx.executionContext)
      }
      parrotReg match {
        case Some(target) =>
          // Registration starts an owned reconnect chain and returns; ActorSystem stays alive.
          val Array(host, p) = target.split(":")
          System.out.println(s"PARROT_JVM_REGISTERING=$target")
          System.out.flush()
          ext.registerTo(host, p.toInt)
        case None =>
          ext.listen(port)
          System.out.println(s"PARROT_JVM_PORT=${ext.port}")
          System.out.flush()
      }
      Behaviors.empty
    }
    val sys = ActorSystem(guardian, "parrot-gw")
    // idle 超时退出（防测试进程泄漏）
    sys.scheduler.scheduleOnce(idle.seconds, () => sys.terminate())(sys.executionContext)
  }
}
