use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{DevPortConfig, DevWorkspaceConfig};
use crate::discovery::DiscoveredProject;
use crate::graph::TargetGraph;
use crate::plugins::{PluginRegistry, Target};
use crate::watch::WatchPlan;

/// Fully resolved plan for one invocation of `aster services up`.
pub struct DevPlan {
    pub services: Vec<ServicePlan>,
    pub ports: HashMap<String, u16>,
    pub control_port: Option<u16>,
}

/// One configured long-running service.
pub struct ServicePlan {
    pub name: String,
    pub target_address: String,
    pub target: Target,
    pub project_root: PathBuf,
    pub port: Option<u16>,
    pub open_url: Option<String>,
    pub env: HashMap<String, String>,
    pub watch: WatchPlan,
}

pub fn resolve_dev_plan(
    workspace_root: &Path,
    config: &DevWorkspaceConfig,
    group: Option<&str>,
    projects: &[DiscoveredProject],
    graph: &TargetGraph,
    plugins: &PluginRegistry,
) -> Result<DevPlan> {
    if config.services.is_empty() {
        bail!("no services configured; add [dev.services.<name>] to aster.toml");
    }

    let ports = resolve_dev_ports(workspace_root, config)?;
    let control_port = config
        .control_port
        .as_deref()
        .map(|name| {
            ports
                .get(name)
                .copied()
                .ok_or_else(|| anyhow!("control_port references unknown service port '{name}'"))
        })
        .transpose()?;
    let selected = select_service_names(config, group)?;

    let project_by_address: HashMap<String, &DiscoveredProject> = projects
        .iter()
        .map(|project| (format!("//{}", project.relative_path.display()), project))
        .collect();
    let mut configured = config.services.iter().collect::<Vec<_>>();
    configured
        .sort_by(|(name_a, a), (name_b, b)| a.order.cmp(&b.order).then_with(|| name_a.cmp(name_b)));

    let mut services = Vec::new();
    for (name, service) in configured {
        if !selected.contains(name.as_str()) {
            continue;
        }

        let (project_address, _) = service.target.split_once(':').ok_or_else(|| {
            anyhow!(
                "service '{name}' target must use //project:target syntax: {}",
                service.target
            )
        })?;
        let project = project_by_address.get(project_address).ok_or_else(|| {
            anyhow!("service '{name}' references unknown project {project_address}")
        })?;
        let node = graph.get(&service.target).ok_or_else(|| {
            anyhow!(
                "service '{name}' references unknown target {}",
                service.target
            )
        })?;
        let target = project
            .targets
            .get(&node.target_name)
            .ok_or_else(|| anyhow!("target definition missing for {}", service.target))?;
        if !target.stream {
            bail!(
                "service '{name}' target {} must set stream = true",
                service.target
            );
        }

        let port = match service.port.as_deref() {
            Some(port_name) => Some(*ports.get(port_name).ok_or_else(|| {
                anyhow!("service '{name}' references unknown port '{port_name}'")
            })?),
            None => None,
        };
        let service_file_env = load_env_files(workspace_root, &service.env_files)?;
        let mut service_env = service_file_env;
        for key in &service.inherit_env {
            validate_environment_key(name, key)?;
            if let Ok(value) = env::var(key) {
                service_env.insert(key.clone(), value);
            }
        }
        for (key, value) in &service.env {
            service_env.insert(
                key.clone(),
                expand_template(value, port, &ports).with_context(|| {
                    format!("invalid env value for service '{name}' key '{key}'")
                })?,
            );
        }
        service_env.insert("ASTER_SERVICE_NAME".to_string(), name.clone());
        if let Some(port) = port {
            service_env.insert("ASTER_SERVICE_PORT".to_string(), port.to_string());
        }
        validate_environment(name, &service_env)?;

        let mut target = target.clone();
        target.command = expand_template(&target.command, port, &ports)
            .with_context(|| format!("invalid command for service '{name}'"))?;
        let open_url = port.map(|port| {
            let path = service.open_path.as_deref().unwrap_or("");
            let path = if path.is_empty() || path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            };
            format!("http://localhost:{port}{path}")
        });
        let watch = WatchPlan::build(
            std::slice::from_ref(&service.target),
            projects,
            graph,
            plugins,
        )
        .with_context(|| format!("failed to build watch plan for service '{name}'"))?;

        services.push(ServicePlan {
            name: name.clone(),
            target_address: service.target.clone(),
            target,
            project_root: project.root.clone(),
            port,
            open_url,
            env: service_env,
            watch,
        });
    }

    if services.is_empty() {
        match group {
            Some(group) => bail!("service group '{group}' has no services"),
            None => bail!("no services configured for the default run"),
        }
    }

    Ok(DevPlan {
        services,
        ports,
        control_port,
    })
}

