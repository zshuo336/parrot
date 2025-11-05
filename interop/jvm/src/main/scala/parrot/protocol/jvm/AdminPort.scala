package parrot.protocol.jvm

import akka.actor.typed.scaladsl.{ActorContext, Behaviors}
import akka.actor.typed.{ActorRef, ActorSystem, Behavior, BehaviorInterceptor, DispatcherSelector, PostStop, TypedActorContext}
import akka.actor.typed.scaladsl.adapter._
import AdminV2Codec._
import AdminV2Codec.AdminCommandV2._
import AdminV2Executor._
import java.net.URLClassLoader
import java.util.concurrent.atomic.AtomicBoolean
import scala.collection.mutable
import scala.concurrent.{Await, Promise}
import scala.concurrent.duration._
import scala.util.{Failure, Success, Try}
import scala.util.control.NonFatal

/** Bounded internal pending queue, serialized lifecycle operations on the blocking dispatcher,
  * and owned component subtrees. Deployed follows a real actor readiness barrier.
  */
object AdminPort {
  sealed trait Msg
  final case class CmdIn(cid: Long, replyTo: String, payload: Array[Byte], sink: ReplySink) extends Msg
  /** Last completed operation/retirement snapshot, not an independent health probe. */
  final case class Snapshot(req: ActorRef[Map[String, ComponentEntry]]) extends Msg
  private final case class Done(components: Map[String, ComponentEntry]) extends Msg
  private final case class Changed(components: Map[String, ComponentEntry]) extends Msg
  private final case class Retired(path: String, ref: ActorRef[Any]) extends Msg
  private case object WorkerStopped extends Msg
  final case class Limits(startup: FiniteDuration = 5.seconds, retirement: FiniteDuration = 5.seconds,
                          pendingCommands: Int = 256, pendingBytes: Long = 16L * 1024 * 1024,
                          instances: Int = 4096) {
    require(startup > Duration.Zero && retirement > Duration.Zero)
    require(startup <= 60.seconds && retirement <= 60.seconds)
    require(pendingCommands > 0 && pendingBytes > 0 && instances > 0)
  }
  trait ReplySink { def send(cid: Long, replyTo: String, payload: Array[Byte]): Unit }
  private def safeDetail(value: String): String = {
    val normalized = new String(Option(value).getOrElse("operation failed").getBytes("UTF-8"), "UTF-8")
    normalized.substring(0, normalized.offsetByCodePoints(0, math.min(1024, normalized.codePointCount(0, normalized.length))))
  }
  private def requestId(command: CmdIn): Long = command.cid
  private def failure(id: Long, code: Int, detail: String): AdminReplyV2 =
    AdminReplyV2.Failed(id, code, safeDetail(detail))
  private def send(command: CmdIn, reply: AdminReplyV2): Unit =
    try command.sink.send(command.cid, command.replyTo, encodeReply(reply))
    catch { case NonFatal(_) => () } // A lost reply sink cannot roll back committed state.

  def apply(system: ActorSystem[_], limits: Limits = Limits()): Behavior[Msg] =
    apply(system, ComponentRoutes(system), limits)

  def apply(system: ActorSystem[_], routes: ComponentRoutes): Behavior[Msg] = apply(system, routes, Limits())

  def apply(system: ActorSystem[_], routes: ComponentRoutes, limits: Limits): Behavior[Msg] = {
    require(routes.belongsTo(system), "component route scope belongs to another ActorSystem")
    setup(system, routes, limits)
  }

  private def setup(system: ActorSystem[_], scope: ComponentRoutes, limits: Limits): Behavior[Msg] = Behaviors.setup { context =>
    require(context.system == system, "AdminPort must run in its owning ActorSystem")
    implicit val routes: ComponentRoutes = scope
    val worker = context.spawn(operations(limits, context.self), "operations", DispatcherSelector.blocking())
    context.watchWith(worker, WorkerStopped)
    val queue = mutable.Queue.empty[CmdIn]
    var current: Option[CmdIn] = None
    var bytes = 0L
    var cached = Map.empty[String, ComponentEntry]
    def weight(command: CmdIn): Long = command.payload.length.toLong + command.replyTo.length * 2L
    def advance(): Unit = if (current.isEmpty && queue.nonEmpty) {
      current = Some(queue.dequeue()); worker ! current.get
    }
    Behaviors.receiveMessage[Msg] {
      case command: CmdIn =>
        // Validate before queue admission or any lifecycle effect. The wire
        // correlation ID owns the failure receipt even if the body is invalid.
        Try(decodeCommand(command.payload)) match {
          case Success(decoded) if decoded.reqId == command.cid =>
            val size = weight(command)
            if (queue.size + current.size >= limits.pendingCommands || size > limits.pendingBytes - bytes)
              send(command, failure(requestId(command), ErrSpawnFailed, "management admission capacity exhausted"))
            else { bytes += size; queue.enqueue(command); advance() }
          case _ => send(command, failure(command.cid, ErrDialectMismatch, "invalid admin payload or request correlation"))
        }
        Behaviors.same
      case Done(next) =>
        cached = next; current.foreach(c => bytes -= weight(c)); current = None; advance()
        Behaviors.same
      case Changed(next) => cached = next; Behaviors.same
      case Snapshot(replyTo) => replyTo ! cached; Behaviors.same
      case WorkerStopped =>
        (current.toVector ++ queue).foreach(c => send(c, failure(requestId(c), ErrSpawnFailed, "management owner stopped")))
        Behaviors.stopped
      case _ => Behaviors.same
    }
  }

