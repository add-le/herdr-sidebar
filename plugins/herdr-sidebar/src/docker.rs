//! Docker model: parse `docker ps` output and group containers by compose project.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long `docker ps` may take before the list attempt is abandoned.
const LIST_DEADLINE: Duration = Duration::from_secs(10);
/// How long start/stop/restart may take (a stop waits for the container's
/// own shutdown grace period).
const RUN_DEADLINE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContainerState {
    Running,
    Paused,
    Restarting,
    Exited,
    Created,
    Dead,
    Other,
}

impl ContainerState {
    fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "paused" => Self::Paused,
            "restarting" => Self::Restarting,
            "exited" => Self::Exited,
            "created" => Self::Created,
            "dead" => Self::Dead,
            _ => Self::Other,
        }
    }

    /// Paused and restarting containers still hold a live process slot.
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Paused | Self::Restarting)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: ContainerState,
    pub status: String,
    pub project: Option<String>,
}

/// `docker ps -a --format` template matching [`parse_ps`].
pub const PS_FORMAT: &str = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.State}}\t{{.Status}}\t{{.Label \"com.docker.compose.project\"}}";

/// One container per non-empty line; lines with fewer than 5 fields are skipped.
pub fn parse_ps(output: &str) -> Vec<Container> {
    output
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 {
                return None;
            }
            let project = f.get(5).map(|p| p.trim()).filter(|p| !p.is_empty());
            Some(Container {
                id: f[0].to_string(),
                name: f[1].to_string(),
                image: f[2].to_string(),
                state: ContainerState::parse(f[3]),
                status: f[4].to_string(),
                project: project.map(str::to_string),
            })
        })
        .collect()
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Group {
    pub project: Option<String>,
    pub containers: Vec<Container>,
}

impl Group {
    pub fn running(&self) -> usize {
        self.containers
            .iter()
            .filter(|c| c.state.is_running())
            .count()
    }

    /// Stable identity: the project name, or `""` for individual containers.
    pub fn key(&self) -> &str {
        self.project.as_deref().unwrap_or("")
    }

    pub fn label(&self) -> &str {
        self.project.as_deref().unwrap_or("Individual containers")
    }
}

/// Projects sorted by name, the individual group last (omitted when empty);
/// inside a group running containers first, then by name.
pub fn group(containers: Vec<Container>) -> Vec<Group> {
    let mut projects: BTreeMap<String, Vec<Container>> = BTreeMap::new();
    let mut individual = Vec::new();
    for c in containers {
        match c.project.clone() {
            Some(p) => projects.entry(p).or_default().push(c),
            None => individual.push(c),
        }
    }
    let mut groups: Vec<Group> = projects
        .into_iter()
        .map(|(p, containers)| Group {
            project: Some(p),
            containers,
        })
        .collect();
    if !individual.is_empty() {
        groups.push(Group {
            project: None,
            containers: individual,
        });
    }
    for g in &mut groups {
        g.containers.sort_by(|a, b| {
            (!a.state.is_running(), &a.name).cmp(&(!b.state.is_running(), &b.name))
        });
    }
    groups
}

/// Drops any `@digest`, then keeps the last `/` segment (registry and namespace vanish).
pub fn short_image(image: &str) -> String {
    let no_digest = image.split('@').next().unwrap_or(image);
    no_digest
        .rsplit('/')
        .next()
        .unwrap_or(no_digest)
        .to_string()
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DockerError {
    NotInstalled,
    DaemonDown,
    Failed(String),
}

/// Map a failed docker command's stderr to a [`DockerError`].
pub fn classify_failure(stderr: &str) -> DockerError {
    let lower = stderr.to_lowercase();
    if [
        "cannot connect to the docker daemon",
        "is the docker daemon running",
        "error during connect",
        "failed to connect to the docker api",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return DockerError::DaemonDown;
    }
    let first = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    DockerError::Failed(first.to_string())
}

/// First `docker` on `path_var`, then well-known install locations.
pub fn docker_bin_in(
    path_var: Option<&str>,
    home: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let from_path = path_var
        .into_iter()
        .flat_map(|p| p.split(':'))
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join("docker"));
    let fallbacks = home
        .map(|h| h.join(".docker/bin/docker"))
        .into_iter()
        .chain([
            PathBuf::from("/usr/local/bin/docker"),
            PathBuf::from("/opt/homebrew/bin/docker"),
        ]);
    from_path.chain(fallbacks).find(|p| exists(p))
}

pub fn docker_bin() -> Option<PathBuf> {
    let path = std::env::var("PATH").ok();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    docker_bin_in(path.as_deref(), home.as_deref(), Path::is_file)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Start,
    Stop,
    Restart,
}

impl Action {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Stop => "Stop",
            Self::Restart => "Restart",
        }
    }
}

pub fn action_args(action: Action, ids: &[String]) -> Vec<String> {
    std::iter::once(action.verb().to_string())
        .chain(ids.iter().cloned())
        .collect()
}

fn exec(args: &[String], deadline: Duration) -> Result<String, DockerError> {
    let bin = docker_bin().ok_or(DockerError::NotInstalled)?;
    let mut cmd = Command::new(bin);
    cmd.args(args);
    let out = output_within(cmd, deadline)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(classify_failure(&String::from_utf8_lossy(&out.stderr)))
    }
}

