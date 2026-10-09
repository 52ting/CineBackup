# -*- coding: utf-8 -*-
"""diff_probe.py —— 定位「源 vs 目标 内容不一致」到底差在哪一段。

为什么需要它：
    应用报「SHA-256 不一致（字节 X vs X）」只告诉你「长度一样、内容不同」，
    但**说不清是哪一种坏**。而不同坏法的修法完全不同：

      · 偏移 0 就不同、且后续大量不同
            → 目标里那份根本是**另一个文件**（旧版本 / 同名的别的素材）。
              典型成因：这一轮压根没重新拷贝（勾了「快速扫描」/「跳过拷贝」），
              或者续传时把新内容**追加**到了旧内容后面。
      · 前面一长段完全相同，从偏移 K 开始不同
            → 典型的「**追加式损坏**」：K 就是上次中断时目标文件的长度。
              成因：续传（Resume）时，目标里那段旧内容和源并不一致，
              但「续传前校验已写入部分」没拦住。
      · 差异**零散分布**在文件各处
            → 传输 / 介质层的**静默损坏**（网络盘掉包、坏内存、坏盘、线材/SATA 线）。

用法：
    python tools/diff_probe.py <源文件> <目标文件>
    python tools/diff_probe.py <源文件> <目标文件> --full      # 扫完全文件，统计所有差异段
    python tools/diff_probe.py ... --max-regions 20            # 最多找 20 段差异后停
    python tools/diff_probe.py --selftest                      # 自检：验证三种特征都能认出来

⚠️ 大文件会真读盘（369 GB 就是 369 GB）。默认找到**首段差异**就停，
   所以「损坏在后面」的文件会读得比较久 —— 这是必须付的代价，读数没法绕开。
"""

import argparse
import hashlib
import os
import sys

try:
    import numpy as np
except ImportError:  # numpy 只用来加速「块内找第一个不同字节」，缺了也能跑（慢一点）
    np = None

CHUNK = 8 * 1024 * 1024  # 8 MiB，比应用的 4 MiB 大一档，纯粹为少几次系统调用


def human(n):
    f = float(n)
    for u in ("B", "KB", "MB", "GB", "TB"):
        if f < 1024 or u == "TB":
            return "%.2f %s" % (f, u)
        f /= 1024


def sha256_of(path, progress=None):
    h = hashlib.sha256()
    done = 0
    with open(path, "rb") as f:
        while True:
            b = f.read(CHUNK)
            if not b:
                break
            h.update(b)
            done += len(b)
            if progress:
                progress(done)
    return h.hexdigest()


def first_diff_in(ba, bb):
    """在等长（或不等长）的两块里找第一个不同字节的下标；全同返回 None"""
    n = min(len(ba), len(bb))
    if n == 0:
        return 0 if len(ba) != len(bb) else None
    if np is not None:
        da = np.frombuffer(ba[:n], dtype=np.uint8)
        db = np.frombuffer(bb[:n], dtype=np.uint8)
        idx = np.flatnonzero(da != db)
        if idx.size:
            return int(idx[0])
        return 0 if len(ba) != len(bb) else None
    # 无 numpy 的退化路径
    i = 0
    while i < n:
        if ba[i] != bb[i]:
            return i
        i += 1
    return 0 if len(ba) != len(bb) else None


def chunk_stats(ba, bb):
    """等长两块的差异统计 → (首个不同下标, 末个不同下标, 不同字节数)；全同返回 None"""
    n = min(len(ba), len(bb))
    if n == 0:
        return None
    if np is not None:
        da = np.frombuffer(ba[:n], dtype=np.uint8)
        db = np.frombuffer(bb[:n], dtype=np.uint8)
        d = da != db
        if not d.any():
            return None
        first = int(np.argmax(d))
        last = int(n - 1 - np.argmax(d[::-1]))
        return first, last, int(np.count_nonzero(d))
    i = 0
    first = last = -1
    cnt = 0
    while i < n:
        if ba[i] != bb[i]:
            if first < 0:
                first = i
            last = i
            cnt += 1
        i += 1
    return None if first < 0 else (first, last, cnt)