  private def operations(limits: Limits, front: ActorRef[Msg])(implicit routes: ComponentRoutes): Behavior[Msg] = Behaviors.setup { context =>
    var components = Map.empty[String, ComponentEntry]
    val loaders = mutable.Set.empty[URLClassLoader]
    def close(loader: Option[URLClassLoader]): Unit = loader.foreach { value =>
      if (loaders.remove(value)) Try(value.close())
    }
    def forgetRetired(path: String, ref: ActorRef[Any]): Unit = {
      routes.removeOwned(actorNameOf(path), ref)
      components = components.flatMap { case (name, entry) =>
        val kept = entry.instances.filterNot(h => h.path == path && h.ref == ref)
        if (kept.isEmpty) { close(entry.loader); None }
        else Some(name -> entry.copy(paths = kept.map(_.path), instances = kept))
      }
    }
    Behaviors.receiveMessage[Msg] {
      case command: CmdIn =>
        val result = Try(execute(context, components, command.payload, limits, loaders, close)) match {
          case Success(value) => value
          case Failure(e: AdminException) => (components, failure(requestId(command), e.code, e.getMessage))
          case Failure(e: CodecException) => (components, failure(requestId(command), ErrDialectMismatch, e.getMessage))
          case Failure(e) => (components, failure(requestId(command), ErrArtifactFetch, e.getMessage))
        }
        components = result._1; send(command, result._2); front ! Done(components)
        Behaviors.same
      case Retired(path, ref) =>
        forgetRetired(path, ref); front ! Changed(components)
        Behaviors.same
      case _ => Behaviors.same
    }.receiveSignal {
      // Akka retires all component children before the owner's PostStop.
      case (_, PostStop) => loaders.foreach(loader => Try(loader.close())); loaders.clear(); Behaviors.same
    }
  }

  private type State = Map[String, ComponentEntry]
  private def measured(components: State): MeasuredMetrics = {
    val states = components.values.toVector.flatMap(e => e.instances.map(h => ComponentState(h.path, h.state, e.version)))
    GatewayMetrics.measured(components.size.toLong, states)
  }
  private def remaining(deadline: Long): Long = deadline - System.nanoTime()
  private def retire(entry: ComponentEntry, selected: InstanceHandle => Boolean, deadline: Long)(implicit routes: ComponentRoutes): ComponentEntry = {
    val kept = entry.instances.flatMap { h =>
      if (!selected(h)) Some(h)
      else {
        routes.removeOwned(actorNameOf(h.path), h.ref)
        if (Try(h.stopper(remaining(deadline))).isSuccess) None else Some(h.copy(state = "retiring"))
      }
    }
    entry.copy(paths = kept.map(_.path), instances = kept)
  }
  private def replace(state: State, entry: ComponentEntry, close: Option[URLClassLoader] => Unit): State =
    if (entry.instances.isEmpty) { close(entry.loader); state - entry.name }
    else state.updated(entry.name, entry)

