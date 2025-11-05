package parrot.protocol.jvm

import org.scalatest.funsuite.AnyFunSuite
import java.net.{InetAddress, InetSocketAddress, ServerSocket, Socket}
import java.util.concurrent.TimeUnit

class ListenerOwnershipSpec extends AnyFunSuite {
  for (address <- Seq("127.0.0.1", "::1")) {
    test(s"wildcard listener cannot claim an occupied specific endpoint $address") {
      val foreign = new ServerSocket()
      foreign.setReuseAddress(true)
      foreign.bind(new InetSocketAddress(InetAddress.getByName(address), 0))
      foreign.setSoTimeout(3000)
      val transport = new ParrotTransportExtension(null, null, "listener-owner")
      try {
        val attempt = scala.util.Try(transport.listen(foreign.getLocalPort))
        // No external process participates: demonstrate where the claimed
        // production endpoint actually delivers this connection.
        val client = new Socket(address, foreign.getLocalPort)
        try {
          val acceptedByForeign = foreign.accept()
          try {
            acceptedByForeign.getOutputStream.write("HTTP/1.1 400 Bad Request\r\n\r\n".getBytes("US-ASCII"))
            acceptedByForeign.getOutputStream.flush()
            client.setSoTimeout(3000)
            assert(client.getInputStream.read() == 'H'.toInt)
          } finally acceptedByForeign.close()
        } finally client.close()
        assert(attempt.isFailure, "transport reported success for an endpoint served by another listener")
        assert(transport.port == -1, "failed bind must not publish a port")
      } finally {
        foreign.close()
        transport.shutdown().get(10, TimeUnit.SECONDS)
      }
    }
  }

  test("two live transports cannot publish the same wildcard port") {
    val first = new ParrotTransportExtension(null, null, "first")
    val second = new ParrotTransportExtension(null, null, "second")
    try {
      first.listen(0)
      val attempt = scala.util.Try(second.listen(first.port))
      assert(attempt.isFailure)
      assert(second.port == -1)
    } finally {
      first.shutdown().get(10, TimeUnit.SECONDS)
      second.shutdown().get(10, TimeUnit.SECONDS)
    }
  }

  test("an idle retired listener releases its fixed port for a fresh transport") {
    val first = new ParrotTransportExtension(null, null, "first")
    val second = new ParrotTransportExtension(null, null, "second")
    try {
      first.listen(0)
      val port = first.port
      first.shutdown().get(10, TimeUnit.SECONDS)
      second.listen(port)
      assert(second.port == port)
    } finally {
      first.shutdown().get(10, TimeUnit.SECONDS)
      second.shutdown().get(10, TimeUnit.SECONDS)
    }
  }
}
