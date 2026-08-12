use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{DevPortConfig, DevWorkspaceConfig};
use crate::discovery::DiscoveredProject;
use crate::graph::TargetGraph;
use crate::plugins::{PluginRegistry, Target};
use crate::watch::WatchPlan;

use super::port_allocator::{PortAllocator, PortLease};

/// Fully resolved plan for one invocation of `aster services up`.
pub struct DevPlan {
    pub services: Vec<ServicePlan>,
    pub ports: HashMap<String, u16>,
    pub control_port: Option<u16>,
    _port_lease: PortLease,
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

    let selected = select_service_names(config, group)?;
    let group_control_port = match group {
        Some(group) => config
            .service_groups
            .get(group)
            .and_then(|config| config.control_port()),
        None => config
            .service_groups
            .get("main")
            .and_then(|config| config.control_port()),
    };
    let project_by_address: HashMap<String, &DiscoveredProject> = projects
        .iter()
        .map(|project| (format!("//{}", project.relative_path.display()), project))
        .collect();
    let selected_control_port = group_control_port.or(config.control_port.as_deref());
    let active_ports = collect_active_ports(config, &selected, selected_control_port)?;
    let (ports, port_lease) = allocate_dev_ports(workspace_root, config, &active_ports)?;
    let control_port = selected_control_port
        .map(|name| {
            ports
                .get(name)
                .copied()
                .ok_or_else(|| anyhow!("control_port references unknown service port '{name}'"))
        })
        .transpose()?;
    let tls_open_urls = resolve_tls_open_urls(config, &selected, &ports)?;
    let mut configured = config.services.iter().collect::<Vec<_>>();
    configured
        .sort_by(|(name_a, a), (name_b, b)| a.order.cmp(&b.order).then_with(|| name_a.cmp(name_b)));

