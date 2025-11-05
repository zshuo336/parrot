package parrot.protocol.jvm

import akka.actor.typed.Behavior
import akka.actor.typed.scaladsl.Behaviors
import java.net.{InetAddress, InetSocketAddress, ServerSocket}
import java.nio.file.{Files, Paths, StandardCopyOption}
import java.util.concurrent.CountDownLatch

/** Real component initialization deliberately never reaches its ready receipt.
  * The test owns this JVM via the standard gateway guardian, including the socket.
  */
final class RustBlockingStartupFixture extends ComponentSpi {
  override def behavior(ctx: ComponentContext): Behavior[Any] = {
    val marker = Paths.get(sys.env("PARROT_TEST_BLOCKING_STARTUP_MARKER"))
    val listener = new ServerSocket()
    listener.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
    // TERM must not make an early-returning host appear to have awaited its
    // guardian. Only actual group retirement can release this stalled JVM.
    Runtime.getRuntime.addShutdownHook(new Thread(() => new CountDownLatch(1).await()))
    val temporary = marker.resolveSibling(marker.getFileName.toString + ".tmp")
    Files.writeString(temporary, s"[${ProcessHandle.current().pid()},${listener.getLocalPort}]")
    Files.move(temporary, marker, StandardCopyOption.ATOMIC_MOVE)
    try new CountDownLatch(1).await()
    finally listener.close()
    Behaviors.empty
  }
}
