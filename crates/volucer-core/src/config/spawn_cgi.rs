//! The `spawn_cgi` backend: FastCGI processes this server starts and supervises.

use super::*;

/// A pool of FastCGI processes volucer starts and supervises.
///
/// **Two drivers, one shape.** The pool's job — assign an address to each slot, spawn one
/// process tree per address, wait until it listens, restart it when it dies, recycle it on
/// `max_requests` — has nothing to do with PHP. What *was* PHP-shaped is the three things that
/// differ between `php-cgi` and any other FastCGI application server, and they are the three
/// this type now expresses as a choice:
///
/// | | `binary` (php-cgi) | `command` (anything else) |
/// | --- | --- | --- |
/// | how the address is given | `-b <address>` | `{address}` in the command, or `VOLUCER_FCGI_ADDRESS` |
/// | concurrency inside a slot | `children` → `PHP_FCGI_CHILDREN` | `concurrency` |
/// | a unix socket on Windows | a named pipe, which is what `-b` binds there | TCP only |
///
/// Exactly one of [`SpawnCgiConfig::binary`] and [`SpawnCgiConfig::command`] is set. `binary`
/// keeps every configuration written before `command` existed meaning exactly what it meant,
/// including the prefork and pipe behaviour, because those are properties of `php-cgi` rather
/// than of the pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnCgiConfig {
    /// The `php-cgi` binary. **Exactly one of `binary` and `command`.**
    ///
    /// Kept as its own key rather than folded into `command = "php-cgi -b {address}"` because
    /// the two are not the same thing: this one also means "export `PHP_FCGI_CHILDREN`",
    /// "`children` is the slot's concurrency", and "a Windows `socket` is a named pipe". A
    /// deployment written before `command` existed must keep all three.
    #[serde(default)]
    pub binary: Option<PathBuf>,

    /// A command that starts **one slot**, e.g.
    /// `gunicorn myapp.wsgi --bind {address}` or `flup-fcgi {address}`.
    ///
    /// **Where the address goes.** `{address}` is replaced in every argument with the address
    /// that slot must bind — `127.0.0.1:19001`, `unix:/run/app/py.0.sock` — and the same string
    /// is exported as [`FCGI_ADDRESS_ENV`], so a program that reads its address from the
    /// environment needs no placeholder at all. A placeholder is unavoidable for the majority
    /// that take it as an argument, and an argument is the only form a shell-free tokenizer can
    /// build safely.
    ///
    /// **[`FCGI_CONCURRENCY_PLACEHOLDER`] is the second**, replaced with the resolved
    /// [`SpawnCgiConfig::concurrency`], so a program that is told how many workers to start is
    /// told the number this file already states: `command = "gunicorn app.wsgi --workers
    /// {concurrency} --bind {address}"`. `{workers}` is refused by name — a slot command has no
    /// sibling count to be told — while every other brace is left exactly as written, because a
    /// command may legitimately carry one (`--json {"a":1}`) and a placeholder checker invented
    /// here would refuse a working deployment.
    ///
    /// **The process must serve FastCGI on that address and nothing else.** The pool connects
    /// to it, so an application server that ignores the address and picks its own is not
    /// reachable — the first request is a 502 after a startup timeout, which is the failure
    /// `check` reports as "did not start listening".
    ///
    /// Split by the same tokenizer a worker's `command` uses ([`crate::program::split_command`]),
    /// which is deliberately not a shell: no variables, no globs, no operators. A deployment
    /// that needs shell syntax writes a script and names it.
    #[serde(default)]
    pub command: Option<String>,

    /// Number of processes, and therefore number of addresses. `0` means one per logical CPU.
    ///
    /// **Each of these is one slot, and for the `binary` driver that is a deliberate choice
    /// rather than the only option.** `php-cgi -b` with `PHP_FCGI_CHILDREN = n` preforking is a
    /// real Linux feature — measured here, not assumed: `n = 4` produced exactly four children
    /// and distributed twelve sequential requests round-robin across all four, and `SIGTERM` to
    /// the master left none behind. The pool sets `PHP_FCGI_CHILDREN` itself (see
    /// `pool.rs`), because `workers` has to be a number the pool can enforce: a slot that
    /// hides twenty children looks idle while twenty requests run, cannot be recycled by
    /// `max_requests`, and does not stop work when it is killed.
    ///
    /// The shape that would collect prefork's memory saving without losing that control is
    /// [`BackendConfig::Attach`]: a supervisor that forks its own children and a server that
    /// dials them, so no process hides anybody.
    #[serde(default = "default_workers")]
    pub workers: u32,

    /// Host each process binds to. Loopback only on Windows.
    #[serde(default = "default_bind_host")]
    pub bind_host: String,

    /// First port to assign. Each worker gets the next free port from here.
    #[serde(default = "default_base_port")]
    pub base_port: u16,

    /// Bind unix sockets instead of TCP, where the platform has them.
    ///
    /// This is a path **prefix**, not a finished path: slot `i` binds
    /// `<prefix>.<i>.sock`, for the same reason each TCP slot needs its own port
    /// — one socket per process is what lets a slot be recycled, counted and
    /// reasoned about on its own, whatever the process can or cannot do with
    /// preforking (see [`SpawnCgiConfig::workers`]).
    ///
    /// `php-cgi -b` accepts an `AF_UNIX` path on any non-Windows platform because
    /// `fcgi_listen` (`main/fastcgi.c`) only falls back to TCP when the argument
    /// contains a `:` or is all digits. On Windows the same code path is a named
    /// pipe, which is a different feature, so this option is rejected there at
    /// validation time rather than accepted and ignored.
    ///
    /// `bind_host` and `base_port` are unused when this is set.
    #[serde(default)]
    pub socket: Option<String>,

    /// PHP processes per slot, sharing that slot's listening socket. `0` — the default
    /// — means one, which is what every configuration written before this key meant.
    ///
    /// **Only meaningful with `binary`.** With `command` the number is
    /// [`SpawnCgiConfig::concurrency`], because a prefork count is `php-cgi`'s own mechanism and
    /// exporting `PHP_FCGI_CHILDREN` to a Python process would be noise it cannot act on.
    /// Setting both is refused rather than silently preferring one.
    ///
    /// **This is `PHP_FCGI_CHILDREN`, and it is measured rather than asserted.** A
    /// `php-cgi -b` master forks this many children (or `CreateProcess`es them on
    /// Windows) *after* binding, and every one of them inherits the listening socket
    /// **and the OPcache segment**. Holding the PHP process count equal at 16, one
    /// master with 16 children measured **44 MB PSS against 311 MB for 16 separate
    /// slots**, with throughput within noise and slightly ahead — because the compiled
    /// scripts exist once instead of sixteen times.
    ///
    /// **Why this was `0` for so long, and why the reason no longer holds.** A slot
    /// that hides children cannot be counted, recycled or killed as a unit — that is a
    /// real problem and it is why the pool forced `PHP_FCGI_CHILDREN=0`. What changed
    /// is the measurement: the pool can hold a slot's concurrency (`children`) and a
    /// slot's address, so the children are no longer hidden from it. What has *not*
    /// changed is that `php-cgi` cannot multiplex — `FCGI_MPXS_CONNS` is `0`
    /// (`main/fastcgi.c:490`) and the accept loop is one request at a time per process
    /// (`sapi/cgi/cgi_main.c:2276`) — so prefork is the only way to get more than one
    /// concurrent request out of a slot, and this key is that way.
    ///
    /// **`max_requests` stays slot-wide.** Each child counts its own requests and the
    /// pool cannot see them, so the count this pool acts on is the slot's; the whole
    /// group is replaced together, which is the only unit `killpg` can express anyway.
    ///
    /// **`SIGKILL` to a master leaves its children running** — `fastcgi_cleanup`, the
    /// handler that signals the group, is installed for `SIGTERM`/`SIGINT`/`SIGQUIT`
    /// only (`cgi_main.c:2007`). That is not a reason to avoid this key: volucer kills
    /// by **process group**, not by pid, and `killpg` reaches every child. Measured:
    /// 16 children before, 0 after. The Linux master's own `setsid()` is a no-op here
    /// because `configure_command` has already made it a process-group leader, where
    /// `setsid()` fails with `EPERM` — its return value is ignored at `cgi_main.c:2001`.
    #[serde(default)]
    pub children: u32,

    /// How many requests one slot may serve **at once**. Only meaningful with `command`.
    ///
    /// **Per slot, not for the backend.** The backend's total is `workers × concurrency`;
    /// writing the total here would hold that many connections open to one address, where the
    /// surplus waits in the socket backlog and nothing counts it.
    ///
    /// The generic form of [`SpawnCgiConfig::children`], and it exists because the pool has to
    /// know the number for a reason that is not PHP's: [`SpawnCgiConfig::slot_concurrency`] is what
    /// admits concurrent requests to one address, and a slot with no prefork behind it can serve
    /// exactly one. An application server that does its own forking must not be handed more
    /// connections than it has children, or requests queue in the socket backlog where nothing
    /// counts them — which is the same argument `ExternalConfig::max_connections` makes about
    /// somebody else's pool.
    ///
    /// `0` means one, the honest answer for a program volucer did not fork.
    ///
    /// **This is the app server's own worker count, because `command` runs once per slot.**
    /// Each slot executes the command with a different `{address}`, so whatever the command is
    /// told to fork or thread belongs to *one* address — and that number is what a slot can
    /// answer at once. Gunicorn's `--workers 4` in the command is therefore `concurrency = 4`,
    /// not `workers`; the backend's process count is `workers × 4`, because the pool starts that
    /// whole command `workers` times. A server that also threads multiplies the two: four
    /// workers with four threads each is `concurrency = 16`.
    ///
    /// **A command may refer to it as [`FCGI_CONCURRENCY_PLACEHOLDER`] instead of repeating it**,
    /// which is the recommended form: `command = "gunicorn app.wsgi --workers {concurrency}
    /// --bind {address}"` states the number here and nowhere else. The substitution is the
    /// resolved value ([`SpawnCgiConfig::slot_concurrency`]), so `concurrency = 0` gives the
    /// program `1` rather than a zero it would read as "start no workers".
    ///
    /// Refused together with `children`: one of them describes a php-cgi prefork count and the
    /// other describes the same concurrency generically, and a file stating both says two
    /// different things about one number.
    #[serde(default)]
    pub concurrency: u32,

    /// Recycle a slot after this many requests, the equivalent of php-fpm's
    /// `pm.max_requests`. **`0` means never recycle**, as it does there.
    #[serde(default)]
    pub max_requests: Option<u64>,

    /// Working directory for each started process.
    ///
    /// The key `worker`, a job consumer and `upstream` already have, and its absence here was
    /// the one thing that made a non-PHP command unusable: a Python application finds its
    /// modules relative to the working directory, a Ruby one finds `config.ru` there, and a
    /// supervisor that guessed would start the process somewhere it cannot boot. Defaults to
    /// the process's own directory, the same rule the other three follow.
    ///
    /// **Not applied to a `binary` deployment by default**, so a configuration written before
    /// this key keeps the behaviour it had — `php-cgi` chdir's into the script's own directory
    /// per request and a fixed working directory would change that.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,

    /// Extra environment variables for every process this backend starts.
    ///
    /// The same key a `worker` and an `upstream` have, and it was missing from this kind for a
    /// reason that no longer holds: every other thing `spawn_cgi` starts a process for is
    /// declared here, so a deployment whose whole configuration is this backend had no way to
    /// set `APP_ENV` at all — the CGI environment volucer builds is the *request's*, and it
    /// carries no application configuration.
    ///
    /// Merged over [`ServerConfig::env`], so a value shared by several backends is written once.
    /// Written *after* `PHP_FCGI_CHILDREN`, `PHP_FCGI_MAX_REQUESTS` and
    /// [`FCGI_ADDRESS_ENV`] at spawn, so a table here cannot move either of the numbers the pool
    /// enforces or the address it is about to connect to.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// The pre-0.3 spelling of [`SpawnCgiConfig::concurrency`].
    ///
    /// Accepted by the parser and refused by [`SpawnCgiConfig::validate`] with a sentence naming
    /// the new key. Without it an upgrading operator reads `unknown field 'capacity'` from `toml`,
    /// which is true and explains nothing: the key did not disappear, it was renamed because
    /// `capacity` read like a total for the backend while the number is per slot.
    ///
    /// **Never serialized.** It exists to be read once and refused, and a shim that appeared in
    /// `config.describe` would tell a reader this deployment sets a key the server will not start
    /// with.
    #[serde(default, rename = "capacity", skip_serializing)]
    pub legacy_capacity: Option<u32>,
}

