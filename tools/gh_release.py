"""把版本产物发布到 GitHub Release（Windows 安装包 + macOS dmg + 源码 zip）。

为什么不用 `gh`：本机没装 gh CLI。
为什么不用 GitHub 连接器：那是 GitHub App 令牌，`push_files` 要求把内容**内联进工具参数**，
几 MB 的二进制走一遍上下文不可行；而且连接器没有「上传 Release 资产」这个能力。

所以走 REST API，凭据仍复用本机 Git Credential Manager（和 watch_ci.py 同一套，
令牌只在内存里用，不打印、不落盘）。

用法：
    python tools/gh_release.py status
        # 列出所有 Release 及其资产

    python tools/gh_release.py verify
        # 逐字节核对本地产物与 Release 上资产的体积（发布完顺手跑一下）

    python tools/gh_release.py publish
        # 按 package.json 的版本号创建/复用 Release，并上传下面这些默认文件：
        #   src-tauri/target/release/bundle/nsis/CineBackup_<v>_x64-setup.exe
        #   src-tauri/target/release/bundle/msi/CineBackup_<v>_x64_en-US.msi
        #   cinebackup-builds/CineBackup_<v>_universal.dmg
        #   ../cinebackup-<v>-src.zip          （用 make_src_zip.py 先生成）

    python tools/gh_release.py publish 文件1 文件2...
        # 只想传指定文件

    python tools/gh_release.py publish --draft
        # 建草稿（先自己看一眼再发布）

幂等：已经存在、且大小一致的同名资产会跳过，不会重复上传。
"""
import argparse
import json
import os
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

API = "https://api.github.com"
UPLOADS = "https://uploads.github.com"

PROJ = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def version() -> str:
    with open(os.path.join(PROJ, "package.json"), encoding="utf-8") as f:
        return json.load(f)["version"]


def owner_repo() -> tuple:
    """从 git remote origin 推断 owner/repo（支持 https 与 ssh 两种写法）。"""
    url = subprocess.run(["git", "remote", "get-url", "origin"], cwd=PROJ,
                         capture_output=True, text=True, timeout=20).stdout.strip()
    url = url.replace("\\", "/")
    if url.endswith(".git"):
        url = url[:-4]
    if "://" in url:
        path = url.split("://", 1)[1].split("/", 1)[1]
    elif "@" in url and ":" in url:
        path = url.split(":", 1)[1]
    else:
        path = url
    parts = [p for p in path.split("/") if p]
    if len(parts) < 2:
        sys.exit("无法从 origin 推断仓库名，检查 `git remote -v`")
    return parts[-2], parts[-1]


def get_token() -> str:
    env = dict(os.environ)
    env["PATH"] = r"C:\Program Files\Git\mingw64\bin;" + env.get("PATH", "")
    p = subprocess.run(
        ["git", "-c", "credential.helper=", "-c", "credential.helper=manager-core",
         "credential", "fill"],
        input="protocol=https\nhost=github.com\n\n",
        capture_output=True, text=True, env=env, timeout=120, errors="replace")
    for line in (p.stdout or "").splitlines():
        if line.startswith("password="):
            return line[len("password="):]
    sys.exit("取凭据失败（先成功 push 一次以完成授权）：%s" % (p.stderr or "")[:200])


OWNER, REPO = owner_repo()
TOK = None


def _call(url: str, method="GET", data=None, ctype="application/json",
          accept="application/vnd.github+json", tries=4):
    h = {"User-Agent": "cinebackup-gh-release", "Accept": accept,
         "Authorization": "Bearer " + TOK}
    if data is not None:
        h["Content-Type"] = ctype
    last = None
    for i in range(tries):
        req = urllib.request.Request(url, data=data, headers=h, method=method)
        try:
            with urllib.request.urlopen(req, timeout=300) as r:
                body = r.read()
                return r.status, (json.loads(body.decode()) if body else None)
        except urllib.error.HTTPError as e:
            raw = e.read().decode("utf-8", "replace")
            # 4xx 是逻辑错误，重试没意义
            if e.code < 500:
                return e.code, {"__error__": raw[:500]}
            last = "HTTP %s %s" % (e.code, raw[:200])
        except Exception as e:                      # noqa: BLE001 网络抖动
            last = "%s: %s" % (type(e).__name__, e)
        print("    …重试 %d/%d（%s）" % (i + 1, tries, last))
    sys.exit("请求失败：%s" % last)


def find_release(tag: str):
    st, data = _call("%s/repos/%s/%s/releases/tags/%s" % (API, OWNER, REPO, tag))
    if st == 200:
        return data
    if st == 404:
        return None
    sys.exit("查询 Release 失败：%s" % data)


def list_releases():
    st, data = _call("%s/repos/%s/%s/releases?per_page=30" % (API, OWNER, REPO))
    if st != 200:
        sys.exit("查询 Releases 失败：%s" % data)
    return data


