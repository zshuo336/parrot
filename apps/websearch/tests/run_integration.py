#!/usr/bin/env python3
"""Real Rust/Erlang/Ray/JVM integration with an owned local HTTP corpus.

No public crawl, shared data removal, process-name killing, or implicit skip.
Every subprocess and its logs belong to this invocation. Failed runs retain data.
"""
from __future__ import annotations

import argparse
from collections import Counter
from contextlib import ExitStack
from html.parser import HTMLParser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import struct
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

REPO = Path(__file__).resolve().parents[3]
APP = REPO / "apps/websearch"
sys.path.insert(0, str(REPO / "apps/test_support"))
from reply_proxy import CorruptFirstReply
from owned_process import OwnedProcess
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def available_port():
    # Applications currently do not expose an inherited listener API. Allocate
    # late; a race fails the test, never authorizes killing the other listener.
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Process:
    def __init__(self, root, name, argv, cwd=REPO, env=None):
        self.log = root / (name + ".log")
        self.stopped = False
        with self.log.open("wb") as out:
            self.child = OwnedProcess(
                [str(a) for a in argv], cwd=cwd, env=env,
                stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT,
            )

    def text(self):
        return self.log.read_text(errors="replace")

    def alive(self):
        rc = self.child.poll()
        require(rc is None, f"process exited {rc}: {self.log}\n{self.text()[-4000:]}")

    def stop(self, grace=15):
        if self.stopped:
            return
        # First let the owner close resources (notably ray.shutdown) while its
        # workers are still alive; then remove any remaining owned descendants.
        self.child.send_signal(signal.SIGTERM)
        try:
            self.child.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            pass
        # wait/poll retain the leader's identity until close sends the final
        # group signal and reaps. Failed cleanup remains retryable.
        self.child.close(timeout=15)
        self.stopped = True


def until(probe, processes=(), seconds=180):
    end = time.monotonic() + seconds
    last = None
    while time.monotonic() < end:
        for process in processes:
            process.alive()
        try:
            value = probe()
            if value:
                return value
        except (OSError, urllib.error.URLError, ValueError) as error:
            last = error
        time.sleep(0.1)
    raise AssertionError(f"deadline expired waiting for {probe!r}; last error={last!r}")


def text_at(port, path):
    with HTTP.open(f"http://127.0.0.1:{port}{path}", timeout=3) as response:
        require(response.status == 200, f"{path}: HTTP {response.status}")
        return response.read().decode("utf-8")


class ResultLinks(HTMLParser):
    def __init__(self):
        super().__init__()
        self.urls = []

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "a" and attrs.get("href", "").startswith("http://"):
            self.urls.append(attrs["href"])


def results(port, token):
    body = text_at(port, "/search?" + urllib.parse.urlencode({"q": token}))
    parser = ResultLinks()
    parser.feed(body)
    return parser.urls, re.findall(r"score (\d+\.\d+)", body)


class Corpus:
    def __init__(self, stack, token):
        self.token = token
        self.requests = Counter()
        self.blocked = []
        self.lock = threading.Lock()
        self.slow_started = threading.Event()
        self.slow_release = threading.Event()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_CONNECT(self):
                self.do_GET()

            def do_GET(self):
                with owner.lock:
                    owner.requests[self.path] += 1
                if self.path == "/robots.txt":
                    body = "User-agent: *\nDisallow: /denied\n"
                elif self.path in ("/root", "/child", "/extra", "/slow"):
                    if self.path == "/slow":
                        owner.slow_started.set()
                        if not owner.slow_release.wait(10):
                            self.send_error(504, "controlled response was not released")
                            return
                    word = {"/root": "alpha", "/child": "beta", "/extra": "gamma", "/slow": "delta"}[self.path]
                    links = '<a href="child">child</a><a href="/denied">denied</a>' if self.path == "/root" else ""
                    body = f"<html><head><title>{word} {token}</title></head><body>{word} {token} {word} 搜索测试 {links}</body></html>"
                elif re.fullmatch(r"/batch-[0-8]", self.path):
                    body = f"<html><head><title>{token} {self.path}</title></head><body>{token} batchword {self.path}</body></html>"
                elif self.path == "/missing":
                    self.send_error(404)
                    return
                else:
                    with owner.lock:
                        owner.blocked.append(self.path)
                    self.send_error(502, "outside the test corpus")
                    return
                data = body.encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_port
        self.base = f"http://127.0.0.1:{self.port}"
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        stack.callback(thread.join, 5)
        stack.callback(self.server.server_close)
        stack.callback(self.server.shutdown)

    def assert_requests(self, expected):
        with self.lock:
            actual = dict(self.requests)
            blocked = list(self.blocked)
        require(not blocked, f"unexpected/outbound requests: {blocked}")
        require(actual == expected, f"HTTP request provenance: {actual} != {expected}")