fn default_bind_host() -> String {
    "127.0.0.1".to_string()
}

fn default_base_port() -> u16 {
    19_001
}

impl SpawnCgiConfig {
    /// How many requests this slot can serve at once.
    ///
    /// `children.max(1)` for a `binary` deployment and `concurrency.max(1)` for a `command` one,
    /// and the `max(1)` is the whole point: zero means "no prefork", which is one process and
    /// therefore one request. Written here rather than at the call site so the pool's concurrency
    /// and the number of processes the driver was told to start cannot be computed differently
    /// in two places.
    pub fn slot_concurrency(&self) -> usize {
        match self.driver() {
            SpawnDriver::PhpCgi => self.children.max(1) as usize,
            SpawnDriver::Command => self.concurrency.max(1) as usize,
        }
    }

    /// How many slots this backend starts, with `workers = 0` resolved.
    ///
    /// `0` means one per logical CPU, so anything that reports or multiplies the slot count has
    /// to resolve it rather than print the configured zero. Here rather than at each call site
    /// for the same reason as [`SpawnCgiConfig::slot_concurrency`]: `check` printing a different
    /// process count from the one `serve` starts is the failure this prevents.
    pub fn slot_count(&self) -> u64 {
        if self.workers > 0 {
            return self.workers as u64;
        }
        std::thread::available_parallelism()
            .map(|count| count.get() as u64)
            .unwrap_or(1)
    }

