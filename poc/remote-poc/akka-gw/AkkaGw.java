import java.io.*;
import java.net.*;
import java.nio.*;
import java.nio.charset.*;
import java.util.*;

/** Historical class name; this fixture is pure JDK, not an Akka actor runtime.
 * One connection, sequential business processing, bounded framing and pending.
 */
public class AkkaGw {
    static final byte ASK = 0x10, REPLY = 0x11, REPLY_ERR = 0x12, TELL = 0x13;
    static final int MAX_BODY = 1024 * 1024;
    record Frame(byte type, long cid, String path, String key, byte[] payload) {}
    record Pending(long original, String expectedKey) {}

    public static void main(String[] args) throws Exception {
        int port = args.length > 0 ? Integer.parseInt(args[0]) : 0;
        String relay = args.length > 1 ? args[1] : null;
        try (ServerSocket listener = new ServerSocket()) {
            listener.bind(new InetSocketAddress("127.0.0.1", port));
            System.out.println("POC_PORT " + listener.getLocalPort());
            try (Socket sock = listener.accept()) {
                sock.setTcpNoDelay(true);
                var in = new DataInputStream(new BufferedInputStream(sock.getInputStream()));
                var out = new DataOutputStream(new BufferedOutputStream(sock.getOutputStream()));
                Map<Long, Pending> pending = new HashMap<>();
                long next = 1, tells = 0;
                while (true) {
                    Frame f;
                    try { f = readFrame(in); }
                    catch (EOFException closed) { break; }
                    switch (f.type) {
                        case ASK -> {
                            try {
                                if (Set.of("bin:u:Probe", "bin:u:Relay", "bin:u:RelayTell").contains(f.key)) {
                                    requireLength(f.payload, 0);
                                    if (pending.size() >= 128) throw new IOException("pending limit");
                                    String target = f.key.equals("bin:u:Probe") ? "/user/rust_service" : relay;
                                    if (target == null) throw new IOException("relay not configured");
                                    boolean tell = f.key.equals("bin:u:RelayTell");
                                    if (tell) send(out, TELL, 0, target, "bin:u:Ping", writeU64(42));
                                    String key = tell ? "bin:u:TellCount" : "bin:u:Ping";
                                    pending.put(next, new Pending(f.cid, tell ? "bin:u:AddR" : "bin:u:Pong"));
                                    send(out, ASK, next++, target, key, tell ? new byte[0] : writeU64(100));
                                } else if (f.key.equals("bin:u:TellCount")) {
                                    requireLength(f.payload, 0);
                                    send(out, REPLY, f.cid, "", "bin:u:AddR", writeU64(tells));
                                } else {
                                    var result = service(f.key, f.payload);
                                    send(out, REPLY, f.cid, "", result.getKey(), result.getValue());
                                }
                            } catch (Exception error) {
                                send(out, REPLY_ERR, f.cid, "", "", String.valueOf(error).getBytes(StandardCharsets.UTF_8));
                            }
                        }
                        case TELL -> {
                            try { service(f.key, f.payload); tells++; }
                            catch (Exception rejected) { /* No receipt for invalid TELL. */ }
                        }
                        case REPLY, REPLY_ERR -> {
                            Pending p = pending.remove(f.cid);
                            if (p != null) {
                                if (f.type == REPLY && f.key.equals(p.expectedKey) && f.payload.length == 8)
                                    send(out, REPLY, p.original, "", "bin:u:AddR", f.payload);
                                else send(out, REPLY_ERR, p.original, "", "", "invalid reverse reply".getBytes(StandardCharsets.UTF_8));
                            }
                        }
                        default -> { /* STOP has no lifecycle meaning in this one-service fixture. */ }
                    }
                }
            }
        }
    }

    static Map.Entry<String, byte[]> service(String key, byte[] payload) throws IOException {
        return switch (key) {
            case "bin:u:Ping" -> {
                requireLength(payload, 8);
                yield Map.entry("bin:u:Pong", writeU64(readU64(payload, 0) + 1));
            }
            case "bin:u:Add" -> {
                requireLength(payload, 16);
                yield Map.entry("bin:u:AddR", writeU64(readU64(payload, 0) * 10 + readU64(payload, 8)));
            }
            default -> throw new IOException("unknown_service: " + key);
        };
    }
    static void requireLength(byte[] payload, int expected) throws IOException {
        if (payload.length != expected) throw new IOException("invalid payload length");
    }
    static String string(ByteBuffer body) throws IOException {
        if (body.remaining() < 4) throw new IOException("missing string length");
        int size = body.getInt();
        if (size < 0 || size > body.remaining()) throw new IOException("invalid string length");
        ByteBuffer value = body.slice(); value.limit(size);
        body.position(body.position() + size);
        return StandardCharsets.UTF_8.newDecoder().onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT).decode(value).toString();
    }
    static Frame readFrame(DataInputStream in) throws IOException {
        int size = Integer.reverseBytes(in.readInt());
        if (size < 28 || size > MAX_BODY) throw new IOException("invalid frame length");
        byte[] data = new byte[size]; in.readFully(data);
        ByteBuffer body = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN);
        byte version = body.get(), type = body.get();
        short flags = body.getShort(); long cid = body.getLong(), reserved = body.getLong();
        if (version != 1 || type < ASK || type > 0x14 || flags != 0 || reserved != 0)
            throw new IOException("unsupported frame header");
        String path = string(body), key = string(body);
        byte[] payload = new byte[body.remaining()]; body.get(payload);
        return new Frame(type, cid, path, key, payload);
    }
    static void send(DataOutputStream out, byte type, long cid, String path, String key, byte[] payload) throws IOException {
        byte[] p = path.getBytes(StandardCharsets.UTF_8), k = key.getBytes(StandardCharsets.UTF_8);
        long size = 28L + p.length + k.length + payload.length;
        if (size > MAX_BODY) throw new IOException("frame exceeds limit");
        ByteBuffer b = ByteBuffer.allocate((int) size + 4).order(ByteOrder.LITTLE_ENDIAN);
        b.putInt((int) size).put((byte)1).put(type).putShort((short)0).putLong(cid).putLong(0);
        b.putInt(p.length).put(p).putInt(k.length).put(k).put(payload);
        out.write(b.array()); out.flush();
    }
    static long readU64(byte[] payload, int offset) {
        return ByteBuffer.wrap(payload, offset, 8).order(ByteOrder.LITTLE_ENDIAN).getLong();
    }
    static byte[] writeU64(long n) {
        return ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(n).array();
    }
}
