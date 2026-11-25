#!/usr/bin/env python3
"""Run zsh's own test suite (Test/*.ztst) against meowsh.

Usage:
    python3 scripts/ztst.py --zsh-src PATH [--meowsh BIN] [-v] [FILE...]

PATH is a zsh source checkout. FILE names select test files (for example
`A01grammar` or `D04parameter.ztst`); by default all are run.

Each .ztst file becomes one meowsh script, so shell state carries from one
test to the next as in zsh's harness (Test/ztst.zsh). Every chunk runs as
`() { eval "$code" }` with its stdin, stdout, stderr and status captured
in files, then compared here. The harness itself runs in Python so a bug
in meowsh cannot break it. Tests run in a temporary copy of Test/, where
`Src/zsh` is a link to meowsh.

A test passes when status and stdout match and, unless flagged `D`,
stderr matches. "stdout-only" counts tests whose status and stdout match
but whose stderr differs, which is mostly wording of error messages.
"""

import argparse
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile

STATUS_RE = re.compile(r"^([-0-9]+)([A-Za-z]*)(?::(.*))?$")


class Case:
    def __init__(self, code, line):
        self.code = code
        self.line = line
        self.status = "-"
        self.flags = ""
        self.message = ""
        self.stdin = None
        self.out = None
        self.err = None
        self.out_pat = False
        self.err_pat = False


def parse(path):
    """Returns (prep chunks, test cases, clean chunks)."""
    with open(path, encoding="utf-8", errors="surrogateescape") as f:
        raw = f.read().split("\n")
    # Like ztst.zsh's ZTST_getline, every line starting with `#` is
    # skipped, even inside code; numbers stay those of the file.
    kept = [(k, l) for k, l in enumerate(raw) if not l.startswith("#")]
    lines = [l for _, l in kept]
    lineno = [k for k, _ in kept]
    prep, tests, clean = [], [], []
    section = None
    i = 0
    n = len(lines)

    def is_code(s):
        return bool(s) and s[0] in " \t" and s.strip() != ""

    def blank(s):
        return s.strip() == ""

    while i < n:
        line = lines[i]
        if line.startswith("%"):
            section = line[1:].split()[0] if line[1:].split() else None
            i += 1
            continue
        if blank(line):
            i += 1
            continue
        if section in ("prep", "clean"):
            if is_code(line):
                start = i
                code = []
                while i < n and is_code(lines[i]):
                    code.append(lines[i])
                    i += 1
                (prep if section == "prep" else clean).append(("\n".join(code), lineno[start] + 1))
                continue
            i += 1
            continue
        if section != "test":
            i += 1
            continue
        if not is_code(line):
            i += 1
            continue
        start = i
        code = []
        while i < n and is_code(lines[i]):
            code.append(lines[i])
            i += 1
        case = Case("\n".join(code), lineno[start] + 1)
        if i < n:
            m = STATUS_RE.match(lines[i])
            if m:
                case.status, case.flags = m.group(1), m.group(2)
                case.message = m.group(3) or ""
                i += 1
        redirs = {"<": [], ">": [], "?": []}
        while i < n:
            l = lines[i]
            if l.startswith("*>") or l.startswith("*?"):
                if l[1] == ">":
                    case.out_pat = True
                else:
                    case.err_pat = True
                l = l[1:]
            if l[:1] in redirs:
                redirs[l[0]].append(l[1:])
                i += 1
                continue
            if l.startswith("F:"):
                i += 1
                continue
            break
        case.stdin = "\n".join(redirs["<"]) + "\n" if redirs["<"] else ""
        case.out = "\n".join(redirs[">"]) + "\n" if redirs[">"] else ""
        case.err = "\n".join(redirs["?"]) + "\n" if redirs["?"] else ""
        tests.append(case)
    return prep, tests, clean