    /// How many OS processes this backend will run, as arithmetic a reader can check.
    ///
    /// **The number an operator cannot get from the file**, which is why it is a method rather
    /// than left to prose: a `command` slot runs its program once and that program forks
    /// `concurrency` children of its own, so `workers × concurrency` is the host total; a
    /// `php-cgi` slot is `workers × (children + 1)` because php-cgi's children come *in
    /// addition to* the master. Both multiply the same way but differ by one factor, and the
    /// two are one line apart in the configuration.
    ///
    /// Returns the process count and the multiplication in the configuration's own key names, so
    /// `check` prints `16 (workers 4 × concurrency 4)` rather than a bare total an operator has to
    /// reverse. The keys are named because naming them is what makes the line teach the rule:
    /// `concurrency` is per slot, and the product is the host.
    pub fn process_count(&self) -> (u64, String) {
        let slots = self.slot_count();
        match self.driver() {
            // The master always runs, so the factor is `children + 1`. `children` is printed as
            // configured rather than as `fcgi_children()`, because it is the key the operator
            // wrote and the `+1` is what the arithmetic accounts for.
            SpawnDriver::PhpCgi => {
                let factor = self.children as u64 + 1;
                (
                    slots * factor,
                    format!("workers {slots} × (children {}+1)", self.children),
                )
            }
            SpawnDriver::Command => {
                let factor = self.slot_concurrency() as u64;
                (
                    slots * factor,
                    format!("workers {slots} × concurrency {factor}"),
                )
            }
        }
    }

