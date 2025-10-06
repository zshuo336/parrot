"""crawler-lab 索引构建组件（ray 方言——app 内 source of truth）。

R1（应用体系架构纠正）：本模块是 apps/crawler-lab 的业务代码，
经 parrot 标准包分发：crawler.app.toml 声明 `artifact = { PyModule =
{ module = "index_builder", ... } }`，ray 网关 deploy 时以本目录为
working_dir 载入 `parrot_entry(ctx)` 起命名 actor。

协议（与 Rust hub 逐字节对齐——run_regression golden 锚定）：
  bin:crawl/IndexPage  [n u32][{doc u64|len u32|html}...] → IndexAck [pages u32][terms u32]
  bin:crawl/IndexStats []                                → IndexStatsR [terms u32][postings u64]
"""

from __future__ import annotations

import struct

_STOP = frozenset(
    "the a an of to in and or for on with at by is it as be html head title page body".split()
)


def _tokenize(html: bytes) -> list[str]:
    text = html.decode("utf-8", errors="replace").lower()
    for ch in "<>=/\"'!?,.:;()[]{}":
        text = text.replace(ch, " ")
    return [t for t in text.split() if t and t not in _STOP and len(t) > 1]


def _decode_pages(p: bytes) -> list[tuple[int, bytes]]:
    """[n u32][{doc u64|len u32|html}...]"""
    (n,) = struct.unpack_from("<I", p, 0)
    off, pages = 4, []
    for _ in range(n):
        doc, ln = struct.unpack_from("<QI", p, off)
        off += 12
        pages.append((doc, p[off : off + ln]))
        off += ln
    return pages


def build_dispatcher():
    """组件入口：返回带 crawl/IndexPage + IndexStats handler 的 dispatcher。

    索引分片：dict[term -> dict[doc_id -> tf]]（ray worker actor 串行化
    保证无锁一致）。
    """
    from parrot_protocol.ray_gw import ParrotDispatcher

    d = ParrotDispatcher()
    _index: dict[str, dict[int, int]] = {}

    @d.handler("bin:crawl/IndexPage")
    def _index_page(_k: str, p: bytes) -> tuple[str, bytes]:
        pages = _decode_pages(p)
        terms = 0
        for doc, html in pages:
            for t in _tokenize(html):
                slot = _index.setdefault(t, {})
                slot[doc] = slot.get(doc, 0) + 1
                terms += 1
        return ("bin:crawl/IndexAck", struct.pack("<II", len(pages), terms))

    @d.handler("bin:crawl/IndexStats")
    def _index_stats(_k: str, _p: bytes) -> tuple[str, bytes]:
        postings = sum(len(v) for v in _index.values())
        return ("bin:crawl/IndexStatsR", struct.pack("<IQ", len(_index), postings))

    return d


def parrot_entry(ctx: dict):
    """ray 方言组件契约入口（deploy 载入点）。

    本地/单机形态：返回 dispatcher 本身（网关单 worker actor 化）。
    真集群形态（TODO）：JobSubmissionClient submit working_dir 后由
    ray actor 包装本 dispatcher。
    """
    return build_dispatcher()
