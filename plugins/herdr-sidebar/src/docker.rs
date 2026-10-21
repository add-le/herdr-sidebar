//! Docker model: parse `docker ps` output and group containers by compose project.

use std::collections::BTreeMap;

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

#[cfg(test)]
mod tests {
    use super::*;

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
