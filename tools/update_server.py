#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Windows 桌面客户端自动更新服务端（零依赖，Python 3.8+）。

客户端启动时会读取  {服务端地址}/latest.json，格式：

    {
      "version": "1.2.0",                     # 最新版本号（只用于界面显示，任意写法；客户端会剥掉 v/V 前缀）
      "url": "files/app_1.2.0.exe",           # 下载地址：相对 latest.json 所在目录，或完整 http(s) 地址
      "notes": "修复了 xxx",                   # 更新说明，可省略
      "sha256": "…",                          # exe 摘要，发布时自动写入；客户端用它判有没有新版、下载后也用它校验
      "published_at": "2026-09-03 20:00:00"   # 发布时间，仅展示
    }

目录结构（本脚本 --publish 自动维护）：

    <版本目录>/
      latest.json
      files/app_1.2.0.exe

用法：

  1) 拖拽发布：把新版 exe 直接拖到 publish_drop.bat（或本脚本）上即可。
     每次都会询问版本号，窗口里带填写说明：
       回车    = 沿用默认值（文件名里的版本 > exe 旁 version.txt /
                 脚本旁 version.txt > 上一次发布的版本）
       任意文字 = 直接写版本号，不限制格式（1.1.2 / 1.2.4-加固 / V1.2.4 / 2026.09.17 都收）
       +       = 默认值补丁号 +1（1.1.1 -> 1.1.2；默认值不是数字和点时给提示）
       ++      = 默认值次版本号 +1、补丁归零（1.1.9 -> 1.2.0）
       q       = 取消本次发布
     更新说明：回车 = 不写（可放 notes.txt 在 exe 旁 / 脚本旁自动沿用）；
               输入里的字面 \n 会被转成换行，整段多行建议直接用 notes.txt
     注意：客户端判「有没有新版」比的是文件 sha256（下面发布时自动写入），不看版本号，
     所以同一版本号重新发一次也会被识别成新构建；版本号只用于界面展示，
     写岔了顶多是界面上那个 V 号与程序内嵌版本（Cargo.toml 的 version）对不上。

  2) 命令行发布（拷贝 exe 进目录、生成/覆盖 latest.json）：
      python update_server.py D:\\update_root --publish app_1.2.0.exe --notes "修复xxx"
      （版本号默认从文件名提取，也可 --version 1.2.0 显式指定）

  3) 启动静态文件服务（客户端「更新服务端地址」填 http://<本机IP>:8666）：
      python update_server.py D:\\update_root --serve --port 8666

  版本目录（root）的确定顺序：命令行参数 > 脚本旁 update_root.txt 的第一行
  > 脚本旁的 update_root\\ 目录。发布只写文件，服务正在运行时无需重启，
  客户端下次检查即可拿到新版本。

