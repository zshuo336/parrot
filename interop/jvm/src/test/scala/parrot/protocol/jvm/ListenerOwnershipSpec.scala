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

  for (address <- Seq("127.0.0.1", "::1")) {
    test(s"automatic port admission rejects an occupied candidate before publishing $address") {
      val foreign = new ServerSocket()
      foreign.setReuseAddress(true)
      foreign.bind(new InetSocketAddress(InetAddress.getByName(address), 0))
      foreign.setSoTimeout(3000)
      val available = new ServerSocket(0)
      val nextPort = available.getLocalPort
      available.close()
      val transport = new ParrotTransportExtension(null, null, "automatic-owner")
      var allocations = 0
      try {
        transport.listenWithPortAllocator(0, () => {
          allocations += 1
          if (allocations == 1) foreign.getLocalPort else nextPort
        })
        assert(allocations == 2, "occupied candidate must require a new allocation")
        assert(transport.port == nextPort, "only the successfully bound candidate may be published")
        val client = new Socket(address, foreign.getLocalPort)
        try {
          val accepted = foreign.accept()
          try {
            accepted.getOutputStream.write(73)
            client.setSoTimeout(3000)
            assert(client.getInputStream.read() == 73, "foreign listener must remain usable")
          } finally accepted.close()
        } finally client.close()
        val peer = new Socket("127.0.0.1", transport.port)
        try {
          peer.setSoTimeout(3000)
          peer.getOutputStream.write(WireFrame.Frame(1, 1, 0, 0L, 0, 8, "", "",
            WireFrame.handshakeBody("automatic-peer")).encode())
          val expected = WireFrame.Frame(1, 2, 0, 0L, 0, 8, "", "",
            WireFrame.handshakeAckBody("automatic-owner")).encode()
          val actual = new Array[Byte](expected.length)
          new java.io.DataInputStream(peer.getInputStream).readFully(actual)
          assert(actual.sameElements(expected), "newly published endpoint must return its own complete ACK")
        } finally peer.close()
      } finally {
        foreign.close()
        transport.shutdown().get(10, TimeUnit.SECONDS)
      }
    }
  }

  test("automatic listener admission fails after eight occupied candidates without publishing") {
    val foreign = new ServerSocket()
    foreign.setReuseAddress(true)
    foreign.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
    val transport = new ParrotTransportExtension(null, null, "automatic-exhaustion")
    var allocations = 0
    try {
      intercept[java.net.BindException] {
        transport.listenWithPortAllocator(0, () => { allocations += 1; foreign.getLocalPort })
      }
      assert(allocations == 8)
      assert(transport.port == -1)
      assert(!foreign.isClosed)
    } finally {
      foreign.close()
      transport.shutdown().get(10, TimeUnit.SECONDS)
    }
  }

  test("fixed listener admission never invokes automatic allocation or retries a conflict") {
    val foreign = new ServerSocket()
    foreign.setReuseAddress(true)
    foreign.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
    val transport = new ParrotTransportExtension(null, null, "fixed-conflict")
    var allocations = 0
    try {
      intercept[java.net.BindException] {
        transport.listenWithPortAllocator(foreign.getLocalPort, () => { allocations += 1; 0 })
      }
      assert(allocations == 0)
      assert(transport.port == -1)
    } finally {
      foreign.close()
      transport.shutdown().get(10, TimeUnit.SECONDS)
    }
  }
}
