package parrot.protocol.jvm

import akka.actor.typed.scaladsl.Behaviors
import akka.actor.typed.{ActorRef, ActorSystem, Behavior}
import AdminV2Codec._
import AdminV2Codec.AdminCommandV2._
import AdminV2Executor._
import scala.collection.mutable
import scala.util.{Failure, Success, Try}

/** B5 AdminPort：admin-v2 四命令的 akka 方言 Port（可测）。
  *
  * 设计：纯 actor 无 Netty 依赖——transport 层收到 SYSTEM_EVENT(tag 0x03)
  * 帧 → `AdminPort.CmdIn(cid, replyTo, payload)`；本 actor 执行四命令并
  * 回 `CmdOut(frame 字节)`。deploy 的反射加载在本 actor 内同步执行
  * （单线程顺序化——登记表无锁）。
  *
  * spawn 策略：实例 actor 直接 spawn 在 system 下（/user/{name}[-{i}]
  * 走 guardian 同层——akka typed 无 user guardian 直挂，实际路径
  * /user/... 由 BridgeActor resolve 前缀映射吸收）。
  */
object AdminPort {

  sealed trait Msg
  /** transport → admin：SYSTEM_EVENT 帧内 admin-v2 命令（sink 随帧携带——多连接安全）。 */
  final case class CmdIn(cid: Long, replyTo: String, payload: Array[Byte], sink: ReplySink) extends Msg
  /** 供测试直接查询登记表快照。 */
  final case class Snapshot(req: ActorRef[Map[String, ComponentEntry]]) extends Msg

  def apply(system: ActorSystem[_]): Behavior[Msg] =
    Behaviors.setup { _ =>
      active(system, Map.empty)
    }

  private def active(
      system: ActorSystem[_],
      components: Map[String, ComponentEntry]
  ): Behavior[Msg] =
    Behaviors.receiveMessage {
      case CmdIn(cid, replyTo, payload, sink) =>
        val (newComps, reply) = Try(execute(system, components, payload)) match {
          case Success((nc, rep)) => (nc, rep)
          case Failure(e: AdminException) =>
            (components, AdminReplyV2.Failed(extractReqId(payload, cid), e.code, e.getMessage))
          case Failure(e: AdminV2Codec.CodecException) =>
            (components, AdminReplyV2.Failed(extractReqId(payload, cid), ErrDialectMismatch, e.getMessage))
          case Failure(e) =>
            (components, AdminReplyV2.Failed(extractReqId(payload, cid), ErrArtifactFetch, Option(e.getMessage).getOrElse(e.getClass.getSimpleName)))
        }
        sink.send(cid, replyTo, encodeReply(reply))
        active(system, newComps)

      case Snapshot(req) =>
        req ! components
        Behaviors.same
    }

  /** 回帧出口（transport 注入；测试可直接消费）。 */
  trait ReplySink { def send(cid: Long, replyTo: String, payload: Array[Byte]): Unit }

  /** 从 payload 恢复 req_id（解码失败前的 Failed 回执用——cid 即 req_id）。 */
  private def extractReqId(payload: Array[Byte], cid: Long): Long =
    Try {
      val r = decodeCommand(payload)
      r.reqId
    }.getOrElse(cid)

  // ---------------- 四命令实现 ----------------

  private def execute(
      system: ActorSystem[_],
      components: Map[String, ComponentEntry],
      payload: Array[Byte]
  ): (Map[String, ComponentEntry], AdminReplyV2) =
    decodeCommand(payload) match {
      case Deploy(reqId, comp) => deploy(system, components, reqId, comp)
      case Drain(reqId, prefix, timeoutMs) =>
        (components, drain(components, reqId, prefix, timeoutMs))
      case Stop(reqId, prefix) =>
        stop(components, reqId, prefix)
      case Status(reqId, prefix) =>
        (components, status(components, reqId, prefix))
    }

  /** Deploy：child loader → 反射 main_class → ComponentSpi.behavior → spawn。
    * 返回（新登记表, 回执）。同名组件已存在 → 先 Stop（原子替换语义——
    * 与 erl 热替换对偶）。
    */
  private def deploy(
      system: ActorSystem[_],
      components: Map[String, ComponentEntry],
      reqId: Long,
      comp: ComponentDeploy
  ): (Map[String, ComponentEntry], AdminReplyV2) = {
    val base = components.get(comp.name).map { old =>
      old.instances.foreach(h => Try(h.stopper()))
      old.loader.foreach(l => Try(l.close()))
      components - comp.name
    }.getOrElse(components)

    val loader = buildLoader(comp.artifact) // AdminException（0x0A02/0x0A00）直接上抛

    val cl = loader.getOrElse(getClass.getClassLoader)
    val mainClass = comp.artifact match {
      case AdminV2Codec.ArtifactRef.Jvm(mc, _) => mc
      case other => throw new AdminException(ErrDialectMismatch, s"not jvm artifact: ${kindOf(other)}")
    }

    val paths = expandPaths(comp.name, comp.instances)
    val handles = mutable.ArrayBuffer.empty[InstanceHandle]
    try {
      paths.foreach { path =>
        val cls = Class.forName(mainClass, true, cl)
        if (!classOf[ComponentSpi].isAssignableFrom(cls))
          throw new AdminException(
            ErrSpawnFailed,
            s"$mainClass does not implement ComponentSpi"
          )
        val spi = cls.getDeclaredConstructor().newInstance().asInstanceOf[ComponentSpi]
        val ctx = ComponentContext(path, comp.version, comp.config)
        // 外包 StopWrapper（StopSignal → 级联停 child impl）——顶层无
        // PoisonPill，ask 语义经 wrapper 转发 impl
        val ref = system.systemActorOf(wrappingBehavior(spi, ctx), actorNameOf(path))
        handles += InstanceHandle(path, spi, () => tellStop(ref))
      }
    } catch {
      case e: AdminException => throw e
      case e: Exception =>
        // 回滚已 spawn 实例（依赖序装配失败回滚语义同族）
        handles.foreach(h => Try(h.stopper()))
        throw new AdminException(ErrSpawnFailed, s"spawn ${comp.name} failed: ${e.getMessage}")
    }

    val entry = ComponentEntry(comp.name, comp.version, paths, loader, handles.toVector)
    (base + (comp.name -> entry), AdminReplyV2.Deployed(reqId, paths))
  }

