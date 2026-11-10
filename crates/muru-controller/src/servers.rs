//! Managed HTTP server releases (design specification section 9.9): the
//! instances of a release that runs an ordinary HTTP server, which the
//! daemon starts, probes, supervises and stops, and which the edge
//! reaches as an upstream named after the release.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use muru_config::{ServerBind, ServerProfile};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpStream, UnixStream};
use tokio::time::Instant;

use crate::host::Identity;
use crate::model::HttpServerSpec;
use crate::supervisor::{self, EventNames, InstanceSpec, Report, RestartPolicy, Supervisor};

/// How long one readiness probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the instances are probed while they start.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// How long the instances get to be ready when the release says nothing.
pub const READINESS_TIMEOUT: Duration = Duration::from_secs(60);

/// The events the instances of a managed server are reported under.
const EVENTS: EventNames = EventNames {
    exited: "server.exited",
    started: "server.started",
    failed: "server.failed",
};

/// Where one instance listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    /// A Unix socket.
    Unix(PathBuf),
    /// A loopback port.
    Port(u16),
}

impl Bind {
    /// The address as an upstream names it: `unix:/path` or
    /// `127.0.0.1:port`.
    pub fn address(&self) -> String {
        match self {
            Self::Unix(path) => format!("unix:{}", path.display()),
            Self::Port(port) => format!("127.0.0.1:{port}"),
        }
    }

    /// The address parsed back from a record.
    fn parse(text: &str) -> Option<Self> {
        if let Some(path) = text.strip_prefix("unix:") {
            return Some(Self::Unix(PathBuf::from(path)));
        }

        text.strip_prefix("127.0.0.1:")
            .and_then(|port| port.parse().ok())
            .map(Self::Port)
    }
}

/// What a previous daemon recorded about the instances, in the
/// release's run directory.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ServersRecord {
    /// Each instance's pid and address.
    pub instances: Vec<InstanceRecord>,
}

/// One instance as recorded: what the supervisor records of its process,
/// and its address.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct InstanceRecord {
    /// The instance's index, from 0.
    pub index: u32,
    /// Its pid; 0 while it waits to be started again.
    pub pid: u32,
    /// Where it listens, as an upstream names it.
    pub address: String,
    /// When its process was started, Unix milliseconds; 0 when not known.
    #[serde(default)]
    pub started_unix_ms: u64,
    /// Its exits and failed starts in a row.
    #[serde(default)]
    pub quick_exits: u32,
    /// The start time the kernel gives its process, which with the pid
    /// names the process; 0 when not known.
    #[serde(default)]
    pub start_time: u64,
}

impl InstanceRecord {
    /// The part of the record that is the supervisor's.
    fn process(&self) -> supervisor::InstanceRecord {
        supervisor::InstanceRecord {
            index: self.index,
            pid: self.pid,
            started_unix_ms: self.started_unix_ms,
            quick_exits: self.quick_exits,
            start_time: self.start_time,
        }
    }
}

/// The instances of one managed HTTP release: their processes, which a
/// supervisor runs, and where each listens.
pub struct Servers {
    readiness_path: Option<String>,
    run_dir: PathBuf,
    /// Each instance's address, in the supervisor's order.
    binds: Vec<Bind>,
    supervisor: Supervisor,
}

impl Servers {
    /// Start every instance under the run directory, each with its own
    /// address, running `spec.command` in `cwd` with `env`, as
    /// `identity` when one is set, its output appended to `log`.
    pub fn start(
        spec: &HttpServerSpec,
        run_dir: &Path,
        cwd: &Path,
        env: Vec<(String, String)>,
        identity: Option<Identity>,
        log: &Path,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(run_dir)
            .with_context(|| format!("creating {}", run_dir.display()))?;
        let binds = (0..spec.instances.max(1))
            .map(|index| allocate(spec.bind, run_dir, index))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let instances = (0..)
            .zip(&binds)
            .map(|(index, bind)| instance_spec(spec, index, bind))
            .collect();
        let supervisor = Supervisor::start(
            process_spec(spec, cwd, env, identity, log),
            RestartPolicy::default(),
            instances,
        )?;

        Ok(Self {
            readiness_path: spec.readiness_path.clone(),
            run_dir: run_dir.to_path_buf(),
            binds,
            supervisor,
        })
    }

