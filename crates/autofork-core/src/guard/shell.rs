//! Static analysis of a shell command line: what it writes, publishes, or
//! cannot be proven about.
//!
//! The command is parsed with the real bash grammar (tree-sitter-bash) and
//! walked with a small abstract interpreter: the working directory is
//! tracked through `cd`/`pushd`/`popd`/subshells/`env -C`/`git -C`, literal
//! variables are expanded, `$(pwd)` is known, redirections are writes,
//! `bash -c`/`sh -c`/`eval`/`source`/`xargs`/`find -exec`/wrappers like
//! `env`, `nice`, `timeout` are unwrapped and analysed recursively, and a
//! readable shell script is analysed by content. Every simple command is
//! then classified with a knowledge table: pure readers produce nothing;
//! writers produce [`Effect::Write`] on the paths they touch; git remote
//! operations, network tools, forges and deploy tools produce their own
//! effects; anything the table cannot vouch for — interpreters, build
//! tools, unknown binaries, unresolvable paths — produces
//! [`Effect::Unknown`], which the guard answers with an OS sandbox rather
//! than a guess.
//!
//! The analyser errs toward *unknown*, never toward *safe*: an unparseable
//! line, a variable it cannot expand, or a command it has never heard of
//! all fall through to the sandbox.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser};

use super::normalize;
use crate::glob;

/// One thing a command line does that the guard must judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Writes (creates, modifies, removes) at or under `path`.
    Write { path: PathBuf, by: String },
    /// Runs an executable at `path` (a script or binary named by path).
    /// Trusted when the path is inside the write set, unknown otherwise.
    Exec { path: PathBuf, by: String },
    /// A git operation that talks to a remote for the repository at `repo`
    /// (`publish`: pushes/sends; otherwise fetch/pull/clone/ls-remote).
    GitRemote { repo: PathBuf, by: String, publish: bool },
    /// Uses the network without publishing anything by itself.
    Network { by: String },
    /// Publishes: posts, pushes, deploys, sends. `what` finishes the
    /// sentence "`by` …".
    Publish { by: String, what: String },
    /// Escalates privileges (sudo, su, doas, chroot).
    Escalate { by: String },
    /// Kills processes or reconfigures the machine.
    Disrupt { by: String },
    /// Cannot be analysed. `why` finishes the sentence "`by` …".
    Unknown { by: String, why: String },
}

#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub effects: Vec<Effect>,
    /// The parser could not make sense of (part of) the line; an
    /// `Unknown` effect was recorded for it.
    pub parse_error: bool,
}

impl Analysis {
    pub fn is_clean(&self) -> bool {
        self.effects.is_empty()
    }
}

/// Analyse `command` as run from `cwd`. `trusted` are directories whose
/// scripts run as the fork's own tooling (the write set): a script found
/// there is reported as [`Effect::Exec`] and not analysed by content.
pub fn analyse(command: &str, cwd: &Path, trusted: &[PathBuf]) -> Analysis {
    let mut w = Walker::new(cwd, trusted);
    w.analyse_source(command);
    Analysis { effects: w.effects, parse_error: w.parse_error }
}

const MAX_DEPTH: u32 = 8;
const MAX_SCRIPT_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Val {
    Lit(String),
    Unknown,
}