也可以直接用 nginx / IIS / 任何静态服务器托管 <版本目录>，效果相同。
"""

import argparse
import functools
import hashlib
import json
import re
import shutil
import sys
from datetime import datetime
from http.server import HTTPServer, SimpleHTTPRequestHandler
from pathlib import Path


# 只用于判断「+ / ++」能不能在默认值上自动递增：版本号本身不限制格式
VERSION_RE = re.compile(r"^\d+(\.\d+)+$")


def script_dir() -> Path:
    return Path(__file__).resolve().parent


def resolve_root(cli_root) -> Path:
    """确定版本目录：命令行参数 > update_root.txt 第一行 > 脚本旁 update_root\\。"""
    if cli_root:
        return Path(cli_root).expanduser().resolve()
    cfg = script_dir() / "update_root.txt"
    if cfg.is_file():
        for line in cfg.read_text(encoding="utf-8-sig").splitlines():
            line = line.strip()
            if line and not line.startswith("#"):
                return Path(line).expanduser().resolve()
    return script_dir() / "update_root"


def pick_version(exe: Path) -> str:
    """版本号：文件名数字段 > exe 旁 version.txt > 脚本旁 version.txt > 现场输入。"""
    m = re.search(r"(\d+(?:\.\d+)+)", exe.name)
    if m:
        return m.group(1)
    for marker in (exe.parent / "version.txt", script_dir() / "version.txt"):
        if marker.is_file():
            v = marker.read_text(encoding="utf-8-sig").strip()
            if v:
                return v
    try:
        v = input("无法从文件名提取版本号，请输入本次发布的版本号（如 1.1.2）：").strip()
    except EOFError:
        sys.exit("无法从文件名提取版本号，请用 --version 1.2.0 显式指定")
    if not v:
        sys.exit("未输入版本号，已取消发布")
    return v


def last_published_version(root: Path) -> str:
    """读取版本目录里 latest.json 的版本号，读不到返回空串。"""
    try:
        data = json.loads((root / "latest.json").read_text(encoding="utf-8-sig"))
        return str(data.get("version", "") or "").strip()
    except Exception:
        return ""


def default_version(exe: Path, prev: str) -> str:
    """版本号默认值：文件名数字段 > exe 旁 version.txt > 脚本旁 version.txt > 上一发布版本。"""
    m = re.search(r"(\d+(?:\.\d+)+)", exe.name)
    if m:
        return m.group(1)
    for marker in (exe.parent / "version.txt", script_dir() / "version.txt"):
        if marker.is_file():
            v = marker.read_text(encoding="utf-8-sig").strip()
            if v:
                return v
    return prev


def bump_version(base: str, minor: bool = False) -> str:
    """+ ：补丁号 +1（1.1.1 -> 1.1.2）；++ ：次版本号 +1、补丁归零（1.1.9 -> 1.2.0）。"""
    parts = [int(x) for x in base.split(".")]
    if minor:
        while len(parts) < 2:
            parts.append(0)
        parts[1] += 1
        parts[2:] = [0] * (len(parts) - 2)
    else:
        parts[-1] += 1
    return ".".join(str(p) for p in parts)


def ask_version(exe: Path, root: Path) -> str:
    """拖拽模式：打印填写说明并询问版本号，回车沿用默认值（含上一发布版本的继承）。

    版本号不限制格式：客户端判「有没有新版」比的是文件 sha256（发布时自动写入），
    版本号只用于界面显示，所以 1.2.4-加固 / V1.2.4 / 2026.09.17 这类写法都直接收。
    """
    prev = last_published_version(root)
    default = default_version(exe, prev)
    print(f"  版本目录：{root}")
    print(f"  上一发布：{prev or '（无）'}")
    print("  填写说明：回车 = 沿用默认值；也可直接填版本号（任意写法都收，如 1.2.4-加固）")
    print("            + = 默认值补丁号+1（1.1.1 -> 1.1.2）  ++ = 次版本号+1（1.1.9 -> 1.2.0）  q = 取消")
    print("            版本号只用于界面显示，客户端按文件 sha256 判有没有新版")
    while True:
        label = f"请输入版本号 [回车 = {default}]" if default else "请输入版本号（如 1.1.2）"
        try:
            raw = input(f"{label}：").strip()
        except EOFError:  # 无交互环境（如自动化测试）直接沿用默认值
            if default:
                return default
            sys.exit("无法确定版本号，已取消发布（可用 --version 显式指定）")
        if not raw:
            if default:
                return default
            print("  没有可用默认值（文件名 / version.txt / 上一发布都没有版本号），请直接输入")
            continue
        if raw.lower() in ("q", "quit", "exit"):
            sys.exit("已取消发布")
        if raw in ("+", "++"):
            base = (default or prev).strip()
            if not base:
                print("  没有可递增的基准版本号，请直接输入完整版本号")
                continue
            # 自定义过版本号（如 1.2.4-加固）就没法自动递增了，让用户手填，
            # 而不是在这里抛 ValueError
            if not VERSION_RE.match(base):
                print(f"  基准版本号「{base}」不是数字和点，没法自动递增，请直接输入完整版本号")
                continue
            return bump_version(base, minor=(raw == "++"))
        return raw


def publish(root: Path, exe: Path, version: str, notes: str) -> None:
    if not exe.is_file():
        sys.exit(f"找不到文件：{exe}")
    files = root / "files"
    files.mkdir(parents=True, exist_ok=True)
    dest = files / exe.name
    shutil.copy2(exe, dest)
    if not version:
        version = pick_version(exe)
    sha = hashlib.sha256(dest.read_bytes()).hexdigest()
    manifest = {
        "version": version,
        "url": f"files/{exe.name}",
        "notes": notes,
        "sha256": sha,
        "published_at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
    }
    (root / "latest.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    print(f"已发布 V{version} -> {root / 'latest.json'}")
    print(f"  下载地址：{manifest['url']}")
    print(f"  更新说明：{notes or '（无）'}")
    print(f"  SHA256  ：{sha}")


def serve(root: Path, port: int) -> None:
    if not (root / "latest.json").is_file():
        print(f"警告：{root / 'latest.json'} 不存在，客户端将检查不到版本（先发布一个版本）")
    handler = functools.partial(SimpleHTTPRequestHandler, directory=str(root))
    print(f"更新服务已启动：http://0.0.0.0:{port}（目录：{root}）")
    print(f"客户端「设置 → 版本更新 → 服务端地址」填 http://<本机IP>:{port}")
    try:
        HTTPServer(("0.0.0.0", port), handler).serve_forever()
    except KeyboardInterrupt:
        print("\n已停止")


def ask_notes(exe: Path) -> str:
    """拖拽模式：询问更新说明，回车不写（可从 exe 旁 / 脚本旁 notes.txt 继承默认值）。

    单行 input 里想换行：输入字面 \\n 会被转成真实换行（如「修复xx\\n新增yy」）；
    整段多行说明建议直接放 notes.txt（exe 旁 / 脚本旁），客户端按原文展示。
    """
    default = ""
    for marker in (exe.parent / "notes.txt", script_dir() / "notes.txt"):
        if marker.is_file():
            default = marker.read_text(encoding="utf-8-sig").strip()
    if default:
        print(f"  更新说明：回车 = 沿用 notes.txt（{default}）")
    else:
        print("  更新说明：回车 = 不写（可放 notes.txt 在 exe 旁 / 脚本旁自动沿用）")
    print("            输入里的 \\n = 换行；整段多行建议直接用 notes.txt")
    try:
        raw = input("  请输入更新说明：").strip()
    except EOFError:  # 无交互环境直接用默认值
        return default
    # 字面 \n 转真实换行：客户端界面（egui Label）与 latest.json 都按多行处理
    return (raw or default).replace("\\n", "\n")


def drag_publish(exes) -> None:
    """拖拽模式：逐个询问版本号与更新说明并发布，结束后停住窗口让用户看清结果。"""
    root = resolve_root(None)
    for exe in exes:
        print(f"\n=== 拖拽发布：{exe.name} ===")
        try:
            version = ask_version(exe, root)
            notes = ask_notes(exe)
            publish(root, exe, version, notes)
        except SystemExit as e:
            print(e)
        except Exception as e:  # 兜底：别让窗口一闪而过
            print(f"发布失败：{e}")
    try:
        input("\n按回车关闭窗口 ...")
    except EOFError:
        pass


def main() -> None:
    argv = sys.argv[1:]
    # 拖拽模式：参数全是已存在的文件路径、且没有任何选项
    if argv and all(not a.startswith("-") and Path(a).is_file() for a in argv):
        drag_publish([Path(a) for a in argv])
        return
    ap = argparse.ArgumentParser(description="桌面客户端更新服务端")
    ap.add_argument("root", nargs="?", default=None,
                    help="版本目录（存放 latest.json 与 files/）；省略时按 "
                         "update_root.txt > 脚本旁 update_root\\ 的顺序确定")
    ap.add_argument("--publish", metavar="EXE", help="发布一个新版本 exe")
    ap.add_argument("--version", help="版本号（任意写法；默认从 exe 文件名提取）")
    ap.add_argument("--notes", default="",
                    help="更新说明；字面 \\n 会被转成换行，多行说明也可用 notes.txt")
    ap.add_argument("--serve", action="store_true", help="启动静态文件服务")
    ap.add_argument("--port", type=int, default=8666)
    args = ap.parse_args()
    root = resolve_root(args.root)
    if args.publish:
        publish(root, Path(args.publish), args.version or "",
                args.notes.replace("\\n", "\n"))
    if args.serve or not args.publish:
        serve(root, args.port)


if __name__ == "__main__":
    main()
