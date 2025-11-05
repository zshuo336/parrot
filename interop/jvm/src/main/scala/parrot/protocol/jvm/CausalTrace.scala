package parrot.protocol.jvm

import akka.actor.typed.{ActorSystem, Behavior, BehaviorInterceptor, Extension, ExtensionId, Signal, TypedActorContext}
import akka.actor.typed.scaladsl.Behaviors
import io.opentelemetry.api.trace.{Span, SpanContext, SpanKind, StatusCode, TraceFlags, TraceState, Tracer}
import io.opentelemetry.context.{Context, ContextKey}

/** Explicit Parrot identities; recording is delegated to the host's OpenTelemetry SDK. */
object CausalTrace {
  final case class Parent(traceId: String, spanId: String) {
    private def valid(value: String, length: Int): Boolean =
      value != null && value.length == length && value.forall(c =>
        (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f')) && value.exists(_ != '0')
    require(valid(traceId, 32) && valid(spanId, 16), "invalid causal identity")
    def encode: Array[Byte] = Array(1.toByte) ++ java.util.HexFormat.of().parseHex(traceId + spanId)
  }

  object Parent {
    def decodePrefix(payload: Array[Byte]): Parent = {
      require(payload.length >= 25 && payload(0) == 1, "invalid causal prefix")
      val hex = java.util.HexFormat.of()
      Parent(hex.formatHex(payload, 1, 17), hex.formatHex(payload, 17, 25))
    }
    def split(payload: Array[Byte]): (Parent, Array[Byte]) = (decodePrefix(payload), payload.drop(25))
  }

  trait Carrier { def traceParent: Option[Parent] }

  final class Settings extends Extension {
    @volatile private var configured: Option[Tracer] = None
    def tracer: Option[Tracer] = configured
    def setTracer(tracer: Tracer): Unit = {
      require(tracer != null, "null tracer")
      configured = Some(tracer)
    }
    def clearTracer(): Unit = { configured = None }
  }
  private object Store extends ExtensionId[Settings] {
    override def createExtension(system: ActorSystem[_]): Settings = new Settings
  }
  def apply(system: ActorSystem[_]): Settings = Store(system)

  private val Key = ContextKey.named[Parent]("parrot.jvm.causal-parent")
  def current: Option[Parent] = Option(Context.current().get(Key))

  private def context(parent: Option[Parent]): Context = parent match {
    case None => Context.root()
    case Some(value) => Context.root().`with`(Key, value).`with`(Span.wrap(
      SpanContext.createFromRemoteParent(value.traceId, value.spanId, TraceFlags.getDefault, TraceState.getDefault)))
  }

  def withParent[A](parent: Option[Parent])(action: => A): A = {
    require(parent != null && parent.forall(_ != null), "null parent option or identity")
    val scope = context(parent).makeCurrent()
    try action finally scope.close()
  }

  /** Capture at registration; ordinary Scala futures/actors do not inherit a thread scope. */
  def capture[A](action: () => A): () => A = {
    val captured = Context.current()
    () => {
      val scope = captured.makeCurrent()
      try action() finally scope.close()
    }
  }

  private def boundedTarget(value: String): String = {
    var offset = 0
    var bytes = 0
    while (offset < value.length) {
      val point = value.codePointAt(offset)
      val width = if (point < 0x80) 1 else if (point < 0x800) 2 else if (point < 0x10000) 3 else 4
      if (bytes + width > 256) return value.substring(0, offset)
      bytes += width
      offset += Character.charCount(point)
    }
    value
  }

  private def receive[A](parent: Option[Parent], target: String, operation: String,
                         tracer: Option[Tracer])(action: => A): A = {
    val base = context(parent)
    val span = if (parent.isEmpty) None else tracer.map { sdk =>
      val bounded = boundedTarget(target)
      sdk.spanBuilder("parrot.jvm.handler").setParent(base).setSpanKind(SpanKind.INTERNAL)
        .setAttribute("parrot.kind", operation).setAttribute("parrot.target", bounded)
        .setAttribute("parrot.target_truncated", bounded.length != target.length).startSpan()
    }
    val active = span.fold(base) { value =>
      val identity = value.getSpanContext
      if (identity.isValid) base.`with`(value).`with`(Key, Parent(identity.getTraceId, identity.getSpanId))
      else base
    }
    val scope = active.makeCurrent()
    try {
      val result = action
      span.foreach(_.setAttribute("parrot.outcome", "Returned"))
      result
    } catch {
      case error: Throwable =>
        // Exception text and business payloads are deliberately not recorded.
        span.foreach { value => value.setStatus(StatusCode.ERROR); value.setAttribute("parrot.outcome", "Panicked") }
        throw error
    } finally {
      try scope.close() finally span.foreach(_.end())
    }
  }

  /** The span covers one actual synchronous Akka receive callback, not reply delivery. */
  def behavior(underlying: Behavior[Any], system: ActorSystem[_], target: String): Behavior[Any] = {
    require(system != null && target != null, "null trace owner or target")
    Behaviors.intercept(() => new BehaviorInterceptor[Any, Any] {
      override def aroundStart(ctx: TypedActorContext[Any], next: BehaviorInterceptor.PreStartTarget[Any]): Behavior[Any] =
        withParent(None)(next.start(ctx))
      override def aroundReceive(ctx: TypedActorContext[Any], message: Any,
                                 next: BehaviorInterceptor.ReceiveTarget[Any]): Behavior[Any] = {
        val parent = message match { case value: Carrier => value.traceParent; case _ => None }
        val kind = message match { case _: BridgeActor.BridgeAsk => "Ask"; case _: BridgeActor.BridgeTell => "Tell"; case _ => "Receive" }
        receive(parent, target, kind, CausalTrace(system).tracer)(next(ctx, message))
      }
      override def aroundSignal(ctx: TypedActorContext[Any], signal: Signal,
                                next: BehaviorInterceptor.SignalTarget[Any]): Behavior[Any] =
        withParent(None)(next(ctx, signal))
    })(underlying)
  }
}