    let mut services = Vec::new();
    for (name, service) in configured {
        if !selected.contains(name.as_str()) {
            continue;
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
        for (key, port_name) in &service.port_env {
            let port = ports.get(port_name).ok_or_else(|| {
                anyhow!(
                    "service '{name}' port_env key '{key}' references unknown port '{port_name}'"
                )
            })?;
            service_env.insert(key.clone(), port.to_string());
        }
        service_env.insert("ASTER_SERVICE_NAME".to_string(), name.clone());
        if let Some(port) = port {
            service_env.insert("ASTER_SERVICE_PORT".to_string(), port.to_string());
        }
        if service.tls_proxy.is_some() {
            service_env.insert(
                "ASTER_RESOLVED_PORTS".to_string(),
                serde_json::to_string(&ports).context("failed to encode resolved TLS ports")?,
            );
        }
        validate_environment(name, &service_env)?;

        let path = service.open_path.as_deref().unwrap_or("");
        let path = if path.is_empty() || path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        let (target_address, mut target, project_root, watch, open_url) =
            if let Some(target_address) = &service.target {
                let (project_address, _) = target_address.split_once(':').ok_or_else(|| {
                    anyhow!(
                        "service '{name}' target must use //project:target syntax: {target_address}"
                    )
                })?;
                let project = project_by_address.get(project_address).ok_or_else(|| {
                    anyhow!("service '{name}' references unknown project {project_address}")
                })?;
                let node = graph.get(target_address).ok_or_else(|| {
                    anyhow!("service '{name}' references unknown target {target_address}")
                })?;
                let target = project
                    .targets
                    .get(&node.target_name)
                    .ok_or_else(|| anyhow!("target definition missing for {target_address}"))?;
                if !target.stream {
                    bail!("service '{name}' target {target_address} must set stream = true");
                }
                let watch = WatchPlan::build(
                    std::slice::from_ref(target_address),
                    projects,
                    graph,
                    plugins,
                )
                .with_context(|| format!("failed to build watch plan for service '{name}'"))?;
                (
                    target_address.clone(),
                    target.clone(),
                    project.root.clone(),
                    watch,
                    service.port.as_ref().and_then(|port_name| {
                        tls_open_urls
                            .get(port_name)
                            .map(|base| format!("{base}{path}"))
                            .or_else(|| port.map(|port| format!("http://localhost:{port}{path}")))
                    }),
                )
            } else {
                let tls = service
                    .tls_proxy
                    .as_ref()
                    .expect("development config validates service variants");
                let executable = std::env::current_exe()
                    .context("failed to locate the Aster executable for TLS proxy service")?;
                let command = format!(
                    "{} services tls serve {}",
                    shell_words::quote(&executable.to_string_lossy()),
                    shell_words::quote(name)
                );
                let target = crate::plugins::Target {
                    command,
                    stream: true,
                    ..Default::default()
                };
                let open_url = port.map(|port| {
                    let authority = if port == 443 {
                        tls.open_host.clone()
                    } else {
                        format!("{}:{port}", tls.open_host)
                    };
                    format!("https://{authority}{path}")
                });
                (
                    format!("builtin:tls-proxy:{name}"),
                    target,
                    workspace_root.to_path_buf(),
                    WatchPlan::empty(),
                    open_url,
                )
            };
        target.command = expand_template(&target.command, port, &ports)
            .with_context(|| format!("invalid command for service '{name}'"))?;

        services.push(ServicePlan {
            name: name.clone(),
            target_address,
            target,
            project_root,
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
        _port_lease: port_lease,
    })
}

fn resolve_tls_open_urls(
    config: &DevWorkspaceConfig,
    selected: &HashSet<&str>,
    ports: &HashMap<String, u16>,
) -> Result<HashMap<String, String>> {
    let mut upstreams = HashMap::new();
    for (service_name, service) in &config.services {
        if !selected.contains(service_name.as_str()) {
            continue;
        }
        let Some(tls) = &service.tls_proxy else {
            continue;
        };
        let port_name = service
            .port
            .as_ref()
            .expect("TLS proxy validation requires a port");
        let port = ports[port_name];
        for route in &tls.routes {
            let Some(hostname) = route.open_host.as_ref().or(route.host.as_ref()) else {
                continue;
            };
            let authority = if port == 443 {
                hostname.clone()
            } else {
                format!("{hostname}:{port}")
            };
            let url = format!("https://{authority}");
            if let Some(existing) = upstreams.insert(route.upstream_port.clone(), url.clone()) {
                if existing != url {
                    bail!(
                        "TLS routes publish multiple open hosts for upstream port '{}': {existing} and {url}",
                        route.upstream_port
                    );
                }
            }
        }
    }
    Ok(upstreams)
}

fn select_service_names<'a>(
    config: &'a DevWorkspaceConfig,
    group: Option<&str>,
) -> Result<HashSet<&'a str>> {
    config.validate()?;
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
        return Ok(services.services().iter().map(String::as_str).collect());
    }

    let grouped = config
        .service_groups
        .values()
        .flat_map(|group| group.services())
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut selected = config
        .services
        .keys()
        .map(String::as_str)
        .filter(|service| !grouped.contains(service))
        .collect::<HashSet<_>>();
    if let Some(main) = config.service_groups.get("main") {
        selected.extend(main.services().iter().map(String::as_str));
    }
    Ok(selected)
}

fn collect_active_ports(
    config: &DevWorkspaceConfig,
    selected: &HashSet<&str>,
    control_port: Option<&str>,
) -> Result<HashSet<String>> {
    let mut active = HashSet::new();
    if let Some(port) = control_port {
        active.insert(port.to_string());
    }

    for name in selected {
        let service = &config.services[*name];
        if let Some(port) = &service.port {
            active.insert(port.clone());
        }
        active.extend(service.port_env.values().cloned());
        // TLS routes consume upstream ports but do not own their listeners.
        // They may intentionally point at services outside this supervisor.
    }

    // Derived ports depend on their offset source, which is part of the same
    // atomic allocation bundle even when only the derived name is referenced.
    let mut pending = active.iter().cloned().collect::<Vec<_>>();
    while let Some(name) = pending.pop() {
        let Some(DevPortConfig::Resolved(port)) = config.ports.get(&name) else {
            continue;
        };
        if let Some(parent) = &port.offset_from {
            if active.insert(parent.clone()) {
                pending.push(parent.clone());
            }
        }
    }
    Ok(active)
}