def zsh_glob_to_regex(p):
    """Translates the zsh pattern syntax used in `*>` lines to a regex."""
    out = []
    i = 0
    while i < len(p):
        c = p[i]
        if c == "\\" and i + 1 < len(p):
            out.append(re.escape(p[i + 1]))
            i += 2
            continue
        if c == "*":
            out.append(".*")
        elif c == "?":
            out.append(".")
        elif c == "[":
            # The closing bracket, skipping [:class:] names inside.
            j = i + 1
            # A `]` first in the set is literal.
            if j < len(p) and p[j] in "!^":
                j += 1
            if j < len(p) and p[j] == "]":
                j += 1
            while j < len(p) and p[j] != "]":
                if p.startswith("[:", j):
                    k = p.find(":]", j + 2)
                    j = k + 2 if k >= 0 else j + 1
                else:
                    j += 1
            if j >= len(p):
                j = -1
            if j < 0:
                out.append(re.escape(c))
            else:
                body = p[i + 1 : j]
                if body.startswith(("!", "^")):
                    body = "^" + body[1:]
                body = body.replace("[:digit:]", r"\d").replace("[:space:]", r"\s")
                body = body.replace("[:alpha:]", "a-zA-Z").replace("[:alnum:]", "a-zA-Z0-9")
                out.append("[" + body + "]")
                i = j
        elif c == "(" and p.startswith("(#c", i):
            # (#cN) or (#cN,M): the previous item N to M times.
            m = re.match(r"\(#c(\d+)(?:,(\d*))?\)", p[i:])
            if m:
                n, top = m.group(1), m.group(2)
                out.append("{%s}" % n if top is None else "{%s,%s}" % (n, top))
                i += m.end()
                continue
            out.append("(?:")
        elif c == "(":
            out.append("(?:")
        elif c == ")":
            out.append(")")
        elif c == "|":
            out.append("|")
        elif c == "#":
            if i + 1 < len(p) and p[i + 1] == "#":
                out.append("+")
                i += 1
            else:
                out.append("*")
        elif c == "<":
            m = re.match(r"<(\d*)-(\d*)>", p[i:])
            if m:
                out.append(r"\d+")
                i += m.end() - 1
            else:
                out.append(re.escape(c))
        else:
            out.append(re.escape(c))
        i += 1
    return "".join(out)


def split_exclusion(p):
    """Splits `a~b` at a top-level `~` with text on both sides."""
    depth = 0
    i = 0
    while i < len(p):
        c = p[i]
        if c == "\\":
            i += 2
            continue
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
        elif c == "~" and depth == 0 and 0 < i < len(p) - 1:
            return p[:i], p[i + 1 :]
        i += 1
    return p, None


def matches(expected, actual, pattern):
    if not pattern:
        return expected == actual
    # Test/ztst.zsh reads files via `"${(f@)$(<file)}"`, which strips
    # trailing newlines before splitting into lines (`""` -> `[""]`).
    el = expected.rstrip("\n").split("\n")
    al = actual.rstrip("\n").split("\n")
    if len(el) != len(al):
        return False
    for e, a in zip(el, al):
        try:
            # EXTENDED_GLOB `pat~excl`: matches pat but not excl.
            main, excl = split_exclusion(e)
            if not re.fullmatch(zsh_glob_to_regex(main), a, re.S):
                return False
            if excl is not None and re.fullmatch(zsh_glob_to_regex(excl), a, re.S):
                return False
        except re.error:
            if e != a:
                return False
    return True


def sq(s):
    return "'" + s.replace("'", "'\\''") + "'"