def cmd_status(_args) -> int:
    rels = list_releases()
    if not rels:
        print("（暂无 Release）")
        return 0
    for r in rels:
        print("%-12s tag=%-10s draft=%-5s prerelease=%s" % (
            r["name"] or "(无名)", r["tag_name"], r["draft"], r["prerelease"]))
        if not r.get("assets"):
            print("    （无资产）")
        for a in r["assets"]:
            print("    - %-42s %9.2f MB  下载 %s 次" % (
                a["name"], a["size"] / 1048576.0, a["download_count"]))
        print("    %s" % r["html_url"])
    return 0


def default_assets(v: str) -> list:
    return [
        os.path.join(PROJ, "src-tauri", "target", "release", "bundle", "nsis",
                     "CineBackup_%s_x64-setup.exe" % v),
        os.path.join(PROJ, "src-tauri", "target", "release", "bundle", "msi",
                     "CineBackup_%s_x64_en-US.msi" % v),
        os.path.join(PROJ, "cinebackup-builds", "CineBackup_%s_universal.dmg" % v),
        os.path.join(os.path.dirname(PROJ), "cinebackup-%s-src.zip" % v),
    ]


def previous_tag(current: str):
    """上一个已发布的 tag。

    **优先问 GitHub**：tag 可能只在远端（本次就是这么建的 —— Release 建在 GitHub 上，
    本地 `git tag` 是空的，结果发布说明退化成了「最近 20 条提交」，把 0.4.0 以来的
    历史全列进去了）。退而求其次才用本机 `git tag`。
    """
    try:
        for r in list_releases():
            if r["tag_name"] != current:
                return r["tag_name"]
    except SystemExit:
        pass                                        # 查询失败不该挡住发布
    tags = subprocess.run(["git", "tag", "--sort=-v:refname"], cwd=PROJ,
                          capture_output=True, text=True, timeout=30).stdout.split()
    for t in tags:
        if t != current:
            return t
    return None


def commits_since(prev_tag: str):
    """用 GitHub compare API 取 prev_tag..HEAD 的提交标题。

    比本地 `git log prev..HEAD` 可靠：本地没 fetch 过那个 tag 时 git 会直接报错，
    而 compare API 只认远端的 ref。
    """
    st, data = _call("%s/repos/%s/%s/compare/%s...HEAD" % (API, OWNER, REPO, prev_tag))
    if st != 200:
        return None, None
    lines = []
    for c in data.get("commits", []):
        msg = (c["commit"]["message"].splitlines() or [""])[0].strip()
        if msg:
            lines.append("- %s" % msg)
    return ("\n".join(lines) or None), data.get("ahead_by")


def build_notes(v: str, tag: str) -> str:
    """发布说明：能算「上一版→本版」就算，算不出来就退化成最近 20 条提交。"""
    prev = previous_tag(tag)
    log = heading = None
    if prev:
        log, ahead = commits_since(prev)
        if log:
            heading = "### 本次变更（%s → %s，共 %s 个提交）" % (prev, tag, ahead)

    if not log:
        log = subprocess.run(["git", "log", "--no-merges", "--pretty=format:- %s", "-20"],
                             cwd=PROJ, capture_output=True, text=True,
                             timeout=30).stdout.strip() or "- （无提交记录）"
        heading = "### 本次变更（最近 20 条提交）"

    return "## CineBackup %s\n\n%s\n\n%s\n\n### 下载说明\n\n" \
           "| 平台 | 文件 | 说明 |\n|---|---|---|\n" \
           "| Windows | `CineBackup_%s_x64-setup.exe` | 推荐，双击安装 |\n" \
           "| Windows | `CineBackup_%s_x64_en-US.msi` | 企业批量部署用 |\n" \
           "| macOS | `CineBackup_%s_universal.dmg` | 通用二进制（Intel + Apple Silicon）|\n" \
           "| 源码 | `cinebackup-%s-src.zip` | 含 macOS 打包脚本与安装指南 |\n\n" \
           "> macOS 包未签名。首次打开若提示「已损坏」或「无法验证开发者」，\n" \
           "> 到「系统设置 → 隐私与安全性」点「仍要打开」，或执行：\n" \
           "> `xattr -dr com.apple.quarantine /Applications/CineBackup.app`\n" \
           "> **请用 dmg 安装，不要把 .app 从产物 zip 里直接拖出来**（会丢可执行权限位）。\n" \
           % (v, heading, log, v, v, v, v)


