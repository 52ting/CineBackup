# -*- coding: utf-8 -*-
"""watch_ci.py —— 在 Windows 上查看 GitHub Actions 构建状态、下载产物（无需 PAT、无需 gh）

原理：本机 Git for Windows 装了 Git Credential Manager，首次 `git push` 时一次浏览器
授权后会凭据存进 Windows 凭据管理器。本脚本用 `git credential fill` 把令牌取回进程内存，
直接调 GitHub API，因此**不需要创建 Personal Access Token，也不打印/落盘任何密钥**。

依赖：git，且该仓库的 remote 已通过 Credential Manager 授权（即成功 push 过一次）。

用法：
    python tools/watch_ci.py status        # 看最新一次运行的每个步骤状态
    python tools/watch_ci.py watch         # 盯到结束：成功则自动下载并解出 .dmg
    python tools/watch_ci.py logs <run_id> # 抓失败日志并打印尾部
    python tools/watch_ci.py get  <run_id> # 只下载产物

产物落在仓库同级的 cinebackup-builds/ 目录。
"""
import subprocess, json, urllib.request, urllib.error, os, sys, time, calendar, zipfile, io

OUTDIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), os.pardir, "cinebackup-builds")
API = "https://api.github.com"

try:
    sys.stdout.reconfigure(line_buffering=True)   # 重定向到文件时也能实时看到进度
except Exception:
    pass


def repo_root():
    return os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def owner_repo():
    """从 git remote origin 推断 owner/repo（支持 https 与 ssh 两种写法）。"""
    try:
        url = subprocess.run(["git", "remote", "get-url", "origin"], cwd=repo_root(),
                             capture_output=True, text=True, timeout=20).stdout.strip()
    except Exception:
        url = ""
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
    if len(parts) >= 2:
        return parts[-2], parts[-1]
    sys.exit("无法从 origin 推断仓库名，请检查 `git remote -v`")


OWNER, REPO = owner_repo()


def get_token():
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
    sys.exit("取凭据失败（先成功 push 一次以完成浏览器授权）：%s" % (p.stderr or "")[:200])


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """禁止自动跟随跳转 —— 下载产物时第二跳在别的域名上，带着令牌过去会泄漏。"""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


TOK = None


def _req(url, tok, accept="application/vnd.github+json"):
    h = {"User-Agent": "cinebackup-watch-ci", "Accept": accept}
    if tok:
        h["Authorization"] = "Bearer " + tok
    return urllib.request.Request(url, headers=h)


def api(path, tries=4):
    """调 GitHub API，带重试。

    本机到 api.github.com 会偶发 `SSL: UNEXPECTED_EOF_WHILE_READING` / DNS 解析失败
    （代理或沙箱抖动），失败一次不代表网络不通 —— 重试即可，别急着排查配置。
    """
    last = None
    for i in range(tries):
        try:
            with urllib.request.urlopen(_req(API + path, TOK), timeout=90) as r:
                return json.loads(r.read().decode())
        except urllib.error.HTTPError as e:
            return {"__status__": e.code, "__body__": e.read().decode()[:500]}
        except Exception as e:  # URLError / ssl.SSLError / timeout …
            last = e
            if i < tries - 1:
                time.sleep(2 + 2 * i)
    sys.exit("访问 api.github.com 失败（已重试 %d 次）：%r" % (tries, last))


def fetch_first_hop(url):
    op = urllib.request.build_opener(NoRedirect)
    cur, use_tok = url, TOK
    for _ in range(6):
        for attempt in range(4):
            try:
                r = op.open(_req(cur, use_tok, accept="*/*"), timeout=300)
                return r.getcode(), r.read()
            except urllib.error.HTTPError as e:
                if e.code in (301, 302, 303, 307, 308) and e.headers.get("Location"):
                    cur, use_tok = e.headers["Location"], None
                    break
                return e.code, e.read()
            except Exception:
                if attempt == 3:
                    return 0, b""
                time.sleep(2 + 3 * attempt)
    return 0, b""


def _ts(s):
    return calendar.timegm(time.strptime(s, "%Y-%m-%dT%H:%M:%SZ")) if s else None


def latest_run():
    d = api("/repos/%s/%s/actions/runs?per_page=1" % (OWNER, REPO))
    if "__status__" in d:
        sys.exit("查询失败 %s" % d)
    runs = d.get("workflow_runs", [])
    return runs[0] if runs else None