def build_driver(work, testdir, prep, tests, clean):
    w = lambda name, data: open(os.path.join(work, name), "w", encoding="utf-8", errors="surrogateescape").write(data)
    tmp = os.path.join(work, "tmp")
    os.makedirs(tmp, exist_ok=True)
    lines = [
        "ZTST_testdir=" + sq(testdir),
        "ZTST_srcdir=" + sq(testdir),
        "ZTST_exe=" + sq(os.path.join(os.path.dirname(testdir), "Src", "zsh")),
        "ZTST_tmp=" + sq(tmp),
        "ZTST_fd=8",
        # ztst.zsh's progress marker, which some tests call.
        "ZTST_hashmark() { :; }",
        "exec 8>/dev/null",
        # From Test/ztst.zsh: prints a UTF-8 locale, if there is one.
        "ZTST_find_UTF8() { print -r -- " + sq(utf8_locale()) + "; }",
        "cd " + sq(testdir),
        "ZTST_work=" + sq(work),
    ]
    # As Test/ztst.zsh: autoload from the source tree's function dirs.
    src = os.path.dirname(testdir)
    dirs = []
    for base in ("Functions", "Completion"):
        top = os.path.join(src, base)
        if not os.path.isdir(top):
            continue
        if base == "Completion":
            dirs.append(top)
            subs = [os.path.join(top, a, b) for a in sorted(os.listdir(top)) if os.path.isdir(os.path.join(top, a))
                    for b in sorted(os.listdir(os.path.join(top, a))) if os.path.isdir(os.path.join(top, a, b))]
        else:
            subs = [os.path.join(top, a) for a in sorted(os.listdir(top)) if os.path.isdir(os.path.join(top, a))]
        dirs.extend(d for d in subs if not d.endswith("CVS"))
    if dirs:
        lines.append("fpath=( " + " ".join(sq(d) for d in dirs) + " )")
    for k, (code, _) in enumerate(prep):
        w(f"prep.{k}", code)
        lines.append(f'() {{ eval "$(<$ZTST_work/prep.{k})" }} >/dev/null; print -r -- $? >$ZTST_work/prepst.{k}')
    lines.append('print -r -- "$ZTST_unimplemented" >$ZTST_work/unimpl')
    lines.append('if [[ -z $ZTST_unimplemented ]]; then')
    for k, t in enumerate(tests):
        w(f"code.{k}", t.code)
        q = "q" in t.flags
        w(f"inraw.{k}", t.stdin)
        if q and t.stdin:
            lines.append(f'print -rn -- "${{(e)$(<$ZTST_work/inraw.{k})}}" >$ZTST_work/in.{k}; print >>$ZTST_work/in.{k}')
        else:
            lines.append(f"cat $ZTST_work/inraw.{k} >$ZTST_work/in.{k}")
        lines.append("ZTST_skip=")
        lines.append(
            f'() {{ () {{ () {{ eval "$(<$ZTST_work/code.{k})" }} }} }} <$ZTST_work/in.{k} >$ZTST_work/out.{k} 2>$ZTST_work/err.{k}; '
            f"print -r -- $? >$ZTST_work/st.{k}; "
            f'print -r -- "$ZTST_skip" >$ZTST_work/skip.{k}'
        )
        if q:
            for kind, text in (("out", t.out), ("err", t.err)):
                if text:
                    w(f"exp{kind}raw.{k}", text)
                    lines.append(
                        f'print -r -- "${{(e)$(<$ZTST_work/exp{kind}raw.{k})}}" >$ZTST_work/exp{kind}.{k} 2>/dev/null'
                    )
    lines.append("fi")
    for k, (code, _) in enumerate(clean):
        w(f"clean.{k}", code)
        lines.append(f'() {{ eval "$(<$ZTST_work/clean.{k})" }} >/dev/null 2>&1')
    w("driver.zsh", "\n".join(lines) + "\n")


def utf8_locale():
    try:
        out = subprocess.run(["locale", "-a"], capture_output=True, text=True).stdout.split()
    except OSError:
        return ""
    for want in ("en_US.UTF-8", "en_US.utf8", "C.UTF-8", "C.utf8"):
        if want in out:
            return want
    return next((l for l in out if l.lower().endswith(("utf-8", "utf8"))), "")


def read(path):
    try:
        with open(path, encoding="utf-8", errors="surrogateescape") as f:
            return f.read()
    except FileNotFoundError:
        return None


def kill_session(sid):
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as f:
                # Fields after the parenthesized command name; session is the 4th.
                fields = f.read().rsplit(")", 1)[1].split()
            if int(fields[3]) == sid:
                os.kill(int(entry), signal.SIGKILL)
        except (OSError, ValueError, IndexError):
            pass