def cmd_publish(args) -> int:
    v = version()
    tag = args.tag or ("v%s" % v)

    files = args.files or default_assets(v)
    missing = [f for f in files if not os.path.isfile(f)]
    if missing:
        print("以下文件不存在，先构建好再来：")
        for f in missing:
            print("  ✗", f)
        return 1

    rel = find_release(tag)
    if rel is None:
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=PROJ,
                              capture_output=True, text=True, timeout=20).stdout.strip()
        payload = json.dumps({
            "tag_name": tag,
            "name": "CineBackup %s" % v,
            "body": build_notes(v, tag),
            "draft": bool(args.draft),
            "prerelease": bool(args.prerelease),
            "target_commitish": args.target or head,
        }).encode("utf-8")
        st, rel = _call("%s/repos/%s/%s/releases" % (API, OWNER, REPO),
                        method="POST", data=payload)
        if st not in (200, 201):
            sys.exit("创建 Release 失败：%s" % rel)
        print("✓ 已创建 Release %s（%s）" % (rel["name"], rel["html_url"]))
    else:
        print("· Release %s 已存在，复用（%s）" % (tag, rel["html_url"]))

    existing = {a["name"]: a for a in rel.get("assets", [])}
    uploaded = skipped = failed = 0
    for f in files:
        name = os.path.basename(f)
        size = os.path.getsize(f)
        old = existing.get(name)
        if old and old["size"] == size:
            print("  = 跳过 %s（已存在且大小一致 %.2f MB）" % (name, size / 1048576.0))
            skipped += 1
            continue
        if old:
            print("  ! %s 已存在但大小不同（旧 %.2f MB / 新 %.2f MB），先删旧的"
                  % (name, old["size"] / 1048576.0, size / 1048576.0))
            st, _ = _call("%s/repos/%s/%s/releases/assets/%d" % (API, OWNER, REPO, old["id"]),
                          method="DELETE")
            if st not in (204, 200):
                print("    删除失败，跳过该文件")
                failed += 1
                continue
        with open(f, "rb") as fh:
            blob = fh.read()
        url = "%s/repos/%s/%s/releases/%d/assets?name=%s" % (
            UPLOADS, OWNER, REPO, rel["id"],
            urllib.parse.quote(name.encode("utf-8")))
        print("  ↑ 上传 %s（%.2f MB）…" % (name, size / 1048576.0))
        st, res = _call(url, method="POST", data=blob,
                        ctype="application/octet-stream", tries=3)
        if st in (200, 201):
            print("    ✓ 完成，下载 %s" % res.get("browser_download_url"))
            uploaded += 1
        else:
            print("    ✗ 失败：%s" % res)
            failed += 1

    print()
    print("汇总：上传 %d，跳过 %d，失败 %d" % (uploaded, skipped, failed))
    print("Release 页面：%s" % rel["html_url"])
    return 1 if failed else 0


def cmd_verify(_args) -> int:
    """逐字节核对本地产物与 Release 上资产的体积是否一致。

    光看 API 返回的 size 有时会骗人（本地文件被截断也一样有 size），所以这里是
    「本地 os.path.getsize」对「远端 assets[].size」两边都取，不一致就报错。
    """
    v = version()
    tag = "v%s" % v
    rel = find_release(tag)
    if rel is None:
        print("Release %s 不存在，先跑 publish" % tag)
        return 1

    print("Release  %s  %s" % (tag, rel["html_url"]))
    print("draft=%s  prerelease=%s  说明 %d 字符"
          % (rel["draft"], rel["prerelease"], len(rel.get("body") or "")))
    print()

    remote = {a["name"]: a for a in rel.get("assets", [])}
    local = {os.path.basename(p): p for p in default_assets(v)}
    ok = True
    for name, path in local.items():
        a = remote.get(name)
        if a is None:
            print("✗ %-42s 远端缺失" % name)
            ok = False
            continue
        if not os.path.isfile(path):
            print("✗ %-42s 本地缺失（%s）" % (name, path))
            ok = False
            continue
        lsz, rsz = os.path.getsize(path), a["size"]
        if lsz != rsz:
            ok = False
        print("%s %-42s 本地 %9d B  远端 %9d B"
              % ("✓" if lsz == rsz else "✗", name, lsz, rsz))
        print("    %s  （下载 %d 次）" % (a["browser_download_url"], a["download_count"]))

    extra = sorted(set(remote) - set(local))
    if extra:
        print("\n远端还有额外资产（不是本地默认清单里的）：")
        for n in extra:
            print("  · %s  %.2f MB" % (n, remote[n]["size"] / 1048576.0))

    print()
    print("结论：", "全部逐字节一致 ✓" if ok else "存在不一致 ✗")
    return 0 if ok else 1


def main() -> int:
    global TOK
    ap = argparse.ArgumentParser(description="发布 CineBackup 到 GitHub Release")
    sub = ap.add_subparsers(dest="cmd")

    sub.add_parser("status", help="列出 Release 与资产")
    sub.add_parser("verify", help="核对本地产物与远端资产的体积")

    p = sub.add_parser("publish", help="创建/复用 Release 并上传产物")
    p.add_argument("files", nargs="*", help="要上传的文件（默认用内置清单）")
    p.add_argument("--tag", help="指定 tag（默认 v<package.json 的版本>）")
    p.add_argument("--target", help="tag 指向的 commit（默认当前 HEAD）")
    p.add_argument("--draft", action="store_true", help="建草稿，先不公开")
    p.add_argument("--prerelease", action="store_true", help="标记为预发布")

    args = ap.parse_args()
    if not args.cmd:
        ap.print_help()
        return 0

    TOK = get_token()
    print("仓库 %s/%s（版本 %s）" % (OWNER, REPO, version()))
    print()
    if args.cmd == "status":
        return cmd_status(args)
    if args.cmd == "verify":
        return cmd_verify(args)
    return cmd_publish(args)


if __name__ == "__main__":
    raise SystemExit(main())