    /// Which of the two spawn shapes this backend is.
    ///
    /// Infallible because `validate` has already refused a table that names neither or both, and
    /// a `Result` here would put an unwrap in the request path for a condition checked at
    /// startup. A table constructed by hand in a test names one or the other; a table that names
    /// neither is read as `Command` with no command, which `validate` refuses and which
    /// [`SpawnCgiConfig::command_line`] reports as the misconfiguration it is.
    pub fn driver(&self) -> SpawnDriver {
        if self.binary.is_some() {
            SpawnDriver::PhpCgi
        } else {
            SpawnDriver::Command
        }
    }

    /// True when the `binary` driver is in use, which is the shape that exports
    /// `PHP_FCGI_CHILDREN` and whose Windows `socket` is a named pipe.
    pub fn is_php_cgi(&self) -> bool {
        self.driver() == SpawnDriver::PhpCgi
    }

    /// The value to write to `PHP_FCGI_CHILDREN` for each slot.
    ///
    /// **`children <= 1` is `0`**, and that is not the same thing as `children`. php-cgi
    /// spawns CHILDREN *children in addition to itself*, so `1` is a master plus one child —
    /// two processes and two PHP boots for a slot that can still only serve one request at a
    /// time, because one child accepts one connection. Measured: `1` gave two processes and
    /// one pid answering. Writing `0` instead gives the one process the concurrency asks for,
    /// and keeps `children = 1` a truthful alias for the default rather than a setting that
    /// silently costs double.
    pub fn fcgi_children(&self) -> u32 {
        if self.children <= 1 {
            0
        } else {
            self.children
        }
    }

