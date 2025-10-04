import java.io.*;
import java.net.*;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.Map;
import java.util.concurrent.*;

/**
 * Parrot Akka Gateway POC（纯 JDK 实现 Parrot Wire 协议）。
 *
 * 帧格式（LE）：
 *   [u32 frame_len][u8 ver][u8 frame_type][u16 flags][u64 cid][u64 reserved]
 *   [u32 path_len][path][u32 key_len][key][payload]
 *
 * 行为：
 *   - ASK(0x10)  → 查服务表 → 执行 → REPLY(0x11)/REPLY_ERR(0x12)
 *   - TELL(0x13) → 异步执行不回
 *   - REPLY/ERR  → 完成 pending Future
 *   - 启动 1s 后主动 ASK rust 侧 /user/rust_service（验证双向）
 *
 * akka 接入形态：services 表的 value 真实实现为
 *   payload -> actorRef.ask(new Cmd(payload), timeout) 的适配层。
 */
public class AkkaGw {
    static final byte ASK = 0x10, REPLY = 0x11, REPLY_ERR = 0x12, TELL = 0x13;
    static final ConcurrentHashMap<Long, CompletableFuture<byte[]>> pending = new ConcurrentHashMap<>();
    /** service: type_key -> (reply_key, handler) */
    static final ConcurrentHashMap<String, Map.Entry<String, java.util.function.Function<byte[], byte[]>>> services = new ConcurrentHashMap<>();
    static long cidSeq = 0;

    public static void main(String[] args) throws Exception {
        int port = args.length > 0 ? Integer.parseInt(args[0]) : 9801;

        // "akka 服务"（POC：lambda；真实：ActorRef 适配）
        services.put("bin:u:Ping", Map.entry("bin:u:Pong", payload -> {
            long n = readU64(payload, 0);
            return writeU64(n + 1);        // akka 方言：echo+1
        }));
        services.put("bin:u:Add", Map.entry("bin:u:AddR", payload -> {
            long a = readU64(payload, 0), b = readU64(payload, 8);
            return writeU64(a * 10 + b);   // akka 方言：a*10+b
        }));

        ServerSocket ss = new ServerSocket(port);
        System.out.println("[akka-gw] listening on " + port);
        Socket sock = ss.accept();
        sock.setTcpNoDelay(true);
        System.out.println("[akka-gw] rust node connected: " + sock.getRemoteSocketAddress());
        ss.close();

        DataInputStream in = new DataInputStream(new BufferedInputStream(sock.getInputStream()));
        LinkedBlockingQueue<byte[]> outQ = new LinkedBlockingQueue<>();

        Thread writer = new Thread(() -> {
            try (DataOutputStream out = new DataOutputStream(new BufferedOutputStream(sock.getOutputStream()))) {
                while (true) {
                    byte[] frame = outQ.take();
                    out.write(frame);
                    out.flush();
                }
            } catch (Exception e) { System.exit(0); }
        });
        writer.setDaemon(true);
        writer.start();

        // 主动 ask rust 侧（双向验证）
        Executors.newSingleThreadScheduledExecutor().schedule(() -> {
            try {
                long cid = ++cidSeq;
                pending.put(cid, new CompletableFuture<>());
                outQ.add(buildFrame(ASK, cid,
                        "/user/rust_service".getBytes(StandardCharsets.UTF_8),
                        "bin:u:Ping".getBytes(StandardCharsets.UTF_8),
                        writeU64(100)));
                byte[] reply = pending.get(cid).get(5, TimeUnit.SECONDS);
                System.out.println("[akka-gw] ask rust /user/rust_service Ping(100) -> " + readU64(reply, 0));
            } catch (Exception e) {
                System.out.println("[akka-gw] ask rust failed: " + e);
            }
        }, 1, TimeUnit.SECONDS);

        // 读循环
        while (true) {
            int frameLen = readU32LE(in);
            byte[] body = new byte[frameLen];
            in.readFully(body);
            ByteBuffer bb = ByteBuffer.wrap(body);
            byte ver = bb.get();               // ver（POC 只支持 1）
            byte ft = bb.get();
            /* short flags = */ readU16LE(bb);
            long cid = readU64LE(bb);
            /* long reserved = */ readU64LE(bb);
            String path = getString(bb);
            String key = getString(bb);
            byte[] payload = new byte[bb.remaining()];
            bb.get(payload);
            if (ver != 1) { System.err.println("[akka-gw] bad ver " + ver); continue; }

            switch (ft) {
                case ASK -> {
                    var svc = services.get(key);
                    if (svc == null) {
                        outQ.add(buildFrame(REPLY_ERR, cid, new byte[0], new byte[0],
                                ("unknown service: " + key).getBytes(StandardCharsets.UTF_8)));
                    } else {
                        try {
                            byte[] result = svc.getValue().apply(payload);
                            outQ.add(buildFrame(REPLY, cid, new byte[0],
                                    svc.getKey().getBytes(StandardCharsets.UTF_8), result));
                        } catch (Exception e) {
                            outQ.add(buildFrame(REPLY_ERR, cid, new byte[0], new byte[0],
                                    String.valueOf(e).getBytes(StandardCharsets.UTF_8)));
                        }
                    }
                }
                case TELL -> {
                    var svc = services.get(key);
                    if (svc != null) CompletableFuture.runAsync(() -> svc.getValue().apply(payload));
                }
                case REPLY -> {
                    var f = pending.remove(cid);
                    if (f != null) f.complete(payload);
                }
                case REPLY_ERR -> {
                    var f = pending.remove(cid);
                    if (f != null) f.completeExceptionally(new RuntimeException(new String(payload, StandardCharsets.UTF_8)));
                }
                default -> System.out.println("[akka-gw] unknown ft " + ft);
            }
        }
    }