class Suite:
    def __init__(self, root, binary, stack):
        self.root, self.binary, self.stack = root, binary, stack
        self.counter = 0
        self.passed = []
        self.token = "parrot" + uuid.uuid4().hex
        self.corpus = Corpus(stack, self.token)
        self.runtime_proxy = Corpus(stack, "runtime-probes")
        self.env = os.environ.copy()
        self.env.pop("PARROT_NODE_ID", None)
        self.env.pop("RAY_ADDRESS", None)
        self.env["PYTHONPATH"] = str(REPO / "interop/python")
        self.env["PYTHONUNBUFFERED"] = "1"
        # Gateways may honor ambient proxies. The Rust crawler deliberately
        # ignores them; app() must restrict its actual fetch policy below.
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
            self.env[key] = self.corpus.base
        for key in ("NO_PROXY", "no_proxy"):
            self.env[key] = "127.0.0.1,localhost"

    def process(self, name, argv, env=None, cwd=REPO):
        self.counter += 1
        process = Process(self.root, f"{self.counter:02d}-{name}", argv, cwd, env or self.env)
        self.stack.callback(process.stop)
        return process

    def passed_case(self, name):
        self.passed.append(name)
        print(f"PASS {name}", flush=True)

    def gateways(self, data, target=None, jvm_only=False, jvm_port=0):
        env = dict(self.env, WS_DATA=str(data))
        # Ray may probe cloud metadata during initialization. Reject it through
        # a separate local proxy so only crawler traffic enters corpus evidence.
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
            env[key] = self.runtime_proxy.base
        env["RAY_USAGE_STATS_ENABLED"] = "0"
        cp = REPO / "interop/jvm/target"
        java_args = ["java", "-cp", str(cp / "parrot-protocol-jvm-0.1.0.jar") + os.pathsep + (cp / "cp.txt").read_text().strip(), "parrot.protocol.jvm.ParrotGatewayMain", str(jvm_port), "node=jvm-search-1", "7200"]
        if target:
            java_args.append("parrot=" + target)
        jvm = self.process("jvm", java_args, env)
        all_processes = [jvm]
        direct = {}
        if not target:
            direct["jvm"] = int(until(lambda: re.search(r"PARROT_JVM_PORT=(\d+)", jvm.text()), [jvm]).group(1))
        if not jvm_only:
            erlang_args = '[0, "parrot=' + target + '"]' if target else '[0]'
            erl = self.process("erlang", ["erl", "-noshell", "-pa", str(REPO / "interop/erlang"), "-eval", f"parrot_gw:main({erlang_args})"], env)
            ray_dir = self.root / f"ray-{self.counter}"
            ray = self.process("ray", [sys.executable, APP / "tests/ray_gateway.py", "0", ray_dir] + ([target] if target else []), env)
            all_processes.extend([erl, ray])
            if not target:
                for name, proc, marker in [("erl", erl, "PARROT_ERL_PORT"), ("ray", ray, "RAY_GW_PORT")]:
                    direct[name] = int(until(lambda p=proc, m=marker: re.search(m + r"=(\d+)", p.text()), all_processes).group(1))
        return all_processes, direct

    def app(self, data, direct, *extra, port=None, crawl_origins=None):
        automatic_port = port is None
        if automatic_port:
            # The application owns this socket continuously from bind through
            # serving; a probe-and-release port can be stolen before startup.
            port = 0
        origins = (self.corpus.base,) if crawl_origins is None else tuple(crawl_origins)
        require(bool(origins), "integration app requires an explicit crawl scope")
        args = [self.binary, "--port", str(port), "--data", data]
        for origin in origins:
            args += ["--crawl-origin", origin]
        args += [f"{name}=127.0.0.1:{p}" for name, p in direct.items()]
        process = self.process("app", args + list(extra))
        if automatic_port:
            def bound_port_or_exit():
                match = re.search(r"^PARROT_WEBSEARCH_PORT=(\d+)$", process.text(), re.MULTILINE)
                if match:
                    actual = int(match.group(1))
                    require(0 < actual <= 65535, "invalid application listener receipt")
                    return actual
                # Negative startup cases still return their real process/error;
                # do not turn an expected rejection into a readiness timeout.
                return -1 if process.child.poll() is not None else None
            actual = until(bound_port_or_exit)
            port = actual if actual > 0 else 0
        return process, port

    def ready(self, app, port, gateways):
        return until(lambda: json.loads(text_at(port, "/stats")), [app, *gateways])

    def completed(self, app, port, gateways, expected):
        until(lambda: "爬取收尾完成" in app.text(), [app, *gateways])
        stats = self.ready(app, port, gateways)
        require(stats["docs"] == expected, f"exact indexed count: {stats}")
        require(stats["terms"] > 0 and stats["postings"] >= expected, f"missing business index: {stats}")
        require(stats["segments"] > 0, f"no committed segment: {stats}")
        return stats

    def assert_search(self, port, paths):
        urls, scores = results(port, self.token)
        expected = {self.corpus.base + path for path in paths}
        require(set(urls) == expected and len(urls) == len(expected), f"search URLs {urls} != {expected}")
        require(len(scores) == len(expected) and all(float(s) > 0 for s in scores), f"invalid scores: {scores}")
        return dict(zip(urls, scores))

    def run_startup_storage_failure(self):
        data = self.root / "startup-storage-failure"
        index = data / "index"
        index.mkdir(parents=True)
        # Independent valid legacy snapshot with one metadata-only document.
        def modified_ascii(value):
            raw = value.encode("ascii")
            return struct.pack(">H", len(raw)) + raw
        snapshot = (struct.pack(">IQ", 1, 1) + modified_ascii("https://retained.test")
                    + modified_ascii("retained") + modified_ascii("body") + struct.pack(">I", 0))
        path = index / "seg-00000.segment"
        path.write_bytes(snapshot)
        journal = data / "dedupe.tsv"
        journal.write_bytes(b"https://retained.test\n")
        with socket.socket() as unavailable:
            unavailable.bind(("127.0.0.1", 0))
            app = self.process("unreachable-preserve", [self.binary, "--port", "0", "--data", data,
                "--insecure-development", f"erl=127.0.0.1:{unavailable.getsockname()[1]}",
                self.corpus.base + "/root", "--pages", "1"])
            until(lambda: app.child.poll() is not None, seconds=15)
            require(path.is_file() and path.read_bytes() == snapshot,
                    f"failed startup deleted or changed committed index: {app.log}")
            require(journal.read_bytes() == b"https://retained.test\n", "failed startup changed crawl journal")
            require(app.child.returncode != 0 and "panicked" not in app.text(),
                    f"connection failure must exit normally with an error: {app.text()}")
            require("爬取收尾完成" not in app.text(), "failed startup reported completion")
        self.passed_case("startup failure: unavailable gateway preserves committed index without panic")

    def run(self):
        self.run_startup_storage_failure()
        with socket.socket() as unavailable:
            unavailable.bind(("127.0.0.1", 0))
            checks = []
            for i, (args, expected_code) in enumerate([
                (["--pages"], 2), (["--unknown"], 2), (["--port", "65536"], 2),
                (["--pages", "0"], 2), (["http://user:password@example.test"], 2), (["--help"], 0),
            ]):
                data = self.root / f"invalid-cli-{i}"
                process = self.process("cli", [self.binary, "--data", data, f"erl=127.0.0.1:{unavailable.getsockname()[1]}", *args])
                checks.append((process, data, expected_code))
            for process, data, expected_code in checks:
                until(lambda p=process: p.child.poll() is not None)
                require(process.child.returncode == expected_code, f"CLI exit={process.child.returncode}: {process.text()}")
                require(not data.exists(), f"CLI validation modified storage: {data}")
                require("panicked" not in process.text(), f"CLI panic: {process.text()}")
            self.passed_case("CLI: invalid and missing arguments fail before storage/network; help exits successfully")
        with ExitStack() as occupied:
            sockets = []
            for _ in range(100):
                base = available_port()
                if base > 65515:
                    continue
                try:
                    for candidate in range(base, base + 21):
                        listener = socket.socket()
                        sockets.append(listener)
                        listener.bind(("0.0.0.0", candidate))
                        listener.listen()
                    break
                except OSError:
                    for listener in sockets:
                        listener.close()
                    sockets = []
            require(len(sockets) == 21, "could not reserve an owned contiguous port range")
            for listener in sockets:
                occupied.callback(listener.close)
            data = self.root / "unavailable-web-data"
            process = self.process("web-bind-failure", [self.binary, "--port", str(base), "--data", data])
            until(lambda: process.child.poll() is not None)
            require(process.child.returncode == 3, f"bind failure exit: {process.text()}")
            require(not data.exists(), "failed Web startup modified data")
            self.passed_case("startup: exhausting the owned Web port range fails before storage or deployment")
        data = self.root / "direct-data"
        data.mkdir()
        gateways, direct = self.gateways(data)
        app, port = self.app(data, direct, self.corpus.base + "/root", self.corpus.base + "/missing", "--sites", "1", "--maxdepth", "1", "--pages", "10")
        self.completed(app, port, gateways, 2)
        self.assert_search(port, ["/root", "/child"])
        self.corpus.assert_requests({"/robots.txt": 1, "/root": 1, "/child": 1, "/missing": 1})
        require("分词表" in text_at(port, "/"), "home navigation missing")
        require("2 个页面" in text_at(port, "/docs"), "docs count incorrect")
        require(self.token in text_at(port, "/terms"), "terms missing indexed token")
        self.passed_case("direct: real crawl, 404 isolation, robots denial, Ray tokenization, exact JVM search")
        app.stop()

        # Resume retains the original depth scope and retries the failed /missing
        # and robots-denied task as well as the new seed: three attempt slots.
        app, port = self.app(data, direct, self.corpus.base + "/extra", "--keep", "--sites", "1", "--maxdepth", "1", "--pages", "3")
        self.completed(app, port, gateways, 3)
        before = self.assert_search(port, ["/root", "/child", "/extra"])
        self.corpus.assert_requests({"/robots.txt": 2, "/root": 1, "/child": 1, "/missing": 2, "/extra": 1})
        self.passed_case("keep: retries pending failures, preserves committed pages and adds the exact new document")
        app.stop()
        for gateway in gateways:
            gateway.stop()

        # Entire JVM process is restarted: an in-memory actor cannot fake replay.
        journal_before = (data / "dedupe.tsv").read_bytes()
        gateways, direct = self.gateways(data, jvm_only=True)
        app, port = self.app(data, direct, "--serve-only")
        self.ready(app, port, gateways)
        after = self.assert_search(port, ["/root", "/child", "/extra"])
        require(before == after, f"replay changed BM25 scores: {before} != {after}")
        require((data / "dedupe.tsv").read_bytes() == journal_before, "serve-only modified the crawl journal")
        self.corpus.assert_requests({"/robots.txt": 2, "/root": 1, "/child": 1, "/missing": 2, "/extra": 1})
        self.passed_case("serve-only: JVM process restart preserves exact results and scores without fetching")
        app.stop()
        for gateway in gateways:
            gateway.stop()

        # New topology/data with actual registration and business assertions.
        data = self.root / "registry-data"
        data.mkdir()
        app, port = self.app(data, {}, self.corpus.base + "/root", "--bind", "127.0.0.1:0", "--wait", "180", "--sites", "1", "--maxdepth", "1", "--pages", "10")
        match = until(lambda: re.search(r"应用监听 (127\.0\.0\.1:\d+)", app.text()), [app])
        gateways, _ = self.gateways(data, target=match.group(1))
        self.completed(app, port, gateways, 2)
        self.assert_search(port, ["/root", "/child"])
        self.corpus.assert_requests({"/robots.txt": 3, "/root": 2, "/child": 2, "/missing": 2, "/extra": 1})
        self.passed_case("registry: ephemeral listener and three real registered gateways")
        app.stop()
        for gateway in gateways:
            gateway.stop()

        # Reuse persisted nonempty data, then cold-start and reuse the live
        # gateway for a second cold crawl. Old disk and memory contents must go.
        gateways, direct = self.gateways(data)
        app, port = self.app(data, direct, self.corpus.base + "/extra", "--sites", "1", "--maxdepth", "0", "--pages", "1")
        self.completed(app, port, gateways, 1)
        self.assert_search(port, ["/extra"])
        self.passed_case("cold start: removes old persisted documents from search")
        app.stop()
        app, port = self.app(data, direct, self.corpus.base + "/root", "--sites", "1", "--maxdepth", "1", "--pages", "10")
        self.completed(app, port, gateways, 2)
        self.assert_search(port, ["/root", "/child"])
        self.passed_case("cold redeploy: commits a cleared index before new crawl")
        require(text_at(port, "/docs?p=999") == text_at(port, "/docs?p=0"), "out-of-range docs must contain the last page's actual rows")
        require(text_at(port, "/terms?p=999") == text_at(port, "/terms?p=0"), "out-of-range terms must contain the last page's actual rows")
        for path in ("/docs", "/terms"):
            require(text_at(port, path + "?p=18446744073709551615") == text_at(port, path + "?p=0"), f"{path}: large offset wrapped or lost rows")
        query = urllib.parse.urlencode({"q": self.token})
        require(text_at(port, "/search?" + query + "&p=0") == text_at(port, "/search?" + query + "&p=1"), "zero search page must clamp to first page")
        require(text_at(port, "/search?" + query + "&p=18446744073709551615") == text_at(port, "/search?" + query + "&p=10"), "large search page must clamp before arithmetic")
        self.passed_case("pagination: out-of-range pages return actual final-page data")
        app.stop()
        app, port = self.app(data, direct, self.corpus.base + "/root", "--sites", "1", "--maxdepth", "1", "--pages", "10")
        self.completed(app, port, gateways, 2)
        self.assert_search(port, ["/root", "/child"])
        self.passed_case("same-seed cold restart: previously seen URLs are fetched and indexed again")
        app.stop()
        budget_data = self.root / "budget-data"
        budget_data.mkdir()
        for gateway in gateways:
            gateway.stop()

        gateways, direct = self.gateways(budget_data)
        before_requests = self.corpus.requests.copy()
        seeds = [self.corpus.base + path for path in ("/root", "/child", "/extra")]
        app, port = self.app(budget_data, direct, *seeds, "--sites", "100", "--maxdepth", "1", "--pages", "1")
        self.completed(app, port, gateways, 1)
        urls, _ = results(port, self.token)
        require(len(urls) == 1 and urls[0] in seeds, f"page budget indexed unexpected results: {urls}")
        budget_path = urllib.parse.urlsplit(urls[0]).path
        expected_requests = dict(before_requests)
        expected_requests["/robots.txt"] += 1
        expected_requests[budget_path] += 1
        self.corpus.assert_requests(expected_requests)
        self.passed_case("page budget: a limit of one permits exactly one attempt with multiple queued seeds")
        app.stop()
        app, port = self.app(budget_data, direct, self.corpus.base + "/missing", "--sites", "100", "--maxdepth", "1", "--pages", "1")
        until(lambda: "爬取收尾完成" in app.text(), [app, *gateways])
        stats = self.ready(app, port, gateways)
        require(stats["docs"] == stats["terms"] == stats["postings"] == 0, f"failed attempt must not retain stale index: {stats}")
        require("fetched=0 fail=1" in app.text(), "failed fetch did not consume exactly one attempt")
        expected_requests["/robots.txt"] += 1
        expected_requests["/missing"] += 1
        self.corpus.assert_requests(expected_requests)
        self.passed_case("page budget: a failed attempt consumes the budget without public-seed refill")
        app.stop()
        # A larger remaining budget must not reintroduce built-in sites when
        # the explicitly scoped corpus has no successful pages left.
        app, port = self.app(budget_data, direct, self.corpus.base + "/missing",
                              "--sites", "100", "--maxdepth", "1", "--pages", "10")
        until(lambda: "爬取收尾完成" in app.text(), [app, *gateways])
        stats = self.ready(app, port, gateways)
        require(stats["docs"] == stats["terms"] == stats["postings"] == 0,
                f"scoped empty corpus must remain empty: {stats}")
        require("fetched=0 fail=1" in app.text(), "scope exhaustion must stop after its single failed seed")
        require((budget_data / "dedupe.tsv").read_text().splitlines() == [self.corpus.base + "/missing"],
                "scope exhaustion injected unrequested seeds")
        expected_requests["/robots.txt"] += 1
        expected_requests["/missing"] += 1
        self.corpus.assert_requests(expected_requests)
        self.passed_case("crawl origin: exhausted corpus never refills public seeds even with remaining budget")
        app.stop()
        for gateway in gateways:
            gateway.stop()

        data = self.root / "empty-data"
        data.mkdir()
        # Start app before the JVM, and occupy requested web port until teardown.
        blocker = socket.socket()
        self.stack.callback(blocker.close)
        blocker.bind(("0.0.0.0", 0))
        blocker.listen()
        blocked_port = blocker.getsockname()[1]
        # Keep the future JVM endpoint reserved while the app chooses its Web
        # fallback port. Otherwise that fallback can steal the JVM's free port
        # and the app attempts a Parrot handshake against its own HTTP socket.
        late_jvm = socket.socket()
        self.stack.callback(late_jvm.close)
        # Match the Web listener's wildcard bind. On macOS, a wildcard listener
        # using SO_REUSEADDR can coexist with a held loopback-only listener.
        late_jvm.bind(("0.0.0.0", 0))
        late_jvm.listen()
        jvm_port = late_jvm.getsockname()[1]
        app, _ = self.app(data, {"jvm": jvm_port}, "--serve-only", port=blocked_port)
        until(lambda: "jvm 网关未就绪" in app.text(), [app])
        web_choice = re.search(r"Web 服务改用端口 (\d+)", app.text())
        require(web_choice and int(web_choice.group(1)) != jvm_port, "Web fallback collided with the reserved JVM endpoint")
        late_jvm.close()
        gateways, _ = self.gateways(data, jvm_only=True, jvm_port=jvm_port)
        match = until(lambda: re.search(r"Web 服务改用端口 (\d+)", app.text()), [app, *gateways])
        actual_port = int(match.group(1))
        require(actual_port != blocked_port, "app claimed the occupied port")
        stats = self.ready(app, actual_port, gateways)
        require(stats["docs"] == stats["terms"] == stats["postings"] == 0, f"empty index: {stats}")
        require("暂无索引" in text_at(actual_port, "/docs"), "empty docs not rendered")
        require("暂无词条" in text_at(actual_port, "/terms"), "empty terms not rendered")
        require(results(actual_port, self.token)[0] == [], "empty index returned results")
        self.corpus.assert_requests(expected_requests)
        self.passed_case("late JVM readiness, owned port collision and empty index business responses")
        app.stop()
        app, _ = self.app(data, {"jvm": jvm_port}, "--serve-only", port=0)
        match = until(lambda: re.search(r"Web listening on 0.0.0.0:(\d+)", app.text()), [app, *gateways])
        actual_port = int(match.group(1))
        require(actual_port > 0, "port zero did not report its actual bound port")
        require(self.ready(app, actual_port, gateways)["docs"] == 0, "ephemeral server could not query the real empty index")
        self.passed_case("ephemeral Web port: reports the actual OS-assigned listener and serves business requests")
        app.stop()
        for gateway in gateways:
            gateway.stop()
        # Exercise one full eight-document submission and the final one-document
        # tail through actual Ray/JVM instances, not only two-page examples.
        batch_data = self.root / "complete-document-batches"
        batch_data.mkdir()
        gateways, addresses = self.gateways(batch_data)
        paths = [f"/batch-{i}" for i in range(9)]
        before_requests = dict(self.corpus.requests)
        app, port = self.app(batch_data, addresses, *[self.corpus.base + path for path in paths],
                             '--pages', '9', '--sites', '1', '--maxdepth', '0')
        self.completed(app, port, gateways, 9)
        self.assert_search(port, paths)
        expected = Counter(before_requests)
        expected['/robots.txt'] += 1
        expected.update(paths)
        self.corpus.assert_requests(dict(expected))
        self.passed_case("full document batch plus tail: nine exact persisted documents and search URLs")
        app.stop()
        for gateway in gateways:
            gateway.stop()


    def run_faults(self):
        data = self.root / "fault-data"
        data.mkdir()
        gateways, direct = self.gateways(data)
        failures = []
        document_id = int.from_bytes(hashlib.sha256((self.corpus.base + '/extra').encode()).digest()[:8], 'little')
        def token_terms(entries):
            return struct.pack('<I', len(entries)) + b''.join(
                struct.pack('<I', len(word)) + word + struct.pack('<QI', doc, tf)
                for word, doc, tf in entries)
        cases = [
            ("clear", "jvm", b"bin:ws/ClearAck", b"", None),
            ("push", "erl", b"bin:ws/PushAck", b"", None),
            ("document", "jvm", b"bin:ws/DocumentsAck", b"", None),
            ("tokenizer-payload", "ray", b"bin:ws/Terms", b"", None),
            ("checkpoint", "jvm", b"bin:ws/CheckpointV2Ack", b"", None),
            ("crawl-state", "jvm", b"bin:ws/CrawlStateV2R", b"", None),
            ("checkpoint-epoch", "jvm", b"bin:ws/CheckpointV2Ack", bytes(56), None),
            ("document-count", "jvm", b"bin:ws/DocumentsAck", struct.pack("<I", 0), None),
            ("tokenizer-foreign-document", "ray", b"bin:ws/Terms", token_terms([(b'foreign', document_id ^ 1, 1)]), None),
            ("tokenizer-duplicate-term", "ray", b"bin:ws/Terms", token_terms([(b'dup', document_id, 1), (b'dup', document_id, 2)]), None),
            ("tokenizer-zero-frequency", "ray", b"bin:ws/Terms", token_terms([(b'zero', document_id, 0)]), None),
            ("tokenizer-empty-term", "ray", b"bin:ws/Terms", token_terms([(b'', document_id, 1)]), None),
            ("document-type", "jvm", b"bin:ws/DocumentsAck", struct.pack("<I", 1), b"bin:ws/FlushAck"),
        ]
        for name, gateway, key, payload, replacement in cases:
            with ExitStack() as owned:
                proxy = CorruptFirstReply(direct[gateway], key, payload, replacement)
                owned.callback(proxy.close)
                addresses = dict(direct, **{gateway: proxy.port})
                app, _port = self.app(data, addresses, self.corpus.base + "/extra", "--sites", "1", "--maxdepth", "0", "--pages", "1")
                owned.callback(app.stop)
                try:
                    until(lambda: app.child.poll() is not None or "爬取收尾完成" in app.text(), gateways, seconds=90)
                    require(proxy.corrupted == 1, f"{name}: target reply was not intercepted")
                    require(app.child.poll() not in (None, 0), f"{name}: app continued after invalid acknowledgement")
                    require("panicked" not in app.text(), f"{name}: app panicked instead of reporting failure")
                    require("爬取完成" not in app.text() and "爬取收尾完成" not in app.text(), f"{name}: false completion marker")
                    require("failed" in app.text() or "acknowledged" in app.text(), f"{name}: missing failure diagnostic")
                    if name == "push":
                        journal = data / "dedupe.tsv"
                        require(not journal.exists() or not journal.read_text(), "rejected Push polluted persistent dedupe")
                    self.passed_case(f"fault {name}: real malformed/rejected reply causes nonzero exit without panic or completion")
                except Exception as error:
                    failures.append(str(error))
                    print(f"FAULT_FAILED {name}: {error}; log={app.log}", flush=True)
        for gateway in gateways:
            gateway.stop()
        require(not self.corpus.blocked, f"fault suite made public requests: {self.corpus.blocked}")
        require(not failures, "; ".join(failures))