    /// Take over the instances a previous daemon recorded, each one
    /// that is still there, with those that are not due to be started
    /// by the next `check`. Only a record with no instances, or with an
    /// address that cannot be read, is refused.
    ///
    /// An instance is there when the process with its pid has the start
    /// time it was recorded with, which the supervisor checks. A record
    /// with no start time for an instance, as one written before the
    /// start time was kept, leaves only the pid, which any process may
    /// have by now. Such an instance is taken over only if something
    /// answers HTTP at its address as well, and is otherwise started
    /// again, with the process that has its pid left alone.
    #[allow(clippy::too_many_arguments)]
    pub async fn adopt(
        spec: &HttpServerSpec,
        run_dir: &Path,
        cwd: &Path,
        env: Vec<(String, String)>,
        identity: Option<Identity>,
        log: &Path,
        record: &ServersRecord,
    ) -> anyhow::Result<Self> {
        let mut binds = Vec::new();

        for instance in &record.instances {
            let bind = Bind::parse(&instance.address).with_context(|| {
                format!(
                    "instance {}: {:?} is not an address",
                    instance.index, instance.address
                )
            })?;
            binds.push(bind);
        }

        let instances = record
            .instances
            .iter()
            .zip(&binds)
            .map(|(instance, bind)| instance_spec(spec, instance.index, bind))
            .collect();
        let mut processes = supervisor::Record {
            instances: record
                .instances
                .iter()
                .map(InstanceRecord::process)
                .collect(),
        };

        for (process, bind) in processes.instances.iter_mut().zip(&binds) {
            let unproven = process.pid != 0 && process.start_time == 0;

            if unproven
                && answer(bind, spec.readiness_path.as_deref(), PROBE_TIMEOUT)
                    .await
                    .is_none()
            {
                process.pid = 0;
            }
        }

        let supervisor = Supervisor::adopt(
            process_spec(spec, cwd, env, identity, log),
            RestartPolicy::default(),
            instances,
            &processes,
        )?;

        Ok(Self {
            readiness_path: spec.readiness_path.clone(),
            run_dir: run_dir.to_path_buf(),
            binds,
            supervisor,
        })
    }