  private def execute(context: ActorContext[Msg], state: State, payload: Array[Byte], limits: Limits,
                      loaders: mutable.Set[URLClassLoader], close: Option[URLClassLoader] => Unit)(implicit routes: ComponentRoutes): (State, AdminReplyV2) =
    decodeCommand(payload) match {
      case Deploy(id, component) => deploy(context, state, id, component, limits, loaders, close)
      case Stop(id, prefix) =>
        val matched = state.values.filter(_.paths.exists(matchesPrefix(_, prefix))).toVector
        if (matched.isEmpty) throw new AdminException(ErrComponentNotFound, "no component under " + prefix)
        val deadline = System.nanoTime() + limits.retirement.toNanos
        var next = state; var incomplete = false
        matched.foreach { entry =>
          val result = retire(entry, h => matchesPrefix(h.path, prefix), deadline)
          incomplete = incomplete || result.instances.exists(h => matchesPrefix(h.path, prefix))
          next = replace(next, result, close)
        }
        (next, if (incomplete) failure(id, ErrDrainTimeout, "component retirement deadline elapsed")
          else AdminReplyV2.Stopped(id))
      case Drain(id, prefix, timeoutMs) =>
        val matched = state.values.filter(_.paths.exists(matchesPrefix(_, prefix))).toVector
        if (matched.isEmpty) throw new AdminException(ErrComponentNotFound, "no component under " + prefix)
        val millis = if (java.lang.Long.compareUnsigned(timeoutMs, 60000L) > 0) 60000L else timeoutMs
        val deadline = System.nanoTime() + millis * 1000000L
        var next = state; var drained = 0L; var aborted = 0L
        matched.foreach { entry =>
          val kept = entry.instances.flatMap { h =>
            if (!matchesPrefix(h.path, prefix)) Some(h)
            else {
              routes.removeOwned(actorNameOf(h.path), h.ref)
              val finished = Try {
                if (h.spi.isDefined) h.drainer(remaining(deadline))
                h.stopper(remaining(deadline))
              }.isSuccess
              if (finished) { drained += 1; None }
              else { aborted += 1; Some(h.copy(state = "draining")) }
            }
          }
          next = replace(next, entry.copy(paths = kept.map(_.path), instances = kept), close)
        }
        (next, AdminReplyV2.Drained(id, drained, aborted))
      case Status(id, prefix) =>
        val states = state.values.toVector.flatMap(e => e.instances.filter(h => matchesPrefix(h.path, prefix))
          .map(h => ComponentState(h.path, h.state, e.version)))
        if (states.isEmpty) throw new AdminException(ErrComponentNotFound, "no component under " + prefix)
        (state, AdminReplyV2.StatusReply(id, states))
      case MetricsReport(id) => (state, AdminReplyV2.Metrics(id, measured(state).snapshot))
      case MeasuredMetricsReport(id) => (state, AdminReplyV2.Measured(id, measured(state)))
    }

  private def deploy(context: ActorContext[Msg], original: State, id: Long, component: ComponentDeploy,
                     limits: Limits, loaders: mutable.Set[URLClassLoader],
                     close: Option[URLClassLoader] => Unit)(implicit routes: ComponentRoutes): (State, AdminReplyV2) = {
    var next = original
    var loader = Option.empty[URLClassLoader]
    val handles = mutable.ArrayBuffer.empty[InstanceHandle]
    try {
      if (component.name.isEmpty || component.name.length > 256 || component.name == "." || component.name == ".." ||
          !component.name.forall(c => c.isLetterOrDigit || c == '-' || c == '_' || c == '.'))
        throw new AdminException(ErrSpawnFailed, "invalid component name")
      val paths = expandPaths(component.name, component.instances)
      val others = original.values.filterNot(_.name == component.name).toVector
      if (others.map(_.instances.size.toLong).sum + paths.size > limits.instances)
        throw new AdminException(ErrSpawnFailed, "management instance capacity exhausted")
      if (others.exists(e => e.paths.exists(paths.contains)))
        throw new AdminException(ErrSpawnFailed, "component logical path collision")
      loader = buildLoader(component.artifact); loader.foreach(loaders.add)
      val mainClass = component.artifact match {
        case ArtifactRef.Jvm(name, _, _) => name
        case _ => throw new AdminException(ErrDialectMismatch, "not a JVM artifact")
      }
      val cls = try Class.forName(mainClass, false, loader.getOrElse(getClass.getClassLoader))
        catch { case e: LinkageError => throw new AdminException(ErrSpawnFailed, safeDetail(e.getMessage)) }
      if (!classOf[ComponentSpi].isAssignableFrom(cls))
        throw new AdminException(ErrSpawnFailed, mainClass + " does not implement ComponentSpi")
      val constructor = cls.getDeclaredConstructor()
      if (java.lang.reflect.Modifier.isAbstract(cls.getModifiers) || !constructor.canAccess(null))
        throw new AdminException(ErrSpawnFailed, "component requires an accessible concrete no-argument constructor")
      // Preflight preserves an old incarnation. Failures after its retirement
      // remove old registration and cannot pretend the old instance still runs.
      original.get(component.name).foreach { old =>
        val retired = retire(old, _ => true, System.nanoTime() + limits.retirement.toNanos)
        next = replace(next, retired, close)
        if (retired.instances.nonEmpty) throw new AdminException(ErrDrainTimeout, "previous incarnation is still retiring")
      }
      val startupDeadline = System.nanoTime() + limits.startup.toNanos
      paths.foreach { path =>
        val ready = Promise[ComponentSpi](); val canceled = new AtomicBoolean(false)
        val alive = new AtomicBoolean(true)
        val ref = context.spawn(wrappingBehavior(() => constructor.newInstance().asInstanceOf[ComponentSpi],
          ComponentContext(path, component.version, component.config), ready, canceled, alive, context.self),
          "component-" + java.util.UUID.randomUUID(), DispatcherSelector.default())
        val handle = InstanceHandle(path, None, ref,
          nanos => { canceled.set(true); stopAndAwait(ref, nanos) },
          nanos => {
            if (nanos <= 0) throw new java.util.concurrent.TimeoutException("drain deadline elapsed")
            val done = Promise[Unit](); ref ! DrainRequest(done); Await.result(done.future, nanos.nanos)
          }, alive, "starting")
        handles += handle
        val time = remaining(startupDeadline)
        if (time <= 0) throw new java.util.concurrent.TimeoutException("component startup deadline elapsed")
        val spi = Await.result(ready.future, time.nanos)
        handles(handles.size - 1) = handle.copy(spi = Some(spi), state = "running")
      }
      if (!routes.publish(handles.toVector.map(h => (actorNameOf(h.path), h.ref, h.alive))))
        throw new AdminException(ErrSpawnFailed, "component retired during startup or route already owned")
      val entry = ComponentEntry(component.name, component.version, paths, loader, handles.toVector)
      (next.updated(component.name, entry), AdminReplyV2.Deployed(id, paths))
    } catch {
      case NonFatal(error) =>
        val failed = ComponentEntry(component.name, component.version, handles.map(_.path).toVector, loader, handles.toVector)
        val leftover = retire(failed, _ => true, System.nanoTime() + limits.retirement.toNanos)
        if (leftover.instances.nonEmpty) next = next.updated(component.name, leftover) else close(loader)
        val code = error match { case e: AdminException => e.code; case _ => ErrSpawnFailed }
        (next, failure(id, code, "deploy " + component.name + ": " + Option(error.getMessage).getOrElse(error.getClass.getSimpleName)))
    }
  }

