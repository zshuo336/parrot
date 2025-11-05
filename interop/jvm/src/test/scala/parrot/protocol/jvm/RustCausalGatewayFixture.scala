package parrot.protocol.jvm

import akka.actor.typed.{ActorSystem, PostStop, SupervisorStrategy}
import akka.actor.typed.scaladsl.Behaviors
import com.fasterxml.jackson.databind.ObjectMapper
import io.opentelemetry.api.common.AttributeKey
import io.opentelemetry.sdk.trace.SdkTracerProvider
import io.opentelemetry.sdk.trace.export.SimpleSpanProcessor
import io.opentelemetry.sdk.trace.samplers.Sampler
import io.opentelemetry.sdk.testing.exporter.InMemorySpanExporter
import java.nio.file.{Files, Path, StandardCopyOption}
import java.util.concurrent.TimeUnit
import scala.concurrent.{Await, Promise}
import scala.concurrent.duration._
import scala.jdk.CollectionConverters._
import BridgeActor._

/** A separate JVM process, driven exclusively by the production Rust sender. */
object RustCausalGatewayFixture {
  private val json = new ObjectMapper()
  private def publish(root: Path, name: String, value: AnyRef): Unit = {
    val pending = root.resolve(name + ".pending")
    Files.write(pending, json.writeValueAsBytes(value))
    Files.move(pending, root.resolve(name), StandardCopyOption.ATOMIC_MOVE)
  }

  def main(args: Array[String]): Unit = {
    require(args.length == 1, "expected evidence directory")
    val directory = Path.of(args(0))
    val exporter = InMemorySpanExporter.create()
    val provider = SdkTracerProvider.builder().setSampler(Sampler.alwaysOn())
      .addSpanProcessor(SimpleSpanProcessor.create(exporter)).build()
    val stopped = Promise[Boolean]()
    val system = ActorSystem[Any](Behaviors.empty, "rust-causal-jvm")
    CausalTrace(system).setTracer(provider.get("parrot-rust-jvm-test"))
    val implementation = Behaviors.receiveMessage[Any] {
      case request: BridgeAsk =>
        val receipt = json.createObjectNode()
        request.traceParent match {
          case Some(value) => receipt.put("parent_trace", value.traceId); receipt.put("parent_span", value.spanId)
          case None => receipt.putNull("parent_trace"); receipt.putNull("parent_span")
        }
        CausalTrace.current match {
          case Some(value) => receipt.put("trace", value.traceId); receipt.put("span", value.spanId)
          case None => receipt.putNull("trace"); receipt.putNull("span")
        }
        receipt.put("payload", java.util.HexFormat.of().formatHex(request.payload))
        request.replyTo ! BridgeReplyOk("bin:jvm.context", json.writeValueAsBytes(receipt))
        Behaviors.same
      case request: BridgeTell if request.payload.sameElements(Array[Byte](99)) =>
        throw new IllegalStateException("private test failure")
      case request: BridgeTell if request.payload.sameElements(Array[Byte](-1)) =>
        system.terminate()
        Behaviors.same
      case _ => Behaviors.same
    }.receiveSignal { case (_, PostStop) => stopped.trySuccess(CausalTrace.current.isEmpty); Behaviors.same }
    val target = system.systemActorOf(Behaviors.supervise(CausalTrace.behavior(implementation, system, "/user/context"))
      .onFailure[IllegalStateException](SupervisorStrategy.resume), "target")
    val bridge = system.systemActorOf(BridgeActor(path => if (path == "context") Some(target) else None), "bridge")
    val transport = new ParrotTransportExtension(system, bridge, "jvm-causal-gw", traceContext = true)
    try {
      transport.listen(0)
      val ready = json.createObjectNode().put("port", transport.port).put("pid", ProcessHandle.current().pid())
      publish(directory, "ready.json", ready)
      Await.result(system.whenTerminated, 60.seconds)
      transport.shutdown().get(10, TimeUnit.SECONDS)
      val receipt = json.createObjectNode().put("stopped_without_parent", Await.result(stopped.future, 5.seconds))
      val spans = receipt.putArray("spans")
      exporter.getFinishedSpanItems.asScala.foreach { span =>
        spans.addObject().put("trace", span.getTraceId).put("span", span.getSpanId)
          .put("parent", span.getParentSpanId).put("status", span.getStatus.getStatusCode.toString)
          .put("kind", span.getAttributes.get(AttributeKey.stringKey("parrot.kind")))
          .put("outcome", span.getAttributes.get(AttributeKey.stringKey("parrot.outcome")))
      }
      publish(directory, "retired.json", receipt)
    } finally {
      transport.shutdown().get(10, TimeUnit.SECONDS)
      system.terminate()
      Await.result(system.whenTerminated, 10.seconds)
      provider.close()
    }
  }
}
