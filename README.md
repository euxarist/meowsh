# meowsh

A POSIX-compliant shell implementation.

## Features

- POSIX compliance (WIP)
- Built-in commands (alias, jobs, etc.)
- Line editing support
- Job control
- Variable expansion
- Redirections

## Installing

Prebuilt binaries for Linux (`x86_64`, `aarch64`) and macOS (`aarch64`, `x86_64`) are published on the [GitHub Releases](https://github.com/euxarist/meowsh/releases) page as `meowsh-<tag>-<target>.tar.gz` archives with `SHA256SUMS.txt`.

## Building

To build `meowsh` from source, run:

```bash
make
```

On Linux this produces a static musl binary matched to the host
architecture; install the musl standard library once with
`rustup target add $(uname -m)-unknown-linux-musl`. Set `TRIPLE=` for a
plain host build, or `TRIPLE=<triple>` to build for another target.

## Usage

After building, you can start the shell with:

```bash
./meowsh                    # interactive when stdin and stderr are terminals
./meowsh script.sh a b      # run a script with positional parameters
./meowsh -c 'echo $1' x y   # run a command string ($0 is x, $1 is y)
./meowsh -s a b < script.sh # read commands from stdin with parameters
```

meowsh behaves like zsh: unquoted parameters are not word-split, a glob
that matches nothing is an error, and so on. Started as `sh` (or `ksh`,
`bash`), or with `--emulate sh` as the first argument, it uses sh
behavior and option letters instead, as zsh does; `emulate sh` switches
inside a script.

Options: `-i` (force interactive), `-l`/`--login`, `-s`, `-c`, `-o name`,
and zsh's option letters (`-e` errexit, `-x` xtrace, `-f` no startup
files, ...); all of zsh's named options are recognized by `setopt`,
`set -o` and `$options`. Startup files are read in zsh order from
`$ZDOTDIR` (or `$HOME` without it): `.zshenv`, `.zprofile` (login),
`.zshrc` (interactive), `.zlogin` (interactive login); `NO_RCS` set in
`.zshenv` skips the rest.

## zsh startup files