  private def actorNameOf(path: String): String = path.stripPrefix("/user/")
  private case object StopNow
  private case object Initialized
  private case object ImplementationStopped
  private final case class DrainRequest(done: Promise[Unit])
  private def stopAndAwait(ref: ActorRef[Any], nanos: Long): Unit = {
    ref ! StopNow
    if (nanos <= 0) throw new java.util.concurrent.TimeoutException("retirement deadline elapsed")
    Await.result(akka.pattern.gracefulStop(ref.toClassic, nanos.nanos, StopNow), nanos.nanos)
    ()
  }
  private def wrappingBehavior(create: () => ComponentSpi, component: ComponentContext,
                               ready: Promise[ComponentSpi], canceled: AtomicBoolean, alive: AtomicBoolean,
                               owner: ActorRef[Msg])(implicit routes: ComponentRoutes): Behavior[Any] = Behaviors.setup { outer =>
    var spi: ComponentSpi = null
    val interceptor = new BehaviorInterceptor[Any, Any] {
      override def aroundStart(ctx: TypedActorContext[Any], target: BehaviorInterceptor.PreStartTarget[Any]): Behavior[Any] =
        try target.start(ctx)
        catch { case NonFatal(error) => ready.tryFailure(error); throw error }

      override def aroundReceive(ctx: TypedActorContext[Any], msg: Any, target: BehaviorInterceptor.ReceiveTarget[Any]): Behavior[Any] =
        msg match {
          case Initialized =>
            if (canceled.get()) Behaviors.stopped else { ready.trySuccess(spi); Behaviors.same }
          case DrainRequest(done) =>
            try {
              val future = spi.parrotDrain()
              if (future == null) done.trySuccess(())
              else future.whenComplete((_: Any, error: Throwable) =>
                if (error == null) { done.trySuccess(()); () } else { done.tryFailure(error); () })
            } catch { case NonFatal(error) => done.tryFailure(error) }
            Behaviors.same
          case other => target(ctx, other)
        }
    }
    val initialize = Behaviors.setup[Any] { _ =>
      try { spi = create(); spi.behavior(component) }
      catch { case e: LinkageError => throw new IllegalStateException("component linkage initialization failed", e) }
    }
    val child = outer.spawn(CausalTrace.behavior(Behaviors.intercept(() => interceptor)(initialize),
      outer.system, component.path), "impl", DispatcherSelector.default())
    outer.watchWith(child, ImplementationStopped); child ! Initialized
    Behaviors.receiveMessage[Any] {
      case StopNow => Behaviors.stopped
      case ImplementationStopped => ready.tryFailure(new IllegalStateException("component terminated before readiness")); Behaviors.stopped
      case other => child ! other; Behaviors.same
    }.receiveSignal {
      case (_, PostStop) =>
        ready.tryFailure(new IllegalStateException("component owner retired before readiness"))
        routes.retired(actorNameOf(component.path), outer.self, alive)
        owner ! Retired(component.path, outer.self)
        Behaviors.same
    }
  }
}