fn select_service_names<'a>(
    config: &'a DevWorkspaceConfig,
    group: Option<&str>,
) -> Result<HashSet<&'a str>> {
    config.validate_service_groups()?;
    if let Some(group) = group {
        let services = config.service_groups.get(group).ok_or_else(|| {
            let mut known = config
                .service_groups
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>();
            known.sort_unstable();
            if known.is_empty() {
                anyhow!("unknown service group '{group}'; no service groups are configured")
            } else {
                anyhow!(
                    "unknown service group '{group}'; configured groups: {}",
                    known.join(", ")
                )
            }
        })?;
        return Ok(services.iter().map(String::as_str).collect());
    }

    let grouped = config
        .service_groups
        .values()
        .flatten()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut selected = config
        .services
        .keys()
        .map(String::as_str)
        .filter(|service| !grouped.contains(service))
        .collect::<HashSet<_>>();
    if let Some(main) = config.service_groups.get("main") {
        selected.extend(main.iter().map(String::as_str));
    }
    Ok(selected)
}

/// Resolve the named development ports without requiring service targets.
///
/// This is intentionally separate from [`resolve_dev_plan`] so maintenance
/// commands can operate even when a service target is temporarily broken.
pub fn resolve_dev_ports(
    workspace_root: &Path,
    config: &DevWorkspaceConfig,
) -> Result<HashMap<String, u16>> {
    let port_file_env = load_env_files(workspace_root, &config.port_env_files)?;
    resolve_ports(&config.ports, &port_file_env)
}

fn validate_environment(service: &str, environment: &HashMap<String, String>) -> Result<()> {
    for (key, value) in environment {
        validate_environment_key(service, key)?;
        if value.contains('\0') {
            bail!("service '{service}' environment value for '{key}' contains NUL");
        }
    }
    Ok(())
}

fn validate_environment_key(service: &str, key: &str) -> Result<()> {
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        bail!("service '{service}' has invalid environment key {key:?}");
    }
    Ok(())
}

fn load_env_files(workspace_root: &Path, files: &[String]) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    for configured_path in files {
        let path = workspace_root.join(configured_path);
        if !path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read env file {}", path.display()))?;
        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            let value = value.trim();
            let value = if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                &value[1..value.len() - 1]
            } else {
                value
            };
            values.insert(key.to_string(), value.to_string());
        }
    }
    Ok(values)
}

fn resolve_ports(
    configs: &HashMap<String, DevPortConfig>,
    file_env: &HashMap<String, String>,
) -> Result<HashMap<String, u16>> {
    validate_port_offsets(configs)?;
    let mut resolved: HashMap<String, u16> = HashMap::new();
    let mut pending: HashSet<&str> = configs.keys().map(String::as_str).collect();

    while !pending.is_empty() {
        let before = pending.len();
        for name in pending.clone() {
            let config = &configs[name];
            let value = match config {
                DevPortConfig::Fixed(value) => Some(*value),
                DevPortConfig::Resolved(config) => {
                    let process_value = config
                        .env
                        .iter()
                        .find_map(|key| env::var(key).ok().map(|raw| (key, raw)));
                    let file_names = config.file_env.as_deref().unwrap_or(&config.env);
                    let explicit = process_value.or_else(|| {
                        file_names
                            .iter()
                            .find_map(|key| file_env.get(key).cloned().map(|raw| (key, raw)))
                    });
                    if let Some((key, raw)) = explicit {
                        Some(raw.parse::<u16>().with_context(|| {
                            format!("port '{name}' environment variable {key} is not a valid port")
                        })?)
                    } else if let Some(base_name) = config.offset_from.as_deref() {
                        let Some(base_value) = resolved.get(base_name).copied() else {
                            continue;
                        };
                        let offset_base = config.offset_base.ok_or_else(|| {
                            anyhow!("port '{name}' sets offset_from but is missing offset_base")
                        })?;
                        let delta = if config.saturating_offset {
                            base_value.saturating_sub(offset_base)
                        } else {
                            base_value.checked_sub(offset_base).ok_or_else(|| {
                                anyhow!(
                                    "port '{name}' offset source '{base_name}' ({base_value}) is below offset_base ({offset_base})"
                                )
                            })?
                        };
                        Some(config.default.checked_add(delta).ok_or_else(|| {
                            anyhow!(
                                "port '{name}' overflows: default {} + offset {delta} exceeds 65535",
                                config.default
                            )
                        })?)
                    } else {
                        Some(config.default)
                    }
                }
            };
            if let Some(value) = value {
                if value == 0 {
                    bail!("port '{name}' must be between 1 and 65535");
                }
                resolved.insert(name.to_string(), value);
                pending.remove(name);
            }
        }
        if pending.len() == before {
            let mut names = pending.into_iter().collect::<Vec<_>>();
            names.sort_unstable();
            bail!(
                "unable to resolve service ports (unknown or cyclic offset_from): {}",
                names.join(", ")
            );
        }
    }

    Ok(resolved)
}