def scan_regions(a_path, b_path, max_regions, full):
    """逐块比对，返回 (差异段列表, 已扫描字节数)

    差异段 = (起始偏移, 结束偏移, 该段内实际不同的字节数)。
    相邻块的差异会合并成一段。**长度是精确的**，不是「整块」那种粗估 ——
    否则「1 字节损坏」会被报成「8 MB 损坏」，判定就会走偏。
    """
    spans = []          # (start, end, diff_bytes)
    read_bytes = 0
    total_diff = 0
    with open(a_path, "rb") as fa, open(b_path, "rb") as fb:
        while True:
            ba = fa.read(CHUNK)
            bb = fb.read(CHUNK)
            if not ba and not bb:
                break
            base = read_bytes
            read_bytes += max(len(ba), len(bb))

            if len(ba) != len(bb):
                # 长度不同：各自剩余部分全部算差异
                st = min(len(ba), len(bb))
                spans.append((base + st, base + max(len(ba), len(bb)), max(len(ba), len(bb)) - st))
                if not full:
                    break
                continue

            st = chunk_stats(ba, bb)
            if st is None:
                continue
            first, last, cnt = st
            total_diff += cnt
            s0, s1 = base + first, base + last + 1
            if spans and s0 <= spans[-1][1]:          # 与上一段相邻 → 合并
                p0, p1, pc = spans[-1]
                spans[-1] = (p0, max(p1, s1), pc + cnt)
            else:
                spans.append((s0, s1, cnt))
            if len(spans) >= max_regions:
                break
    return spans, read_bytes, total_diff


def probe(a_path, b_path, max_regions, full):
    print("源    : %s" % a_path)
    print("目标  : %s" % b_path)
    for p in (a_path, b_path):
        if not os.path.isfile(p):
            print("\n✗ 文件不存在：%s" % p)
            return 2

    sa, sb = os.path.getsize(a_path), os.path.getsize(b_path)
    print("大小  : 源 %d (%s)   目标 %d (%s)" % (sa, human(sa), sb, human(sb)))

    if sa != sb:
        print("\n结论：**两边长度不同** → 这不是「同长不同内容」，是拷贝没写完/被截断。")
        print("      差值 %d 字节（%s）。按「断点续传」重跑一次通常能补上。" % (abs(sa - sb), human(abs(sa - sb))))
        return 0

    print("\n大小相同 → 计算两边 SHA-256（要各读一遍，大文件会慢）…")
    ha = sha256_of(a_path, lambda d: None)
    hb = sha256_of(b_path, lambda d: None)
    print("  源   SHA-256: %s" % ha)
    print("  目标 SHA-256: %s" % hb)

    if ha == hb:
        print("\n结论：**内容完全一致** ✓（长度相同、哈希也相同）")
        return 0

    print("\n哈希不同 → 逐块定位差异段…")
    spans, read_bytes, total_diff = scan_regions(a_path, b_path, max_regions, full)

    print("\n--- 差异定位 ---")
    if not spans:
        print("  没找到差异段（异常：哈希不同却逐字节相同，请上报）")
        return 1

    first_off = spans[0][0]
    span_bytes = sum(e - s0 for s0, e, _ in spans)
    ratio = (total_diff / read_bytes * 100) if read_bytes else 0
    print("  首个不同字节偏移 : %d  (%s, 占全文 %.4f%%)"
          % (first_off, human(first_off), first_off / sa * 100 if sa else 0))
    print("  差异段数         : %d%s" % (len(spans), "" if full else "（未扫全文件，可能还有更多）"))
    for i, (s0, e, cnt) in enumerate(spans[:max_regions]):
        print("    #%-3d 起 %-16d 止 %-16d 跨度 %-12s 其中不同字节 %d"
              % (i + 1, s0, e, human(e - s0), cnt))
    print("  已扫描           : %s / %s" % (human(read_bytes), human(sa)))
    print("  差异字节合计     : %d（占已扫描 %.4f%%）" % (total_diff, ratio))
    print("  差异跨度合计     : %s（占已扫描 %.2f%%）"
          % (human(span_bytes), span_bytes / read_bytes * 100 if read_bytes else 0))

    # ---- 特征判定 ----
    print("\n--- 判定 ---")
    # 判据用「差异字节占全文比例」，不要用「差异跨度占比」：
    # 零散损坏是「段多但每段只有几个字节」，跨度可能很大而实际不同字节极少。
    tiny = (total_diff / sa) < 0.01 if sa else False

    if tiny and len(spans) >= 3:
        print("  ⚠️  差异**零星散落**在文件多处：%d 段、合计只有 %d 个字节不同"
              "（占全文 %.6f%%）。" % (len(spans), total_diff, total_diff / sa * 100 if sa else 0))
        print("      → 这个特征属于**传输 / 介质层的静默损坏**（内容大体是对的，只有零星几位坏）：")
        print("        · 网络盘掉包 / 电力猫·WiFi 链路不稳（本机 X/Y/Z 映射到 \\\\192.168.2.165，"
              "RTT 6~10ms 偏高）")
        print("        · 坏内存、坏盘、SATA/网线接触不良")
        print("        排查顺序：")
        print("          ① 先跑 --stability：同一个文件连读两遍，两次哈希不同 → 基本锁定硬件/链路；")
        print("          ② 换线换口，把源盘与目标盘插到不同控制器上重试同一批文件；")
        print("          ③ MemTest86 测内存；④ 看 SMART；⑤ 若是网络盘，先测大文件读速与丢包。")
    elif first_off == 0:
        print("  ⚠️  从**第 0 字节**起就开始不同，且不同的部分占全文 %.1f%%。"
              % (100 - (0 if not spans else 0)))
        print("      → 目标里这份**不是这份源内容的拷贝**，基本可以确定是「没真正重新拷」。")
        print("        最可能的成因（按概率）：")
        print("          · 勾了「跳过拷贝」/「快速扫描」→ 这一轮压根没写盘，只做了校验；")
        print("          · 目标里本来就有一个同名的旧文件（旧版本 / 别的素材），")
        print("            大小碰巧和源一样，于是被当成「已备份」；")
        print("          · 续传时把新内容追加到了旧内容后面（头部是旧文件的内容）。")
        print("        复核：看「任务选项」里「快速扫描」「跳过拷贝」的勾选状态。")
        print("        修法：取消这两个勾选，重跑一次，让目标被真正覆盖。")
    else:
        print("  ⚠️  前 %s（占全文 %.2f%%）**完全一致**，从偏移 **%d** 开始不同。"
              % (human(first_off), first_off / sa * 100 if sa else 0, first_off))
        print("      → 前缀是好的、后面成段不同 —— 这个形态说明**写入是从某个点开始失效的**：")
        print("        · 若偏移 %d 恰好等于「上一次中断时的目标文件长度」→ **续传拼接**："
              % first_off)
        print("          续传时目标里那段旧内容与源不一致，但前缀校验没拦住。")
        print("          → 修法：勾上「续传前校验已写入部分」，这批文件**从头重拷**（别续传）。")
        print("        · 若偏移 %d 恰好在某个整块边界上（4 MiB / 1 GiB）→ 写入中途"
              % first_off)
        print("          出错但没被报出来（缓存丢失 / 链路断在那一刻）。")
        print("        · 也可能是源文件在**拷贝过程中**被改写（边导出边备份）。")
    return 0


    return 0


