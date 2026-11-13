//! Checking the dependency graph before anything starts.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Service;

/// The services to supervise, and those that cannot be started.
#[derive(Debug, Default)]
pub struct Plan {
    pub services: BTreeMap<String, Service>,
    /// Services that will not be started, with the reason.
    pub skipped: BTreeMap<String, String>,
}

impl Plan {
    /// Check `services` for missing requirements and dependency cycles.
    /// `missing` names services whose files could not be loaded, so that
    /// their dependents get a precise reason.
    pub fn new(services: BTreeMap<String, Service>, missing: &BTreeSet<String>) -> Plan {
        let mut skipped = BTreeMap::new();

        for service in services.values() {
            if let Some(dependency) = service
                .requires
                .iter()
                .find(|dependency| !services.contains_key(*dependency))
            {
                let why = if missing.contains(dependency) {
                    "could not be loaded"
                } else {
                    "does not exist"
                };
                skipped.insert(
                    service.name.clone(),
                    format!("requires {dependency}, which {why}"),
                );
            }
        }

        for cycle in cycles(&services) {
            let description = format!("dependency cycle: {}", cycle.join(" -> "));
            for name in cycle {
                skipped.entry(name).or_insert_with(|| description.clone());
            }
        }

        // Anything that requires a skipped service is skipped too.
        loop {
            let newly: Vec<(String, String)> = services
                .values()
                .filter(|service| !skipped.contains_key(&service.name))
                .filter_map(|service| {
                    let dependency = service.requires.iter().find(|d| skipped.contains_key(*d))?;
                    Some((
                        service.name.clone(),
                        format!("requires {dependency}, which cannot start"),
                    ))
                })
                .collect();
            if newly.is_empty() {
                break;
            }
            skipped.extend(newly);
        }

        Plan { services, skipped }
    }
}

/// Every set of services that depend on each other in a loop (strongly
/// connected components of more than one service), each listed in name
/// order. Both `requires` and `after` are dependencies here.
fn cycles(services: &BTreeMap<String, Service>) -> Vec<Vec<String>> {
    // Tarjan's algorithm.
    struct State<'a> {
        services: &'a BTreeMap<String, Service>,
        index: BTreeMap<&'a str, usize>,
        low: BTreeMap<&'a str, usize>,
        stack: Vec<&'a str>,
        on_stack: BTreeSet<&'a str>,
        next: usize,
        found: Vec<Vec<String>>,
    }

    fn visit<'a>(state: &mut State<'a>, name: &'a str) {
        state.index.insert(name, state.next);
        state.low.insert(name, state.next);
        state.next += 1;
        state.stack.push(name);
        state.on_stack.insert(name);

        let service = &state.services[name];
        for dependency in service.requires.iter().chain(&service.after) {
            let Some((dependency, _)) = state.services.get_key_value(dependency) else {
                continue;
            };
            let dependency = dependency.as_str();
            if !state.index.contains_key(dependency) {
                visit(state, dependency);
                let low = state.low[name].min(state.low[dependency]);
                state.low.insert(name, low);
            } else if state.on_stack.contains(dependency) {
                let low = state.low[name].min(state.index[dependency]);
                state.low.insert(name, low);
            }
        }

        if state.low[name] == state.index[name] {
            let mut component = Vec::new();
            while let Some(member) = state.stack.pop() {
                state.on_stack.remove(member);
                component.push(member.to_string());
                if member == name {
                    break;
                }
            }
            if component.len() > 1 {
                component.sort();
                state.found.push(component);
            }
        }
    }

    let mut state = State {
        services,
        index: BTreeMap::new(),
        low: BTreeMap::new(),
        stack: Vec::new(),
        on_stack: BTreeSet::new(),
        next: 0,
        found: Vec::new(),
    };
    for name in services.keys() {
        if !state.index.contains_key(name.as_str()) {
            visit(&mut state, name);
        }
    }
    state.found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(name: &str, requires: &[&str], after: &[&str]) -> (String, Service) {
        (
            name.to_string(),
            Service {
                name: name.to_string(),
                description: name.to_string(),
                command: None,
                requires: requires.iter().map(|s| s.to_string()).collect(),
                after: after.iter().map(|s| s.to_string()).collect(),
            },
        )
    }

    fn plan(services: &[(String, Service)], missing: &[&str]) -> Plan {
        Plan::new(
            services.iter().cloned().collect(),
            &missing.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn a_valid_graph_skips_nothing() {
        let plan = plan(
            &[
                service("net", &[], &[]),
                service("sshd", &["net"], &["log"]),
                service("web", &["net"], &["sshd", "absent"]),
            ],
            &[],
        );
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
    }

    #[test]
    fn missing_requirements_skip_dependents_transitively() {
        let plan = plan(
            &[
                service("a", &["ghost"], &[]),
                service("b", &["a"], &[]),
                service("c", &["broken"], &[]),
                service("d", &[], &["a"]),
            ],
            &["broken"],
        );
        assert_eq!(plan.skipped["a"], "requires ghost, which does not exist");
        assert_eq!(plan.skipped["b"], "requires a, which cannot start");
        assert_eq!(
            plan.skipped["c"],
            "requires broken, which could not be loaded"
        );
        // `after` does not depend on success.
        assert!(!plan.skipped.contains_key("d"));
    }

    #[test]
    fn cycles_through_requires_and_after_are_skipped() {
        let plan = plan(
            &[
                service("a", &["b"], &[]),
                service("b", &[], &["c"]),
                service("c", &["a"], &[]),
                service("d", &["a"], &[]),
                service("e", &[], &[]),
            ],
            &[],
        );
        assert_eq!(plan.skipped["a"], "dependency cycle: a -> b -> c");
        assert_eq!(plan.skipped["c"], "dependency cycle: a -> b -> c");
        assert_eq!(plan.skipped["d"], "requires a, which cannot start");
        assert!(!plan.skipped.contains_key("e"));
    }
}
