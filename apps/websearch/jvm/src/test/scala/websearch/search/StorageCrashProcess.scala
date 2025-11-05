package websearch.search

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.AskPattern._
import parrot.protocol.jvm.ComponentContext
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk}
import java.nio.{ByteBuffer, ByteOrder}
import scala.concurrent.Await
import scala.concurrent.duration._

/** A separate JVM commits through the real actor and is then SIGKILLed by the
  * parent. READY is printed only after the Flush/Clear reply is observed. */
object StorageCrashProcess extends App {
  require(args.length == 2)
  val system = ActorSystem(new SearchComponent().behavior(ComponentContext("/user/search", "test",
    Some(s"data_dir = \"${args(0)}\"".getBytes("UTF-8")))), "storage-crash-child")
  implicit val timeout: akka.util.Timeout = 5.seconds
  implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
  def i32(n: Int): Array[Byte] = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(n).array()
  def str(s: String): Array[Byte] = { val b = s.getBytes("UTF-8"); i32(b.length) ++ b }
  val id = Array[Byte](1, 0, 0, 0, 0, 0, 0, 0)
  def request(key: String, p: Array[Byte] = Array.emptyByteArray): Unit = {
    val reply = Await.result(system.ask[Any](r => BridgeAsk("bin:ws/" + key, p, r)), 6.seconds)
    assert(reply.isInstanceOf[BridgeReplyOk], reply.toString)
  }
  try {
    if (args(1) != "hold") {
      request("DocMeta", id ++ str("https://durable.test") ++ str("durable") ++ str("alpha"))
      request("IndexTerms", i32(1) ++ str("alpha") ++ id ++ i32(1))
      request("Flush")
      if (args(1) == "clear") request("Clear")
    }
    println("STORAGE_READY"); System.out.flush()
    while (true) Thread.sleep(1000)
  } finally { system.terminate(); Await.result(system.whenTerminated, 10.seconds) }
}