meowsh reads an existing zsh configuration, including oh-my-zsh, its
plugins and themes, and the hooks that tools such as starship and direnv
generate. It sets `$ZSH_VERSION` (to `5.9-meowsh`, so per-version files
stay apart from real zsh's) and `$MEOWSH_VERSION`. Supported zsh syntax
and builtins include:

- process substitution `<(...)` and `>(...)`, here-strings `<<<`, `&>`,
  `&|`, `{ ... } always { ... }`, `{ cmd }` without a closing `;`
- `(( ... ))`, `for (( ; ; ))`, `for x (list)`, `for k v (list)`,
  anonymous functions `() { ... } args`, `function a b { ... }`
- floating-point arithmetic and `zsh/mathfunc` functions (`int`, `rint`,
  `floor`, `sqrt`, ...)
- parameter expansion: nested `${${x}[2]}`, flags such as `(s:,:)`,
  `(j: :)`, `(f)`, `(@)`, `(k)`, `(kv)`, `(o)`, `(u)`, `(q)`, `(Q)`, `(U)`,
  `(%)`, `(P)`, `(t)`, `(l:n:)`; modifiers `:h`, `:t`, `:r`, `:e`, `:a`,
  `:A`, `:l`, `:u`, `:q`, `:s/a/b/`; `${a[2,-1]}`, `${^a}`, `${=a}`,
  `${==a}`, `${~a}`, `${~~a}`, `${a:#pat}`, `${a:^b}`; `$name[sub]`,
  `$+name`, `$#name` without braces; flags apply in zsh's documented
  order, and quoted arrays join before operators and nested subscripts
- brace expansion after parameter expansion (`IF{${^a},x}`), numeric
  sorting `(n)` and `(-)`, `$11` as the eleventh argument
- match flags `(S)`, `(I:n:)`, `(M)`, `(R)`, `(B)`, `(E)`, `(N)` with
  `#`, `%` and `/`; `${a:/pat/repl}`, `${name::=word}` (also
  `${a[2]::=x}` and `${2::=x}`), `${a:|b}`,
  `${a:*b}`, `(A)` assignment, `(e)`, `(%)`/`(%%)`, `(V)`, `(g)`,
  `(qqqq)`, `(c)`/`(w)`/`(W)` counts, `(l)`/`(r)` padding with two strings
- subscript flags `(i)`, `(I)`, `(r)`, `(R)`, `(e)`, `(w)`, `(s:sep:)`,
  `(n:N:)`, `(b:N:)` and `^pat`, also as range ends (`${s[(r)x,(R)y]}`);
  `(z)` and `(Z:cCn:)` split like the shell's lexer
- `EXTENDED_GLOB`: `x#`, `x##`, `x(#cN,M)`, `^pat`, `a~b`, `(#i)`, and
  `(#b)`/`(#m)`, which set `$match`, `$mbegin`, `$mend` and `$MATCH`,
  `$MBEGIN`, `$MEND`, also inside a `${x//pat/repl}` replacement
- `KSH_ARRAYS`, `KSH_ZERO_SUBSCRIPT`, `OCTAL_ZEROES`, `IGNORE_BRACES`
  (meowsh started as sh or bash keeps brace expansion, as bash does)
- arithmetic `#name` and `##c` character codes
- namerefs (`typeset -n`), `[key]=value` items in array assignments and
  slice assignment (`a[2,4]=(x y)`)
- `whence` (`-v`, `-c`, `-w`, `-a`, `-p`, `-m`, `-s`, `-S`), `which`,
  `where`, `type`
- patterns and globs with `(a|b)` groups and `<n-m>` ranges, `**/`, and
  glob qualifiers such as `(N)`, `(.)`, `(/)`, `(@)`, `(m-1)`, `(L+1k)`,
  `(D)`, with `-` to follow symbolic links; matches sort in the locale's
  collation order
- `[[ x == (a|b)* ]]`, `[[ -v name[key] ]]`, `[[ -o option ]]`
- `=(cmd)` (output in a temporary file), `=cmd` (the command's path),
  glob qualifier modifiers (`*(:t)`), `a~b` exclusions on whole paths,
  `(#s)`/`(#e)`
- an `eval` or `-c` string is parsed whole before it runs, as in zsh, so
  an alias it defines applies only to later input (sourced files and sh
  mode still go command by command)
- `>& file` sends standard output and error to the file; `>!`, `>>|`,
  `>&|`, `&>|`; `{fd}>file` opens a new descriptor and stores its number
- `enable`/`disable` (`-r` reserved words, `-a` aliases, `-f` functions),
  `read -e`/`-E`/`-t`, `ZERR` and `DEBUG` traps, `TRAPINT`-style trap
  functions, `$LINENO`, non-ASCII parameter names (unless
  `POSIX_IDENTIFIERS`), and `#!interp` lines naming an interpreter in `$PATH`
- error messages in zsh's form, naming the script, function, `(eval)` or
  sourced file and the line: `f:2: command not found: x`
- hooks: `precmd`, `preexec`, `chpwd`, `zshexit`, their `*_functions`
  arrays, and `add-zsh-hook`
- `typeset` and `declare`, `local`, `export`, `readonly`, `integer`,
  `float` with zsh's attributes (`-i`, `-E`/`-F`, `-L`/`-R`/`-Z`, `-l`/`-u`,
  `-r`, `-x`, `-U`, `-T` tied pairs, `-g`, `-m`, `-p`), array values
  (`typeset a=(x y)`), and `functions` to print definitions
- `repeat`, `select`, `coproc` (`print -p`, `read -p`, `>&p`), `foreach`,
  short and brace forms (`if [[ x ]] cmd`, `for x in a b; cmd`,
  `while c { }`, `case w { }`), `;|`, `|&`, `noglob`, `-`, `exec -a`/`-l`/`-c`
- `autoload` from `$fpath` or a given path (`autoload /dir/name`, `-d`,
  `-r`, `-R`, `-X`), `functions -c` and `-M` math functions, anonymous
  `function { ... } args`, `command_not_found_handler`, `$FUNCNEST`, `$argv`,
  `print`, `emulate`, `is-at-least`,
  `zparseopts`, `fc -l`, `fc -W`/`-A`/`-R` (zsh's history file format), `fc -p`/`-P`,
  `print -s`, `HIST_IGNORE_SPACE`, `colors`, `unfunction`, `builtin`, `read -k`/`-q`
  (from the terminal, or `-u fd`),
  global aliases (`alias -g`) and the `$aliases`/`$galiases` arrays
- function-scoped EXIT traps, `LOCAL_OPTIONS`/`LOCAL_TRAPS` (also set by
  `emulate -L`), `AUTO_CD`, `$0` as the function or sourced file name
- `$RANDOM`, `$SECONDS`, `$EPOCHSECONDS`, `$EPOCHREALTIME`, `$COLUMNS`,
  `$LINES`, `$pipestatus`, `$HOST`, `$commands`, `$langinfo`
- right prompts: `$RPROMPT`/`$RPS1` (and `$RPROMPT2`/`$RPS2` on
  continuation lines) at the right edge, `$ZLE_RPROMPT_INDENT` columns in,
  hidden when the input reaches them; starship's `right_format` uses this
- prompts: `$PROMPT`/`$PS1` and `$PROMPT2`/`$PS2` get parameter and
  command substitution first, then zsh `%` escapes including `%N<..<`
  truncation
- dynamic named directories: `~[name]` and `%~` call `zsh_directory_name`
  (and `$zsh_directory_name_functions`)
- history expansion (`BANG_HIST`): `!!`, `!n`, `!-n`, `!str`, `!?str?`,
  word designators (`!$`, `!*`, `:0`, `:2-4`), modifiers (`:h`,
  `:s/a/b/`, ...) and `^old^new`
- `MULTIOS` (`cmd >a >b`, `cmd <a <b`), `NO_CLOBBER` with
  `CLOBBER_EMPTY` and `APPEND_CREATE`, `ERR_RETURN`, `PATH_DIRS`,
  `CDABLE_VARS`, `CHECK_JOBS`, `PRINT_EXIT_VALUE`, and sticky emulation
  for functions defined under `emulate -c`
- `let`, `limit`/`unlimit`/`ulimit`, `pushd`/`popd`/`dirs`, `hash -d`,
  `kill`, `getopts` and `shift -p` as zsh has them; `zpty` (commands on
  a pseudo-terminal) and `zselect` work
- zsh's modules are built in: zsh/zutil (`zstyle`, `zformat`,
  `zparseopts`, `zregexparse`), zsh/datetime (`strftime`), zsh/system
  (`sysread`, `sysopen`, `zsystem flock`, ...), zsh/mathfunc,
  zsh/parameter (assigning to `$functions` defines functions;
  `$funcsourcetrace`, `$functions_source`), zsh/param/private (`private`,
  `local -P`), zsh/stat (`zstat`, `stat`), zsh/termcap (`$termcap`,
  `echotc`), zsh/terminfo (`$terminfo`, `echoti`), zsh/nearcolor,
  zsh/watch (`WATCH`/`watch`), zsh/pcre (`pcre_compile`, `pcre_match`,
  `-pcre-match`, `REMATCH_PCRE` via `libpcre2-8`), zsh/db/gdbm (`ztie`,
  `zuntie`, `zgdbmpath`, `$zgdbm_tied` via `libgdbm`), zsh/random and
  the completion modules. `zmodload`
  tracks them as zsh does (features, `-F`, autoloads, `-u`); a module's
  builtins work without loading it until `zmodload` is given that
  module. Loading one meowsh does not have (zsh/compctl, zsh/zftp, ...)
  fails

meowsh edits lines with its port of zsh's line editor (zle: keymaps,
widgets, vi mode, `$BUFFER`/`$CURSOR` parameters, completion modules,
`region_highlight`, `POSTDISPLAY`). `+Z` or `unsetopt zle` reads lines
without editing, as in zsh. It queries the terminal as zsh does
(`.term.*` parameters, OSC 133 prompt marks, cursor forms from
`zle_cursorform`, OSC 52 for the `"+` and `"*` vi registers) and redraws
with zsh's screen-update code. Default `$fpath` includes the host's
system zsh function directories when `FPATH` is not inherited, so
autoloadable zsh functions (`compinit`, `url-quote-magic`,
`bracketed-paste-magic`, ...) and zle plugins such as
`zsh-autosuggestions` work directly. `zcompile` builds, tests (`-t`),
and loads `.zwc` wordcode dump files on `source` / `.`. zsh options
meowsh does not implement are recorded by `setopt` but have no effect.

