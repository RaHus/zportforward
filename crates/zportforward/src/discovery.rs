//! Finding dev containers and reading their port configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::runtime::Runtime;

const LABEL_FOLDER: &str = "devcontainer.local_folder";
const LABEL_CONFIG: &str = "devcontainer.config_file";
const LABEL_METADATA: &str = "devcontainer.metadata";

#[derive(Debug, Clone)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub local_folder: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    metadata: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Inspect {
    id: String,
    name: String,
    config: InspectConfig,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectConfig {
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

impl From<Inspect> for Container {
    fn from(i: Inspect) -> Self {
        let labels = i.config.labels.unwrap_or_default();
        Container {
            id: i.id,
            name: i.name.trim_start_matches('/').to_string(),
            local_folder: labels.get(LABEL_FOLDER).map(PathBuf::from),
            config_file: labels.get(LABEL_CONFIG).map(PathBuf::from),
            metadata: labels.get(LABEL_METADATA).cloned(),
        }
    }
}

impl Container {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }

    pub fn matches(&self, reference: &str) -> bool {
        self.name == reference || (reference.len() >= 4 && self.id.starts_with(reference))
    }

    /// Port settings from `devcontainer.json` on disk, falling back to the
    /// metadata label (which is frozen at container creation and can be stale).
    pub fn port_config(&self) -> PortConfig {
        if let Some(text) = self.config_file.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            if let Ok(cfg) = PortConfig::from_devcontainer_json(&text) {
                return cfg;
            }
        }
        self.metadata.as_deref().map(PortConfig::from_metadata_label).unwrap_or_default()
    }
}

/// Running containers that carry the dev container label.
pub async fn list(rt: &Runtime) -> Result<Vec<Container>> {
    let ids = rt
        .output(&["ps", "-q", "--no-trunc", "--filter", &format!("label={LABEL_FOLDER}")])
        .await?;
    let ids: Vec<&str> = ids.split_whitespace().collect();
    inspect(rt, &ids).await
}

/// IDs of all dev containers, running or not, with whether each is running.
/// Cheap enough to poll.
pub async fn list_states(rt: &Runtime) -> Result<Vec<(String, bool)>> {
    let out = rt
        .output(&[
            "ps",
            "-a",
            "--no-trunc",
            "--filter",
            &format!("label={LABEL_FOLDER}"),
            "--format",
            "{{.ID}} {{.State}}",
        ])
        .await?;
    Ok(out
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(id, state)| (id.to_string(), state.trim().eq_ignore_ascii_case("running")))
        .collect())
}

pub async fn inspect(rt: &Runtime, refs: &[&str]) -> Result<Vec<Container>> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["inspect", "--type", "container"];
    args.extend_from_slice(refs);
    let json = rt.output(&args).await?;
    let parsed: Vec<Inspect> = serde_json::from_str(&json).context("unexpected inspect output")?;
    Ok(parsed.into_iter().map(Container::from).collect())
}