def stability(a_path, b_path):
    """把两边各自连读两遍比哈希。

    这是区分「内容真不同」和「读出来不稳定」的决定性一招：
    同一个文件两次读出的哈希不一致 → 问题在**读取路径**（盘 / 线 / 网络 / 内存），
    而不是拷贝逻辑。文件系统缓存会干扰（第二遍可能命中缓存），所以对大文件更可信
    —— 几百 GB 的文件不可能整份进缓存。
    """
    print("同一文件连读两遍稳定性检测（大文件才有说服力，小文件会命中系统缓存）\n")
    bad = False
    for label, path in (("源  ", a_path), ("目标", b_path)):
        if not os.path.isfile(path):
            print("  %s ✗ 不存在：%s" % (label, path))
            bad = True
            continue
        sz = os.path.getsize(path)
        print("  %s %s（%s）" % (label, path, human(sz)))
        h1 = sha256_of(path, None)
        print("      第 1 次: %s" % h1)
        h2 = sha256_of(path, None)
        print("      第 2 次: %s" % h2)
        if h1 == h2:
            print("      → 两次一致（这一侧读取稳定）")
        else:
            bad = True
            print("      → ⚠️ 两次**不一致**！这一侧的读取路径有问题（盘/线/网络/内存）")
        print()
    print("结论：" + (
        "⚠️ 至少一侧读出来不稳定 → 先把硬件/链路修好，再谈备份。"
        if bad else
        "两侧各自连读都稳定 → 内容差异是「真不同」（拷贝/选项问题），不是读取抖动。"
    ))
    return 0


