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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::ToString, vec};

    fn key(role: &str, endpoint: u32) -> CoverageKey {
        CoverageKey { role: role.into(), endpoint }
    }

    fn set(pairs: &[(&str, u32)]) -> BTreeSet<CoverageKey> {
        pairs.iter().map(|(role, endpoint)| key(role, *endpoint)).collect()
    }

    fn endpoint(code: u32, roles: &[&str]) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code,
            name: "endpoint".into(),
            service: "service".into(),
            roles: roles.iter().map(|role| (*role).into()).collect(),
            params: Vec::new(),
            response: Vec::new(),
            stream_response: None,
            description: String::new(),
        }
    }

    fn service(id: u16, name: &str, endpoints: Vec<EndpointSchemaSpec>) -> ServiceSpec {
        ServiceSpec { id, name: name.into(), endpoints }
    }

    fn model(steps: Vec<Step>) -> Scenario {
        Scenario { name: "suite".into(), timeout_ms: DEFAULT_TIMEOUT_MS, steps }
    }

    fn connect(name: &str, role: &str) -> Step {
        Step::Connect { name: name.into(), uri: "custom://transport".into(), role: role.into() }
    }

    fn call(connection: &str, endpoint: u32, save_as: &str) -> Step {
        Step::Call {
            connection: connection.into(), endpoint,
            params: Payload::from_slice(b"null"), save_as: save_as.into(),
        }
    }

    fn close(connection: &str) -> Step {
        Step::Close { connection: connection.into() }
    }

    fn receive(connection: &str, save_as: &str) -> Step {
        Step::Receive { connection: connection.into(), save_as: save_as.into() }
    }

    fn assertion(response: &str, path: &str, expected: Expectation) -> Step {
        Step::Assert { response: response.into(), path: path.into(), expected }
    }

    fn report(pairs: &[(&str, u32, u64)]) -> RunReport {
        RunReport {
            scenario: "same-name".into(),
            calls: pairs.iter().map(|(role, endpoint, count)| (key(role, *endpoint), *count)).collect(),
            ..RunReport::default()
        }
    }

    fn assert_invalid<T: core::fmt::Debug>(result: Result<T>, expected: Option<usize>) -> String {
        match result.unwrap_err() {
            RunnerError::InvalidScenario { step, detail } => {
                assert_eq!(step, expected);
                assert!(!detail.is_empty());
                detail
            }
            other => panic!("expected scenario error, got {other:?}"),
        }
    }

    #[test]
    fn empty_catalogs_suites_and_reports_have_empty_complete_coverage() {
        let required = required_coverage(&[]).unwrap();
        assert!(required.is_empty());
        assert!(required_coverage(&[service(0, "", vec![])]).unwrap().is_empty());
        assert!(planned_coverage(&[]).unwrap().is_empty());
        assert!(planned_coverage(&[model(vec![])]).unwrap().is_empty());
        let observed = observed_coverage(&[]);
        assert!(observed.is_empty());
        assert!(observed_coverage(&[RunReport::default()]).is_empty());
        let comparison = compare_coverage(&required, &observed);
        assert_eq!(comparison, CoverageReport::default());
        assert!(comparison.is_complete());
    }

    #[test]
    fn declarations_expand_all_roles_and_only_empty_lists_gain_anonymous_access() {
        let catalog = vec![service(1, "service", vec![
            endpoint(0, &[]), endpoint(1, &["reader", "writer"]),
            endpoint(2, &[""]), endpoint(3, &["", "reader"]),
        ])];
        let original = catalog.clone();
        assert_eq!(required_coverage(&catalog).unwrap(), set(&[
            ("", 0), ("reader", 1), ("writer", 1), ("", 2), ("", 3), ("reader", 3),
        ]));
        assert_eq!(catalog, original);
        assert!(!required_coverage(&catalog).unwrap().contains(&key("", 1)));
    }

    #[test]
    fn identifiers_and_roles_preserve_case_unicode_whitespace_and_numeric_boundaries() {
        let catalog = [
            service(0, "Service", vec![endpoint(0, &["Public", "public", "λ", "Λ", "é", "e\u{301}", " "])]),
            service(u16::MAX, "service", vec![endpoint(u32::MAX, &["public"])]),
        ];
        let coverage = required_coverage(&catalog).unwrap();
        assert_eq!(coverage, set(&[
            ("Public", 0), ("public", 0), ("λ", 0), ("Λ", 0),
            ("é", 0), ("e\u{301}", 0), (" ", 0), ("public", u32::MAX),
        ]));
        assert_eq!(coverage.into_iter().collect::<Vec<_>>(), vec![
            key(" ", 0), key("Public", 0), key("e\u{301}", 0), key("public", 0),
            key("public", u32::MAX), key("é", 0), key("Λ", 0), key("λ", 0),
        ]);
    }

    #[test]
    fn duplicate_service_ids_and_names_are_rejected_even_without_endpoints() {
        for id in [0, u16::MAX] {
            let detail = assert_invalid(required_coverage(&[
                service(id, "a", vec![]), service(id, "b", vec![]),
            ]), None);
            assert!(detail.contains("IDs"));
        }
        for name in ["", "service", "λ"] {
            let detail = assert_invalid(required_coverage(&[
                service(0, name, vec![]), service(1, name, vec![]),
            ]), None);
            assert!(detail.contains("names"));
        }
    }

    #[test]
    fn duplicate_endpoint_codes_are_global_even_for_disjoint_roles() {
        for code in [0, 7, u32::MAX] {
            let detail = assert_invalid(required_coverage(&[
                service(1, "a", vec![endpoint(code, &["reader"]), endpoint(code, &["writer"])]),
            ]), None);
            assert!(detail.contains("endpoint codes"));
            assert_invalid(required_coverage(&[
                service(1, "a", vec![endpoint(code, &[])]),
                service(2, "b", vec![endpoint(code, &["private"])]),
            ]), None);
        }
    }

    #[test]
    fn repeated_roles_are_errors_including_anonymous_and_nonadjacent_repetitions() {
        for roles in [&["", ""][..], &["reader", "writer", "reader"], &["λ", "λ"]] {
            let detail = assert_invalid(required_coverage(&[
                service(1, "s", vec![endpoint(5, roles)]),
            ]), None);
            assert!(detail.contains("roles"));
        }
        assert_eq!(required_coverage(&[service(1, "s", vec![
            endpoint(1, &["reader"]), endpoint(2, &["reader"]),
        ])]).unwrap(), set(&[("reader", 1), ("reader", 2)]));
    }

    #[test]
    fn catalog_order_does_not_change_sets_or_duplicate_diagnostics() {
        let mut catalog = vec![
            service(2, "b", vec![endpoint(9, &["z", "a"]), endpoint(3, &[])]),
            service(1, "a", vec![endpoint(5, &["reader"])]),
        ];
        let expected = required_coverage(&catalog).unwrap();
        catalog.reverse();
        for service in &mut catalog {
            service.endpoints.reverse();
            for endpoint in &mut service.endpoints { endpoint.roles.reverse(); }
        }
        assert_eq!(required_coverage(&catalog).unwrap(), expected);
        catalog[0].endpoints.push(endpoint(9, &["secret-role", "secret-role"]));
        let error = required_coverage(&catalog).unwrap_err();
        catalog.reverse();
        assert_eq!(required_coverage(&catalog).unwrap_err(), error);
        assert!(matches!(error, RunnerError::InvalidScenario { step: None, detail }
            if detail.contains("endpoint codes")));
    }

    #[test]
    fn plans_follow_simultaneous_connections_close_and_reuse() {
        let scenario = model(vec![
            connect("c", "public"), connect("C", "Public"), connect("λ", "reader"),
            call("c", 0, "r0"), call("C", 0, "r1"), call("λ", 7, "r2"),
            close("c"), connect("c", ""), call("c", u32::MAX, "r3"),
            call("C", 7, "r4"), close("C"), close("λ"), close("c"),
            connect("c", "writer"), call("c", 0, "r5"),
        ]);
        assert_eq!(planned_coverage(&[scenario]).unwrap(), set(&[
            ("public", 0), ("Public", 0), ("reader", 7), ("", u32::MAX),
            ("Public", 7), ("writer", 0),
        ]));
    }

    #[test]
    fn plans_deduplicate_repeated_calls_connections_and_scenarios() {
        let first = model(vec![connect("c", "reader"), connect("d", "reader"),
            call("c", 8, "one"), call("c", 8, "two"), call("d", 8, "three")]);
        let second = model(vec![connect("c", "writer"), call("c", 8, "one")]);
        let scenarios = vec![first.clone(), second, first];
        let result = planned_coverage(&scenarios).unwrap();
        assert_eq!(result, set(&[("reader", 8), ("writer", 8)]));
        let reversed: Vec<_> = scenarios.iter().rev().cloned().collect();
        assert_eq!(planned_coverage(&reversed).unwrap(), result);
        assert_eq!(scenarios[0].name, "suite");
        match &scenarios[0].steps[2] {
            Step::Call { params, save_as, .. } => {
                assert_eq!(params.as_slice(), b"null");
                assert_eq!(save_as, "one");
            }
            other => panic!("expected call, got {other:?}"),
        }
    }

    #[test]
    fn plans_ignore_non_calls_and_never_carry_connections_between_scenarios() {
        let first = model(vec![connect("c", "reader"), receive("c", "r"), close("c"),
            assertion("r", "", Expectation::Exists), connect("c", "writer")]);
        assert!(planned_coverage(core::slice::from_ref(&first)).unwrap().is_empty());
        let second = model(vec![call("c", 1, "r")]);
        assert_invalid(planned_coverage(&[first, second]), Some(0));
    }

    #[test]
    fn all_scenarios_are_validated_and_original_errors_pass_through() {
        let valid = model(vec![connect("c", "reader"), call("c", 1, "r")]);
        let malformed = model(vec![connect("c", "reader"), call("c", 1, "r"),
            assertion("r", "", Expectation::JsonEquals(Payload::from_slice(b"[0,]")))]);
        let expected = scenario::validate_scenario(&malformed).unwrap_err();
        assert!(matches!(&expected, RunnerError::Json(_)));
        assert_eq!(planned_coverage(&[valid, malformed]).unwrap_err(), expected);

        let mut zero_timeout = model(vec![]);
        zero_timeout.timeout_ms = 0;
        let mut empty_name = model(vec![]);
        empty_name.name.clear();
        let mut invalid_params = call("c", 1, "r");
        if let Step::Call { params, .. } = &mut invalid_params {
            *params = Payload::from_slice(br#"{"a":0,"a":1}"#);
        }
        for invalid in [
            zero_timeout, empty_name,
            model(vec![connect("c", ""), connect("c", "other")]),
            model(vec![connect("c", ""), close("c"), call("c", 1, "r")]),
            model(vec![connect("c", ""), receive("c", "r"), call("c", 1, "r")]),
            model(vec![connect("c", ""), receive("c", "r"),
                assertion("r", "a..b", Expectation::Exists)]),
            model(vec![assertion("absent", "", Expectation::Missing)]),
            model(vec![connect("c", ""), invalid_params]),
        ] {
            let expected = scenario::validate_scenario(&invalid).unwrap_err();
            assert_eq!(planned_coverage(&[invalid]).unwrap_err(), expected);
        }
    }

    #[test]
    fn plans_accept_the_step_limit_and_reject_the_first_excess_step() {
        let mut steps = Vec::new();
        for index in 0..MAX_STEPS {
            steps.push(if index % 2 == 0 { connect("c", "reader") } else { close("c") });
        }
        let mut scenario = model(steps);
        scenario.timeout_ms = u32::MAX;
        assert!(planned_coverage(core::slice::from_ref(&scenario)).unwrap().is_empty());
        scenario.steps.push(connect("extra", ""));
        assert_invalid(planned_coverage(&[scenario]), None);
    }

    #[test]
    fn many_connection_names_use_exact_sorted_lookup() {
        let mut steps = Vec::new();
        for index in (0..64).rev() {
            steps.push(connect(&format!("c{index:02}"), &format!("role{index:02}")));
        }
        for index in 0..64 {
            steps.push(call(&format!("c{index:02}"), index, &format!("r{index:02}")));
        }
        let expected: BTreeSet<_> = (0..64).map(|index| key(&format!("role{index:02}"), index)).collect();
        assert_eq!(planned_coverage(&[model(steps)]).unwrap(), expected);
    }

    #[test]
    fn observed_requires_positive_counts_and_deduplicates_without_summing() {
        let mut reports = vec![
            report(&[("", 0, 0), ("reader", 1, 1), ("reader", 2, u64::MAX)]),
            report(&[("reader", 1, 0), ("reader", 2, u64::MAX), ("", 0, 1), ("Reader", u32::MAX, 1)]),
            report(&[("writer", 9, 0)]),
        ];
        reports[0].steps_completed = usize::MAX;
        reports[0].assertions_passed = usize::MAX;
        let original: Vec<_> = reports.iter().map(|report| (
            report.scenario.clone(), report.steps_completed, report.assertions_passed, report.calls.clone(),
        )).collect();
        let observed = observed_coverage(&reports);
        assert_eq!(observed, set(&[("", 0), ("reader", 1), ("reader", 2), ("Reader", u32::MAX)]));
        let after: Vec<_> = reports.iter().map(|report| (
            report.scenario.clone(), report.steps_completed, report.assertions_passed, report.calls.clone(),
        )).collect();
        assert_eq!(after, original);
        reports.reverse();
        assert_eq!(observed_coverage(&reports), observed);
        reports.push(reports[0].clone());
        assert_eq!(observed_coverage(&reports), observed);
    }

    #[test]
    fn comparison_preserves_requirements_and_separates_missing_and_unexpected() {
        let required = set(&[("", 0), ("reader", 1), ("reader", 2), ("writer", u32::MAX)]);
        let observed = set(&[("reader", 1), ("writer", u32::MAX), ("Reader", 1), ("other", 9)]);
        let required_before = required.clone();
        let observed_before = observed.clone();
        let comparison = compare_coverage(&required, &observed);
        assert_eq!(comparison.required, required);
        assert_eq!(comparison.covered, set(&[("reader", 1), ("writer", u32::MAX)]));
        assert_eq!(comparison.missing, set(&[("", 0), ("reader", 2)]));
        assert_eq!(comparison.unexpected, set(&[("Reader", 1), ("other", 9)]));
        assert!(!comparison.is_complete());
        assert_eq!(required, required_before);
        assert_eq!(observed, observed_before);
        assert!(comparison.covered.is_disjoint(&comparison.missing));
        assert!(comparison.covered.is_disjoint(&comparison.unexpected));
    }

    #[test]
    fn completeness_depends_on_missing_pairs_and_all_empty_side_cases_are_exact() {
        let empty = BTreeSet::new();
        let required = set(&[("reader", 1)]);
        let observed = set(&[("reader", 1), ("unexpected", 2)]);
        let complete = compare_coverage(&required, &observed);
        assert!(complete.is_complete());
        assert_eq!(complete.covered, required);
        assert_eq!(complete.unexpected, set(&[("unexpected", 2)]));
        let no_requirements = compare_coverage(&empty, &observed);
        assert!(no_requirements.is_complete());
        assert!(no_requirements.required.is_empty());
        assert!(no_requirements.covered.is_empty());
        assert_eq!(no_requirements.unexpected, observed);
        let no_observations = compare_coverage(&required, &empty);
        assert!(!no_observations.is_complete());
        assert_eq!(no_observations.missing, required);
        assert!(no_observations.covered.is_empty());
        assert!(no_observations.unexpected.is_empty());
        let equal = compare_coverage(&required, &required);
        assert!(equal.is_complete());
        assert_eq!(equal.covered, required);
        assert!(equal.missing.is_empty());
        assert!(equal.unexpected.is_empty());
    }

    #[test]
    fn exhaustive_small_sets_satisfy_partition_and_completeness_laws() {
        let universe = [key("", 0), key("Reader", 1), key("reader", 1), key("λ", u32::MAX)];
        for required_mask in 0u8..16 {
            for observed_mask in 0u8..16 {
                let required: BTreeSet<_> = universe.iter().enumerate()
                    .filter(|(index, _)| required_mask & (1u8 << *index) != 0)
                    .map(|(_, key)| key.clone()).collect();
                let observed: BTreeSet<_> = universe.iter().enumerate()
                    .filter(|(index, _)| observed_mask & (1u8 << *index) != 0)
                    .map(|(_, key)| key.clone()).collect();
                let comparison = compare_coverage(&required, &observed);
                assert_eq!(comparison.required, required);
                assert_eq!(comparison.is_complete(), required_mask & !observed_mask == 0);
                for (index, key) in universe.iter().enumerate() {
                    let required = required_mask & (1 << index) != 0;
                    let observed = observed_mask & (1 << index) != 0;
                    assert_eq!(comparison.covered.contains(key), required && observed);
                    assert_eq!(comparison.missing.contains(key), required && !observed);
                    assert_eq!(comparison.unexpected.contains(key), !required && observed);
                }
                assert_eq!(comparison.covered.len() + comparison.missing.len(), required.len());
                assert_eq!(comparison.covered.len() + comparison.unexpected.len(), observed.len());
            }
        }
    }

    #[test]
    fn planning_never_fills_observed_gaps_and_outputs_own_their_roles() {
        let (required, planned, observed) = {
            let catalog = [service(1, "s", vec![endpoint(5, &["reader"]), endpoint(6, &[])])];
            let scenarios = [model(vec![connect("c", "reader"), call("c", 5, "r")]),
                model(vec![connect("c", ""), call("c", 6, "r")])];
            let reports = [report(&[("reader", 5, 1), ("", 6, 0)])];
            (required_coverage(&catalog).unwrap(), planned_coverage(&scenarios).unwrap(),
                observed_coverage(&reports))
        };
        assert_eq!(required, planned);
        assert!(compare_coverage(&required, &planned).is_complete());
        let comparison = compare_coverage(&required, &observed);
        assert!(!comparison.is_complete());
        assert_eq!(comparison.missing, set(&[("", 6)]));
        assert_eq!(comparison.covered, set(&[("reader", 5)]));
    }

    #[test]
    fn deterministic_capacity_overflow_returns_canonical_budget_error_without_mutation() {
        let mut bytes = vec![7u8];
        match reserve(&mut bytes, usize::MAX).unwrap_err() {
            RunnerError::Json(error) => {
                assert_eq!(error.code, ErrorCode::BudgetExhausted);
                assert_eq!(error.source, ErrorSource::Decode);
                assert_eq!(error.byte_offset, 0);
                assert_eq!(error, Error::new(ErrorCode::BudgetExhausted, ErrorSource::Decode,
                    0, "coverage allocation failed"));
            }
            other => panic!("expected allocation error, got {other:?}"),
        }
        assert_eq!(bytes, vec![7]);
        reserve(&mut bytes, 0).unwrap();
        assert_eq!(owned_string("").unwrap(), "");
        assert_eq!(owned_string("λ🚀").unwrap(), "λ🚀");
    }

    #[test]
    fn defensive_connection_lookup_reports_missing_state_with_step() {
        let mut connections = [("c", Some("reader"))];
        assert_eq!(*connection_state(&mut connections, "c", 2).unwrap(), Some("reader"));
        assert_invalid(connection_state(&mut connections, "absent", 7), Some(7));
        assert_eq!(connections, [("c", Some("reader"))]);
    }

    #[test]
    fn diagnostics_do_not_echo_catalog_or_connection_values() {
        let secret = "private-credential-value";
        let errors = [
            required_coverage(&[service(1, secret, vec![]), service(2, secret, vec![])]).unwrap_err(),
            required_coverage(&[service(1, "s", vec![endpoint(1, &[secret, secret])])]).unwrap_err(),
            planned_coverage(&[model(vec![call(secret, 1, "r")])]).unwrap_err(),
        ];
        for error in errors { assert!(!error.to_string().contains(secret)); }
    }
}