def run_file(meowsh, testdir, path, timeout, verbose):
    # As zsh's harness (ztst.zsh) does, start each file without the
    # *.tmp files and directories an earlier one left.
    for entry in os.listdir(testdir):
        if entry.endswith(".tmp"):
            full = os.path.join(testdir, entry)
            if os.path.isdir(full) and not os.path.islink(full):
                shutil.rmtree(full, ignore_errors=True)
            else:
                os.remove(full)
    prep, tests, clean = parse(path)
    work = tempfile.mkdtemp(prefix="ztst.", dir=testdir)
    build_driver(work, testdir, prep, tests, clean)
    # A minimal environment, as the tests expect and so nothing of the
    # caller's leaks into results.
    env = {k: os.environ[k] for k in ("PATH", "USER", "LOGNAME") if k in os.environ}
    env["LANG"] = "C"
    env["TERM"] = "xterm-256color"
    env["SHELL"] = meowsh
    env["HOME"] = os.path.join(testdir, "home.tmp")
    os.makedirs(env["HOME"], exist_ok=True)
    timed_out = False
    proc = subprocess.Popen(
        [meowsh, os.path.join(work, "driver.zsh")],
        cwd=testdir,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
    # Kill everything left in the driver's session, including background
    # jobs in their own process groups; a stray test process can loop
    # forever writing to a deleted file.
    kill_session(proc.pid)
    proc.wait()
    res = {"pass": 0, "stdout-only": 0, "fail": 0, "skip": 0, "xfail": 0, "aborted": 0}
    unimpl = (read(os.path.join(work, "unimpl")) or "").strip()
    bad_prep = [k for k in range(len(prep)) if (read(os.path.join(work, f"prepst.{k}")) or "1").strip() != "0"]
    if unimpl:
        res["skip"] = len(tests)
        return res, [f"unimplemented: {unimpl}"]
    details = []
    if bad_prep:
        details.append(f"prep chunk(s) failed at line(s) {', '.join(str(prep[k][1]) for k in bad_prep)}")
    for k, t in enumerate(tests):
        st = read(os.path.join(work, f"st.{k}"))
        if st is None:
            res["aborted"] += 1
            if verbose and res["aborted"] == 1:
                details.append(f"line {t.line}: shell exited before this test{' (timeout)' if timed_out else ''}")
            continue
        if (read(os.path.join(work, f"skip.{k}")) or "").strip():
            res["skip"] += 1
            continue
        out = read(os.path.join(work, f"out.{k}")) or ""
        err = read(os.path.join(work, f"err.{k}")) or ""
        exp_out = read(os.path.join(work, f"expout.{k}")) if "q" in t.flags and t.out else t.out
        exp_err = read(os.path.join(work, f"experr.{k}")) if "q" in t.flags and t.err else t.err
        exp_out = exp_out if exp_out is not None else t.out
        exp_err = exp_err if exp_err is not None else t.err
        ok_status = t.status == "-" or t.status == st.strip()
        ok_out = "d" in t.flags or matches(exp_out, out, t.out_pat)
        ok_err = "D" in t.flags or matches(exp_err, err, t.err_pat)
        if "f" in t.flags:
            res["xfail"] += 1
            continue
        if ok_status and ok_out and ok_err:
            res["pass"] += 1
        elif ok_status and ok_out:
            res["stdout-only"] += 1
            if verbose:
                details.append(f"line {t.line}: {t.message} (stderr only)\n    stderr {err!r} (expected {exp_err!r})")
        else:
            res["fail"] += 1
            if verbose:
                why = []
                if not ok_status:
                    why.append(f"status {st.strip()} (expected {t.status})")
                if not ok_out:
                    why.append(f"stdout {out!r} (expected {exp_out!r})")
                if not ok_err:
                    why.append(f"stderr {err!r}")
                details.append(f"line {t.line}: {t.message}\n    " + "\n    ".join(why))
    return res, details


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--zsh-src", required=True, help="zsh source checkout")
    ap.add_argument("--meowsh", default=os.path.join(os.path.dirname(__file__), "..", "target", "release", "meowsh"))
    ap.add_argument("--timeout", type=float, default=60)
    ap.add_argument("--native", action="store_true", help="run zsh's own Test/ztst.zsh directly with meowsh")
    ap.add_argument("-v", "--verbose", action="store_true", help="show each failing test")
    ap.add_argument("files", nargs="*")
    args = ap.parse_args()
    meowsh = os.path.abspath(args.meowsh)
    src_test = os.path.join(args.zsh_src, "Test")
    if not os.access(meowsh, os.X_OK):
        sys.exit(f"meowsh binary not found: {meowsh} (run cargo build --release)")
    names = sorted(f for f in os.listdir(src_test) if f.endswith(".ztst"))
    if args.files:
        want = [f if f.endswith(".ztst") else f + ".ztst" for f in args.files]
        names = [f for f in names if f in want or any(f.startswith(w[:-5]) for w in want)]
    root = tempfile.mkdtemp(prefix="meowsh-ztst.")
    try:
        testdir = os.path.join(root, "Test")
        shutil.copytree(src_test, testdir, symlinks=True)
        os.makedirs(os.path.join(root, "Src"), exist_ok=True)
        zsh_bin = os.path.join(root, "Src", "zsh")
        try:
            os.link(meowsh, zsh_bin)
        except OSError:
            shutil.copy2(meowsh, zsh_bin)
        # Some tests source helper scripts from Misc/ (globtests).
        for extra in ("Misc", "Functions", "Completion"):
            src_extra = os.path.join(args.zsh_src, extra)
            if os.path.isdir(src_extra):
                shutil.copytree(src_extra, os.path.join(root, extra), symlinks=True)
        # The modules meowsh has built in, as a static zsh build lists them
        # in config.modules (V01zmodload reads it and each .mdd file).
        provided = {
            "zsh/main", "zsh/complete", "zsh/complist", "zsh/computil", "zsh/datetime", "zsh/db/gdbm",
            "zsh/langinfo", "zsh/mathfunc", "zsh/parameter", "zsh/pcre", "zsh/random", "zsh/regex",
            "zsh/rlimits", "zsh/system", "zsh/termcap", "zsh/terminfo", "zsh/watch",
            "zsh/zle", "zsh/zleparameter", "zsh/zpty",
            "zsh/zselect", "zsh/zutil", "zsh/param/private", "zsh/nearcolor",
        }
        config = []
        for dirpath, _, files in os.walk(os.path.join(args.zsh_src, "Src")):
            for fname in sorted(files):
                if not fname.endswith(".mdd"):
                    continue
                full = os.path.join(dirpath, fname)
                with open(full) as f:
                    m = re.search(r"^name=(\S+)", f.read(), re.M)
                if not m or m.group(1) not in provided:
                    continue
                rel = os.path.relpath(full, args.zsh_src)
                os.makedirs(os.path.join(root, os.path.dirname(rel)), exist_ok=True)
                shutil.copy(full, os.path.join(root, rel))
                config.append(f"name={m.group(1)} modfile={rel} link=static auto=yes load=yes")
        with open(os.path.join(root, "config.modules"), "w") as f:
            f.write("\n".join(sorted(config)) + "\n")
        # Build features some tests check before running.
        with open(os.path.join(root, "config.h"), "w") as f:
            f.write("#define PATH_DEV_FD 1\n#define HAVE_FIFOS 1\n#define MULTIBYTE_SUPPORT 1\n")
        if args.native:
            passed_files = 0
            xpass_files = 0
            skipped_files = 0
            failed_files = 0
            zsh_bin = os.path.join(root, "Src", "zsh")
            ztst_sh = os.path.join(testdir, "ztst.zsh")
            for name in names:
                env = {k: os.environ[k] for k in ("PATH", "USER", "LOGNAME") if k in os.environ}
                env["LANG"] = "C"
                env["TERM"] = "xterm-256color"
                env["SHELL"] = zsh_bin
                env["ZTST_testdir"] = testdir
                env["ZTST_srcdir"] = testdir
                env["ZTST_exe"] = zsh_bin
                env["HOME"] = os.path.join(testdir, "home.tmp")
                os.makedirs(env["HOME"], exist_ok=True)
                try:
                    p = subprocess.run(
                        [zsh_bin, "-f", ztst_sh, os.path.join(testdir, name)],
                        cwd=testdir,
                        env=env,
                        stdin=subprocess.DEVNULL,
                        capture_output=True,
                        text=True,
                        errors="surrogateescape",
                        timeout=args.timeout,
                    )
                    if p.returncode == 0:
                        status_str = "PASS"
                        passed_files += 1
                    elif p.returncode == 2:
                        status_str = "SKIP"
                        skipped_files += 1
                    elif "test XPassed." in p.stdout:
                        status_str = "XPASS"
                        xpass_files += 1
                    else:
                        status_str = f"FAIL({p.returncode})"
                        failed_files += 1
                    print(f"{name[:-5]:24} {status_str}")
                    if status_str.startswith("FAIL") and args.verbose:
                        if p.stdout.strip():
                            print("  " + p.stdout.strip().replace("\n", "\n  "))
                        if p.stderr.strip():
                            print("  " + p.stderr.strip().replace("\n", "\n  "))
                except subprocess.TimeoutExpired:
                    failed_files += 1
                    print(f"{name[:-5]:24} TIMEOUT")
            print(
                f"\nnative ztst.zsh: {passed_files} pass, {xpass_files} xpass, "
                f"{skipped_files} skip, {failed_files} fail"
            )
            if failed_files:
                sys.exit(1)
            return
        total = {}
        print(f"{'file':24} {'pass':>5} {'out-ok':>6} {'fail':>5} {'abort':>5} {'skip':>5}")
        for name in names:
            res, details = run_file(meowsh, testdir, os.path.join(testdir, name), args.timeout, args.verbose)
            for k, v in res.items():
                total[k] = total.get(k, 0) + v
            print(
                f"{name[:-5]:24} {res['pass']:5} {res['stdout-only']:6} {res['fail']:5} "
                f"{res['aborted']:5} {res['skip'] + res['xfail']:5}"
            )
            if args.verbose:
                for d in details:
                    print("  " + d)
        run = sum(total.get(k, 0) for k in ("pass", "stdout-only", "fail", "aborted"))
        print(
            f"{'total':24} {total.get('pass', 0):5} {total.get('stdout-only', 0):6} "
            f"{total.get('fail', 0):5} {total.get('aborted', 0):5} "
            f"{total.get('skip', 0) + total.get('xfail', 0):5}"
        )
        if run:
            print(f"\npassed {total.get('pass', 0)}/{run} ({100 * total.get('pass', 0) / run:.1f}%), "
                  f"stdout and status right in {total.get('pass', 0) + total.get('stdout-only', 0)}/{run}")
    finally:
        if os.environ.get("ZTST_KEEP"):
            print(f"kept {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