    static String getString(ByteBuffer bb) {
        int len = readU32LEBB(bb);
        byte[] b = new byte[len];
        bb.get(b);
        return new String(b, StandardCharsets.UTF_8);
    }

    // ---- 帧构造（LE） ----
    // 头部实际布局（与 rust POC encode 一致）：
    //   [u32 frame_len] + ver(1) ft(1) flags(2) cid(8) reserved(8) path_len(4) key_len(4)
    //   = 28B 头 + path + key + payload
    static byte[] buildFrame(byte ft, long cid, byte[] path, byte[] key, byte[] payload) {
        int bodyLen = 28 + path.length + key.length + payload.length;
        ByteBuffer buf = ByteBuffer.allocate(4 + bodyLen);
        writeU32LE(buf, bodyLen);
        buf.put((byte) 1);
        buf.put(ft);
        writeU16LE(buf, (short) 0);
        writeU64LE(buf, cid);
        writeU64LE(buf, 0L);                 // reserved
        writeU32LE(buf, path.length); buf.put(path);
        writeU32LE(buf, key.length);  buf.put(key);
        buf.put(payload);
        return buf.array();
    }

    static void writeU32LE(ByteBuffer b, int v) {
        b.put((byte) v); b.put((byte)(v >>> 8)); b.put((byte)(v >>> 16)); b.put((byte)(v >>> 24));
    }
    static void writeU16LE(ByteBuffer b, short v) { b.put((byte) v); b.put((byte)(v >>> 8)); }
    static void writeU64LE(ByteBuffer b, long v) { for (int i = 0; i < 8; i++) b.put((byte)(v >>> (8 * i))); }
    static byte[] writeU64(long v) {
        byte[] a = new byte[8];
        for (int i = 0; i < 8; i++) a[i] = (byte)(v >>> (8 * i));
        return a;
    }
    static int readU32LE(DataInputStream in) throws IOException {
        int b0 = in.readUnsignedByte(), b1 = in.readUnsignedByte(), b2 = in.readUnsignedByte(), b3 = in.readUnsignedByte();
        return (b3 << 24) | (b2 << 16) | (b1 << 8) | b0;
    }
    static short readU16LE(ByteBuffer bb) {
        int b0 = bb.get() & 0xFF, b1 = bb.get() & 0xFF;
        return (short) ((b1 << 8) | b0);
    }
    static long readU64LE(ByteBuffer bb) {
        long v = 0;
        for (int i = 0; i < 8; i++) v |= (bb.get() & 0xFFL) << (8 * i);
        return v;
    }
    static int readU32LEBB(ByteBuffer bb) {
        int b0 = bb.get() & 0xFF, b1 = bb.get() & 0xFF, b2 = bb.get() & 0xFF, b3 = bb.get() & 0xFF;
        return (b3 << 24) | (b2 << 16) | (b1 << 8) | b0;
    }
    static long readU64(byte[] a, int off) {
        long v = 0;
        for (int i = 0; i < 8; i++) v |= (a[off + i] & 0xFFL) << (8 * i);
        return v;
    }
}