def build(root):
    # Required dependencies fail before expensive builds; no warning-only success.
    for executable in ("cargo", "java", "erl", "erlc", "mvn", "jar", "unzip"):
        require(shutil.which(executable), f"required executable missing: {executable}")
    commands = [
        [sys.executable, "-c", "import ray, jieba"],
        ["make", "build-jvm"],
        ["erlc", "-o", "interop/erlang", "interop/erlang/parrot_gw.erl", "interop/erlang/parrot_component_runtime.erl",
         "interop/erlang/parrot_beam_snapshot.erl", "interop/erlang/parrot_trace.erl", "interop/erlang/parrot_beam_guest.erl",
         "interop/erlang/parrot_beam_host.erl"],
        ["bash", "apps/websearch/build.sh", "--profile", "dev"],
        ["bash", "apps/websearch/jvm/test.sh"],
    ]
    for i, command in enumerate(commands):
        process = Process(root, f"build-{i}", command)
        try:
            rc = process.child.wait(timeout=1200)
            require(rc == 0, f"build exited {rc}: {process.log}\n{process.text()[-4000:]}")
        finally:
            process.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fast", action="store_true", help="compatibility alias: this runner covers host processes only")
    parser.add_argument("--no-build", action="store_true", help="explicitly test existing debug artifacts; default builds current sources")
    parser.add_argument("--faults-only", action="store_true", help="run all acknowledgement failure paths against real gateways")
    args = parser.parse_args()
    # macOS's default TMPDIR exceeds Ray's Unix socket pathname budget.
    root = Path(tempfile.mkdtemp(prefix="ws-e2e-", dir="/tmp"))
    print(f"Artifacts: {root}\nScope: host processes. Container matrix NOT RUN.", flush=True)
    report = {"scope": "host", "build": "explicitly skipped" if args.no_build else "required", "passed": [], "excluded": ["containers"]}
    suite = None

    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f"signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    try:
        if not args.no_build:
            build(root)
        inputs = ["target/debug/websearch", "interop/jvm/target/parrot-protocol-jvm-0.1.0.jar", "interop/erlang/parrot_gw.beam", "apps/websearch/erlang/frontier.beam", "apps/websearch/jvm/target/websearch-jvm-1.0.0.jar", "apps/websearch/python/tokenizer.py", "interop/python/parrot_protocol/ray_gw.py"]
        report["artifact_sha256"] = {}
        for relative in inputs:
            digest = hashlib.sha256()
            with (REPO / relative).open("rb") as source:
                for block in iter(lambda: source.read(1024 * 1024), b""):
                    digest.update(block)
            report["artifact_sha256"][relative] = digest.hexdigest()
        with ExitStack() as stack:
            suite = Suite(root, REPO / "target/debug/websearch", stack)
            if not args.faults_only:
                suite.run()
            suite.run_faults()
        report["status"] = "passed"
        print(f"PASS host suite ({len(suite.passed)} cases); artifacts: {root}", flush=True)
        return 0
    except (Exception, KeyboardInterrupt) as error:
        report["status"] = "failed"
        report["error"] = str(error)
        print(f"FAIL: {error}\nArtifacts retained: {root}", file=sys.stderr, flush=True)
        return 1
    finally:
        if suite is not None:
            report["passed"] = suite.passed
            report["crawl_origin"] = suite.corpus.base
            report["corpus_token"] = suite.token
            report["http_requests"] = dict(suite.corpus.requests)
            report["blocked_crawler_requests"] = suite.corpus.blocked
            report["blocked_runtime_probes"] = suite.runtime_proxy.blocked
        (root / "result.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    sys.exit(main())
