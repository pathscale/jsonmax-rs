//! Deterministic role/endpoint coverage for service catalogs and scenario suites.
//!
//! Required coverage describes declarations, planned coverage describes validated
//! calls, and observed coverage describes positive execution counters. None of
//! these operations performs I/O or changes its inputs.

use super::*;
use alloc::collections::BTreeSet;

/// Return every declared endpoint/role pair, with one anonymous pair for an
/// endpoint whose role list is empty.
///
/// Service IDs, service names, and endpoint codes must be globally unique. Roles
/// must be unique within each endpoint. All comparisons preserve exact spelling;
/// in particular, an explicitly declared empty role means anonymous access.
/// Catalog errors have no scenario step and never include supplied values.
pub fn required_coverage(services: &[ServiceSpec]) -> Result<BTreeSet<CoverageKey>> {
    // Borrowed sorted indexes avoid copying catalog strings for validation.
    let mut ordered_services: Vec<&ServiceSpec> = Vec::new();
    reserve(&mut ordered_services, services.len())?;
    ordered_services.extend(services.iter());
    ordered_services.sort_unstable_by_key(|service| service.id);
    if ordered_services.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err(invalid(None, "coverage requires unique service IDs"));
    }
    ordered_services.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    if ordered_services.windows(2).any(|pair| pair[0].name == pair[1].name) {
        return Err(invalid(None, "coverage requires unique service names"));
    }

    let mut endpoints: Vec<&EndpointSchemaSpec> = Vec::new();
    for service in services {
        reserve(&mut endpoints, service.endpoints.len())?;
        endpoints.extend(service.endpoints.iter());
    }
    endpoints.sort_unstable_by_key(|endpoint| endpoint.code);
    if endpoints.windows(2).any(|pair| pair[0].code == pair[1].code) {
        return Err(invalid(None, "coverage requires globally unique endpoint codes"));
    }

    let mut pairs: Vec<(&str, u32)> = Vec::new();
    let mut roles: Vec<&str> = Vec::new();
    for endpoint in endpoints {
        roles.clear();
        reserve(&mut roles, endpoint.roles.len())?;
        roles.extend(endpoint.roles.iter().map(String::as_str));
        roles.sort_unstable();
        if roles.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid(None, "coverage requires unique roles within each endpoint"));
        }
        if roles.is_empty() {
            reserve(&mut pairs, 1)?;
            pairs.push(("", endpoint.code));
        } else {
            reserve(&mut pairs, roles.len())?;
            pairs.extend(roles.iter().map(|role| (*role, endpoint.code)));
        }
    }
    owned_coverage(pairs)
}

/// Return the union of calls in a suite after validating every scenario.
///
/// Connection state is local to each scenario. Closing and reopening a name
/// replaces its role, and simultaneous connections retain independent roles.
/// Receives, assertions, and connects without calls contribute no coverage.
/// This set is a plan and provides no evidence of successful execution.
pub fn planned_coverage(scenarios: &[Scenario]) -> Result<BTreeSet<CoverageKey>> {
    for scenario in scenarios {
        scenario::validate_scenario(scenario)?;
    }

    let mut pairs: Vec<(&str, u32)> = Vec::new();
    for scenario in scenarios {
        let mut connections: Vec<(&str, Option<&str>)> = Vec::new();
        for step in &scenario.steps {
            if let Step::Connect { name, .. } = step {
                reserve(&mut connections, 1)?;
                connections.push((name.as_str(), None));
            }
        }
        connections.sort_unstable_by(|a, b| a.0.cmp(b.0));
        connections.dedup_by(|a, b| a.0 == b.0);

        for (index, step) in scenario.steps.iter().enumerate() {
            match step {
                Step::Connect { name, role, .. } => {
                    *connection_state(&mut connections, name, index)? = Some(role.as_str());
                }
                Step::Close { connection } => {
                    *connection_state(&mut connections, connection, index)? = None;
                }
                Step::Call { connection, endpoint, .. } => {
                    let role = (*connection_state(&mut connections, connection, index)?)
                        .ok_or_else(|| invalid(Some(index), "coverage call requires an active connection"))?;
                    reserve(&mut pairs, 1)?;
                    pairs.push((role, *endpoint));
                }
                Step::Receive { .. } | Step::Assert { .. } => {}
            }
        }
    }
    owned_coverage(pairs)
}

/// Return the union of report keys with a strictly positive call count.
///
/// Counts are never summed, so even repeated maximum counters cannot overflow.
/// Scenario names and progress counters do not establish or suppress membership.
pub fn observed_coverage(reports: &[RunReport]) -> BTreeSet<CoverageKey> {
    reports.iter().flat_map(|report| report.calls.iter())
        .filter(|(_, count)| **count > 0)
        .map(|(key, _)| key.clone())
        .collect()
}

/// Partition required and observed pairs without changing either input.
///
/// Unexpected pairs remain visible even when all requirements are covered.
/// Completeness is determined solely by the absence of missing pairs.
pub fn compare_coverage(
    required: &BTreeSet<CoverageKey>,
    observed: &BTreeSet<CoverageKey>,
) -> CoverageReport {
    CoverageReport {
        required: required.clone(),
        covered: required.intersection(observed).cloned().collect(),
        missing: required.difference(observed).cloned().collect(),
        unexpected: observed.difference(required).cloned().collect(),
    }
}

fn connection_state<'s, 'a>(
    connections: &'s mut [(&str, Option<&'a str>)],
    name: &str,
    step: usize,
) -> Result<&'s mut Option<&'a str>> {
    let index = connections.binary_search_by(|entry| entry.0.cmp(name))
        .map_err(|_| invalid(Some(step), "coverage connection is absent from validated state"))?;
    Ok(&mut connections[index].1)
}

// Deduplicate borrowed pairs before allocating role strings. Output ordering is
// exactly CoverageKey's derived order: role first, then numeric endpoint code.
fn owned_coverage(mut pairs: Vec<(&str, u32)>) -> Result<BTreeSet<CoverageKey>> {
    pairs.sort_unstable();
    pairs.dedup();
    let mut coverage = BTreeSet::new();
    for (role, endpoint) in pairs {
        coverage.insert(CoverageKey { role: owned_string(role)?, endpoint });
    }
    Ok(coverage)
}

fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<()> {
    values.try_reserve(additional).map_err(|_| allocation_error())
}

fn owned_string(value: &str) -> Result<String> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|_| allocation_error())?;
    owned.push_str(value);
    Ok(owned)
}

fn allocation_error() -> RunnerError {
    RunnerError::Json(Error::new(
        ErrorCode::BudgetExhausted, ErrorSource::Decode, 0, "coverage allocation failed",
    ))
}

fn invalid(step: Option<usize>, detail: &'static str) -> RunnerError {
    match owned_string(detail) {
        Ok(detail) => RunnerError::InvalidScenario { step, detail },
        Err(error) => error,
    }
}