def show_jobs(run_id):
    now = time.time()
    jobs = api("/repos/%s/%s/actions/runs/%s/jobs" % (OWNER, REPO, run_id))
    for j in jobs.get("jobs", []):
        print("  job [%s] %s / %s" % (j["name"], j["status"], j.get("conclusion") or "-"))
        for s in j.get("steps", []):
            a, b = _ts(s.get("started_at")), _ts(s.get("completed_at"))
            dur = "%.1f 分" % ((b - a) / 60) if a and b else ("进行中 %.1f 分" % ((now - a) / 60) if a else "-")
            mark = s.get("conclusion") or s["status"]
            mark = {"success": "ok", "failure": "FAIL", "skipped": "skip",
                    "in_progress": "..>"}.get(mark, mark)
            print("     %-5s %-28s %s" % (mark, s["name"], dur))


def download_artifacts(run_id):
    d = api("/repos/%s/%s/actions/runs/%s/artifacts" % (OWNER, REPO, run_id))
    arts = d.get("artifacts", [])
    if not arts:
        print("  没有产物（可能被 if 条件跳过）")
        return []
    os.makedirs(OUTDIR, exist_ok=True)
    saved = []
    for a in arts:
        print("  产物 %s  %.2f MB" % (a["name"], a["size_in_bytes"] / 1048576))
        code, data = fetch_first_hop(a["archive_download_url"])
        if code != 200 or data[:2] != b"PK":
            print("    下载失败 HTTP %s" % code)
            continue
        zp = os.path.abspath(os.path.join(OUTDIR, a["name"] + ".zip"))
        open(zp, "wb").write(data)
        print("    -> %s  (%.2f MB)" % (zp, len(data) / 1048576))
        saved.append(zp)
        with zipfile.ZipFile(io.BytesIO(data)) as z:
            for n in z.namelist():
                print("       %-56s %9.2f MB" % (n, z.getinfo(n).file_size / 1048576))
            for n in z.namelist():
                if n.lower().endswith(".dmg"):
                    dst = os.path.join(os.path.dirname(zp), os.path.basename(n))
                    open(dst, "wb").write(z.read(n))
                    print("       >> 已解出 dmg: %s" % dst)
                    saved.append(dst)
    return saved


def dump_logs(run_id):
    code, data = fetch_first_hop("%s/repos/%s/%s/actions/runs/%s/logs" % (API, OWNER, REPO, run_id))
    if code != 200 or data[:2] != b"PK":
        print("  日志下载失败 HTTP %s" % code)
        return
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        for n in [x for x in z.namelist() if x.endswith(".txt")]:
            lines = z.read(n).decode("utf-8", "replace").splitlines()
            print("\n  ==== %s（末尾 60 行）====" % n)
            for line in lines[-60:]:
                print("   ", line[:200])


def cmd_watch(max_wait=3000):
    w = latest_run()
    if not w:
        print("没有运行记录")
        return
    print("运行 #%s  %s  %s" % (w["run_number"], w["status"], w["html_url"]))
    last, deadline = None, time.time() + max_wait
    while True:
        st = (w["status"], w.get("conclusion"))
        if st != last:
            el = time.time() - _ts(w.get("run_started_at") or "") if w.get("run_started_at") else -1
            print("[%s] %s / %s  已用 %.1f 分" % (time.strftime("%H:%M:%S"), w["status"],
                                                 w.get("conclusion") or "-", el / 60))
            show_jobs(w["id"])
            last = st
        if w["status"] == "completed":
            break
        if time.time() > deadline:
            print("等待超时，运行仍在进行")
            return
        time.sleep(20)
        w = api("/repos/%s/%s/actions/runs/%s" % (OWNER, REPO, w["id"]))
    print()
    if w.get("conclusion") == "success":
        print("== 构建成功，下载产物 ==")
        print("== 完成，共 %d 个文件 ==" % len(download_artifacts(w["id"])))
    else:
        print("== 构建失败（%s），抓日志 ==" % w.get("conclusion"))
        dump_logs(w["id"])


if __name__ == "__main__":
    TOK = get_token()
    cmd = sys.argv[1] if len(sys.argv) > 1 else "status"
    if cmd == "watch":
        cmd_watch()
    elif cmd == "status":
        w = latest_run()
        if w:
            print("运行 #%s  %s / %s  %s" % (w["run_number"], w["status"],
                                            w.get("conclusion") or "-", w["html_url"]))
            show_jobs(w["id"])
        else:
            print("还没有运行记录")
    elif cmd == "logs":
        dump_logs(sys.argv[2])
    elif cmd == "get":
        download_artifacts(sys.argv[2])
    else:
        print(__doc__)