fn allocate_dev_ports(
    workspace_root: &Path,
    config: &DevWorkspaceConfig,
    active: &HashSet<String>,
) -> Result<(HashMap<String, u16>, PortLease)> {
    let file_env = load_env_files(workspace_root, &config.port_env_files)?;
    validate_port_offsets(&config.ports)?;

    for name in active {
        if !config.ports.contains_key(name) {
            bail!("selected services reference unknown port '{name}'");
        }
    }

    let mut groups: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
    for name in active {
        let root = dynamic_root(name, &config.ports)?;
        groups.entry(root).or_default().push(name.clone());
    }
    for names in groups.values_mut() {
        names.sort();
        names.dedup();
    }

    let mut allocator = PortAllocator::lock()?;
    // Resolution produces a complete named-port map. Seed roots that are not
    // active in this run with deterministic display values; only active groups
    // are probed and leased below.
    let mut dynamic_values = config
        .ports
        .iter()
        .filter_map(|(name, port)| match port {
            DevPortConfig::Dynamic(dynamic) => {
                Some((name.clone(), dynamic.preferred.unwrap_or(dynamic.range[0])))
            }
            _ => None,
        })
        .collect::<HashMap<_, _>>();

    if let Some(static_names) = groups.remove(&None) {
        let bundle =
            resolve_named_bundle(&config.ports, &file_env, &dynamic_values, &static_names)?;
        if !bundle.is_empty() && !allocator.try_bundle(workspace_root, &bundle)? {
            bail!(
                "configured static service ports are already allocated or listening: {}",
                format_named_ports(&bundle)
            );
        }
    }

    for (root, names) in groups {
        let root = root.expect("dynamic groups have a root");
        let DevPortConfig::Dynamic(dynamic) = &config.ports[&root] else {
            unreachable!("dynamic_root returns only dynamic port names");
        };
        let mut candidates = Vec::new();
        if let Some(preferred) = dynamic.preferred {
            candidates.push(preferred);
        }
        candidates.extend(dynamic.range[0]..=dynamic.range[1]);
        candidates.dedup();

        let mut selected = None;
        for candidate in candidates {
            dynamic_values.insert(root.clone(), candidate);
            let bundle = resolve_named_bundle(&config.ports, &file_env, &dynamic_values, &names)?;
            if allocator.try_bundle(workspace_root, &bundle)? {
                selected = Some(candidate);
                break;
            }
        }
        let Some(value) = selected else {
            bail!(
                "no collision-free port bundle is available for dynamic port '{root}' in range {}-{}",
                dynamic.range[0],
                dynamic.range[1]
            );
        };
        dynamic_values.insert(root, value);
    }

    let ports = resolve_ports(&config.ports, &file_env, &dynamic_values)?;
    Ok((ports, allocator.finish(workspace_root)?))
}

fn dynamic_root(name: &str, configs: &HashMap<String, DevPortConfig>) -> Result<Option<String>> {
    let mut current = name;
    loop {
        match configs
            .get(current)
            .ok_or_else(|| anyhow!("unknown service port '{current}'"))?
        {
            DevPortConfig::Dynamic(_) => return Ok(Some(current.to_string())),
            DevPortConfig::Resolved(config) => {
                let Some(parent) = config.offset_from.as_deref() else {
                    return Ok(None);
                };
                current = parent;
            }
            DevPortConfig::Fixed(_) => return Ok(None),
        }
    }
}

fn resolve_named_bundle(
    configs: &HashMap<String, DevPortConfig>,
    file_env: &HashMap<String, String>,
    dynamic_values: &HashMap<String, u16>,
    names: &[String],
) -> Result<BTreeMap<String, u16>> {
    let resolved = resolve_ports(configs, file_env, dynamic_values)?;
    names
        .iter()
        .map(|name| Ok((name.clone(), resolved[name])))
        .collect()
}

fn format_named_ports(ports: &BTreeMap<String, u16>) -> String {
    ports
        .iter()
        .map(|(name, port)| format!("{name}={port}"))
        .collect::<Vec<_>>()
        .join(", ")
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
    let dynamic_values = config
        .ports
        .iter()
        .filter_map(|(name, port)| match port {
            DevPortConfig::Dynamic(config) => {
                Some((name.clone(), config.preferred.unwrap_or(config.range[0])))
            }
            _ => None,
        })
        .collect();
    resolve_ports(&config.ports, &port_file_env, &dynamic_values)
}