    /// Wait until every instance answers its readiness probe, or one
    /// exits, or the time is up.
    pub async fn wait_ready(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut ready = vec![false; self.binds.len()];
        let path = self.readiness_path.clone();
        let indexes: Vec<u32> = self
            .supervisor
            .instances()
            .map(supervisor::Instance::index)
            .collect();

        loop {
            for (position, bind) in self.binds.iter().enumerate() {
                if ready[position] {
                    continue;
                }

                if let Some(exit) = self.supervisor.try_exited(position)? {
                    anyhow::bail!("instance {} {exit} before it was ready", indexes[position]);
                }

                if probe(bind, path.as_deref(), PROBE_TIMEOUT).await {
                    ready[position] = true;
                }
            }

            if ready.iter().all(|ready| *ready) {
                return Ok(());
            }

            if Instant::now() >= deadline {
                let waiting = indexes
                    .iter()
                    .zip(&ready)
                    .find(|(_, ready)| !**ready)
                    .map(|(index, _)| *index)
                    .unwrap_or(0);
                anyhow::bail!(
                    "instance {waiting} was not ready within {} s{}",
                    timeout.as_secs(),
                    path.as_deref()
                        .map(|path| format!(": {path} did not answer 2xx or 3xx"))
                        .unwrap_or_default()
                );
            }

            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    }

    /// Reap instances that exited and start again those whose wait has
    /// passed, with what happened for the event stream.
    pub fn check(&mut self) -> Vec<Report> {
        self.supervisor.check()
    }

    /// Every instance's address, as the snapshot names them.
    pub fn addresses(&self) -> Vec<String> {
        self.binds.iter().map(Bind::address).collect()
    }

    /// How many instances have a process right now.
    pub fn running(&self) -> usize {
        self.supervisor.running()
    }

    /// The instances as a record for the next daemon.
    pub fn record(&self) -> ServersRecord {
        ServersRecord {
            instances: self
                .supervisor
                .record()
                .instances
                .into_iter()
                .zip(&self.binds)
                .map(|(process, bind)| InstanceRecord {
                    index: process.index,
                    pid: process.pid,
                    address: bind.address(),
                    started_unix_ms: process.started_unix_ms,
                    quick_exits: process.quick_exits,
                    start_time: process.start_time,
                })
                .collect(),
        }
    }

    /// Stop every instance: the graceful signal, then SIGKILL for one
    /// still there after `grace`, and the sockets and run directory
    /// removed.
    pub async fn stop(self, grace: Duration) {
        self.stop_until(std::time::Instant::now() + grace).await;
    }

    /// Stop the instances as `stop` does, killing what is still there at
    /// `kill_at`.
    pub async fn stop_until(self, kill_at: std::time::Instant) {
        self.supervisor.stop_until(Instant::from_std(kill_at)).await;
        let _ = std::fs::remove_dir_all(&self.run_dir);
    }

    /// Leave the instances running and forget them, for a daemon that
    /// detaches.
    pub fn release(self) {
        self.supervisor.release();
    }
}

/// A socket path or a free loopback port for an instance.
fn allocate(bind: ServerBind, run_dir: &Path, index: u32) -> anyhow::Result<Bind> {
    match bind {
        ServerBind::Unix => Ok(Bind::Unix(run_dir.join(format!("server-{index}.sock")))),
        ServerBind::Port => {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
                .context("finding a free loopback port")?;
            Ok(Bind::Port(listener.local_addr()?.port()))
        }
    }
}

/// What the supervisor needs to know of every instance of a release.
fn process_spec(
    spec: &HttpServerSpec,
    cwd: &Path,
    env: Vec<(String, String)>,
    identity: Option<Identity>,
    log: &Path,
) -> supervisor::Spec {
    supervisor::Spec {
        cwd: cwd.to_path_buf(),
        env,
        identity,
        log: log.to_path_buf(),
        stop_signal: stop_signal(spec),
        stop_group: false,
        events: EVENTS,
    }
}

/// One instance for the supervisor: the command with the instance's
/// address filled in, the variables that tell the server where to
/// listen, and the socket file a crashed instance leaves behind.
fn instance_spec(spec: &HttpServerSpec, index: u32, bind: &Bind) -> InstanceSpec {
    let mut env = vec![
        ("MURU_HTTP_BIND".to_owned(), bind.address()),
        ("MURU_INSTANCE".to_owned(), index.to_string()),
    ];

    let stale = match bind {
        Bind::Unix(path) => vec![path.clone()],
        Bind::Port(port) => {
            env.push(("PORT".to_owned(), port.to_string()));
            Vec::new()
        }
    };

    InstanceSpec {
        command: arguments_for(spec.profile, &spec.command, bind),
        env,
        stale,
    }
}

/// The signal that stops an instance gracefully: the profile's, or the
/// one the release names.
fn stop_signal(spec: &HttpServerSpec) -> i32 {
    let name = spec.stop_signal.as_deref().unwrap_or(match spec.profile {
        ServerProfile::Gunicorn
        | ServerProfile::Uvicorn
        | ServerProfile::Granian
        | ServerProfile::ModWsgiExpress
        | ServerProfile::Uwsgi
        | ServerProfile::Generic => "TERM",
    });

    match name {
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "HUP" => libc::SIGHUP,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "WINCH" => libc::SIGWINCH,
        _ => libc::SIGTERM,
    }
}

/// The command with its address filled in: `{bind}` becomes the
/// address in the server's own form, `{socket}` the socket path and
/// `{port}` the port, and a command that names none of them gets the
/// profile's own flags appended.
fn arguments_for(profile: ServerProfile, command: &[String], bind: &Bind) -> Vec<String> {
    let address = match (profile, bind) {
        (
            ServerProfile::Uvicorn | ServerProfile::Granian | ServerProfile::Uwsgi,
            Bind::Unix(path),
        ) => path.display().to_string(),
        (_, bind) => bind.address(),
    };

    let socket = match bind {
        Bind::Unix(path) => path.display().to_string(),
        Bind::Port(_) => String::new(),
    };

    let port = match bind {
        Bind::Port(port) => port.to_string(),
        Bind::Unix(_) => String::new(),
    };

    let mentioned = command.iter().any(|argument| {
        argument.contains("{bind}") || argument.contains("{socket}") || argument.contains("{port}")
    });
    let mut arguments: Vec<String> = command
        .iter()
        .map(|argument| {
            argument
                .replace("{bind}", &address)
                .replace("{socket}", &socket)
                .replace("{port}", &port)
        })
        .collect();

    // uWSGI 2.0 takes TERM as a brutal reload and drops the requests in
    // flight, so its profile has a master whose hook makes TERM a
    // graceful stop instead, whatever the command binds with. Both flags
    // are harmless given twice.

    if profile == ServerProfile::Uwsgi {
        arguments.extend([
            "--master".to_owned(),
            "--hook-master-start".to_owned(),
            "unix_signal:15 gracefully_kill_them_all".to_owned(),
        ]);
    }

    if mentioned {
        return arguments;
    }

    match (profile, bind) {
        (ServerProfile::Gunicorn, _) => arguments.extend(["--bind".to_owned(), address]),
        (ServerProfile::Uwsgi, _) => arguments.extend(["--http-socket".to_owned(), address]),
        (ServerProfile::Uvicorn | ServerProfile::Granian, Bind::Unix(_)) => {
            arguments.extend(["--uds".to_owned(), socket]);
        }
        (
            ServerProfile::Uvicorn | ServerProfile::Granian | ServerProfile::ModWsgiExpress,
            Bind::Port(_),
        ) => {
            arguments.extend([
                "--host".to_owned(),
                "127.0.0.1".to_owned(),
                "--port".to_owned(),
                port,
            ]);
        }
        (ServerProfile::ModWsgiExpress, Bind::Unix(_)) | (ServerProfile::Generic, _) => {}
    }

    arguments
}

/// Whether an instance answers: with a path, a GET of it answered 2xx
/// or 3xx; without one, any HTTP response at all.
async fn probe(bind: &Bind, path: Option<&str>, timeout: Duration) -> bool {
    match answer(bind, path, timeout).await {
        Some(status) => path.is_none() || (200..400).contains(&status),
        None => false,
    }
}

/// The status an instance answers a GET of the path with, `/` when
/// there is none, or nothing when no HTTP response comes in time.
async fn answer(bind: &Bind, path: Option<&str>, timeout: Duration) -> Option<u16> {
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: localhost\r\nUser-Agent: muru-readiness\r\n\r\n",
        path.unwrap_or("/")
    );
    let answered = tokio::time::timeout(timeout, async {
        match bind {
            Bind::Unix(socket) => {
                let mut stream = UnixStream::connect(socket).await.ok()?;
                stream.write_all(request.as_bytes()).await.ok()?;
                status_of(&mut stream).await
            }
            Bind::Port(port) => {
                let mut stream = TcpStream::connect(("127.0.0.1", *port)).await.ok()?;
                stream.write_all(request.as_bytes()).await.ok()?;
                status_of(&mut stream).await
            }
        }
    })
    .await;