  /** Drain：全部实例 drain 钩子 + 共享 deadline；超时实例 aborted。 */
  private def drain(
      components: Map[String, ComponentEntry],
      reqId: Long,
      prefix: String,
      timeoutMs: Long
  ): AdminReplyV2 = {
    val matched = components.values.filter(_.paths.exists(matchesPrefix(_, prefix))).toVector
    if (matched.isEmpty)
      throw new AdminException(ErrComponentNotFound, s"no component under $prefix")
    val deadline = System.nanoTime() + math.min(timeoutMs, 60000) * 1000000L
    var drained = 0L
    var aborted = 0L
    matched.foreach { entry =>
      entry.instances.foreach { h =>
        val fut = Try(h.spi.parrotDrain()).getOrElse(null)
        val ok = if (fut == null) true
        else {
          val remain = deadline - System.nanoTime()
          if (remain <= 0) false
          else Try(fut.get(remain, java.util.concurrent.TimeUnit.NANOSECONDS)).isSuccess
        }
        if (ok) { Try(h.stopper()); drained += 1 }
        else aborted += 1 // 超时：实例保留运行（与 ray drain 语义一致）
      }
    }
    AdminReplyV2.Drained(reqId, drained, aborted)
  }

  /** Stop：立即停 + 登记/加载器清理。返回新登记表。 */
  private def stop(
      components: Map[String, ComponentEntry],
      reqId: Long,
      prefix: String
  ): (Map[String, ComponentEntry], AdminReplyV2) = {
    val matched = components.values.filter(_.paths.exists(matchesPrefix(_, prefix))).toVector
    if (matched.isEmpty)
      throw new AdminException(ErrComponentNotFound, s"no component under $prefix")
    var next = components
    matched.foreach { entry =>
      entry.instances.foreach(h => Try(h.stopper()))
      entry.loader.foreach(l => Try(l.close()))
      next = next - entry.name
    }
    (next, AdminReplyV2.Stopped(reqId))
  }

  /** Status：登记表（探活首期=登记即运行——akka ref 本地存活）。 */
  private def status(
      components: Map[String, ComponentEntry],
      reqId: Long,
      prefix: String
  ): AdminReplyV2 = {
    val matched = components.values.filter(_.paths.exists(matchesPrefix(_, prefix))).toVector
    if (matched.isEmpty)
      throw new AdminException(ErrComponentNotFound, s"no component under $prefix")
    val states = matched.flatMap { entry =>
      entry.paths.map(p => ComponentState(p, "running", entry.version))
    }
    AdminReplyV2.StatusReply(reqId, states)
  }

  private def actorNameOf(path: String): String = {
    val n = path.stripPrefix("/user/")
    if (n.isEmpty) throw new AdminException(ErrSpawnFailed, s"bad path $path")
    n
  }

  /** 顶层实例停止：akka typed 顶层 actor 无 PoisonPill——包一层
    * StopWrapper：其行为收到任何 StopSignal 即 Behaviors.stopped；
    * 目标 SPI behavior 作为其子，随父停止级联（akka 停止协议：
    * 父停 → 子先停）。
    */
  private sealed trait StopSignal
  private case object StopNow extends StopSignal

  private def tellStop(ref: akka.actor.typed.ActorRef[StopSignal]): Unit = ref ! StopNow

  private def wrappingBehavior(spi: ComponentSpi, ctx: ComponentContext): Behavior[Any] =
    Behaviors
      .setup[Any] { outer =>
        val child = outer.spawn[Any](spi.behavior(ctx), "impl")
        Behaviors.receiveMessage[Any] {
          case StopNow =>
            Behaviors.stopped // 父停 → akka 自动停 child（停止协议级联）
          case BridgeActor.BridgeAsk(key, payload, replyTo) =>
            child.unsafeUpcast[Any] ! BridgeActor.BridgeAsk(key, payload, replyTo)
            Behaviors.same
          case BridgeActor.BridgeTell(key, payload) =>
            child.unsafeUpcast[Any] ! BridgeActor.BridgeTell(key, payload)
            Behaviors.same
          case other =>
            child.unsafeUpcast[Any] ! other
            Behaviors.same
        }
      }
}
