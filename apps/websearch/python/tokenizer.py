"""websearch 中文分词索引组件（ray 方言——app 内 source of truth）。

R1 架构（五运行时分工，同 crawler-lab 原则）：
  - Rust 漫爬 worker 抓真实网页 → 整页 HTML 发本组件
  - 本组件 jieba 中文分词 + 词频统计 → 词项条目回 Rust（Rust 转发 akka 建倒排）
  - 落盘：段式索引文件由本组件写（data/index/seg-*.segment——重启不丢）

分词：jieba（开源中文分词——Python 原版）；与 JVM 侧 jieba-analysis、
Rust 侧 jieba-rs 同族（词典算法一致的三语言移植）。

协议（bin:ws/* 键空间）：
  bin:ws/Tokenize  [n u32][{docid u64|len u32|text}...] → Terms [n u32][{len u32|term|docid u64|tf u32}...]
  bin:ws/IndexStats []                                   → [terms u32][postings u64]
"""

from __future__ import annotations

import os
import struct
import threading

import jieba

_STOP = frozenset(
    "the a an of to in and or for on with at by is it as be 的 了 和 是 在 也 有 就 "
    "不 人 都 一 一个 我们 你们 他们 这 那 这个 那个 什么 没有 还有 因为 所以 但是 如果 "
    "可以 这个 那些 这些 而且 以及 或者 并且 不过 然而 因此 于是 然后 还是 只是 已经 "
    "www com http https html css js 页面 网站 链接 查看 更多 相关 参考文献 外部".split()
)

_lock = threading.Lock()
_index: dict[str, dict[int, int]] = {}  # term -> {docid: tf}
_docs: dict[int, str] = {}              # docid -> url（段落盘时写 doc 表）


def _tokenize(text: str) -> list[str]:
    """jieba 切词 + 停用词过滤 + 单字过滤（中文单字无检索力）。"""
    with _lock:
        words = jieba.lcut(text)
    return [w for w in words if w.strip() and w not in _STOP and len(w.strip()) > 1]


def _decode_texts(payload: bytes) -> list[tuple[int, str]]:
    """[n u32][{docid u64 | len u32 | text}...]"""
    (n,) = struct.unpack_from("<I", payload, 0)
    off, out = 4, []
    for _ in range(n):
        docid, ln = struct.unpack_from("<QI", payload, off)
        off += 12
        out.append((docid, payload[off : off + ln].decode("utf-8", errors="replace")))
        off += ln
    return out


def build_dispatcher():
    from parrot_protocol.ray_gw import ParrotDispatcher

    d = ParrotDispatcher()

    @d.handler("bin:ws/Tokenize")
    def _tokenize_batch(_k: str, p: bytes) -> tuple[str, bytes]:
        """批量分词：多页文本 → 词项条目（全 doc 的 (term, docid, tf) 流）。

        布局与 JVM IndexTerms 同构：[n u32][{len u32|term|docid u64|tf u32}...]
        """
        entries: list[tuple[str, int, int]] = []
        for docid, text in _decode_texts(p):
            freq: dict[str, int] = {}
            for t in _tokenize(text):
                freq[t] = freq.get(t, 0) + 1
            for term, tf in freq.items():
                entries.append((term, docid, tf))
                with _lock:
                    _index.setdefault(term, {})[docid] = tf
        buf = struct.pack("<I", len(entries))
        for term, docid, tf in entries:
            tb = term.encode("utf-8")
            buf += struct.pack("<I", len(tb)) + tb + struct.pack("<QI", docid, tf)
        return ("bin:ws/Terms", buf)

    @d.handler("bin:ws/IndexStats")
    def _stats(_k: str, _p: bytes) -> tuple[str, bytes]:
        with _lock:
            terms = len(_index)
            postings = sum(len(v) for v in _index.values())
        return ("bin:ws/IndexStatsR", struct.pack("<IQ", terms, postings))

    return d


def parrot_entry(ctx: dict):
    """ray 方言组件契约入口（deploy 载入点）。"""
    return build_dispatcher()