impl Val {
    fn lit(&self) -> Option<&str> {
        match self {
            Val::Lit(s) => Some(s.as_str()),
            Val::Unknown => None,
        }
    }
    fn is(&self, s: &str) -> bool {
        self.lit() == Some(s)
    }
    fn starts_with(&self, s: &str) -> bool {
        self.lit().is_some_and(|l| l.starts_with(s))
    }
    /// A literal option: starts with `-` and is not `-` alone.
    fn is_opt(&self) -> bool {
        self.lit().is_some_and(|l| l.starts_with('-') && l != "-")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cwd {
    Known(PathBuf),
    Unknown,
}

#[derive(Debug, Clone)]
enum PVal {
    Known(PathBuf),
    Unknown,
}

#[derive(Clone)]
struct Snapshot {
    cwd: Cwd,
    prev_cwd: Cwd,
    dir_stack: Vec<Cwd>,
    vars: HashMap<String, String>,
}

struct Walker {
    cwd: Cwd,
    prev_cwd: Cwd,
    dir_stack: Vec<Cwd>,
    vars: HashMap<String, String>,
    functions: HashSet<String>,
    trusted: Vec<PathBuf>,
    home: Option<PathBuf>,
    effects: Vec<Effect>,
    parse_error: bool,
    depth: u32,
}

impl Walker {
    fn new(cwd: &Path, trusted: &[PathBuf]) -> Self {
        Walker {
            cwd: Cwd::Known(normalize(cwd)),
            prev_cwd: Cwd::Known(normalize(cwd)),
            dir_stack: Vec::new(),
            vars: HashMap::new(),
            functions: HashSet::new(),
            trusted: trusted.to_vec(),
            home: crate::sys::home_dir(),
            effects: Vec::new(),
            parse_error: false,
            depth: 0,
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            cwd: self.cwd.clone(),
            prev_cwd: self.prev_cwd.clone(),
            dir_stack: self.dir_stack.clone(),
            vars: self.vars.clone(),
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.cwd = s.cwd;
        self.prev_cwd = s.prev_cwd;
        self.dir_stack = s.dir_stack;
        self.vars = s.vars;
    }

    // -- source ------------------------------------------------------------

    fn analyse_source(&mut self, src: &str) {
        if self.depth > MAX_DEPTH {
            self.effects.push(Effect::Unknown {
                by: excerpt(src),
                why: "nests shells deeper than autofork follows".into(),
            });
            return;
        }
        let mut parser = Parser::new();
        if parser.set_language(&tree_sitter_bash::LANGUAGE.into()).is_err() {
            self.effects.push(Effect::Unknown { by: excerpt(src), why: "could not be parsed (grammar unavailable)".into() });
            return;
        }
        let Some(tree) = parser.parse(src, None) else {
            self.effects.push(Effect::Unknown { by: excerpt(src), why: "could not be parsed as shell".into() });
            return;
        };
        let root = tree.root_node();
        if root.has_error() {
            self.parse_error = true;
            self.effects.push(Effect::Unknown { by: excerpt(src), why: "could not be fully parsed as shell".into() });
        }
        self.depth += 1;
        self.walk(root, src.as_bytes());
        self.depth -= 1;
    }

    fn walk(&mut self, node: Node, src: &[u8]) {
        match node.kind() {
            "command" => self.handle_command(node, src),
            "file_redirect" => self.handle_redirect(node, src),
            "variable_assignment" => self.handle_assignment(node, src),
            "subshell" => {
                let snap = self.snapshot();
                self.walk_children(node, src);
                self.restore(snap);
            }
            "for_statement" | "c_style_for_statement" => {
                if let Some(v) = node.child_by_field_name("variable") {
                    if let Ok(name) = v.utf8_text(src) {
                        self.vars.remove(name);
                    }
                }
                self.walk_children(node, src);
            }
            "function_definition" => {
                if let Some(n) = node.child_by_field_name("name") {
                    if let Ok(name) = n.utf8_text(src) {
                        self.functions.insert(name.to_string());
                    }
                }
                self.walk_children(node, src);
            }
            "heredoc_body" | "heredoc_content" | "comment" => {}
            _ => self.walk_children(node, src),
        }
    }

    fn walk_children(&mut self, node: Node, src: &[u8]) {
        let mut c = node.walk();
        let children: Vec<Node> = node.children(&mut c).collect();
        for ch in children {
            self.walk(ch, src);
        }
    }

    // -- values ------------------------------------------------------------

    fn eval(&mut self, node: Node, src: &[u8]) -> Val {
        let text = |n: Node| n.utf8_text(src).unwrap_or("").to_string();
        match node.kind() {
            "word" => {
                let t = unescape_word(&text(node));
                self.tilde(&t)
            }
            "raw_string" => {
                let t = text(node);
                Val::Lit(t.trim_start_matches('\'').trim_end_matches('\'').to_string())
            }
            "ansi_c_string" => {
                let t = text(node);
                let inner = t.strip_prefix("$'").and_then(|s| s.strip_suffix('\'')).unwrap_or(&t);
                Val::Lit(unescape_ansi(inner))
            }
            "number" => Val::Lit(text(node)),
            "regex" | "extglob_pattern" => Val::Lit(text(node)),
            "string" => {
                let mut out = String::new();
                let mut c = node.walk();
                let children: Vec<Node> = node.children(&mut c).collect();
                let mut unknown = false;
                for ch in children {
                    match ch.kind() {
                        "string_content" => out.push_str(&unescape_dq(&text(ch))),
                        "\"" => {}
                        _ => match self.eval(ch, src) {
                            Val::Lit(s) => out.push_str(&s),
                            Val::Unknown => unknown = true,
                        },
                    }
                }
                if unknown {
                    Val::Unknown
                } else {
                    Val::Lit(out)
                }
            }
            "concatenation" => {
                let mut out = String::new();
                let mut c = node.walk();
                let children: Vec<Node> = node.children(&mut c).collect();
                let mut unknown = false;
                for ch in children {
                    match self.eval(ch, src) {
                        Val::Lit(s) => out.push_str(&s),
                        Val::Unknown => unknown = true,
                    }
                }
                if unknown {
                    Val::Unknown
                } else {
                    Val::Lit(out)
                }
            }
            "simple_expansion" => {
                let name = node.named_child(0).map(text).unwrap_or_default();
                self.lookup(&name)
            }
            "expansion" => {
                if node.child_by_field_name("operator").is_some() {
                    return Val::Unknown;
                }
                let named: Vec<Node> = {
                    let mut c = node.walk();
                    node.named_children(&mut c).collect()
                };
                if named.len() == 1 && named[0].kind() == "variable_name" {
                    self.lookup(&text(named[0]))
                } else {
                    Val::Unknown
                }
            }
            "command_substitution" => {
                // `$(pwd)` is the one substitution whose value we know.
                let inner: Vec<Node> = {
                    let mut c = node.walk();
                    node.named_children(&mut c).collect()
                };
                if inner.len() == 1 && inner[0].kind() == "command" {
                    let t = text(inner[0]);
                    if t.trim() == "pwd" {
                        if let Cwd::Known(p) = &self.cwd {
                            return Val::Lit(p.display().to_string());
                        }
                    }
                }
                self.walk_children(node, src);
                Val::Unknown
            }
            "arithmetic_expansion" | "process_substitution" => {
                self.walk_children(node, src);
                Val::Unknown
            }
            _ => Val::Unknown,
        }
    }

    fn tilde(&self, t: &str) -> Val {
        if t == "~" {
            return match &self.home {
                Some(h) => Val::Lit(h.display().to_string()),
                None => Val::Unknown,
            };
        }
        if let Some(rest) = t.strip_prefix("~/") {
            return match &self.home {
                Some(h) => Val::Lit(format!("{}/{rest}", h.display())),
                None => Val::Unknown,
            };
        }
        if t.starts_with('~') {
            return Val::Unknown;
        }
        Val::Lit(t.to_string())
    }

    fn lookup(&self, name: &str) -> Val {
        if let Some(v) = self.vars.get(name) {
            return Val::Lit(v.clone());
        }
        match name {
            "HOME" => self.home.as_ref().map(|h| Val::Lit(h.display().to_string())).unwrap_or(Val::Unknown),
            "PWD" => match &self.cwd {
                Cwd::Known(p) => Val::Lit(p.display().to_string()),
                Cwd::Unknown => Val::Unknown,
            },
            "OLDPWD" => match &self.prev_cwd {
                Cwd::Known(p) => Val::Lit(p.display().to_string()),
                Cwd::Unknown => Val::Unknown,
            },
            "TMPDIR" | "USER" | "LOGNAME" | "SHELL" | "LANG" | "TERM" | "PATH" => {
                std::env::var(name).map(Val::Lit).unwrap_or(Val::Unknown)
            }
            _ => Val::Unknown,
        }
    }

    fn handle_assignment(&mut self, node: Node, src: &[u8]) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = name_node.utf8_text(src).unwrap_or("").to_string();
        let whole = node.utf8_text(src).unwrap_or("");
        let value = match node.child_by_field_name("value") {
            Some(v) => self.eval(v, src),
            None => Val::Lit(String::new()),
        };
        if name_node.kind() != "variable_name" || whole.contains("+=") {
            self.vars.remove(&name);
            return;
        }
        match value {
            Val::Lit(s) => {
                self.vars.insert(name, s);
            }
            Val::Unknown => {
                self.vars.remove(&name);
            }
        }
    }

    // -- paths -------------------------------------------------------------

    fn path(&self, v: &Val) -> PVal {
        let Val::Lit(s) = v else { return PVal::Unknown };
        if s.is_empty() {
            return PVal::Unknown;
        }
        let abs = if s.starts_with('/') {
            PathBuf::from(s)
        } else {
            match &self.cwd {
                Cwd::Known(c) => c.join(s),
                Cwd::Unknown => return PVal::Unknown,
            }
        };
        let abs = normalize(&abs);
        let text = abs.to_string_lossy();
        if glob::has_wildcard(&text) || text.contains('[') || text.contains('{') {
            return PVal::Known(glob::literal_prefix(&text));
        }
        PVal::Known(abs)
    }

    fn write(&mut self, v: &Val, by: &str) {
        match self.path(v) {
            PVal::Known(p) => {
                if !is_device(&p) {
                    self.effects.push(Effect::Write { path: p, by: by.to_string() });
                }
            }
            PVal::Unknown => self.effects.push(Effect::Unknown {
                by: by.to_string(),
                why: "targets a path autofork cannot resolve".into(),
            }),
        }
    }

    fn write_cwd(&mut self, by: &str) {
        match &self.cwd {
            Cwd::Known(c) => self.effects.push(Effect::Write { path: c.clone(), by: by.to_string() }),
            Cwd::Unknown => self.effects.push(Effect::Unknown {
                by: by.to_string(),
                why: "writes into a working directory autofork lost track of".into(),
            }),
        }
    }

    fn unknown(&mut self, by: &str, why: &str) {
        self.effects.push(Effect::Unknown { by: by.to_string(), why: why.to_string() });
    }

    fn is_trusted(&self, p: &Path) -> bool {
        let real = super::canonical_prefix(p);
        self.trusted.iter().any(|t| {
            let t = super::canonical_prefix(t);
            real == t || real.starts_with(&t)
        })
    }

    // -- redirects ---------------------------------------------------------

    fn handle_redirect(&mut self, node: Node, src: &[u8]) {
        let Some(dest) = node.child_by_field_name("destination") else { return };
        let whole = node.utf8_text(src).unwrap_or("");
        let dest_start = dest.start_byte() - node.start_byte();
        let op = whole[..dest_start.min(whole.len())].trim();
        let op = op.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&');
        let dest_text = dest.utf8_text(src).unwrap_or("");
        // fd duplication (`2>&1`, `>&-`) is not a file.
        if op.ends_with('&') && (dest_text.chars().all(|c| c.is_ascii_digit()) || dest_text == "-") {
            return;
        }
        let writes = matches!(op, ">" | ">>" | ">|" | "&>" | "&>>" | "<>" | ">&" | ">>&");
        if !writes {
            return;
        }
        let by = excerpt(whole);
        let v = self.eval(dest, src);
        self.write(&v, &by);
    }

    // -- commands ----------------------------------------------------------

    fn handle_command(&mut self, node: Node, src: &[u8]) {
        let by = excerpt(node.utf8_text(src).unwrap_or(""));
        let mut argv: Vec<Val> = Vec::new();
        let mut redirects: Vec<Node> = Vec::new();
        let mut c = node.walk();
        if c.goto_first_child() {
            loop {
                let ch = c.node();
                match (c.field_name(), ch.kind()) {
                    (Some("name"), _) => {
                        let v = match ch.named_child(0) {
                            Some(inner) => self.eval(inner, src),
                            None => Val::Unknown,
                        };
                        argv.push(v);
                    }
                    (Some("argument"), _) => {
                        let v = self.eval(ch, src);
                        argv.push(v);
                    }
                    (Some("redirect"), _) => redirects.push(ch),
                    (_, "variable_assignment") => {
                        // A `VAR=x cmd` prefix: scoped to the command; we
                        // neither keep nor need it.
                    }
                    (_, "file_redirect") | (_, "herestring_redirect") => redirects.push(ch),
                    _ => {}
                }
                if !c.goto_next_sibling() {
                    break;
                }
            }
        }
        if !argv.is_empty() {
            self.run(argv, &by);
        }
        for r in redirects {
            if r.kind() == "file_redirect" {
                self.handle_redirect(r, src);
            }
        }
    }

    /// Classify one simple command. `argv[0]` is the command name.
    fn run(&mut self, argv: Vec<Val>, by: &str) {
        if argv.is_empty() {
            return;
        }
        let Some(name) = argv[0].lit().map(|s| s.to_string()) else {
            self.unknown(by, "has a command name autofork cannot resolve");
            return;
        };
        if name.is_empty() {
            return;
        }
        if self.functions.contains(&name) {
            return; // body analysed at its definition
        }
        let base = name.rsplit('/').next().unwrap_or(&name).to_string();
        let args: Vec<Val> = argv[1..].to_vec();

        // `x --version` / `x --help` never does anything.
        if args.len() == 1 && matches!(args[0].lit(), Some("--version" | "--help" | "-h" | "version" | "help")) {
            return;
        }

        match base.as_str() {
            // -- shell state ------------------------------------------------
            "cd" => self.cmd_cd(&args),
            "pushd" => self.cmd_pushd(&args),
            "popd" => {
                if let Some(top) = self.dir_stack.pop() {
                    self.prev_cwd = self.cwd.clone();
                    self.cwd = top;
                }
            }
            "export" | "local" | "declare" | "typeset" | "readonly" | "unset" | "set" | "shift" | "return" | "exit"
            | "break" | "continue" | "wait" | "trap" | "ulimit" | "umask" | "alias" | "unalias" | "hash" | "type"
            | "true" | "false" | ":" | "test" | "[" | "[[" | "read" | "getopts" | "let" | "printf" | "echo" | "pwd"
            | "dirs" | "jobs" | "fg" | "bg" | "disown" | "times" | "help" | "which" | "whereis" | "whatis"
            | "compgen" | "complete" | "enable" | "logout" | "suspend" | "history" | "fc" | "caller" | "mapfile"
            | "readarray" | "sleep" => {
                // `export X=y` etc.: plain assignments are handled by the
                // grammar as variable_assignment nodes; here only side
                // effects matter and there are none.
            }

            // -- wrappers -----------------------------------------------------
            "env" => self.cmd_env(&args, by),
            "nice" => {
                let rest = strip_leading_opts(&args, &["-n", "--adjustment"]);
                self.run(rest, by);
            }
            "nohup" | "exec" | "builtin" | "chronic" | "unbuffer" | "time" | "ionice" | "caffeinate" | "stdbuf" => {
                let takes = match base.as_str() {
                    "ionice" => &["-c", "-n", "-p"][..],
                    "caffeinate" => &["-t", "-w"][..],
                    "stdbuf" => &["-i", "-o", "-e"][..],
                    _ => &[][..],
                };
                let rest = strip_leading_opts(&args, takes);
                self.run(rest, by);
            }
            "timeout" => {
                let rest = strip_leading_opts(&args, &["-s", "--signal", "-k", "--kill-after"]);
                // first positional is the duration
                let rest: Vec<Val> = rest.into_iter().skip(1).collect();
                self.run(rest, by);
            }
            "command" => {
                if args.iter().any(|a| a.is("-v") || a.is("-V")) {
                    return;
                }
                let rest = strip_leading_opts(&args, &[]);
                self.run(rest, by);
            }
            "sudo" | "doas" | "su" | "chroot" | "pfexec" => {
                self.effects.push(Effect::Escalate { by: by.to_string() });
            }

            // -- shells and evaluators ----------------------------------------
            "bash" | "sh" | "zsh" | "dash" | "ksh" | "mksh" | "ash" | "fish" => self.cmd_shell(&args, by),
            "eval" => {
                let mut parts = Vec::new();
                for a in &args {
                    match a.lit() {
                        Some(s) => parts.push(s.to_string()),
                        None => {
                            self.unknown(by, "evaluates text autofork cannot see");
                            return;
                        }
                    }
                }
                let joined = parts.join(" ");
                self.analyse_source(&joined);
            }
            "source" | "." => match args.first() {
                Some(f) => {
                    let f = f.clone();
                    self.analyse_script(&f, by, false);
                }
                None => {}
            },
            "xargs" | "gxargs" => self.cmd_xargs(&args, by),
            "find" | "gfind" | "fd" | "fdfind" => {
                if base == "fd" || base == "fdfind" {
                    self.cmd_fd(&args, by);
                } else {
                    self.cmd_find(&args, by);
                }
            }
            "parallel" => self.unknown(by, "runs commands autofork cannot see"),

            // -- git and forges ----------------------------------------------
            "git" => self.cmd_git(&args, by),
            "gh" | "glab" | "hub" | "tea" => self.cmd_forge(&args, by),
            "git-lfs" => self.unknown(by, "is a git extension autofork does not analyse"),

            // -- readers -----------------------------------------------------
            "cat" | "ls" | "ll" | "dir" | "head" | "tail" | "wc" | "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack"
            | "diff" | "cmp" | "comm" | "stat" | "file" | "id" | "whoami" | "date" | "basename" | "dirname"
            | "realpath" | "readlink" | "printenv" | "uname" | "sort" | "uniq" | "cut" | "tr" | "jq" | "yq" | "tree"
            | "du" | "df" | "md5" | "md5sum" | "shasum" | "sha1sum" | "sha256sum" | "sha512sum" | "cksum" | "xxd"
            | "od" | "hexdump" | "strings" | "column" | "paste" | "join" | "less" | "more" | "nl" | "tac" | "rev"
            | "fold" | "expand" | "unexpand" | "seq" | "bc" | "expr" | "hostname" | "arch" | "nproc" | "getconf"
            | "locale" | "ps" | "top" | "lsof" | "pgrep" | "netstat" | "ifconfig" | "ip" | "sw_vers" | "sysctl"
            | "system_profiler" | "uptime" | "w" | "who" | "last" | "env_parallel" | "look" | "tsort" | "fmt"
            | "pr" | "yes" | "cal" | "man" | "info" | "tty" | "stty" | "base64" | "base32" | "iconv"
            | "dos2unix_" | "sum" | "numfmt" | "factor" | "units" | "ncal" | "ldd" | "otool" | "nm"
            | "objdump" | "readelf" | "lipo" | "codesign" | "spctl" | "plutil" | "sqlite3" | "bat" | "exa" | "eza"
            | "lsd" | "fzf" | "delta" | "dig" | "nslookup" | "host" | "ping" | "traceroute" | "mtr" | "arp"
            | "route" | "ss" | "vm_stat" | "iostat" | "vmstat" | "free" | "lscpu" | "lsblk" | "dmesg" | "journalctl"
            | "watch" | "pbpaste" | "getent" | "groups" | "finger" => {
                // Tools whose only writes are to stdout. A few can be
                // pointed at the network (dig, ping, sqlite3 on a file —
                // sqlite3 writes!); handle the exceptions below.
                if base == "sqlite3" {
                    self.cmd_sqlite(&args, by);
                } else if matches!(base.as_str(), "dig" | "nslookup" | "host" | "ping" | "traceroute" | "mtr") {
                    self.effects.push(Effect::Network { by: by.to_string() });
                } else if base == "plutil" && args.iter().any(|a| a.is("-convert") || a.is("-insert") || a.is("-replace") || a.is("-remove")) {
                    if let Some(last) = non_opts(&args).last() {
                        let last = (*last).clone();
                        self.write(&last, by);
                    }
                }
            }
            "sed" | "gsed" => self.cmd_sed(&args, by),
            "awk" | "gawk" | "mawk" | "nawk" => {
                let dangerous = args.iter().any(|a| {
                    a.lit().is_some_and(|s| s.contains('>') || s.contains("system(") || s.contains("| ") || s.contains("|\""))
                });
                if dangerous {
                    self.unknown(by, "runs an awk program that writes files or commands");
                }
            }
            "perl" | "ruby" | "python" | "python2" | "python3" | "pythonw" | "node" | "nodejs" | "deno" | "bun" | "php"
            | "swift" | "java" | "kotlin" | "kotlinc" | "scala" | "lua" | "luajit" | "tclsh" | "Rscript" | "R" | "julia"
            | "ghc" | "runghc" | "runhaskell" | "elixir" | "erl" | "escript" | "groovy" | "clojure" | "clj" | "ocaml"
            | "racket" | "guile" | "osascript" | "open" | "automator" | "expect" => {
                self.unknown(by, "runs a program autofork cannot see inside");
            }
            "go" | "rustc" | "gcc" | "cc" | "clang" | "clang++" | "g++" | "ld" | "make" | "gmake" | "cmake" | "ninja"
            | "gradle" | "gradlew" | "mvn" | "ant" | "sbt" | "bazel" | "bazelisk" | "buck" | "meson" | "xcodebuild"
            | "swiftc" | "tsc" | "esbuild" | "vite" | "webpack" | "dx" | "trunk" | "wasm-pack" | "zig" | "nim" | "dune"
            | "stack" | "cabal" | "mix" | "rebar3" | "lein" | "pytest" | "tox" | "nox" | "jest" | "mocha" | "vitest" => {
                self.unknown(by, "builds or tests, which writes wherever the build system decides");
            }
            "cargo" => self.cmd_tool(&args, by, &["publish", "login", "owner", "yank"], "publishes to crates.io"),
            "npm" | "pnpm" | "yarn" | "npx" | "bunx" => {
                self.cmd_tool(&args, by, &["publish", "login", "adduser", "deprecate", "unpublish", "owner", "dist-tag"], "publishes to the npm registry")
            }
            "pip" | "pip3" | "pipx" | "uv" | "poetry" | "conda" | "twine" | "gem" | "bundle" | "composer" | "cpan" | "cpanm"
            | "luarocks" | "nix" | "nix-env" | "nix-shell" | "flatpak" | "snap" => {
                self.cmd_tool(&args, by, &["upload", "publish", "push"], "publishes a package")
            }
            "brew" | "apt" | "apt-get" | "dnf" | "yum" | "pacman" | "apk" | "port" | "zypper" | "emerge" | "mas" => {
                let sub = args.iter().find(|a| !a.is_opt()).and_then(|a| a.lit()).unwrap_or("");
                if matches!(sub, "list" | "info" | "search" | "deps" | "outdated" | "config" | "doctor" | "show" | "--prefix" | "prefix" | "leaves" | "which" | "cat" | "home" | "desc" | "policy" | "ls") {
                    return;
                }
                self.unknown(by, "installs or removes software system-wide");
            }
            "docker" | "podman" | "nerdctl" | "docker-compose" => {
                let sub = args.iter().find(|a| !a.is_opt()).and_then(|a| a.lit()).unwrap_or("");
                if matches!(sub, "push" | "login" | "logout" | "manifest" | "buildx") {
                    self.effects.push(Effect::Publish { by: by.to_string(), what: "pushes container images".into() });
                } else if matches!(sub, "ps" | "images" | "logs" | "inspect" | "version" | "info" | "stats" | "top" | "port" | "diff" | "history" | "events" | "context") {
                    self.effects.push(Effect::Network { by: by.to_string() });
                } else {
                    self.unknown(by, "drives a container engine, whose effects autofork cannot see");
                }
            }
            "kubectl" | "oc" | "helm" | "terraform" | "tofu" | "pulumi" | "ansible" | "ansible-playbook" | "gcloud" | "aws"
            | "az" | "flyctl" | "fly" | "vercel" | "netlify" | "heroku" | "railway" | "wrangler" | "serverless" | "sls"
            | "cdk" | "sam" | "eb" | "doctl" | "linode-cli" | "hcloud" | "scw" | "argocd" | "flux" | "istioctl" | "velero"
            | "k9s" | "stern" => {
                let sub = args.iter().find(|a| !a.is_opt()).and_then(|a| a.lit()).unwrap_or("");
                let reads = matches!(
                    sub,
                    "get" | "describe" | "logs" | "top" | "explain" | "version" | "config" | "list" | "ls" | "status" | "show"
                        | "plan" | "validate" | "fmt" | "output" | "state" | "history" | "search" | "info" | "whoami" | "auth"
                        | "cluster-info" | "api-resources" | "api-versions" | "diff" | "lint" | "template" | "env" | "repo"
                        | "inspect" | "preview" | "console" | "help" | "completion"
                ) || sub.starts_with("describe");
                if reads {
                    self.effects.push(Effect::Network { by: by.to_string() });
                } else {
                    self.effects.push(Effect::Publish { by: by.to_string(), what: "changes remote infrastructure".into() });
                }
            }

            // -- writers -----------------------------------------------------
            "tee" => {
                let targets: Vec<Val> = non_opts(&args).into_iter().cloned().collect();
                for t in targets {
                    self.write(&t, by);
                }
            }
            "cp" | "install" => {
                let takes = &["-t", "--target-directory", "-m", "-o", "-g", "-S", "--suffix", "--backup"][..];
                let rest = skip_opts_with(&args, takes);
                if let Some(t) = opt_value(&args, &["-t", "--target-directory"]) {
                    self.write(&t, by);
                } else if base == "install" && args.iter().any(|a| a.is("-d")) {
                    for t in rest {
                        self.write(&t, by);
                    }
                } else if rest.len() >= 2 {
                    let dest = rest[rest.len() - 1].clone();
                    self.write(&dest, by);
                } else if rest.len() == 1 {
                    self.unknown(by, "has no destination autofork can see");
                }
            }
            "mv" => {
                let rest = skip_opts_with(&args, &["-t", "--target-directory", "-S", "--suffix", "--backup"]);
                let target = opt_value(&args, &["-t", "--target-directory"]);
                let (sources, dest): (Vec<Val>, Option<Val>) = match target {
                    Some(t) => (rest.clone(), Some(t)),
                    None if rest.len() >= 2 => (rest[..rest.len() - 1].to_vec(), Some(rest[rest.len() - 1].clone())),
                    None => (rest.clone(), None),
                };
                for s in sources {
                    self.write(&s, by);
                }
                match dest {
                    Some(d) => self.write(&d, by),
                    None => self.unknown(by, "has no destination autofork can see"),
                }
            }
            "ln" => {
                let rest = skip_opts_with(&args, &["-t", "--target-directory", "-S", "--suffix", "--backup"]);
                if let Some(t) = opt_value(&args, &["-t", "--target-directory"]) {
                    self.write(&t, by);
                } else if rest.len() >= 2 {
                    let dest = rest[rest.len() - 1].clone();
                    self.write(&dest, by);
                } else {
                    self.write_cwd(by);
                }
            }
            "rsync" => {
                let rest = skip_opts_with(&args, &["-e", "--rsh", "--exclude", "--include", "--exclude-from", "--include-from", "--files-from", "--filter", "--log-file", "--password-file", "--bwlimit", "--timeout", "--port", "--chmod", "--chown", "--backup-dir", "--suffix", "--temp-dir", "--partial-dir", "--compare-dest", "--copy-dest", "--link-dest", "--max-size", "--min-size", "--out-format", "--rsync-path"]);
                let remote = rest.iter().any(|a| a.lit().is_some_and(|s| is_remote_spec(s)));
                if remote {
                    self.effects.push(Effect::Network { by: by.to_string() });
                }
                if args.iter().any(|a| a.is("-n") || a.is("--dry-run") || a.is("--list-only")) {
                    return;
                }
                if let Some(dest) = rest.last() {
                    if !dest.lit().is_some_and(|s| is_remote_spec(s)) {
                        let dest = dest.clone();
                        self.write(&dest, by);
                    }
                } else {
                    self.unknown(by, "has no destination autofork can see");
                }
            }
            "scp" | "sftp" | "ssh" | "ssh-copy-id" | "telnet" | "nc" | "ncat" | "netcat" | "socat" | "ftp" | "lftp" | "mosh"
            | "rlogin" | "rsh" | "et" | "autossh" => {
                self.effects.push(Effect::Network { by: by.to_string() });
                if base == "scp" {
                    let rest = skip_opts_with(&args, &["-i", "-P", "-o", "-F", "-S", "-c", "-l", "-J"]);
                    if let Some(dest) = rest.last() {
                        if !dest.lit().is_some_and(|s| is_remote_spec(s)) {
                            let dest = dest.clone();
                            self.write(&dest, by);
                        }
                    }
                }
            }
            "curl" | "wget" | "wget2" | "http" | "https" | "httpie" | "xh" | "aria2c" | "axel" => self.cmd_http(&base, &args, by),
            "rm" | "rmdir" | "unlink" | "shred" | "srm" | "trash" => {
                let targets: Vec<Val> = non_opts(&args).into_iter().cloned().collect();
                if targets.is_empty() {
                    self.unknown(by, "names nothing autofork can see");
                }
                for t in targets {
                    self.write(&t, by);
                }
            }
            "mkdir" | "touch" | "mkfifo" | "mknod" | "truncate" | "fallocate" => {
                let takes = match base.as_str() {
                    "mkdir" => &["-m", "--mode"][..],
                    "touch" => &["-t", "-r", "-d", "--date", "--reference"][..],
                    "truncate" => &["-s", "--size", "-r", "--reference"][..],
                    "fallocate" => &["-l", "-o", "-n"][..],
                    _ => &[][..],
                };
                let targets = skip_opts_with(&args, takes);
                for t in targets {
                    self.write(&t, by);
                }
            }
            "chmod" | "chown" | "chgrp" | "chflags" => {
                let rest = skip_opts_with(&args, &["--reference"]);
                for t in rest.into_iter().skip(1) {
                    self.write(&t, by);
                }
            }
            "xattr" => {
                if args.iter().any(|a| a.is("-w") || a.is("-d") || a.is("-c")) {
                    if let Some(last) = non_opts(&args).last() {
                        let last = (*last).clone();
                        self.write(&last, by);
                    }
                }
            }
            "dd" => {
                let mut wrote = false;
                for a in &args {
                    if let Some(s) = a.lit() {
                        if let Some(of) = s.strip_prefix("of=") {
                            let v = self.tilde(of);
                            self.write(&v, by);
                            wrote = true;
                        }
                    } else {
                        self.unknown(by, "has an argument autofork cannot resolve");
                        return;
                    }
                }
                let _ = wrote;
            }
            "tar" | "gtar" | "bsdtar" => self.cmd_tar(&args, by),
            "zip" => {
                let rest = skip_opts_with(&args, &["-b", "-t", "-tt", "-n", "-x", "-i"]);
                match rest.first() {
                    Some(f) => {
                        let f = f.clone();
                        self.write(&f, by);
                    }
                    None => self.unknown(by, "names no archive autofork can see"),
                }
            }
            "unzip" => {
                if args.iter().any(|a| a.is("-l") || a.is("-t") || a.is("-z") || a.is("-p") || a.is("-c")) {
                    return;
                }
                match opt_value(&args, &["-d"]) {
                    Some(d) => self.write(&d, by),
                    None => self.write_cwd(by),
                }
            }
            "gzip" | "gunzip" | "bzip2" | "bunzip2" | "xz" | "unxz" | "zstd" | "unzstd" | "lz4" | "brotli" | "compress"
            | "uncompress" | "zcat" | "bzcat" | "xzcat" | "zstdcat" => {
                if base.ends_with("cat") || args.iter().any(|a| a.is("-c") || a.is("--stdout") || a.is("-t") || a.is("--test") || a.is("-l") || a.is("--list")) {
                    return;
                }
                let targets: Vec<Val> = non_opts(&args).into_iter().cloned().collect();
                if targets.is_empty() {
                    return; // stdin → stdout
                }
                for t in targets {
                    // The output lands next to the input.
                    match self.path(&t) {
                        PVal::Known(p) => {
                            let parent = p.parent().map(|x| x.to_path_buf()).unwrap_or(p);
                            self.effects.push(Effect::Write { path: parent, by: by.to_string() });
                        }
                        PVal::Unknown => self.unknown(by, "targets a path autofork cannot resolve"),
                    }
                }
            }
            "patch" => match opt_value(&args, &["-d", "--directory"]) {
                Some(d) => self.write(&d, by),
                None => self.write_cwd(by),
            },
            "defaults" => {
                let sub = args.iter().find(|a| !a.is_opt()).and_then(|a| a.lit()).unwrap_or("");
                if matches!(sub, "read" | "read-type" | "domains" | "find" | "export") {
                    return;
                }
                self.unknown(by, "changes macOS preferences");
            }
            "pbcopy" | "say" | "afplay" | "tput" | "clear" | "reset" => {}
            "kill" | "pkill" | "killall" | "launchctl" | "systemctl" | "service" | "reboot" | "shutdown" | "halt"
            | "poweroff" | "diskutil" | "mount" | "umount" | "nvram" | "dscl" | "pmset" | "networksetup" | "scutil"
            | "tmutil" | "softwareupdate" | "csrutil" | "fdesetup" | "profiles" | "sysadminctl" | "dseditgroup"
            | "iptables" | "nft" | "ufw" | "pfctl" | "swapoff" | "swapon" | "modprobe" | "insmod"
            | "rmmod" => {
                if base == "launchctl" && args.iter().any(|a| a.is("list") || a.is("print")) {
                    return;
                }
                if base == "systemctl" && args.iter().any(|a| a.is("status") || a.is("list-units") || a.is("show") || a.is("is-active") || a.is("cat")) {
                    return;
                }
                if base == "diskutil" && args.iter().any(|a| a.is("list") || a.is("info")) {
                    return;
                }
                self.effects.push(Effect::Disrupt { by: by.to_string() });
            }
            "crontab" => {
                if args.iter().any(|a| a.is("-l")) {
                    return;
                }
                self.effects.push(Effect::Disrupt { by: by.to_string() });
            }
            "mail" | "mailx" | "sendmail" | "mutt" | "neomutt" | "msmtp" | "swaks" => {
                self.effects.push(Effect::Publish { by: by.to_string(), what: "sends mail".into() });
            }
            "slack" | "discord" | "tweet" | "toot" | "ntfy" | "pushover" | "telegram-send" | "signal-cli" => {
                self.effects.push(Effect::Publish { by: by.to_string(), what: "posts a message".into() });
            }

            // -- by path -----------------------------------------------------
            _ => {
                if name.contains('/') {
                    let v = self.tilde(&name);
                    match self.path(&v) {
                        PVal::Known(p) => {
                            let trusted = self.is_trusted(&p);
                            self.effects.push(Effect::Exec { path: p.clone(), by: by.to_string() });
                            if !trusted {
                                // Learn what we can from its content: a
                                // literal `rm -rf` inside gets a proper
                                // refusal instead of a sandbox failure.
                                self.analyse_script(&v, by, true);
                            }
                        }
                        PVal::Unknown => self.unknown(by, "runs a program at a path autofork cannot resolve"),
                    }
                } else {
                    self.unknown(by, "is not a command autofork knows");
                }
            }
        }
    }

    // -- individual commands -----------------------------------------------

    fn cmd_cd(&mut self, args: &[Val]) {
        let rest = skip_opts_with(args, &[]);
        let target = match rest.first() {
            None => match &self.home {
                Some(h) => Cwd::Known(h.clone()),
                None => Cwd::Unknown,
            },
            Some(v) if v.is("-") => self.prev_cwd.clone(),
            Some(v) => match self.path(v) {
                PVal::Known(p) => Cwd::Known(p),
                PVal::Unknown => Cwd::Unknown,
            },
        };
        self.prev_cwd = std::mem::replace(&mut self.cwd, target);
    }

    fn cmd_pushd(&mut self, args: &[Val]) {
        let rest = skip_opts_with(args, &[]);
        match rest.first() {
            None => {
                if let Some(top) = self.dir_stack.pop() {
                    let cur = std::mem::replace(&mut self.cwd, top);
                    self.dir_stack.push(cur);
                }
            }
            Some(v) => {
                let target = match self.path(v) {
                    PVal::Known(p) => Cwd::Known(p),
                    PVal::Unknown => Cwd::Unknown,
                };
                let cur = std::mem::replace(&mut self.cwd, target);
                self.prev_cwd = cur.clone();
                self.dir_stack.push(cur);
            }
        }
    }

    fn cmd_env(&mut self, args: &[Val], by: &str) {
        let mut i = 0;
        let mut chdir: Option<Val> = None;
        while i < args.len() {
            let a = &args[i];
            match a.lit() {
                Some("-i") | Some("--ignore-environment") | Some("-0") | Some("--null") | Some("-v") | Some("--debug") => i += 1,
                Some("-u") | Some("--unset") | Some("-S") | Some("--split-string") => i += 2,
                Some("-C") | Some("--chdir") => {
                    chdir = args.get(i + 1).cloned();
                    i += 2;
                }
                Some(s) if s.starts_with("--chdir=") => {
                    chdir = Some(Val::Lit(s["--chdir=".len()..].to_string()));
                    i += 1;
                }
                Some(s) if s.starts_with("-C") && s.len() > 2 => {
                    chdir = Some(Val::Lit(s[2..].to_string()));
                    i += 1;
                }
                Some(s) if s.starts_with("-u") && s.len() > 2 => i += 1,
                Some("--") => {
                    i += 1;
                    break;
                }
                Some(s) if is_assignment(s) => i += 1,
                Some(s) if s.starts_with('-') => i += 1,
                _ => break,
            }
        }
        let rest: Vec<Val> = args[i..].to_vec();
        if rest.is_empty() {
            return;
        }
        match chdir {
            Some(d) => {
                let snap = self.snapshot();
                self.cmd_cd(&[d]);
                self.run(rest, by);
                self.restore(snap);
            }
            None => self.run(rest, by),
        }
    }

    fn cmd_shell(&mut self, args: &[Val], by: &str) {
        let mut i = 0;
        let mut script: Option<Val> = None;
        let mut want_c = false;
        while i < args.len() {
            let a = &args[i];
            match a.lit() {
                Some("--") => {
                    i += 1;
                    break;
                }
                Some("-o") | Some("+o") | Some("--rcfile") | Some("--init-file") => i += 2,
                Some(s) if s.starts_with('-') && !s.starts_with("--") && s.len() > 1 => {
                    if s.contains('c') {
                        want_c = true;
                    }
                    i += 1;
                    if want_c {
                        script = args.get(i).cloned();
                        break;
                    }
                }
                Some(s) if s.starts_with("--") => i += 1,
                _ => break,
            }
        }
        if want_c {
            match script {
                Some(Val::Lit(s)) => {
                    let snap = self.snapshot();
                    self.analyse_source(&s);
                    self.restore(snap);
                }
                _ => self.unknown(by, "runs shell text autofork cannot see"),
            }
            return;
        }
        match args.get(i) {
            Some(f) => {
                let f = f.clone();
                self.analyse_script(&f, by, true);
            }
            None => self.unknown(by, "starts a shell whose input autofork cannot see"),
        }
    }

    /// Analyse a script file by content. `isolate`: the script's `cd`s and
    /// variables do not leak into the caller (a subprocess); `source` does
    /// not isolate.
    fn analyse_script(&mut self, file: &Val, by: &str, isolate: bool) {
        let p = match self.path(file) {
            PVal::Known(p) => p,
            PVal::Unknown => {
                self.unknown(by, "runs a script at a path autofork cannot resolve");
                return;
            }
        };
        if self.is_trusted(&p) {
            self.effects.push(Effect::Exec { path: p, by: by.to_string() });
            return;
        }
        let meta = match std::fs::metadata(&p) {
            Ok(m) if m.is_file() => m,
            _ => {
                self.unknown(by, "runs a script autofork cannot read");
                return;
            }
        };
        if meta.len() > MAX_SCRIPT_BYTES {
            self.unknown(by, "runs a script too large for autofork to analyse");
            return;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            self.unknown(by, "runs a script autofork cannot read");
            return;
        };
        if let Some(first) = content.lines().next() {
            if let Some(interp) = first.strip_prefix("#!") {
                let interp = interp.trim();
                let base = interp.split_whitespace().last().unwrap_or("").rsplit('/').next().unwrap_or("");
                let is_shell = matches!(base, "sh" | "bash" | "zsh" | "dash" | "ksh")
                    || (interp.contains("env") && interp.split_whitespace().any(|w| matches!(w, "sh" | "bash" | "zsh" | "dash" | "ksh")));
                if !is_shell {
                    self.unknown(by, &format!("runs a {base} script autofork cannot see inside"));
                    return;
                }
            }
        }
        // Positional parameters of the script are unknown; `$0` is the file.
        let snap = self.snapshot();
        self.vars.insert("0".into(), p.display().to_string());
        self.analyse_source(&content);
        if isolate {
            self.restore(snap);
        }
    }

    fn cmd_xargs(&mut self, args: &[Val], by: &str) {
        let mut i = 0;
        let mut replace: Option<String> = None;
        while i < args.len() {
            let a = &args[i];
            match a.lit() {
                Some("--") => {
                    i += 1;
                    break;
                }
                Some("-I") | Some("--replace") | Some("-i") => {
                    replace = args.get(i + 1).and_then(|v| v.lit()).map(|s| s.to_string());
                    if a.is("-i") && replace.as_deref().map(|s| s.starts_with('-')).unwrap_or(true) {
                        replace = Some("{}".into());
                        i += 1;
                    } else {
                        i += 2;
                    }
                }
                Some(s) if s.starts_with("-I") && s.len() > 2 => {
                    replace = Some(s[2..].to_string());
                    i += 1;
                }
                Some("-n") | Some("-P") | Some("-L") | Some("-s") | Some("-d") | Some("-a") | Some("-E") | Some("--max-args")
                | Some("--max-procs") | Some("--max-lines") | Some("--max-chars") | Some("--delimiter") | Some("--arg-file")
                | Some("--eof") | Some("--process-slot-var") => i += 2,
                Some(s) if s.starts_with('-') => i += 1,
                _ => break,
            }
        }
        let mut cmd: Vec<Val> = args[i..].to_vec();
        if cmd.is_empty() {
            return; // xargs alone = echo
        }
        match replace {
            Some(r) => {
                for v in cmd.iter_mut() {
                    if v.lit().is_some_and(|s| s.contains(&r)) {
                        *v = Val::Unknown;
                    }
                }
            }
            None => cmd.push(Val::Unknown),
        }
        self.run(cmd, by);
    }

    fn cmd_find(&mut self, args: &[Val], by: &str) {
        let mut i = 0;
        // Leading options.
        while i < args.len() && matches!(args[i].lit(), Some("-H" | "-L" | "-P" | "-E" | "-X" | "-d" | "-s" | "-x")) {
            i += 1;
        }
        let mut paths: Vec<Val> = Vec::new();
        while i < args.len() {
            let a = &args[i];
            let stop = a.lit().is_some_and(|s| s.starts_with('-') || s == "(" || s == "!" || s == ",");
            if stop {
                break;
            }
            paths.push(a.clone());
            i += 1;
        }
        if paths.is_empty() {
            paths.push(Val::Lit(".".into()));
        }
        while i < args.len() {
            match args[i].lit() {
                Some("-delete") => {
                    for p in paths.clone() {
                        self.write(&p, by);
                    }
                    i += 1;
                }
                Some("-exec" | "-execdir" | "-ok" | "-okdir") => {
                    i += 1;
                    let mut cmd: Vec<Val> = Vec::new();
                    while i < args.len() && !(args[i].is(";") || args[i].is("+")) {
                        let mut v = args[i].clone();
                        if v.lit().is_some_and(|s| s.contains("{}")) {
                            v = Val::Unknown;
                        }
                        cmd.push(v);
                        i += 1;
                    }
                    i += 1;
                    if !cmd.is_empty() {
                        self.run(cmd, by);
                    }
                }
                Some("-fprint" | "-fprint0" | "-fprintf" | "-fls") => {
                    if let Some(f) = args.get(i + 1) {
                        let f = f.clone();
                        self.write(&f, by);
                    }
                    i += 2;
                }
                _ => i += 1,
            }
        }
    }

    fn cmd_fd(&mut self, args: &[Val], by: &str) {
        let mut i = 0;
        while i < args.len() {
            match args[i].lit() {
                Some("-x" | "--exec" | "-X" | "--exec-batch") => {
                    i += 1;
                    let mut cmd: Vec<Val> = Vec::new();
                    while i < args.len() && !args[i].is(";") {
                        let mut v = args[i].clone();
                        if v.lit().is_some_and(|s| s.contains('{')) {
                            v = Val::Unknown;
                        }
                        cmd.push(v);
                        i += 1;
                    }
                    if cmd.is_empty() {
                        cmd.push(Val::Unknown);
                    }
                    self.run(cmd, by);
                    i += 1;
                }
                _ => i += 1,
            }
        }
    }

    fn cmd_git(&mut self, args: &[Val], by: &str) {
        let mut i = 0;
        let mut repo: PVal = match &self.cwd {
            Cwd::Known(c) => PVal::Known(c.clone()),
            Cwd::Unknown => PVal::Unknown,
        };
        while i < args.len() {
            let a = &args[i];
            match a.lit() {
                Some("-C") => {
                    let d = args.get(i + 1).cloned().unwrap_or(Val::Unknown);
                    repo = self.join_repo(&repo, &d);
                    i += 2;
                }
                Some("-c") | Some("--git-dir") | Some("--work-tree") | Some("--namespace") | Some("--exec-path") | Some("--super-prefix") | Some("--config-env") | Some("--list-cmds") => {
                    if a.is("--work-tree") {
                        let d = args.get(i + 1).cloned().unwrap_or(Val::Unknown);
                        repo = self.join_repo(&repo, &d);
                    }
                    if a.is("--git-dir") {
                        let d = args.get(i + 1).cloned().unwrap_or(Val::Unknown);
                        repo = self.join_repo(&repo, &d);
                    }
                    i += 2;
                }
                Some(s) if s.starts_with("--work-tree=") || s.starts_with("--git-dir=") => {
                    let d = Val::Lit(s.split_once('=').map(|x| x.1).unwrap_or("").to_string());
                    repo = self.join_repo(&repo, &d);
                    i += 1;
                }
                Some(s) if s.starts_with("-C") && s.len() > 2 => {
                    let d = Val::Lit(s[2..].to_string());
                    repo = self.join_repo(&repo, &d);
                    i += 1;
                }
                Some(s) if s.starts_with('-') => i += 1,
                _ => break,
            }
        }
        let Some(sub) = args.get(i).and_then(|a| a.lit()).map(|s| s.to_string()) else {
            if args.get(i).is_some() {
                self.unknown(by, "runs a git subcommand autofork cannot resolve");
            }
            return;
        };
        let rest: Vec<Val> = args[i + 1..].to_vec();
        let positional: Vec<&Val> = rest.iter().filter(|a| !a.is_opt()).collect();
        let has = |o: &[&str]| rest.iter().any(|a| a.lit().is_some_and(|s| o.contains(&s)));

        let repo_write = |w: &mut Walker| match &repo {
            PVal::Known(p) => w.effects.push(Effect::Write { path: p.clone(), by: by.to_string() }),
            PVal::Unknown => w.unknown(by, "changes a repository autofork cannot locate"),
        };
        let repo_remote = |w: &mut Walker, publish: bool| match &repo {
            PVal::Known(p) => w.effects.push(Effect::GitRemote { repo: p.clone(), by: by.to_string(), publish }),
            PVal::Unknown => w.unknown(by, "talks to a remote of a repository autofork cannot locate"),
        };

        match sub.as_str() {
            "status" | "log" | "diff" | "show" | "rev-parse" | "ls-files" | "ls-tree" | "blame" | "describe" | "cat-file"
            | "grep" | "shortlog" | "var" | "count-objects" | "name-rev" | "merge-base" | "cherry" | "for-each-ref"
            | "check-ignore" | "check-attr" | "diff-tree" | "diff-index" | "diff-files" | "help" | "version" | "--version"
            | "rev-list" | "show-ref" | "whatchanged" | "range-diff" | "show-branch" | "verify-commit" | "verify-tag"
            | "fsck" | "annotate" | "difftool" | "get-tar-commit-id" | "mailinfo"
            | "merge-tree" | "patch-id" | "stripspace" | "verify-pack" | "check-ref-format" | "column"
            | "interpret-trailers" | "web--browse" => {}
            "ls-remote" => repo_remote(self, false),
            "push" | "send-email" | "send-pack" => repo_remote(self, true),
            "fetch" | "pull" | "remote-update" => repo_remote(self, false),
            "clone" => {
                let dest = match positional.len() {
                    0 => None,
                    1 => positional[0].lit().map(|url| {
                        let name = url.trim_end_matches('/').rsplit('/').next().unwrap_or("repo").trim_end_matches(".git");
                        Val::Lit(name.to_string())
                    }),
                    _ => Some((*positional[positional.len() - 1]).clone()),
                };
                match dest.map(|d| self.path(&d)) {
                    Some(PVal::Known(p)) => self.effects.push(Effect::GitRemote { repo: p, by: by.to_string(), publish: false }),
                    _ => self.unknown(by, "clones into a place autofork cannot resolve"),
                }
            }
            "remote" => {
                let sub2 = positional.first().and_then(|a| a.lit()).unwrap_or("");
                match sub2 {
                    "" | "show" | "get-url" => {}
                    "update" | "prune" => repo_remote(self, false),
                    _ => repo_write(self),
                }
            }
            "submodule" => {
                let sub2 = positional.first().and_then(|a| a.lit()).unwrap_or("");
                match sub2 {
                    "status" | "summary" | "" => {}
                    "update" | "add" | "sync" | "foreach" | "init" => repo_remote(self, false),
                    _ => repo_write(self),
                }
            }
            "branch" => {
                let creates_or_deletes = has(&["-d", "-D", "-m", "-M", "-c", "-C", "--delete", "--move", "--copy", "--set-upstream-to", "-u", "--unset-upstream", "--edit-description", "-f", "--force"]);
                let lists = has(&["-a", "-r", "-v", "-vv", "--list", "--show-current", "--contains", "--merged", "--no-merged", "--points-at", "-l"]);
                if creates_or_deletes || (!positional.is_empty() && !lists) {
                    repo_write(self);
                }
            }
            "tag" => {
                let lists = has(&["-l", "--list", "-n", "--contains", "--points-at", "--merged", "--no-merged", "-v", "--verify"]);
                if !positional.is_empty() && !lists {
                    repo_write(self);
                }
            }
            "stash" => {
                let sub2 = positional.first().and_then(|a| a.lit()).unwrap_or("");
                if !matches!(sub2, "list" | "show") {
                    repo_write(self);
                }
            }
            "config" => {
                let reads = has(&["--get", "--get-all", "--get-regexp", "-l", "--list", "--show-origin", "--get-urlmatch", "--get-color", "--get-colorbool", "--show-scope"]);
                if reads || positional.len() == 1 && !has(&["--unset", "--unset-all", "--add", "--replace-all", "--remove-section", "--rename-section", "-e", "--edit"]) {
                    return;
                }
                if has(&["--global"]) {
                    match &self.home {
                        Some(h) => {
                            let cfg = h.join(".gitconfig");
                            self.effects.push(Effect::Write { path: cfg, by: by.to_string() });
                        }
                        None => self.unknown(by, "changes the global git config"),
                    }
                } else if has(&["--system"]) {
                    self.effects.push(Effect::Write { path: PathBuf::from("/etc/gitconfig"), by: by.to_string() });
                } else {
                    repo_write(self);
                }
            }
            "worktree" => {
                let sub2 = positional.first().and_then(|a| a.lit()).unwrap_or("");
                match sub2 {
                    "list" | "" => {}
                    "add" => {
                        repo_write(self);
                        if let Some(p) = positional.get(1) {
                            let p = (*p).clone();
                            self.write(&p, by);
                        }
                    }
                    _ => repo_write(self),
                }
            }
            "reflog" => {
                let sub2 = positional.first().and_then(|a| a.lit()).unwrap_or("");
                if matches!(sub2, "expire" | "delete") {
                    repo_write(self);
                }
            }
            "notes" | "bisect" | "rerere" | "sparse-checkout" | "maintenance" | "lfs" | "replace" | "credential" => {
                let sub2 = positional.first().and_then(|a| a.lit()).unwrap_or("");
                if matches!(sub2, "list" | "show" | "log" | "visualize" | "view" | "status" | "diff" | "fill" | "" | "env" | "version") {
                    return;
                }
                repo_write(self);
            }
            "symbolic-ref" => {
                if positional.len() >= 2 || has(&["-d", "--delete"]) {
                    repo_write(self);
                }
            }
            "archive" | "format-patch" | "bundle" => {
                if let Some(o) = opt_value(&rest, &["-o", "--output", "--output-directory"]) {
                    self.write(&o, by);
                } else if sub == "format-patch" || (sub == "bundle" && positional.first().is_some_and(|a| a.is("create"))) {
                    if sub == "bundle" {
                        if let Some(f) = positional.get(1) {
                            let f = (*f).clone();
                            self.write(&f, by);
                        }
                    } else if !has(&["--stdout"]) {
                        self.write_cwd(by);
                    }
                }
            }
            "init" => match positional.first() {
                Some(d) => {
                    let d = (*d).clone();
                    self.write(&d, by);
                }
                None => repo_write(self),
            },
            "add" | "commit" | "checkout" | "switch" | "restore" | "reset" | "merge" | "rebase" | "rm" | "mv" | "clean"
            | "apply" | "cherry-pick" | "revert" | "am" | "update-index" | "filter-branch" | "gc" | "prune" | "update-ref"
            | "mergetool" | "read-tree" | "write-tree" | "commit-tree" | "hash-object" | "pack-refs" | "repack" | "fast-import"
            | "unpack-objects" | "index-pack" | "prune-packed" | "checkout-index" | "mktag" | "mktree" | "multi-pack-index"
            | "commit-graph" | "stage" | "citool" | "gui" | "instaweb" => repo_write(self),
            _ => self.unknown(by, &format!("runs `git {sub}`, which autofork does not know (an alias?)")),
        }
    }

    fn join_repo(&self, repo: &PVal, d: &Val) -> PVal {
        let Val::Lit(s) = d else { return PVal::Unknown };
        if s.starts_with('/') {
            return PVal::Known(normalize(Path::new(s)));
        }
        match repo {
            PVal::Known(r) => PVal::Known(normalize(&r.join(s))),
            PVal::Unknown => PVal::Unknown,
        }
    }

    fn cmd_forge(&mut self, args: &[Val], by: &str) {
        let words: Vec<&str> = args.iter().filter(|a| !a.is_opt()).filter_map(|a| a.lit()).collect();
        let first = words.first().copied().unwrap_or("");
        let second = words.get(1).copied().unwrap_or("");
        let read_words = ["view", "list", "ls", "status", "diff", "checks", "download", "search", "browse", "config", "help", "version", "completion", "whoami", "logs"];
        let reads = matches!(first, "help" | "version" | "search" | "browse" | "completion" | "status")
            || (first == "auth" && matches!(second, "status" | "token"))
            || read_words.contains(&second)
            || (first == "api" && !args.iter().any(|a| {
                a.lit().is_some_and(|s| {
                    matches!(s, "-X" | "--method" | "-f" | "-F" | "--field" | "--raw-field" | "--input")
                        || s.starts_with("--method=")
                        || s.starts_with("-X")
                        || s.starts_with("-f")
                        || s.starts_with("-F")
                })
            }))
            || (first == "run" && matches!(second, "view" | "list" | "watch" | "download"))
            || (first == "repo" && matches!(second, "view" | "list" | "clone"));
        if reads {
            self.effects.push(Effect::Network { by: by.to_string() });
        } else {
            self.effects.push(Effect::Publish { by: by.to_string(), what: "acts on the forge (comment, PR, issue, review, release, …)".into() });
        }
    }

    fn cmd_tool(&mut self, args: &[Val], by: &str, publish_subs: &[&str], what: &str) {
        let sub = args.iter().find(|a| !a.is_opt()).and_then(|a| a.lit()).unwrap_or("");
        if publish_subs.contains(&sub) {
            self.effects.push(Effect::Publish { by: by.to_string(), what: what.into() });
        } else if matches!(sub, "--version" | "-V" | "version" | "help" | "--help" | "metadata" | "tree" | "search" | "info" | "view" | "show" | "ls" | "list" | "outdated" | "audit" | "why" | "explain" | "locate-project" | "pkgid" | "config" | "env" ) {
            // Some of these use the network (search, info, audit) but
            // change nothing.
            if matches!(sub, "search" | "info" | "view" | "show" | "audit" | "outdated") {
                self.effects.push(Effect::Network { by: by.to_string() });
            }
        } else {
            self.unknown(by, "runs a build or package tool whose effects autofork cannot see");
        }
    }

    fn cmd_http(&mut self, base: &str, args: &[Val], by: &str) {
        self.effects.push(Effect::Network { by: by.to_string() });
        match base {
            "curl" => {
                if let Some(o) = opt_value(args, &["-o", "--output", "-D", "--dump-header", "--trace", "--trace-ascii", "-c", "--cookie-jar", "--output-dir", "--etag-save"]) {
                    self.write(&o, by);
                }
                if args.iter().any(|a| a.is("-O") || a.is("--remote-name") || a.is("--remote-name-all") || a.is("-J") || a.lit().is_some_and(|s| s.starts_with('-') && !s.starts_with("--") && s.len() > 1 && s[1..].contains('O'))) {
                    self.write_cwd(by);
                }
            }
            "wget" | "wget2" => {
                if let Some(o) = opt_value(args, &["-O", "--output-document", "-P", "--directory-prefix", "-o", "--output-file", "-a", "--append-output"]) {
                    if !o.is("-") {
                        self.write(&o, by);
                    }
                } else if !args.iter().any(|a| a.is("--spider") || a.is("-q") && false) {
                    self.write_cwd(by);
                }
            }
            "aria2c" | "axel" => match opt_value(args, &["-d", "--dir", "-o", "--out"]) {
                Some(o) => self.write(&o, by),
                None => self.write_cwd(by),
            },
            _ => {
                // httpie: `-d/--download` saves to cwd or -o.
                if args.iter().any(|a| a.is("-d") || a.is("--download")) {
                    match opt_value(args, &["-o", "--output"]) {
                        Some(o) => self.write(&o, by),
                        None => self.write_cwd(by),
                    }
                }
            }
        }
    }

    fn cmd_sed(&mut self, args: &[Val], by: &str) {
        let in_place = args.iter().any(|a| {
            a.lit().is_some_and(|s| {
                s == "-i" || s.starts_with("-i") && !s.starts_with("--") || s == "--in-place" || s.starts_with("--in-place=")
                    || (s.starts_with('-') && !s.starts_with("--") && s.len() > 1 && s[1..].contains('i') && !s[1..].contains('n') || (s.starts_with('-') && !s.starts_with("--") && s[1..].chars().all(|c| "nrEisuz".contains(c)) && s.contains('i')))
            })
        });
        if !in_place {
            return;
        }
        let has_script_opt = args.iter().any(|a| a.is("-e") || a.is("-f") || a.starts_with("--expression") || a.starts_with("--file") || a.lit().is_some_and(|s| s.starts_with("-e") && s.len() > 2));
        let mut files = skip_opts_with(args, &["-e", "-f", "--expression", "--file", "-l", "--line-length"]);
        files.retain(|f| !f.is("")); // BSD `-i ''`
        if !has_script_opt && !files.is_empty() {
            files.remove(0); // the script
        }
        if files.is_empty() {
            self.unknown(by, "edits in place a file autofork cannot see");
        }
        for f in files {
            self.write(&f, by);
        }
    }

    fn cmd_tar(&mut self, args: &[Val], by: &str) {
        let mut mode = ' ';
        let mut file: Option<Val> = None;
        let mut dir: Option<Val> = None;
        let mut i = 0;
        let mut old_style_f = false;
        while i < args.len() {
            let a = &args[i];
            match a.lit() {
                Some("-C") | Some("--directory") => {
                    dir = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                Some(s) if s.starts_with("--directory=") => {
                    dir = Some(Val::Lit(s["--directory=".len()..].to_string()));
                }
                Some("-f") | Some("--file") => {
                    file = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                Some(s) if s.starts_with("--file=") => file = Some(Val::Lit(s["--file=".len()..].to_string())),
                Some("-x") | Some("--extract") | Some("--get") => mode = 'x',
                Some("-c") | Some("--create") => mode = 'c',
                Some("-r") | Some("--append") | Some("-u") | Some("--update") | Some("--delete") => mode = 'r',
                Some("-t") | Some("--list") => mode = 't',
                Some(s) if i == 0 && !s.starts_with("--") => {
                    // old-style or clustered: `xzf`, `-xzf`
                    let flags = s.trim_start_matches('-');
                    for ch in flags.chars() {
                        match ch {
                            'x' => mode = 'x',
                            'c' => mode = 'c',
                            'r' | 'u' => mode = 'r',
                            't' => mode = 't',
                            'f' => old_style_f = true,
                            'C' => {}
                            _ => {}
                        }
                    }
                    if old_style_f {
                        file = args.get(i + 1).cloned();
                        i += 2;
                        continue;
                    }
                }
                Some(s) if s.starts_with('-') && !s.starts_with("--") => {
                    let flags = &s[1..];
                    for ch in flags.chars() {
                        match ch {
                            'x' => mode = 'x',
                            'c' => mode = 'c',
                            'r' | 'u' => mode = 'r',
                            't' => mode = 't',
                            _ => {}
                        }
                    }
                    if flags.ends_with('f') {
                        file = args.get(i + 1).cloned();
                        i += 2;
                        continue;
                    }
                    if flags.ends_with('C') {
                        dir = args.get(i + 1).cloned();
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        match mode {
            'x' => match dir {
                Some(d) => self.write(&d, by),
                None => self.write_cwd(by),
            },
            'c' | 'r' => match file {
                Some(f) if f.is("-") => {}
                Some(f) => self.write(&f, by),
                None => {}
            },
            't' => {}
            _ => self.unknown(by, "runs tar in a mode autofork cannot tell"),
        }
    }

    fn cmd_sqlite(&mut self, args: &[Val], by: &str) {
        let rest = skip_opts_with(args, &["-init", "-cmd", "-separator", "-newline", "-nullvalue", "-vfs", "-A", "-batch_"]);
        if args.iter().any(|a| a.is("-readonly")) {
            return;
        }
        match rest.first() {
            Some(db) => {
                let db = db.clone();
                self.write(&db, by);
            }
            None => {}
        }
    }
}

// -- helpers -------------------------------------------------------------

fn excerpt(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 100 {
        let cut: String = one.chars().take(97).collect();
        format!("{cut}...")
    } else {
        one
    }
}

fn is_assignment(s: &str) -> bool {
    let Some((name, _)) = s.split_once('=') else { return false };
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit())
}

fn is_remote_spec(s: &str) -> bool {
    if s.starts_with("rsync://") || s.starts_with("ssh://") {
        return true;
    }
    // host:path or user@host:path (a colon before any slash)
    match s.find(':') {
        Some(i) => !s[..i].contains('/') && i > 0,
        None => false,
    }
}

fn is_device(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s == "/dev/null" || s == "/dev/stdout" || s == "/dev/stderr" || s == "/dev/stdin" || s.starts_with("/dev/tty") || s.starts_with("/dev/fd/") || s.starts_with("/dev/pts/") || s == "/dev/zero" || s == "/dev/random" || s == "/dev/urandom"
}

/// Arguments minus options. `takes` lists options that consume the next
/// argument (`-o FILE`); `--` ends option parsing. Unknown values are kept.
fn skip_opts_with(args: &[Val], takes: &[&str]) -> Vec<Val> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut opts_done = false;
    while i < args.len() {
        let a = &args[i];
        if !opts_done {
            if a.is("--") {
                opts_done = true;
                i += 1;
                continue;
            }
            if let Some(s) = a.lit() {
                if s.starts_with('-') && s != "-" {
                    if takes.contains(&s) {
                        i += 2;
                    } else if s.starts_with("--") && s.contains('=') {
                        i += 1;
                    } else {
                        i += 1;
                    }
                    continue;
                }
            }
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

/// Leading options removed (a wrapper's own flags), the rest untouched:
/// the wrapped command keeps its arguments, options included.
fn strip_leading_opts(args: &[Val], takes: &[&str]) -> Vec<Val> {
    let mut i = 0;
    while i < args.len() {
        match args[i].lit() {
            Some("--") => {
                i += 1;
                break;
            }
            Some(s) if takes.contains(&s) => i += 2,
            Some(s) if s.starts_with('-') && s != "-" => i += 1,
            _ => break,
        }
    }
    args[i..].to_vec()
}

/// The value of the first of `names` present (`-o FILE` or `--out=FILE`).
fn opt_value(args: &[Val], names: &[&str]) -> Option<Val> {
    let mut i = 0;
    while i < args.len() {
        if let Some(s) = args[i].lit() {
            if names.contains(&s) {
                return args.get(i + 1).cloned();
            }
            for n in names {
                if n.starts_with("--") {
                    if let Some(v) = s.strip_prefix(&format!("{n}=")) {
                        return Some(Val::Lit(v.to_string()));
                    }
                } else if n.len() == 2 && s.len() > 2 && s.starts_with(n) && !s.starts_with("--") {
                    return Some(Val::Lit(s[2..].to_string()));
                }
            }
        }
        i += 1;
    }
    None
}

fn non_opts(args: &[Val]) -> Vec<&Val> {
    let mut out = Vec::new();
    let mut done = false;
    for a in args {
        if !done && a.is("--") {
            done = true;
            continue;
        }
        if !done && a.is_opt() {
            continue;
        }
        out.push(a);
    }
    out
}

fn unescape_word(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape_dq(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('"') | Some('\\') | Some('$') | Some('`') | Some('\n') => {
                    out.push(chars.next().unwrap());
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('\'') => out.push('\''),
                Some(o) => {
                    out.push('\\');
                    out.push(o);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cmd: &str) -> Vec<Effect> {
        analyse(cmd, Path::new("/work/proj"), &[]).effects
    }

    fn writes(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Write { path, .. } => Some(path.display().to_string()),
                _ => None,
            })
            .collect()
    }

    fn unknowns(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Unknown { why, .. } => Some(why.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn readers_are_clean() {
        for c in [
            "ls -la",
            "cat foo.md | grep x | wc -l",
            "git status && git log --oneline -5",
            "find . -name '*.rs' | xargs grep -n TODO",
            "rg --files | head",
            "echo hi > /dev/null",
            "cargo --version",
            "sed -n '1,10p' file.txt",
            "git -C /other/repo diff",
            "ls ~/Downloads",
            "test -f x && echo yes",
            "[ -d dir ] || echo no",
            "for f in *.md; do wc -l \"$f\"; done",
            "git branch -a",
            "git remote -v",
            "gh pr view 26789",
            "gh api repos/x/y/pulls/1",
        ] {
            let e = run(c);
            assert!(e.iter().all(|x| matches!(x, Effect::Network { .. })), "{c}: {e:?}");
        }
    }

    #[test]
    fn simple_writes_resolve_against_cwd() {
        let e = run("mkdir -p out && cp a.txt out/b.txt");
        assert_eq!(writes(&e), vec!["/work/proj/out", "/work/proj/out/b.txt"]);
        let e = run("rm -rf /tmp/x ../sibling");
        assert_eq!(writes(&e), vec!["/tmp/x", "/work/sibling"]);
        let e = run("echo hi > notes.txt");
        assert_eq!(writes(&e), vec!["/work/proj/notes.txt"]);
        let e = run("cmd_unknown 2>&1 | tee -a log.txt");
        assert!(writes(&e).contains(&"/work/proj/log.txt".to_string()));
    }

    #[test]
    fn cd_is_followed() {
        let e = run("cd /brain && echo x > a.md");
        assert_eq!(writes(&e), vec!["/brain/a.md"]);
        let e = run("cd sub; cd ..; touch here");
        assert_eq!(writes(&e), vec!["/work/proj/here"]);
        let e = run("(cd /brain && touch in) && touch out");
        assert_eq!(writes(&e), vec!["/brain/in", "/work/proj/out"]);
        let e = run("cd /brain; cd -; touch back");
        assert_eq!(writes(&e), vec!["/work/proj/back"]);
        let e = run("pushd /brain >/dev/null; touch a; popd; touch b");
        assert_eq!(writes(&e), vec!["/brain/a", "/work/proj/b"]);
        let e = run("env -C /brain touch c; touch d");
        assert_eq!(writes(&e), vec!["/brain/c", "/work/proj/d"]);
    }

    #[test]
    fn unknown_cd_poisons_relative_paths() {
        let e = run("cd \"$SOMEWHERE\" && touch x");
        assert!(!unknowns(&e).is_empty(), "{e:?}");
        assert!(writes(&e).is_empty());
        // absolute paths are still known
        let e = run("cd $SOMEWHERE && touch /brain/x");
        assert_eq!(writes(&e), vec!["/brain/x"]);
    }

    #[test]
    fn variables_expand_when_literal() {
        let e = run("DIR=/brain/k; mkdir -p \"$DIR\"; echo x > $DIR/f.md");
        assert_eq!(writes(&e), vec!["/brain/k", "/brain/k/f.md"]);
        let e = run("OUT=$(some_cmd); touch \"$OUT\"");
        assert!(!unknowns(&e).is_empty());
        let e = run("touch \"$(pwd)/z\"");
        assert_eq!(writes(&e), vec!["/work/proj/z"]);
    }

    #[test]
    fn nested_shells_are_analysed() {
        let e = run("bash -c 'cd /brain && rm -rf old'");
        assert_eq!(writes(&e), vec!["/brain/old"]);
        let e = run("sh -c \"touch /etc/x\"");
        assert_eq!(writes(&e), vec!["/etc/x"]);
        let e = run("eval 'rm -f /work/proj/a'");
        assert_eq!(writes(&e), vec!["/work/proj/a"]);
        let e = run("eval \"$CMD\"");
        assert!(!unknowns(&e).is_empty());
        let e = run("bash -c \"$SCRIPT\"");
        assert!(!unknowns(&e).is_empty());
        // a nested cd does not leak out
        let e = run("bash -c 'cd /brain'; touch after");
        assert_eq!(writes(&e), vec!["/work/proj/after"]);
    }

    #[test]
    fn xargs_and_find_exec() {
        let e = run("find . -name '*.tmp' -delete");
        assert_eq!(writes(&e), vec!["/work/proj"]);
        let e = run("find /brain -name '*.md' -exec touch {} \\;");
        assert!(!unknowns(&e).is_empty(), "{e:?}");
        let e = run("find . -type f | xargs rm");
        assert!(!unknowns(&e).is_empty());
        let e = run("ls | xargs -I{} cp {} /brain/inbox/");
        assert_eq!(writes(&e), vec!["/brain/inbox"]);
        let e = run("find . -name '*.md' | xargs grep -l foo");
        assert!(e.is_empty(), "{e:?}");
    }

    #[test]
    fn git_semantics() {
        let e = run("git add -A && git commit -m x");
        assert_eq!(writes(&e), vec!["/work/proj", "/work/proj"]);
        let e = run("git push --force origin feature");
        assert_eq!(e, vec![Effect::GitRemote { repo: "/work/proj".into(), by: "git push --force origin feature".into(), publish: true }]);
        let e = run("git -C /brain pull --rebase");
        assert_eq!(e, vec![Effect::GitRemote { repo: "/brain".into(), by: "git -C /brain pull --rebase".into(), publish: false }]);
        let e = run("cd /brain && git push");
        assert!(matches!(&e[0], Effect::GitRemote { repo, publish: true, .. } if repo == Path::new("/brain")));
        let e = run("git clone https://x/y/repo.git");
        assert!(matches!(&e[0], Effect::GitRemote { repo, .. } if repo == Path::new("/work/proj/repo")));
        let e = run("git config --global user.name x");
        assert!(writes(&e)[0].ends_with(".gitconfig"));
        let e = run("git branch");
        assert!(e.is_empty());
        let e = run("git branch new-branch");
        assert_eq!(writes(&e), vec!["/work/proj"]);
        let e = run("git stash list");
        assert!(e.is_empty());
        let e = run("git stash");
        assert_eq!(writes(&e), vec!["/work/proj"]);
        let e = run("git wat");
        assert!(!unknowns(&e).is_empty());
    }

    #[test]
    fn forges_and_network() {
        let e = run("gh pr comment 26789 --body 'Addressed'");
        assert!(matches!(&e[0], Effect::Publish { .. }), "{e:?}");
        let e = run("gh api -X POST repos/x/y/issues/1/comments -f body=hi");
        assert!(matches!(&e[0], Effect::Publish { .. }));
        let e = run("curl -s https://example.com");
        assert_eq!(e, vec![Effect::Network { by: "curl -s https://example.com".into() }]);
        let e = run("curl -o out.json https://x");
        assert!(writes(&e).contains(&"/work/proj/out.json".to_string()));
        let e = run("ssh grand-impala 'bazel build //...'");
        assert!(matches!(&e[0], Effect::Network { .. }));
        let e = run("scp file host:/tmp/");
        assert!(matches!(&e[0], Effect::Network { .. }));
        let e = run("rsync -av src/ /brain/dst/");
        assert_eq!(writes(&e), vec!["/brain/dst"]);
        let e = run("kubectl apply -f x.yaml");
        assert!(matches!(&e[0], Effect::Publish { .. }));
        let e = run("kubectl get pods");
        assert!(matches!(&e[0], Effect::Network { .. }));
    }

    #[test]
    fn interpreters_and_builds_are_unknown() {
        for c in ["python3 -c 'print(1)'", "node script.js", "cargo build", "make test", "bazel build //...", "npm install", "perl -pi -e 's/a/b/' f"] {
            let e = run(c);
            assert!(!unknowns(&e).is_empty(), "{c}: {e:?}");
        }
        let e = run("cargo publish");
        assert!(matches!(&e[0], Effect::Publish { .. }));
        let e = run("npm publish");
        assert!(matches!(&e[0], Effect::Publish { .. }));
    }

    #[test]
    fn escalation_and_disruption() {
        assert!(matches!(&run("sudo rm -rf /")[0], Effect::Escalate { .. }));
        assert!(matches!(&run("kill -9 1234")[0], Effect::Disrupt { .. }));
        assert!(matches!(&run("launchctl unload x")[0], Effect::Disrupt { .. }));
        assert!(run("launchctl list").is_empty());
        assert!(run("crontab -l").is_empty());
    }

    #[test]
    fn sed_in_place() {
        let e = run("sed -i '' 's/a/b/' /brain/x.md");
        assert_eq!(writes(&e), vec!["/brain/x.md"]);
        let e = run("sed -i.bak -e 's/a/b/' notes.md");
        assert_eq!(writes(&e), vec!["/work/proj/notes.md"]);
        let e = run("sed -n 's/a/b/p' notes.md");
        assert!(e.is_empty());
        let e = run("sed -Ei 's/a/b/' f");
        assert_eq!(writes(&e), vec!["/work/proj/f"]);
    }

    #[test]
    fn tar_modes() {
        let e = run("tar xzf a.tgz -C /brain/x");
        assert_eq!(writes(&e), vec!["/brain/x"]);
        let e = run("tar -czf /tmp/out.tgz .");
        assert_eq!(writes(&e), vec!["/tmp/out.tgz"]);
        let e = run("tar tf a.tgz");
        assert!(e.is_empty());
        let e = run("tar -xf a.tar");
        assert_eq!(writes(&e), vec!["/work/proj"]);
    }

    #[test]
    fn scripts_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("do.sh");
        std::fs::write(&s, "#!/bin/bash\nset -e\nrm -rf /work/proj/target\n").unwrap();
        let cmd = format!("{} now", s.display());
        let e = analyse(&cmd, Path::new("/work/proj"), &[]);
        assert!(e.effects.iter().any(|x| matches!(x, Effect::Exec { .. })));
        assert!(writes(&e.effects).contains(&"/work/proj/target".to_string()));
        // trusted: reported as Exec only, content not analysed
        let e = analyse(&cmd, Path::new("/work/proj"), &[dir.path().to_path_buf()]);
        assert_eq!(e.effects.len(), 1);
        assert!(matches!(&e.effects[0], Effect::Exec { .. }));
        // a python script by path is unknown
        let py = dir.path().join("x.py");
        std::fs::write(&py, "#!/usr/bin/env python3\nprint(1)\n").unwrap();
        let e = analyse(&format!("{}", py.display()), Path::new("/work/proj"), &[]);
        assert!(!unknowns(&e.effects).is_empty());
        // `bash script.sh`
        let e = analyse(&format!("bash {}", s.display()), Path::new("/work/proj"), &[]);
        assert!(writes(&e.effects).contains(&"/work/proj/target".to_string()));
    }

    #[test]
    fn parse_errors_are_unknown() {
        let a = analyse("echo 'unterminated", Path::new("/work"), &[]);
        assert!(a.parse_error);
        assert!(!unknowns(&a.effects).is_empty());
    }

    #[test]
    fn wrappers_are_stripped() {
        let e = run("nohup nice -n 5 timeout 30 rm -rf /tmp/x");
        assert_eq!(writes(&e), vec!["/tmp/x"]);
        let e = run("env FOO=bar rm x");
        assert_eq!(writes(&e), vec!["/work/proj/x"]);
        let e = run("command -v rm");
        assert!(e.is_empty());
        let e = run("time cargo build");
        assert!(!unknowns(&e).is_empty());
    }

    #[test]
    fn unknown_command_is_unknown() {
        let e = run("frobnicate --all");
        assert!(!unknowns(&e).is_empty());
        let e = run("$CMD --all");
        assert!(!unknowns(&e).is_empty());
        let e = run("./run.sh");
        assert!(!unknowns(&e).is_empty()); // not readable
    }

    #[test]
    fn heredoc_to_file() {
        let e = run("cat > /brain/x.md <<'EOF'\nhello\nEOF");
        assert_eq!(writes(&e), vec!["/brain/x.md"]);
        let e = run("cat <<EOF > out.txt\nhi\nEOF");
        assert_eq!(writes(&e), vec!["/work/proj/out.txt"]);
    }

    #[test]
    fn globs_resolve_to_their_literal_prefix() {
        let e = run("rm -f /brain/tmp/*.bak");
        assert_eq!(writes(&e), vec!["/brain/tmp"]);
        let e = run("rm *.log");
        assert_eq!(writes(&e), vec!["/work/proj"]);
    }

    #[test]
    fn pipelines_and_lists() {
        let e = run("make build 2>&1 | tail -20 || true");
        assert!(!unknowns(&e).is_empty());
        assert!(writes(&e).is_empty());
        let e = run("ls; rm a; ls");
        assert_eq!(writes(&e), vec!["/work/proj/a"]);
    }
}