    /// Refuse a table whose values cannot mean what they look like.
    ///
    /// Split from `Config::validate` only to keep that function readable; the messages name the
    /// keys exactly as the file spells them.
    pub fn validate(&self, backend: &str) -> std::result::Result<(), String> {
        // The rename is refused by name and with the new spelling, because `deny_unknown_fields`
        // would otherwise report `capacity` as unknown — true, and useless to an operator whose
        // file said exactly what the key used to be called.
        if self.legacy_capacity.is_some() {
            return Err(format!(
                "backends.{backend}.capacity was renamed to `concurrency` in 0.3.0, because the \
                 number is requests per slot and not a total for the backend (that is `workers` × \
                 `concurrency`); write `concurrency = <n>` instead"
            ));
        }

        match (&self.binary, &self.command) {
            (None, None) => {
                return Err(format!(
                    "backends.{backend} names neither `binary` nor `command`; set `binary` to a \
                     php-cgi executable, or `command` to a program that serves FastCGI on \
                     `{{address}}` (e.g. \"gunicorn app.wsgi --bind {{address}}\")"
                ))
            }
            (Some(_), Some(_)) => {
                return Err(format!(
                    "backends.{backend} sets both `binary` and `command`; they are two ways to \
                     start the same slot — `binary` is php-cgi's own argv (`-b`) plus its prefork \
                     environment, `command` is any FastCGI server — so keep the one that \
                     describes this deployment"
                ))
            }
            (None, Some(command)) if command.trim().is_empty() => {
                return Err(format!(
                    "backends.{backend}.command is empty; name the program that serves this slot, \
                     with `{{address}}` where it expects the address to bind"
                ))
            }
            _ => {}
        }

        // `children` and `concurrency` describe one number — how many requests one address
        // serves at once — for the two drivers respectively. A file that sets both has said two
        // different things about it, and silently picking one is how a pool ends up admitting
        // more than the server behind it can answer.
        if !self.is_php_cgi() && self.children > 0 {
            return Err(format!(
                "backends.{backend}.children is php-cgi's prefork count and has no meaning with \
                 `command`; use `concurrency` for the number of requests one slot may serve at once"
            ));
        }
        if self.is_php_cgi() && self.concurrency > 0 {
            return Err(format!(
                "backends.{backend}.concurrency is the generic form of `children`; with `binary` \
                 the prefork count is `children`, which is also what `PHP_FCGI_CHILDREN` receives \
                 — set that instead"
            ));
        }

        // `{workers}` has no meaning here, and passing it through would be the worst of both:
        // the command starts, the program's argument parser rejects `{workers}` (or worse, reads
        // it as a name), and the message names neither volucer nor the key. A slot is started
        // once per slot with its own address, so it has no sibling count to be told.
        if let Some(command) = &self.command {
            if command.contains(WORKERS_PLACEHOLDER) {
                return Err(format!(
                    "backends.{backend}.command uses {WORKERS_PLACEHOLDER}, which does not exist: \
                     each slot runs this command once, with its own address, so a slot has no \
                     sibling count — `workers` is how many copies of this command volucer starts, \
                     and the parallelism of one of them is `{FCGI_CONCURRENCY_PLACEHOLDER}` (write \
                     `--workers {FCGI_CONCURRENCY_PLACEHOLDER}`, or set `concurrency` and name the \
                     number in the command once)"
                ));
            }
        }

        // `{concurrency}` needs no refusal of its own for `binary`, because a `binary` table
        // cannot reach here with a `command` at all: the `(Some, Some)` arm above already
        // refuses naming both. Writing one would be dead code that looks like protection.

        crate::env::check_env_table(&format!("backends.{backend}.env"), &self.env)?;

        if let Some(socket) = &self.socket {
            // A pipe is `php-cgi -b`'s Windows behaviour, not a property of the pool: a generic
            // command is given one string and has to understand it, and no other FastCGI server
            // binds a named pipe. Refused rather than started and left unreachable.
            #[cfg(windows)]
            if !self.is_php_cgi() && socket.starts_with(r"\\.\pipe\") {
                return Err(format!(
                    "backends.{backend}.socket is a named pipe, which only `php-cgi -b` binds; a \
                     `command` deployment must use loopback TCP — leave `socket` unset"
                ));
            }
            validate_cgi_socket(socket)
                .map_err(|err| format!("backends.{backend}.socket: {err}"))?;
        }

        if self.bind_host != "127.0.0.1" && self.bind_host != "::1" {
            return Err(format!(
                "bind_host {:?} is not loopback; the FastCGI backend must never be exposed",
                self.bind_host
            ));
        }

        // A typo here is expensive in a way no other key is: `children = 16000`
        // would ask php-cgi to fork sixteen thousand processes before the pool
        // accepted its first request, and the failure would be a machine that has
        // stopped responding rather than a message. The ceiling is well above any
        // sane deployment — a slot is meant to be roughly core-sized — and low
        // enough that a decimal-point mistake is a refusal.
        const MAX_PER_SLOT: u32 = 1024;
        if self.children > MAX_PER_SLOT || self.concurrency > MAX_PER_SLOT {
            return Err(format!(
                "backends.{backend} asks for {} processes behind one slot, above the {MAX_PER_SLOT} \
                 this pool will start per slot; that is almost certainly a typo — `workers` is how \
                 many slots, `children`/`concurrency` is how many serve each one",
                self.children.max(self.concurrency)
            ));
        }

        Ok(())
    }
}

/// Which of the two ways a `spawn_cgi` slot is started.
///
/// A value rather than a `bool` because three decisions turn on it — the argv, whether
/// `PHP_FCGI_*` is exported, and whether a Windows `socket` is a pipe — and a boolean would have
/// to be reinterpreted at each of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnDriver {
    /// `php-cgi` with `-b`, `PHP_FCGI_CHILDREN` and the pipe behaviour.
    PhpCgi,
    /// A command with `{address}` substituted, and [`FCGI_ADDRESS_ENV`] exported.
    Command,
}

impl SpawnDriver {
    /// The name this is written as, for `check` and for messages.
    pub fn as_str(self) -> &'static str {
        match self {
            SpawnDriver::PhpCgi => "php-cgi",
            SpawnDriver::Command => "command",
        }
    }
}