/// Resolve only ports whose value does not depend on a dynamic allocation.
/// Maintenance commands must not guess a dynamic run's current value.
pub fn resolve_static_dev_ports(
    workspace_root: &Path,
    config: &DevWorkspaceConfig,
) -> Result<HashMap<String, u16>> {
    let resolved = resolve_dev_ports(workspace_root, config)?;
    resolved
        .into_iter()
        .filter_map(|(name, port)| match dynamic_root(&name, &config.ports) {
            Ok(None) => Some(Ok((name, port))),
            Ok(Some(_)) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
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
    dynamic_values: &HashMap<String, u16>,
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
                DevPortConfig::Dynamic(_) => Some(
                    *dynamic_values
                        .get(name)
                        .ok_or_else(|| anyhow!("dynamic port '{name}' has not been allocated"))?,
                ),
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
        if let DevPortConfig::Dynamic(config) = config {
            let [start, end] = config.range;
            if start == 0 || start > end {
                bail!("dynamic port '{name}' range must contain valid ports in ascending order");
            }
            if let Some(preferred) = config.preferred {
                if preferred < start || preferred > end {
                    bail!(
                        "dynamic port '{name}' preferred value {preferred} is outside range {start}-{end}"
                    );
                }
            }
            continue;
        }
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
    use crate::config::{
        DetailedDevServiceGroupConfig, DevServiceConfig, DevServiceGroupConfig, DevTlsProxyConfig,
        DevTlsRouteConfig, ResolvedDevPortConfig,
    };

    fn service(target: &str) -> DevServiceConfig {
        DevServiceConfig {
            target: Some(target.to_string()),
            tls_proxy: None,
            port: None,
            open_path: None,
            env_files: Vec::new(),
            env: HashMap::new(),
            port_env: HashMap::new(),
            inherit_env: Vec::new(),
            order: 0,
        }
    }

    #[test]
    fn tls_routes_publish_https_open_urls_for_upstream_services() {
        let tls_service = DevServiceConfig {
            target: None,
            tls_proxy: Some(DevTlsProxyConfig {
                certificate_hosts: vec![
                    "intern.dev".to_string(),
                    "*.local.sites.intern.dev".to_string(),
                ],
                open_host: "intern.dev".to_string(),
                dns_domain: None,
                routes: vec![
                    DevTlsRouteConfig {
                        host: Some("intern.dev".to_string()),
                        host_suffix: None,
                        open_host: None,
                        upstream_port: "frontend".to_string(),
                    },
                    DevTlsRouteConfig {
                        host: None,
                        host_suffix: Some(".sites.intern.dev".to_string()),
                        open_host: Some("test.local.sites.intern.dev".to_string()),
                        upstream_port: "gateway".to_string(),
                    },
                ],
            }),
            port: Some("https".to_string()),
            open_path: None,
            env_files: Vec::new(),
            env: HashMap::new(),
            port_env: HashMap::new(),
            inherit_env: Vec::new(),
            order: 0,
        };
        let config = DevWorkspaceConfig {
            ports: HashMap::from([
                ("https".to_string(), DevPortConfig::Fixed(8443)),
                ("frontend".to_string(), DevPortConfig::Fixed(3100)),
                ("gateway".to_string(), DevPortConfig::Fixed(3800)),
            ]),
            services: HashMap::from([
                ("frontend".to_string(), service("//frontend:dev")),
                ("gateway".to_string(), service("//gateway:dev")),
                ("edge".to_string(), tls_service),
            ]),
            service_groups: HashMap::from([(
                "intern".to_string(),
                DevServiceGroupConfig::Services(vec![
                    "frontend".to_string(),
                    "gateway".to_string(),
                    "edge".to_string(),
                ]),
            )]),
            ..DevWorkspaceConfig::default()
        };
        let selected = select_service_names(&config, Some("intern")).unwrap();
        let ports = resolve_ports(&config.ports, &HashMap::new(), &HashMap::new()).unwrap();

        let open_urls = resolve_tls_open_urls(&config, &selected, &ports).unwrap();
        assert_eq!(open_urls["frontend"], "https://intern.dev:8443");
        assert_eq!(
            open_urls["gateway"],
            "https://test.local.sites.intern.dev:8443"
        );
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
                (
                    "main".to_string(),
                    DevServiceGroupConfig::Services(vec!["platform".to_string()]),
                ),
                (
                    "intern".to_string(),
                    DevServiceGroupConfig::Services(vec![
                        "intern-data".to_string(),
                        "intern-fe".to_string(),
                    ]),
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
                (
                    "small".to_string(),
                    DevServiceGroupConfig::Services(vec!["shared".to_string()]),
                ),
                (
                    "full".to_string(),
                    DevServiceGroupConfig::Services(vec!["shared".to_string(), "one".to_string()]),
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
    fn service_group_control_port_overrides_global_control_port() {
        let config = DevWorkspaceConfig {
            control_port: Some("global-control".to_string()),
            ports: HashMap::from([
                ("global-control".to_string(), DevPortConfig::Fixed(5000)),
                ("intern-control".to_string(), DevPortConfig::Fixed(5001)),
            ]),
            services: HashMap::from([("api".to_string(), service("//api:dev"))]),
            service_groups: HashMap::from([(
                "intern".to_string(),
                DevServiceGroupConfig::Detailed(DetailedDevServiceGroupConfig {
                    services: vec!["api".to_string()],
                    control_port: Some("intern-control".to_string()),
                }),
            )]),
            ..DevWorkspaceConfig::default()
        };

        let ports = resolve_ports(&config.ports, &HashMap::new(), &HashMap::new()).unwrap();
        let selected = config.service_groups["intern"].control_port();
        assert_eq!(selected.and_then(|name| ports.get(name)), Some(&5001));
    }

    #[test]
    fn resolves_offset_ports_and_templates() {
        let configs = HashMap::from([
            ("api".to_string(), DevPortConfig::Fixed(4004)),
            (
                "web".to_string(),
                DevPortConfig::Resolved(ResolvedDevPortConfig {
                    allocation: None,
                    env: vec![],
                    file_env: None,
                    default: 3300,
                    offset_from: Some("api".to_string()),
                    offset_base: Some(4000),
                    saturating_offset: false,
                }),
            ),
        ]);
        let ports = resolve_ports(&configs, &HashMap::new(), &HashMap::new()).unwrap();
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
    fn maintenance_resolution_excludes_dynamic_bundles() {
        let config = DevWorkspaceConfig {
            ports: HashMap::from([
                (
                    "dynamic".to_string(),
                    DevPortConfig::Dynamic(crate::config::DynamicDevPortConfig {
                        allocation: crate::config::DynamicPortAllocation::Dynamic,
                        range: [4000, 4099],
                        preferred: Some(4000),
                    }),
                ),
                (
                    "derived".to_string(),
                    DevPortConfig::Resolved(ResolvedDevPortConfig {
                        allocation: None,
                        env: vec![],
                        file_env: None,
                        default: 3000,
                        offset_from: Some("dynamic".to_string()),
                        offset_base: Some(4000),
                        saturating_offset: false,
                    }),
                ),
                ("static".to_string(), DevPortConfig::Fixed(5432)),
            ]),
            ..DevWorkspaceConfig::default()
        };
        let root = tempfile::tempdir().unwrap();

        let ports = resolve_static_dev_ports(root.path(), &config).unwrap();
        assert_eq!(ports, HashMap::from([("static".to_string(), 5432)]));
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
                        allocation: None,
                        env: vec![],
                        file_env: None,
                        default,
                        offset_from: Some("api".to_string()),
                        offset_base: Some(4000),
                        saturating_offset: false,
                    }),
                ),
            ]);
            let error = resolve_ports(&configs, &HashMap::new(), &HashMap::new()).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn rejects_zero_named_port() {
        let configs = HashMap::from([("control".to_string(), DevPortConfig::Fixed(0))]);
        let error = resolve_ports(&configs, &HashMap::new(), &HashMap::new()).unwrap_err();
        assert!(error.to_string().contains("between 1 and 65535"));
    }

    #[test]
    fn saturating_offset_accepts_source_below_baseline() {
        let configs = HashMap::from([
            ("api".to_string(), DevPortConfig::Fixed(3999)),
            (
                "web".to_string(),
                DevPortConfig::Resolved(ResolvedDevPortConfig {
                    allocation: None,
                    env: vec![],
                    file_env: None,
                    default: 3300,
                    offset_from: Some("api".to_string()),
                    offset_base: Some(4000),
                    saturating_offset: true,
                }),
            ),
        ]);
        let ports = resolve_ports(&configs, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(ports["web"], 3300);
    }

    #[test]
    fn explicit_empty_file_env_disables_port_file_lookup() {
        let key = "ASTER_TEST_PORT_EXPLICIT_EMPTY_FILE_ENV";
        let configs = HashMap::from([(
            "api".to_string(),
            DevPortConfig::Resolved(ResolvedDevPortConfig {
                allocation: None,
                env: vec![key.to_string()],
                file_env: Some(vec![]),
                default: 4100,
                offset_from: None,
                offset_base: None,
                saturating_offset: false,
            }),
        )]);
        let file_env = HashMap::from([(key.to_string(), "4200".to_string())]);

        let ports = resolve_ports(&configs, &file_env, &HashMap::new()).unwrap();
        assert_eq!(ports["api"], 4100);
    }

    #[test]
    fn validates_offset_graph_before_considering_overrides() {
        let malformed = ResolvedDevPortConfig {
            allocation: None,
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
                allocation: None,
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
