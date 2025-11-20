"""Run the book's actual static site through the shell launcher and JVM restart."""
from collections import Counter
from contextlib import ExitStack
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import argparse
import hashlib
import json
import os
import re
import signal
import sys
import tempfile
import threading

import run_integration as web

SITE = web.REPO / "documents/examples/websearch-site"


class TutorialSite:
    def __init__(self, stack):
        self.requests = Counter()
        self.lock = threading.Lock()
        owner = self

        class Handler(SimpleHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                with owner.lock:
                    owner.requests[self.path] += 1
                super().do_GET()

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=str(SITE)))
        self.server.daemon_threads = True
        self.base = f"http://127.0.0.1:{self.server.server_port}"
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        stack.callback(thread.join, 5)
        stack.callback(self.server.server_close)
        stack.callback(self.server.shutdown)

    def counts(self):
        with self.lock:
            return dict(self.requests)


def search_contract(port, base):
    queries = {"鹦鹉实验": {"/", "/actors.html", "/search.html"},
               "BM25": {"/search.html"}, "bm25": {"/search.html"},
               "Bm25": {"/search.html"}, "紫水晶禁区": set()}
    observed = {}
    for query, paths in queries.items():
        urls, scores = web.results(port, query)
        expected = {base + path for path in paths}
        web.require(set(urls) == expected and len(urls) == len(expected),
                    f"tutorial query {query!r}: {urls} != {expected}")
        web.require(len(scores) == len(urls) and all(float(score) > 0 for score in scores),
                    f"tutorial query scores: {query!r}: {scores}")
        observed[query] = dict(zip(urls, scores))
    web.require(observed["BM25"] == observed["bm25"] == observed["Bm25"],
                "ASCII query case changed tutorial ranking")
    web.require("3 个页面" in web.text_at(port, "/docs"), "tutorial document browser count")
    web.require("bm25" in web.text_at(port, "/terms").lower(), "tutorial term browser missing BM25")
    return observed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--no-build", action="store_true", help="explicitly reuse existing runtime artifacts")
    parser.add_argument("--binary", type=Path, help="existing application; requires --no-build")
    options = parser.parse_args()
    if options.binary is not None and not options.no_build:
        parser.error("--binary requires --no-build")
    root = Path(tempfile.mkdtemp(prefix="ws-tutorial-", dir="/tmp"))
    report = dict(status="failed", runtime_root=str(root), cases=[],
                  build="explicitly skipped" if options.no_build else "required")
    print("Artifacts:", root, flush=True)

    def interrupted(signum, _frame):
        raise KeyboardInterrupt(signum)

    signal.signal(signal.SIGTERM, interrupted)
    site = None
    try:
        binary = ((options.binary or web.REPO / "target/debug/websearch").resolve()
                  if options.no_build else web.build(root))
        web.require(binary.is_file() and os.access(binary, os.X_OK), f"missing executable: {binary}")
        report.update(binary=str(binary), binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                      corpus_sha256={path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                                     for path in sorted(SITE.iterdir()) if path.is_file()})
        with ExitStack() as stack:
            site = TutorialSite(stack)
            suite = web.Suite(root, binary, stack)
            original = None
            data = root / "data"
            for name in ("crawl", "serve-only"):
                run = root / name
                env = {key: value for key, value in suite.env.items() if not key.startswith("WS_")}
                env.update(WS_PROFILE="dev", WS_SKIP_BUILD="1", WS_BIN=str(binary),
                           WS_RUN_DIR=str(run), WS_STARTUP_TIMEOUT="90", WS_TIMEOUT="300",
                           PYTHON=sys.executable, WS_RAY_HTTP_PROXY=suite.runtime_proxy.base)
                command = ["bash", str(web.APP / "run.sh"), "--data", str(data), "--port", "0"]
                if name == "crawl":
                    command += ["--crawl-origin", site.base, "--pages", "10", "--sites", "10",
                                "--maxdepth", "2", site.base + "/"]
                else:
                    command += ["--serve-only"]
                owner = suite.process("tutorial-" + name, command, env)
                match = web.until(lambda: re.search(r"WEBSEARCH_READY=http://127\.0\.0\.1:(\d+)", owner.text()),
                                  [owner], seconds=180)
                port = int(match.group(1))
                if name == "crawl":
                    web.until(lambda: "爬取收尾完成" in (run / "app.log").read_text(), [owner], seconds=90)
                stats = json.loads(web.text_at(port, "/stats"))
                case = dict(name=name, stats=stats, requests=site.counts())
                report["cases"].append(case)
                web.require(stats["docs"] == 3 and stats["segments"] > 0,
                            f"tutorial must commit exactly three pages: {stats}; requests={site.counts()}")
                current = search_contract(port, site.base)
                case["queries"] = current
                if original is None:
                    original = current
                else:
                    web.require(current == original, "fresh JVM changed tutorial URLs or scores")
                web.require(site.counts() == {"/robots.txt": 1, "/": 1, "/actors.html": 1, "/search.html": 1},
                            f"duplicate, forbidden or restart fetch: {site.counts()}")
                suite.corpus.assert_requests({})
                owner.child.send_signal(signal.SIGTERM)
                owner.child.wait(timeout=85)
                terminal = json.loads((run / "result.json").read_text())
                case["terminal"] = terminal
                web.require(owner.child.returncode == 143 and terminal["exit_code"] == 143 and terminal["ready"],
                            f"launcher retirement: {terminal}")
                web.require(terminal["binary"] == str(binary), "launcher used a different application")
                children = terminal["children"]
                web.require(all(child["retired"] and child["guardian_exit"] is not None for child in children),
                            "tutorial guardian did not retire")
                app = next(child for child in children if child["name"] == "app")
                web.require(app["command"][0] == str(binary) and app["exit_code"] == 0
                            and "shutdown complete" in (run / "app.log").read_text(), "application retirement")
                roles = {child["name"] for child in children}
                expected = {"jvm", "app"} if name == "serve-only" else {"erl", "ray", "jvm", "app"}
                web.require(roles == expected | {"python-check", "validate"},
                            f"wrong tutorial runtime roles: {roles}")
                web.require(all(child["exit_code"] == 0 for child in children
                                if child["name"] in {"python-check", "validate"}), "preflight did not succeed")
                print("PASS actual tutorial:", name, flush=True)
            report["status"] = "passed"
    except (Exception, KeyboardInterrupt) as error:
        report["error"] = str(error)
        raise
    finally:
        if site is not None:
            report["requests"] = site.counts()
        body = json.dumps(report, indent=2, ensure_ascii=False) + "\n"
        (root / "result.json").write_text(body)
        if options.report:
            options.report.write_text(body)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
