//! Reusable, fail-closed platform conformance probes. See `CONFORMANCE.md`.
use crate::{Driver, ToolResponse};
use cua_driver_contract::ErrorDisposition;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractOutcome {
    Pass,
    Fail { observed: String },
    NotWired { reason: String },
    NotSupported { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Contract {
    Ambiguity,
    Stale,
    ForeignSession,
    GoneReadError,
    Disposition,
    Actionability,
    Completeness,
}
pub type ContractReport = BTreeMap<Contract, ContractOutcome>;

/// Two independently scoped clients to the same daemon. Callers must establish
/// both shared-daemon identity and per-client session continuity; CLI clients
/// must remain unsupported until a labeled read/act round trip proves this.
pub struct SessionPair<'a> {
    pub a: &'a mut dyn Driver,
    pub b: &'a mut dyn Driver,
    pub session_evidence: Result<(), String>,
}

/// Fixture control must be independent of the driver's responses. The failing
/// read mode must leave the window authoritative and report actual read errors.
pub trait ConformanceFixture {
    fn pid(&self) -> i32;
    fn window_id(&self) -> u64;
    /// Index of a destructible element in the current full window read.
    fn destructible_index(&self) -> u64;
    fn text_field_name(&self) -> &str;
    fn disabled_button_name(&self) -> &str;
    fn destroy_element(&mut self) -> Result<(), String>;
    fn dispatch_count(&self) -> Result<u64, String>;
    fn start_read_failure(&mut self) -> Result<(), String>;
    /// Restore reads and return the number of injected read errors observed.
    fn finish_read_failure(&mut self) -> Result<u64, String>;
}

fn fail(value: impl Into<String>) -> ContractOutcome {
    ContractOutcome::Fail {
        observed: value.into(),
    }
}
fn describe(r: &ToolResponse) -> String {
    format!(
        "error={} text={:?} structured={}",
        r.is_error(),
        r.text(),
        r.structured()
    )
}
fn code(r: &ToolResponse) -> Option<&str> {
    let s = r.structured();
    s.pointer("/steps/0/code")
        .or_else(|| s.pointer("/refusal/code"))
        .or_else(|| s.get("code"))
        .and_then(Value::as_str)
}
fn disposition(r: &ToolResponse) -> bool {
    r.structured()
        .get("disposition")
        .cloned()
        .and_then(|v| serde_json::from_value::<ErrorDisposition>(v).ok())
        .is_some_and(ErrorDisposition::is_valid)
}
fn no_dispatch(before: Result<u64, String>, after: Result<u64, String>) -> bool {
    matches!((before, after), (Ok(a), Ok(b)) if a == b)
}
fn read(d: &mut dyn Driver, f: &dyn ConformanceFixture) -> ToolResponse {
    d.call(
        "get_window_state",
        json!({"pid":f.pid(), "window_id":f.window_id(), "full_output":true}),
    )
}
fn token(d: &mut dyn Driver, f: &dyn ConformanceFixture) -> Result<(String, String), String> {
    let r = read(d, f);
    let token = r.structured()["elements"]
        .as_array()
        .and_then(|es| {
            es.iter()
                .find(|e| e["element_index"] == f.destructible_index())
        })
        .and_then(|e| e["element_token"].as_str())
        .filter(|s| !s.is_empty());
    let snapshot = r.structured()["snapshot_id"]
        .as_str()
        .filter(|s| !s.is_empty());
    if let (false, Some((token, snapshot))) = (r.is_error(), token.zip(snapshot)) {
        return Ok((token.to_owned(), snapshot.to_owned()));
    }
    Err(format!("token read failed: {}", describe(&r)))
}
fn click(d: &mut dyn Driver, f: &dyn ConformanceFixture, token: &str) -> ToolResponse {
    d.call(
        "click",
        json!({"pid":f.pid(), "window_id":f.window_id(), "element_token":token}),
    )
}
fn ambiguity_response(d: &mut dyn Driver, f: &dyn ConformanceFixture) -> ToolResponse {
    d.call(
        "run_steps",
        json!({"steps":[{"click":{"pid":f.pid(),"window_id":f.window_id(),"name":"Save"}}]}),
    )
}
fn stale_response(
    d: &mut dyn Driver,
    f: &mut dyn ConformanceFixture,
) -> Result<ToolResponse, String> {
    let (old, snapshot) = token(d, f)?;
    f.destroy_element()?;
    let fresh = read(d, f);
    if fresh.is_error()
        || fresh
            .structured()
            .get("snapshot_id")
            .and_then(Value::as_str)
            .is_none_or(|id| id.is_empty() || id == snapshot)
        || fresh.structured()["elements"]
            .as_array()
            .is_none_or(|elements| elements.iter().any(|e| e["element_token"] == old))
    {
        return Err(format!("invalidation read failed: {}", describe(&fresh)));
    }
    Ok(click(d, f, &old))
}
fn foreign_response(
    pair: &mut SessionPair<'_>,
    f: &dyn ConformanceFixture,
) -> Result<ToolResponse, String> {
    let (old, _) = token(pair.a, f)?;
    Ok(click(pair.b, f, &old))
}
fn gone_response(
    d: &mut dyn Driver,
    f: &mut dyn ConformanceFixture,
) -> Result<ToolResponse, String> {
    f.start_read_failure()?;
    let r = d.call("run_steps", json!({"steps":[{"wait_for":{"pid":f.pid(),"window_id":f.window_id(),"name":"Save","gone":true,"timeout_ms":0}}]}));
    let errors = f.finish_read_failure()?;
    if errors == 0 {
        return Err(format!("no injected read error observed: {}", describe(&r)));
    }
    Ok(r)
}
fn check_response(r: &ToolResponse, expected: &[&str]) -> bool {
    r.is_error() && code(r).is_some_and(|c| expected.contains(&c))
}
fn observed_result(r: ToolResponse, valid: bool, silent: bool) -> ContractOutcome {
    if valid && silent {
        ContractOutcome::Pass
    } else {
        fail(format!("{}; no_dispatch={silent}", describe(&r)))
    }
}

pub fn ambiguity(d: &mut dyn Driver, f: &dyn ConformanceFixture) -> ContractOutcome {
    let before = f.dispatch_count();
    let r = ambiguity_response(d, f);
    let valid = check_response(&r, &["ambiguous"])
        && r.text().contains("nth 0:")
        && r.text().contains("nth 1:");
    observed_result(r, valid, no_dispatch(before, f.dispatch_count()))
}
pub fn stale(d: &mut dyn Driver, f: &mut dyn ConformanceFixture) -> ContractOutcome {
    let before = f.dispatch_count();
    match stale_response(d, f) {
        Ok(r) => {
            let valid = check_response(&r, &["stale_element_token"])
                && r.structured()["disposition"]
                    == json!({"delivery":"not_delivered","retry":"safe"});
            observed_result(r, valid, no_dispatch(before, f.dispatch_count()))
        }
        Err(e) => fail(e),
    }
}
pub fn foreign_session(pair: &mut SessionPair<'_>, f: &dyn ConformanceFixture) -> ContractOutcome {
    if let Err(reason) = &pair.session_evidence {
        return ContractOutcome::NotSupported {
            reason: reason.clone(),
        };
    }
    let before = f.dispatch_count();
    match foreign_response(pair, f) {
        Ok(r) => {
            let valid = check_response(&r, &["stale_element_token"])
                && (r.text().contains("belongs to another session")
                    || r.structured()
                        .to_string()
                        .contains("belongs to another session"));
            observed_result(r, valid, no_dispatch(before, f.dispatch_count()))
        }
        Err(e) => fail(e),
    }
}
pub fn gone_read_error(d: &mut dyn Driver, f: &mut dyn ConformanceFixture) -> ContractOutcome {
    let before = f.dispatch_count();
    match gone_response(d, f) {
        Ok(r) => {
            let valid = check_response(&r, &["gone_unconfirmed", "lookup_failed"]);
            observed_result(r, valid, no_dispatch(before, f.dispatch_count()))
        }
        Err(e) => fail(e),
    }
}
/// Checks the disposition of each actual refusal from contracts 1–4. A fresh
/// fixture is required because the stale scenario destroys its element.
pub fn dispositions(pair: &mut SessionPair<'_>, f: &mut dyn ConformanceFixture) -> ContractOutcome {
    if let Err(reason) = &pair.session_evidence {
        return ContractOutcome::NotSupported {
            reason: reason.clone(),
        };
    }
    let before = f.dispatch_count();
    let a = ambiguity_response(pair.a, f);
    let foreign = foreign_response(pair, f);
    let gone = gone_response(pair.a, f);
    let stale = stale_response(pair.a, f);
    let responses = [Ok(a), foreign, gone, stale];
    let expected: [&[&str]; 4] = [
        &["ambiguous"],
        &["stale_element_token"],
        &["gone_unconfirmed", "lookup_failed"],
        &["stale_element_token"],
    ];
    for (index, result) in responses.into_iter().enumerate() {
        match result {
            Ok(r) if check_response(&r, expected[index]) && disposition(&r) => {}
            Ok(r) => return fail(format!("refusal {index}: {}", describe(&r))),
            Err(e) => return fail(format!("refusal {index}: {e}")),
        }
    }
    if no_dispatch(before, f.dispatch_count()) {
        ContractOutcome::Pass
    } else {
        fail("dispatch count changed or unavailable")
    }
}
pub fn actionability() -> ContractOutcome {
    ContractOutcome::NotWired {
        reason: "platform actionability fixture is not wired".into(),
    }
}
pub fn completeness() -> ContractOutcome {
    ContractOutcome::NotWired {
        reason: "platform completeness fixture is not wired".into(),
    }
}

/// A missing, failed, unwired, or unsupported required contract fails the job.
pub fn assert_required(report: &ContractReport, required: &[Contract]) {
    let failures: Vec<_> = required
        .iter()
        .filter_map(|c| match report.get(c) {
            Some(ContractOutcome::Pass) => None,
            other => Some(format!("{c:?}: {other:?}")),
        })
        .collect();
    assert!(
        failures.is_empty(),
        "required conformance failures: {}",
        failures.join(", ")
    );
}