    answered.ok().flatten()
}

/// The status of the response a stream carries, from its status line.
async fn status_of<S: AsyncRead + Unpin>(stream: &mut S) -> Option<u16> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];

    while head.len() < 256 {
        if stream.read(&mut byte).await.ok()? == 0 {
            break;
        }

        if byte[0] == b'\n' {
            break;
        }

        head.push(byte[0]);
    }

    let line = String::from_utf8_lossy(&head);
    let mut words = line.split_whitespace();
    let version = words.next()?;

    if !version.starts_with("HTTP/") {
        return None;
    }

    words.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn placeholders_take_the_address_in_the_servers_form() {
        let unix = Bind::Unix(PathBuf::from("/run/s.sock"));
        let port = Bind::Port(8001);
        assert_eq!(
            arguments_for(
                ServerProfile::Gunicorn,
                &command(&["gunicorn", "--bind", "{bind}", "app:app"]),
                &unix
            ),
            command(&["gunicorn", "--bind", "unix:/run/s.sock", "app:app"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Uvicorn,
                &command(&["uvicorn", "app:app", "--uds", "{bind}"]),
                &unix
            ),
            command(&["uvicorn", "app:app", "--uds", "/run/s.sock"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Generic,
                &command(&["server", "--listen={bind}", "--port", "{port}"]),
                &port
            ),
            command(&["server", "--listen=127.0.0.1:8001", "--port", "8001"])
        );
    }

    #[test]
    fn a_command_without_placeholders_gets_the_profiles_flags() {
        let unix = Bind::Unix(PathBuf::from("/run/s.sock"));
        let port = Bind::Port(8001);
        assert_eq!(
            arguments_for(
                ServerProfile::Gunicorn,
                &command(&["gunicorn", "app:app"]),
                &port
            ),
            command(&["gunicorn", "app:app", "--bind", "127.0.0.1:8001"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Uvicorn,
                &command(&["uvicorn", "app:app"]),
                &unix
            ),
            command(&["uvicorn", "app:app", "--uds", "/run/s.sock"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Granian,
                &command(&["granian", "app:app"]),
                &port
            ),
            command(&[
                "granian",
                "app:app",
                "--host",
                "127.0.0.1",
                "--port",
                "8001"
            ])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::ModWsgiExpress,
                &command(&["mod_wsgi-express", "start-server", "app.wsgi"]),
                &port
            ),
            command(&[
                "mod_wsgi-express",
                "start-server",
                "app.wsgi",
                "--host",
                "127.0.0.1",
                "--port",
                "8001"
            ])
        );
        assert_eq!(
            arguments_for(ServerProfile::Generic, &command(&["server"]), &port),
            command(&["server"])
        );
    }

    #[test]
    fn uwsgi_binds_an_http_socket_and_stops_gracefully_on_term() {
        let unix = Bind::Unix(PathBuf::from("/run/s.sock"));
        let port = Bind::Port(8001);
        let graceful = [
            "--master",
            "--hook-master-start",
            "unix_signal:15 gracefully_kill_them_all",
        ];
        assert_eq!(
            arguments_for(
                ServerProfile::Uwsgi,
                &command(&["uwsgi", "--module", "app:application"]),
                &unix
            ),
            command(
                &[
                    &["uwsgi", "--module", "app:application"][..],
                    &graceful,
                    &["--http-socket", "/run/s.sock"],
                ]
                .concat()
            )
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Uwsgi,
                &command(&["uwsgi", "--http-socket", "{bind}", "app.ini"]),
                &port
            ),
            command(
                &[
                    &["uwsgi", "--http-socket", "127.0.0.1:8001", "app.ini"][..],
                    &graceful
                ]
                .concat()
            )
        );
    }

    #[test]
    fn addresses_round_trip_through_records() {
        for bind in [Bind::Unix(PathBuf::from("/run/s.sock")), Bind::Port(8001)] {
            assert_eq!(Bind::parse(&bind.address()), Some(bind));
        }

        assert_eq!(Bind::parse("10.0.0.1:80"), None);
    }

    #[tokio::test]
    async fn an_instance_recorded_with_no_start_time_is_adopted_only_if_its_address_answers() {
        // The record names a process that is running, a child of the
        // test, and has no start time for it, so the pid is all there
        // is to say the process is the instance.

        let dir = tempfile::tempdir().unwrap();
        let mut bystander = std::process::Command::new("/bin/sleep")
            .arg("600")
            .spawn()
            .unwrap();
        let spec = HttpServerSpec {
            profile: ServerProfile::Generic,
            command: command(&["/bin/sleep", "600"]),
            bind: ServerBind::Port,
            instances: 1,
            readiness_path: None,
            readiness_timeout_ms: 1000,
            stop_signal: None,
        };

        // Something answers HTTP at one address, and nothing listens at
        // the other.

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let answering = Bind::Port(listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await;
                let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n").await;
            }
        });
        let silent = Bind::Unix(dir.path().join("nothing.sock"));

        for (bind, adopted) in [(answering, true), (silent, false)] {
            let record = ServersRecord {
                instances: vec![InstanceRecord {
                    index: 0,
                    pid: bystander.id(),
                    address: bind.address(),
                    started_unix_ms: 0,
                    quick_exits: 0,
                    start_time: 0,
                }],
            };
            let servers = Servers::adopt(
                &spec,
                dir.path(),
                dir.path(),
                Vec::new(),
                None,
                &dir.path().join("output.log"),
                &record,
            )
            .await
            .unwrap();
            assert_eq!(
                servers.record().instances[0].pid,
                if adopted { bystander.id() } else { 0 }
            );
            assert_eq!(servers.running(), usize::from(adopted));
            servers.release();
        }

        // Neither adoption signalled the process.

        assert_eq!(bystander.try_wait().unwrap(), None);
        bystander.kill().unwrap();
        bystander.wait().unwrap();
    }

    #[tokio::test]
    async fn the_probe_reads_a_status_line() {
        let mut stream = std::io::Cursor::new(b"HTTP/1.1 503 Service Unavailable\r\n\r\n".to_vec());
        assert_eq!(status_of(&mut stream).await, Some(503));
        let mut stream = std::io::Cursor::new(b"nonsense\r\n".to_vec());
        assert_eq!(status_of(&mut stream).await, None);
    }
}