fn validate_port_offsets(configs: &HashMap<String, DevPortConfig>) -> Result<()> {
    for (name, config) in configs {
        let DevPortConfig::Resolved(config) = config else {
            continue;
        };
        if config.offset_from.is_some() && config.offset_base.is_none() {
            bail!("port '{name}' sets offset_from but is missing offset_base");
        }
        if config.offset_from.is_none() && config.offset_base.is_some() {
            bail!("port '{name}' sets offset_base but is missing offset_from");
        }
        let mut current = name.as_str();
        let mut seen = HashSet::new();
        while let DevPortConfig::Resolved(current_config) = &configs[current] {
            let Some(next) = current_config.offset_from.as_deref() else {
                break;
            };
            if !seen.insert(current) {
                bail!("service port offset_from cycle includes '{current}'");
            }
            if !configs.contains_key(next) {
                bail!("port '{current}' offset_from references unknown port '{next}'");
            }
            current = next;
        }
    }
    Ok(())
}

fn expand_template(
    value: &str,
    service_port: Option<u16>,
    ports: &HashMap<String, u16>,
) -> Result<String> {
    let mut expanded = value.to_string();
    if expanded.contains("{port}") {
        let port = service_port
            .ok_or_else(|| anyhow!("uses {{port}} but the service has no configured port"))?;
        expanded = expanded.replace("{port}", &port.to_string());
    }
    for (name, port) in ports {
        expanded = expanded.replace(&format!("{{ports.{name}}}"), &port.to_string());
    }
    if let Some(start) = expanded.find("{ports.") {
        let rest = &expanded[start..];
        let end = rest.find('}').unwrap_or(rest.len().saturating_sub(1));
        bail!("references unknown port template {}", &rest[..=end]);
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DevServiceConfig, ResolvedDevPortConfig};

    fn service(target: &str) -> DevServiceConfig {
        DevServiceConfig {
            target: target.to_string(),
            port: None,
            open_path: None,
            env_files: Vec::new(),
            env: HashMap::new(),
            inherit_env: Vec::new(),
            order: 0,
        }
    }

    #[test]
    fn service_groups_select_grouped_or_ungrouped_services() {
        let config = DevWorkspaceConfig {
            services: HashMap::from([
                ("platform".to_string(), service("//platform:dev")),
                ("metrics".to_string(), service("//metrics:dev")),
                ("intern-data".to_string(), service("//intern-data:dev")),
                ("intern-fe".to_string(), service("//intern-fe:dev")),
            ]),
            service_groups: HashMap::from([
                ("main".to_string(), vec!["platform".to_string()]),
                (
                    "intern".to_string(),
                    vec!["intern-data".to_string(), "intern-fe".to_string()],
                ),
            ]),
            ..DevWorkspaceConfig::default()
        };

        assert_eq!(
            select_service_names(&config, None).unwrap(),
            HashSet::from(["platform", "metrics"])
        );
        assert_eq!(
            select_service_names(&config, Some("intern")).unwrap(),
            HashSet::from(["intern-data", "intern-fe"])
        );
        assert_eq!(
            select_service_names(&config, Some("main")).unwrap(),
            HashSet::from(["platform"])
        );
        let error = select_service_names(&config, Some("missing")).unwrap_err();
        assert!(error
            .to_string()
            .contains("configured groups: intern, main"));
    }

    #[test]
    fn services_can_belong_to_multiple_groups() {
        let config = DevWorkspaceConfig {
            services: HashMap::from([
                ("shared".to_string(), service("//shared:dev")),
                ("one".to_string(), service("//one:dev")),
            ]),
            service_groups: HashMap::from([
                ("small".to_string(), vec!["shared".to_string()]),
                (
                    "full".to_string(),
                    vec!["shared".to_string(), "one".to_string()],
                ),
            ]),
            ..DevWorkspaceConfig::default()
        };

        assert!(select_service_names(&config, None).unwrap().is_empty());
        assert_eq!(
            select_service_names(&config, Some("small")).unwrap(),
            HashSet::from(["shared"])
        );
    }

    #[test]
    fn resolves_offset_ports_and_templates() {
        let configs = HashMap::from([
            ("api".to_string(), DevPortConfig::Fixed(4004)),
            (
                "web".to_string(),
                DevPortConfig::Resolved(ResolvedDevPortConfig {
                    env: vec![],
                    file_env: None,
                    default: 3300,
                    offset_from: Some("api".to_string()),
                    offset_base: Some(4000),
                    saturating_offset: false,
                }),
            ),
        ]);
        let ports = resolve_ports(&configs, &HashMap::new()).unwrap();
        assert_eq!(ports["web"], 3304);
        assert_eq!(
            expand_template("serve {port} --api {ports.api}", Some(3304), &ports).unwrap(),
            "serve 3304 --api 4004"
        );
    }

    #[test]
    fn rejects_unknown_port_template() {
        let error = expand_template("serve {ports.missing}", None, &HashMap::new()).unwrap_err();
        assert!(error.to_string().contains("unknown port template"));
    }

    #[test]
    fn rejects_invalid_derived_port_ranges() {
        for (base, default, expected) in [
            (3999, 3300, "below offset_base"),
            (65000, 10000, "overflows"),
        ] {
            let configs = HashMap::from([
                ("api".to_string(), DevPortConfig::Fixed(base)),
                (
                    "web".to_string(),
                    DevPortConfig::Resolved(ResolvedDevPortConfig {
                        env: vec![],
                        file_env: None,
                        default,
                        offset_from: Some("api".to_string()),
                        offset_base: Some(4000),
                        saturating_offset: false,
                    }),
                ),
            ]);
            let error = resolve_ports(&configs, &HashMap::new()).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn rejects_zero_named_port() {
        let configs = HashMap::from([("control".to_string(), DevPortConfig::Fixed(0))]);
        let error = resolve_ports(&configs, &HashMap::new()).unwrap_err();
        assert!(error.to_string().contains("between 1 and 65535"));
    }

    #[test]
    fn saturating_offset_accepts_source_below_baseline() {
        let configs = HashMap::from([
            ("api".to_string(), DevPortConfig::Fixed(3999)),
            (
                "web".to_string(),
                DevPortConfig::Resolved(ResolvedDevPortConfig {
                    env: vec![],
                    file_env: None,
                    default: 3300,
                    offset_from: Some("api".to_string()),
                    offset_base: Some(4000),
                    saturating_offset: true,
                }),
            ),
        ]);
        let ports = resolve_ports(&configs, &HashMap::new()).unwrap();
        assert_eq!(ports["web"], 3300);
    }

    #[test]
    fn explicit_empty_file_env_disables_port_file_lookup() {
        let key = "ASTER_TEST_PORT_EXPLICIT_EMPTY_FILE_ENV";
        let configs = HashMap::from([(
            "api".to_string(),
            DevPortConfig::Resolved(ResolvedDevPortConfig {
                env: vec![key.to_string()],
                file_env: Some(vec![]),
                default: 4100,
                offset_from: None,
                offset_base: None,
                saturating_offset: false,
            }),
        )]);
        let file_env = HashMap::from([(key.to_string(), "4200".to_string())]);

        let ports = resolve_ports(&configs, &file_env).unwrap();
        assert_eq!(ports["api"], 4100);
    }

    #[test]
    fn validates_offset_graph_before_considering_overrides() {
        let malformed = ResolvedDevPortConfig {
            env: vec!["PORT_OVERRIDE".to_string()],
            file_env: None,
            default: 3300,
            offset_from: Some("missing".to_string()),
            offset_base: None,
            saturating_offset: false,
        };
        let configs = HashMap::from([("web".to_string(), DevPortConfig::Resolved(malformed))]);
        let error = validate_port_offsets(&configs).unwrap_err();
        assert!(error.to_string().contains("missing offset_base"));

        let cyclic = |next: &str| {
            DevPortConfig::Resolved(ResolvedDevPortConfig {
                env: vec![],
                file_env: None,
                default: 3000,
                offset_from: Some(next.to_string()),
                offset_base: Some(3000),
                saturating_offset: false,
            })
        };
        let configs = HashMap::from([
            ("one".to_string(), cyclic("two")),
            ("two".to_string(), cyclic("one")),
        ]);
        let error = validate_port_offsets(&configs).unwrap_err();
        assert!(error.to_string().contains("cycle"));
    }

    #[test]
    fn rejects_environment_entries_that_command_cannot_accept() {
        let invalid_key = HashMap::from([("BAD=KEY".to_string(), "value".to_string())]);
        assert!(validate_environment("api", &invalid_key)
            .unwrap_err()
            .to_string()
            .contains("invalid environment key"));

        let invalid_value = HashMap::from([("KEY".to_string(), "bad\0value".to_string())]);
        assert!(validate_environment("api", &invalid_value)
            .unwrap_err()
            .to_string()
            .contains("contains NUL"));
    }
}