/// `Command::output`, but a child still running at `deadline` is killed and
/// reaped, and reported as `Failed`: a wedged daemon must not hold the
/// caller's worker (and, with it, every later refresh) forever. Blocks; call
/// from a worker thread. The pipes are drained on their own threads so a
/// chatty child can't stall on a full pipe buffer.
fn output_within(mut cmd: Command, deadline: Duration) -> Result<Output, DockerError> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| DockerError::Failed(e.to_string()))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DockerError::Failed(format!(
                    "docker didn't answer within {} s",
                    deadline.as_secs_f32()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(DockerError::Failed(e.to_string())),
        }
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// All containers (running or not). Blocks; call from a worker thread.
pub fn list() -> Result<Vec<Container>, DockerError> {
    let args = ["ps", "-a", "--format", PS_FORMAT].map(String::from);
    exec(&args, LIST_DEADLINE).map(|out| parse_ps(&out))
}

/// Start/stop/restart containers. Blocks; call from a worker thread.
pub fn run(action: Action, ids: &[String]) -> Result<(), DockerError> {
    exec(&action_args(action, ids), RUN_DEADLINE).map(|_| ())
}

/// Launch Docker Desktop without waiting for it to finish starting.
pub fn start_desktop() -> std::io::Result<()> {
    Command::new("open")
        .args(["-a", "Docker"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|mut child| {
            // Reap the child off-thread so it doesn't linger as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_command_past_its_deadline_is_killed_and_reported() {
        let started = std::time::Instant::now();
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("5");
        let result = output_within(cmd, Duration::from_millis(200));
        assert_eq!(
            result.map(|o| o.status.success()),
            Err(DockerError::Failed(
                "docker didn't answer within 0.2 s".into()
            ))
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "killed at the deadline, not waited for"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_fast_command_finishes_within_its_deadline() {
        let out = output_within(Command::new("/usr/bin/true"), Duration::from_secs(5))
            .expect("true runs");
        assert!(out.status.success());
    }

    #[test]
    fn daemon_errors_are_classified_as_daemon_down() {
        for s in [
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
            "error during connect: Get \"http://%2Fvar%2Frun%2Fdocker.sock/v1.47/containers/json\": EOF",
            "failed to connect to the docker API at unix:///Users/me/.docker/run/docker.sock; check if the path is correct and if the daemon is running: dial unix /Users/me/.docker/run/docker.sock: connect: no such file or directory",
        ] {
            assert!(
                matches!(classify_failure(s), DockerError::DaemonDown),
                "{s}"
            );
        }
        assert!(matches!(
            classify_failure("\nError response from daemon: No such container: x\n"),
            DockerError::Failed(m) if m == "Error response from daemon: No such container: x"
        ));
    }

    #[test]
    fn docker_bin_falls_back_to_known_locations() {
        let home = Path::new("/Users/me");
        let only = |want: &'static str| move |p: &Path| p == Path::new(want);
        assert_eq!(
            docker_bin_in(
                Some("/usr/bin:/bin"),
                Some(home),
                only("/Users/me/.docker/bin/docker")
            ),
            Some(PathBuf::from("/Users/me/.docker/bin/docker"))
        );
        assert_eq!(
            docker_bin_in(Some("/x:/y"), Some(home), only("/y/docker")),
            Some(PathBuf::from("/y/docker"))
        );
        assert_eq!(docker_bin_in(None, None, |_| false), None);
    }

    #[test]
    fn action_args_put_the_verb_first() {
        let ids = vec!["a1".to_string(), "c3".to_string()];
        assert_eq!(action_args(Action::Stop, &ids), ["stop", "a1", "c3"]);
    }

    const PS: &str = "a1\tda-mongo\tmongo:7.0.43\texited\tExited (255) 4 weeks ago\ts3pweb-border-engine\n\
b2\ts3pweb-redis-stack\tredis/redis-stack:latest\trunning\tUp 50 minutes\t\n\
c3\tda-kafka\tapache/kafka:3.7.0\texited\tExited (255) 4 weeks ago\ts3pweb-border-engine\n\
d4\tda-redis\tredis/redis-stack-server:7.4.0-v3\trunning\tUp 2 minutes\ts3pweb-border-engine\n";

    #[test]
    fn parse_ps_reads_every_field() {
        let c = parse_ps(PS);
        assert_eq!(c.len(), 4);
        assert_eq!(c[0].name, "da-mongo");
        assert_eq!(c[0].state, ContainerState::Exited);
        assert_eq!(c[0].project.as_deref(), Some("s3pweb-border-engine"));
        assert_eq!(c[1].project, None);
        assert!(c[1].state.is_running());
    }

    #[test]
    fn parse_ps_skips_blank_and_short_lines() {
        assert!(parse_ps("\n\nonly\ttwo\n").is_empty());
    }

    #[test]
    fn group_puts_projects_first_and_running_first_inside() {
        let g = group(parse_ps(PS));
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].label(), "s3pweb-border-engine");
        assert_eq!(g[0].running(), 1);
        assert_eq!(g[0].containers.len(), 3);
        assert_eq!(g[0].containers[0].name, "da-redis"); // the running one
        assert_eq!(g[0].containers[1].name, "da-kafka"); // then by name
        assert_eq!(g[1].label(), "Individual containers");
        assert_eq!(g[1].key(), "");
    }

    #[test]
    fn short_image_drops_registry_and_namespace() {
        assert_eq!(short_image("apache/kafka:3.7.0"), "kafka:3.7.0");
        assert_eq!(
            short_image("docker.io/redis/redis-stack-server:7.4.0-v3"),
            "redis-stack-server:7.4.0-v3"
        );
        assert_eq!(short_image("localhost:5000/app:dev"), "app:dev");
        assert_eq!(short_image("mongo@sha256:0123456789abcdef"), "mongo");
        assert_eq!(short_image("mongo"), "mongo");
    }
}