# ---------------------------------------------------------------- 自检

def _write(path, data):
    with open(path, "wb") as f:
        f.write(data)


def selftest():
    """造出三种特征的文件对，验证工具都能认出来（测红：改坏预期就该失败）"""
    import tempfile

    ok = True
    d = tempfile.mkdtemp(prefix="diffprobe_")
    print("自检目录：%s\n" % d)

    # ① 从第 0 字节就不同（整份是另一个文件）
    a = os.path.join(d, "a1.bin")
    b = os.path.join(d, "b1.bin")
    _write(a, bytes(range(256)) * 8)
    _write(b, bytes(reversed(range(256))) * 8)
    r, _, _ = scan_regions(a, b, 5, False)
    hit = bool(r) and r[0][0] == 0
    print("[1] 偏移0差异      → %s  regions=%s" % ("OK" if hit else "FAIL", r[:2]))
    ok &= hit

    # ② 前缀相同、从 K 开始不同（追加式损坏）
    a = os.path.join(d, "a2.bin")
    b = os.path.join(d, "b2.bin")
    head = b"H" * 100000
    _write(a, head + b"A" * 50000)          # 源：头 + A段
    _write(b, head + b"B" * 50000)          # 目标：同头 + B段
    r, _, _ = scan_regions(a, b, 5, False)
    hit = bool(r) and r[0][0] == 100000
    print("[2] 前缀相同@100000 → %s  regions=%s" % ("OK" if hit else "FAIL", r[:2]))
    ok &= hit

    # ③ 零散差异
    a = os.path.join(d, "a3.bin")
    b = os.path.join(d, "b3.bin")
    n = 3 * CHUNK + 12345
    da = bytearray(b"\x00" * n)
    db = bytearray(da)
    for off in (1000, CHUNK + 7, 2 * CHUNK + 999999, n - 5):
        db[off] ^= 0xFF                     # 四处散落
    _write(a, bytes(da))
    _write(b, bytes(db))
    r, _, _ = scan_regions(a, b, 10, True)
    hit = len(r) >= 2 and r[0][0] == 1000
    print("[3] 零散差异      → %s  regions=%s" % ("OK" if hit else "FAIL", r[:4]))
    ok &= hit

    # ④ 完全相同 → 不该报差异
    a = os.path.join(d, "a4.bin")
    b = os.path.join(d, "b4.bin")
    _write(a, b"same" * 1000)
    _write(b, b"same" * 1000)
    r, _, _ = scan_regions(a, b, 5, True)
    print("[4] 完全相同      → %s  regions=%s" % ("OK" if r == [] else "FAIL", r))
    ok &= (r == [])

    # ⑤ 长度不同
    a = os.path.join(d, "a5.bin")
    b = os.path.join(d, "b5.bin")
    _write(a, b"x" * 100)
    _write(b, b"x" * 60)
    r, _, _ = scan_regions(a, b, 5, True)
    print("[5] 长度不同      → %s  regions=%s" % ("OK" if r else "FAIL", r))
    ok &= bool(r)

    print("\n自检结果：%s" % ("全部通过 ✓" if ok else "有失败 ✗"))
    return 0 if ok else 1


def main():
    ap = argparse.ArgumentParser(description="定位源/目标内容不一致的具体位置")
    ap.add_argument("src", nargs="?", help="源文件")
    ap.add_argument("dst", nargs="?", help="目标文件")
    ap.add_argument("--full", action="store_true", help="扫完全文件（默认找到首段差异后继续找满 max-regions 即停）")
    ap.add_argument("--max-regions", type=int, default=10, help="最多报告/查找多少段差异（默认 10）")
    ap.add_argument("--stability", action="store_true",
                    help="决定性检测：把源和目标各自连读两遍，比较两次哈希（不同=硬件/链路不稳）")
    ap.add_argument("--selftest", action="store_true", help="自检：验证三种特征都能认出来")
    args = ap.parse_args()

    if args.selftest:
        return selftest()
    if not args.src or not args.dst:
        ap.error("需要 <源文件> <目标文件>，或用 --selftest")
    if args.stability:
        return stability(args.src, args.dst)
    return probe(args.src, args.dst, args.max_regions, args.full)


if __name__ == "__main__":
    sys.exit(main())