History is kept in
`~/.meowsh_history`, in zsh's `EXTENDED_HISTORY` format: each line is
`: <start>:<elapsed>;<command>`, with the time the command started and how
many seconds it ran. A file from an older meowsh (rustyline's format) is
converted on first use and kept as `~/.meowsh_history.rustyline`. `$HISTFILE`
is not used for it, so zsh's own history file is never touched. Only an explicit `fc -W` or `fc -A`
without a file name writes `$HISTFILE`, in zsh's own format. With `-f` (no
startup files) no history file is read or written, as in zsh.

## Using meowsh as your shell

Install it, then point your terminal at it before changing your login
shell, so a broken build cannot lock you out:

```bash
sudo make install                     # installs /usr/local/bin/meowsh
# Terminal only, for example foot: shell=/usr/local/bin/meowsh in foot.ini
# Login shell:
echo /usr/local/bin/meowsh | sudo tee -a /etc/shells
chsh -s /usr/local/bin/meowsh
```

Add meowsh to `/etc/shells` even if you only use it as your terminal's
shell. Tools that open a new terminal in the current terminal's directory,
such as Omarchy's Super + Enter, read the directory of the shell running in
the focused window, but only trust it when that shell's path is listed in
`/etc/shells`. Otherwise the new terminal opens in `$HOME`. List the path
you actually run, for example `~/.cargo/bin/meowsh` after `cargo install`.

Reinstalling meowsh replaces the binary under shells that are still
running, so the kernel reports their path as `meowsh (deleted)`. That path
no longer matches `/etc/shells`, so those windows fall back to `$HOME`
until you open a fresh shell.

## Development

The source code is written in Rust and located in the `src/` directory. Run the checks with:

```bash
cargo fmt --check
RUSTFLAGS="-Dwarnings" cargo clippy --all-targets
cargo test
```

To measure zsh compatibility, run zsh's own test suite (`Test/*.ztst` in a
zsh source checkout) against a release build:

```bash
cargo build --release
python3 scripts/ztst.py --zsh-src ../zsh            # summary per file
python3 scripts/ztst.py --zsh-src ../zsh -v D04parameter  # show failures
```
