"""量化测试各 GitHub 加速镜像的下载吞吐，挑最快的用来拉 WiX。

背景：本机直连 GitHub Release 资产（objects.githubusercontent.com）
吞吐极低，tauri 内置下载器因此直接超时。此处对每个候选镜像
各拉取前 512 KB 并计时，用实测速度决定用哪个。
"""
import ssl
import time
import urllib.request

ORIGIN = "https://github.com/wixtoolset/wix3/releases/download/wix3141rtm/wix314-binaries.zip"

CANDIDATES = [
    ("直连 GitHub", ORIGIN),
    ("ghproxy.net", "https://ghproxy.net/" + ORIGIN),
    ("gh-proxy.com", "https://gh-proxy.com/" + ORIGIN),
    ("ghfast.top", "https://ghfast.top/" + ORIGIN),
    ("gh.llkk.cc", "https://gh.llkk.cc/" + ORIGIN),
    ("github.moeyy.xyz", "https://github.moeyy.xyz/" + ORIGIN),
    ("ghproxy.cc", "https://ghproxy.cc/" + ORIGIN),
    ("hub.gitmirror.com", "https://hub.gitmirror.com/" + ORIGIN),
]

PROBE_BYTES = 512 * 1024
TIMEOUT = 20


def measure(name, url):
    req = urllib.request.Request(
        url, headers={"User-Agent": "Mozilla/5.0", "Range": f"bytes=0-{PROBE_BYTES-1}"}
    )
    ctx = ssl.create_default_context()
    try:
        t0 = time.time()
        got = 0
        with urllib.request.urlopen(req, timeout=TIMEOUT, context=ctx) as r:
            while got < PROBE_BYTES:
                chunk = r.read(65536)
                if not chunk:
                    break
                got += len(chunk)
                if time.time() - t0 > TIMEOUT:
                    break
        dt = max(time.time() - t0, 1e-6)
        speed = got / dt
        return got, dt, speed, None
    except Exception as e:
        return 0, 0.0, 0.0, f"{type(e).__name__}: {e}"


def human(n):
    for u in ("B", "KB", "MB"):
        if n < 1024:
            return f"{n:.0f} {u}"
        n /= 1024
    return f"{n:.1f} GB"


def main():
    print(f"{'镜像':<22}{'收到':>10}{'耗时':>9}{'速度':>12}   备注")
    print("-" * 74)
    results = []
    for name, url in CANDIDATES:
        got, dt, sp, err = measure(name, url)
        if err:
            print(f"{name:<22}{'-':>10}{'-':>9}{'-':>12}   {err[:40]}")
            continue
        print(f"{name:<22}{human(got):>10}{dt:>8.1f}s{human(sp)+'/s':>12}")
        results.append((sp, name, url))

    print()
    if not results:
        print("没有任何镜像可用。")
        return
    results.sort(reverse=True)
    sp, name, url = results[0]
    print(f"最快：{name}  ({human(sp)}/s)")
    print(f"URL：{url}")
    print()
    print("把这一行填进 fetch_wix.py 的 URL 即可：" if sp > 0 else "")
    print(f'URL = "{url}"')


if __name__ == "__main__":
    main()