/// Pick the container a CLI call refers to: an explicit name/ID, else the dev
/// container whose project folder contains `cwd`, else the only one running.
pub fn select<'a>(
    containers: &'a [Container],
    reference: Option<&str>,
    cwd: Option<&Path>,
) -> Result<&'a Container> {
    if let Some(reference) = reference {
        let hits: Vec<&Container> = containers.iter().filter(|c| c.matches(reference)).collect();
        return match hits.as_slice() {
            [one] => Ok(one),
            [] => Err(anyhow!("no running dev container matches '{reference}'")),
            _ => Err(anyhow!("'{reference}' matches more than one container")),
        };
    }
    if let Some(cwd) = cwd {
        let best = containers
            .iter()
            .filter(|c| c.local_folder.as_deref().is_some_and(|f| cwd.starts_with(f)))
            .max_by_key(|c| c.local_folder.as_ref().map(|f| f.as_os_str().len()));
        if let Some(best) = best {
            return Ok(best);
        }
    }
    match containers {
        [one] => Ok(one),
        [] => bail!("no running dev containers found"),
        _ => bail!(
            "several dev containers are running; name the container (one of: {})",
            containers
                .iter()
                .map(|c| match &c.local_folder {
                    Some(folder) => format!("{} for {}", c.name, folder.display()),
                    None => c.name.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortAttrs {
    pub require_local_port: bool,
    pub ignore_auto_forward: bool,
}

/// A `forwardPorts` entry: a container-local port, or `host:port` reachable
/// from inside the container (for example another compose service).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Target {
    /// Empty for the container's own loopback.
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortConfig {
    pub forward_ports: Vec<Target>,
    /// (first port, last port, attributes, label)
    attrs: Vec<(u16, u16, PortAttrs, Option<String>)>,
    other: PortAttrs,
}

impl PortConfig {
    pub fn from_devcontainer_json(text: &str) -> Result<Self> {
        let value: Value = jsonc_parser::parse_to_serde_value(text, &Default::default())
            .map_err(|e| anyhow!("devcontainer.json: {e}"))?;
        let mut cfg = PortConfig::default();
        cfg.merge(&value);
        Ok(cfg)
    }

    /// The label holds a JSON array of config fragments (image, features, user config).
    fn from_metadata_label(label: &str) -> Self {
        let mut cfg = PortConfig::default();
        match serde_json::from_str::<Value>(label) {
            Ok(Value::Array(fragments)) => fragments.iter().for_each(|f| cfg.merge(f)),
            Ok(single) => cfg.merge(&single),
            Err(_) => {}
        }
        cfg
    }

    fn merge(&mut self, value: &Value) {
        for entry in value.get("forwardPorts").and_then(Value::as_array).into_iter().flatten() {
            if let Some(target) = parse_forward_port(entry) {
                if !self.forward_ports.contains(&target) {
                    self.forward_ports.push(target);
                }
            }
        }
        for (key, attrs) in value.get("portsAttributes").and_then(Value::as_object).into_iter().flatten() {
            if let Some((lo, hi)) = parse_port_range(key) {
                let label = attrs.get("label").and_then(Value::as_str).map(str::to_string);
                self.attrs.push((lo, hi, parse_attrs(attrs), label));
            }
        }
        if let Some(other) = value.get("otherPortsAttributes") {
            self.other = parse_attrs(other);
        }
    }

    pub fn attrs(&self, port: u16) -> PortAttrs {
        self.attrs
            .iter()
            .find(|(lo, hi, ..)| (*lo..=*hi).contains(&port))
            .map(|(_, _, attrs, _)| *attrs)
            .unwrap_or(self.other)
    }

    pub fn label(&self, port: u16) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(lo, hi, ..)| (*lo..=*hi).contains(&port))
            .and_then(|(.., label)| label.as_deref())
    }
}

fn parse_attrs(value: &Value) -> PortAttrs {
    PortAttrs {
        require_local_port: value.get("requireLocalPort").and_then(Value::as_bool).unwrap_or(false),
        ignore_auto_forward: value.get("onAutoForward").and_then(Value::as_str) == Some("ignore"),
    }
}

fn parse_forward_port(entry: &Value) -> Option<Target> {
    match entry {
        Value::Number(n) => Some(Target { host: String::new(), port: u16::try_from(n.as_u64()?).ok()? }),
        Value::String(s) => match s.rsplit_once(':') {
            Some((host, port)) => {
                let host = if matches!(host, "localhost" | "127.0.0.1") { "" } else { host };
                Some(Target { host: host.to_string(), port: port.parse().ok()? })
            }
            None => Some(Target { host: String::new(), port: s.parse().ok()? }),
        },
        _ => None,
    }
}

/// `"3000"` or `"40000-55000"`. Regex and `host:port` keys are not supported.
fn parse_port_range(key: &str) -> Option<(u16, u16)> {
    match key.split_once('-') {
        Some((lo, hi)) => Some((lo.trim().parse().ok()?, hi.trim().parse().ok()?)),
        None => key.trim().parse().ok().map(|p| (p, p)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape of a real-world file: comments, tabs, trailing whitespace before `}`.
    const DEVCONTAINER_JSON: &str = r#"// For format details, see https://aka.ms/devcontainer.json.
{
	"name": "Example",
	"dockerComposeFile": "docker-compose.yml",
	"service": "app",
	// Use 'forwardPorts' to make a list of ports inside the container available locally.
	"forwardPorts": [
		5000,
		5007, // sidecar
		5432, // postgresql
		"db:6379",
		"localhost:9000",
		80   // nginx
	],
	"portsAttributes": {
		"5000": { "label": "oauth callback", "requireLocalPort": true, "onAutoForward": "ignore" },
		"5432": { "label": "postgresql", "requireLocalPort": true },
		"40000-40010": { "onAutoForward": "ignore" },
		"80": { "label": "nginx - dev", "requireLocalPort": true }
	},
	"otherPortsAttributes": { "onAutoForward": "notify" },
	"remoteUser": "vscode",
}
"#;

    fn local(port: u16) -> Target {
        Target { host: String::new(), port }
    }

    #[test]
    fn parses_jsonc_devcontainer() {
        let cfg = PortConfig::from_devcontainer_json(DEVCONTAINER_JSON).unwrap();
        assert_eq!(
            cfg.forward_ports,
            vec![
                local(5000),
                local(5007),
                local(5432),
                Target { host: "db".into(), port: 6379 },
                local(9000),
                local(80)
            ]
        );
        assert_eq!(cfg.attrs(5000), PortAttrs { require_local_port: true, ignore_auto_forward: true });
        assert_eq!(cfg.attrs(5432), PortAttrs { require_local_port: true, ignore_auto_forward: false });
        assert_eq!(cfg.attrs(40005), PortAttrs { require_local_port: false, ignore_auto_forward: true });
        assert_eq!(cfg.attrs(5007), PortAttrs::default());
        assert_eq!(cfg.label(80), Some("nginx - dev"));
        assert_eq!(cfg.label(5007), None);
    }

    #[test]
    fn other_ports_attributes_are_the_default() {
        let cfg = PortConfig::from_devcontainer_json(
            r#"{ "otherPortsAttributes": { "onAutoForward": "ignore" }, "portsAttributes": { "3000": {} } }"#,
        )
        .unwrap();
        assert!(cfg.attrs(8080).ignore_auto_forward);
        assert!(!cfg.attrs(3000).ignore_auto_forward);
    }

    #[test]
    fn metadata_label_merges_fragments() {
        let cfg = PortConfig::from_metadata_label(
            r#"[ {"id":"feature"}, {"forwardPorts":[3000]}, {"forwardPorts":[3000,5432],"portsAttributes":{"5432":{"requireLocalPort":true}}} ]"#,
        );
        assert_eq!(cfg.forward_ports, vec![local(3000), local(5432)]);
        assert!(cfg.attrs(5432).require_local_port);
    }

    fn container(name: &str, folder: &str) -> Container {
        Container {
            id: format!("{name}0000000000000000"),
            name: name.into(),
            local_folder: Some(folder.into()),
            config_file: None,
            metadata: None,
        }
    }

    #[test]
    fn selects_by_reference_cwd_or_singleton() {
        let all = vec![container("aaaa", "/work/a"), container("bbbb", "/work/a/nested")];
        assert_eq!(select(&all, Some("bbbb"), None).unwrap().name, "bbbb");
        assert_eq!(select(&all, None, Some(Path::new("/work/a/src"))).unwrap().name, "aaaa");
        assert_eq!(select(&all, None, Some(Path::new("/work/a/nested/x"))).unwrap().name, "bbbb");
        assert!(select(&all, None, Some(Path::new("/elsewhere"))).is_err());
        assert!(select(&all, Some("zzzz"), None).is_err());
        assert_eq!(select(&all[..1], None, None).unwrap().name, "aaaa");
        assert!(select(&[], None, None).is_err());
    }
}
