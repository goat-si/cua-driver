//! Decorates an existing source without changing its requests or results.
use super::batch_attrs::BatchFailure;
use super::bindings::{AXError, StringishAttrValue};
use super::content_attrs::AttrStatus;
use super::evidence::{attr_status, read_status, EvidenceStatus, Observed, ReadEvidence, ReadSite};
use super::tree_source::{AxTreeSource, Read, ReadFailure, TreeBatch};
use super::write_outcome::Settable;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone, Copy)]
pub(crate) enum Domain {
    Control,
    Frame,
    Scrollbar,
    Tab,
    Dense,
}

impl Domain {
    fn site(self, name: &str) -> Option<ReadSite> {
        // Only known attribute names become metadata; target-provided strings
        // can never enter the evidence stream.
        let name = match name {
            "AXEnabled" => "AXEnabled",
            "AXPosition" => "AXPosition",
            "AXSize" => "AXSize",
            "AXValueDescription" => "AXValueDescription",
            "AXMinValue" => "AXMinValue",
            "AXMaxValue" => "AXMaxValue",
            "AXSelected" => "AXSelected",
            "AXVerticalScrollBar" => "AXVerticalScrollBar",
            "AXHorizontalScrollBar" => "AXHorizontalScrollBar",
            "AXRole" => "AXRole",
            "AXValue" => "AXValue",
            "AXTitle" => "AXTitle",
            "AXDescription" => "AXDescription",
            "AXRoleDescription" => "AXRoleDescription",
            "AXRows" => "AXRows",
            "AXVisibleRows" => "AXVisibleRows",
            "AXVisibleChildren" => "AXVisibleChildren",
            _ => return None,
        };
        Some(match self {
            Self::Control => ReadSite::Control(name),
            Self::Frame => ReadSite::Frame(name),
            Self::Scrollbar => ReadSite::Scrollbar(name),
            Self::Tab => ReadSite::Tab(name),
            Self::Dense => ReadSite::Dense(name),
        })
    }
    fn batch_site(self) -> ReadSite {
        ReadSite::Batch(match self {
            Self::Control => "control",
            Self::Frame => "frame",
            Self::Scrollbar => "scrollbar",
            Self::Tab => "tab",
            Self::Dense => "dense",
        })
    }
}

#[derive(Default)]
struct State {
    evidence: Vec<ReadEvidence>,
    attempt: u32,
}

pub(crate) struct EvidenceSource<'a, S> {
    source: &'a S,
    domain: Domain,
    owner: Option<usize>,
    state: Rc<RefCell<State>>,
}

impl<'a, S: AxTreeSource> EvidenceSource<'a, S> {
    pub(crate) fn new(source: &'a S, domain: Domain, owner: Option<usize>) -> Self {
        Self {
            source,
            domain,
            owner,
            state: Rc::new(RefCell::new(State::default())),
        }
    }
    pub(crate) fn finish<T>(&self, legacy: T) -> Observed<T> {
        Observed {
            legacy,
            evidence: std::mem::take(&mut self.state.borrow_mut().evidence),
        }
    }
    pub(crate) fn note(&self, site: ReadSite, status: EvidenceStatus) {
        let owner = self.owner.or_else(super::evidence::current_owner);
        let mut state = self.state.borrow_mut();
        let attempt = state.attempt;
        state.evidence.push(ReadEvidence {
            owner,
            site,
            status,
            attempt,
            listing: None,
        });
    }
    fn owner_for(&self, element: &S::Element) -> Option<usize> {
        match self.domain {
            Domain::Scrollbar | Domain::Tab | Domain::Dense => Some(self.source.key(element)),
            Domain::Control | Domain::Frame => self.owner.or_else(super::evidence::current_owner),
        }
    }
    fn record<T>(&self, element: &S::Element, name: &str, result: &Read<T>) {
        self.record_status(element, name, read_status(result));
    }
    fn record_status(&self, element: &S::Element, name: &str, status: EvidenceStatus) {
        if let Some(site) = self.domain.site(name) {
            let owner = self.owner_for(element);
            let mut state = self.state.borrow_mut();
            let attempt = state.attempt;
            state.evidence.push(ReadEvidence {
                owner,
                site,
                status,
                attempt,
                listing: None,
            });
        }
    }
}

pub(crate) struct EvidenceBatch<B> {
    inner: B,
    sites: Vec<Option<ReadSite>>,
    owner: Option<usize>,
    attempt: u32,
    state: Rc<RefCell<State>>,
}

impl<B: TreeBatch> EvidenceBatch<B> {
    fn record(
        &self,
        slot: usize,
        status: EvidenceStatus,
        listing: Option<super::content_attrs::AttributeListing>,
    ) {
        if let Some(site) = self.sites[slot] {
            self.state.borrow_mut().evidence.push(ReadEvidence {
                owner: self.owner,
                site,
                status,
                attempt: self.attempt,
                listing,
            });
        }
    }
}

impl<B: TreeBatch> TreeBatch for EvidenceBatch<B> {
    fn text(&self, slot: usize) -> AttrStatus {
        let value = self.inner.text(slot);
        let (status, listing) = attr_status(&value);
        self.record(slot, status, listing);
        value
    }
    fn boolean(&self, slot: usize) -> Read<bool> {
        let value = self.inner.boolean(slot);
        self.record(slot, read_status(&value), None);
        value
    }
    fn number(&self, slot: usize) -> Read<f64> {
        let value = self.inner.number(slot);
        self.record(slot, read_status(&value), None);
        value
    }
    fn position(&self, slot: usize) -> Read<(f64, f64)> {
        let value = self.inner.position(slot);
        self.record(slot, read_status(&value), None);
        value
    }
    fn size(&self, slot: usize) -> Read<(f64, f64)> {
        let value = self.inner.size(slot);
        self.record(slot, read_status(&value), None);
        value
    }
}

impl<S: AxTreeSource> AxTreeSource for EvidenceSource<'_, S> {
    type Element = S::Element;
    type Batch = EvidenceBatch<S::Batch>;
    type ElementSet = S::ElementSet;
    fn budget_expired(&self, budget: &cua_driver_core::walk_budget::WalkBudget) -> bool {
        self.source.budget_expired(budget)
    }
    fn key(&self, e: &Self::Element) -> usize {
        self.source.key(e)
    }
    fn set_messaging_timeout(&self, e: &Self::Element) {
        self.source.set_messaging_timeout(e)
    }
    fn set_optional_timeout(&self, e: &Self::Element, seconds: f32) {
        self.source.set_optional_timeout(e, seconds)
    }
    fn text(&self, e: &Self::Element, name: &str) -> Read<String> {
        let v = self.source.text(e, name);
        self.record(e, name, &v);
        if name == "AXRole"
            && matches!(&v, Read::Known(role) if role.trim().is_empty() || role == "AXUnknown")
        {
            self.record_status(e, name, EvidenceStatus::Unavailable);
        }
        v
    }
    fn text_status(&self, e: &Self::Element, name: &str) -> AttrStatus {
        let v = self.source.text_status(e, name);
        if let Some(site) = self.domain.site(name) {
            let (status, listing) = attr_status(&v);
            let owner = self.owner_for(e);
            let mut state = self.state.borrow_mut();
            let attempt = state.attempt;
            state.evidence.push(ReadEvidence {
                owner,
                site,
                status,
                attempt,
                listing,
            });
        }
        v
    }
    fn boolean(&self, e: &Self::Element, name: &str) -> Read<bool> {
        let v = self.source.boolean(e, name);
        self.record(e, name, &v);
        v
    }
    fn number(&self, e: &Self::Element, name: &str) -> Read<f64> {
        let v = self.source.number(e, name);
        self.record(e, name, &v);
        v
    }
    fn position(&self, e: &Self::Element) -> Read<(f64, f64)> {
        let v = self.source.position(e);
        self.record(e, "AXPosition", &v);
        v
    }
    fn size(&self, e: &Self::Element) -> Read<(f64, f64)> {
        let v = self.source.size(e);
        self.record(e, "AXSize", &v);
        v
    }
    fn value(&self, e: &Self::Element) -> Result<Option<StringishAttrValue>, AXError> {
        self.source.value(e)
    }
    fn observed_value(
        &self,
        e: &Self::Element,
    ) -> Observed<Result<Option<StringishAttrValue>, AXError>> {
        self.source.observed_value(e)
    }
    fn batch(&self, e: &Self::Element, names: &[&str]) -> Result<Self::Batch, BatchFailure> {
        let attempt = self.state.borrow().attempt;
        let result = self.source.batch(e, names);
        let owner = self.owner_for(e);
        match result {
            Ok(inner) => Ok(EvidenceBatch {
                inner,
                sites: names.iter().map(|name| self.domain.site(name)).collect(),
                owner,
                attempt,
                state: self.state.clone(),
            }),
            Err(error) => {
                let status = match error {
                    BatchFailure::WholeCall(code) => EvidenceStatus::Unknown(ReadFailure::Ax(code)),
                    BatchFailure::Decode => EvidenceStatus::Unknown(ReadFailure::WrongType),
                };
                let mut state = self.state.borrow_mut();
                state.evidence.push(ReadEvidence {
                    owner,
                    site: self.domain.batch_site(),
                    status,
                    attempt,
                    listing: None,
                });
                state.attempt = state.attempt.saturating_add(1);
                Err(error)
            }
        }
    }
    fn children(&self, e: &Self::Element) -> Read<Vec<Self::Element>> {
        self.source.children(e)
    }
    fn element_attribute(&self, e: &Self::Element, name: &str) -> Read<Self::Element> {
        let v = self.source.element_attribute(e, name);
        self.record(e, name, &v);
        v
    }
    fn element_set(&self, e: &Self::Element, name: &str) -> Read<Self::ElementSet> {
        let v = self.source.element_set(e, name);
        self.record(e, name, &v);
        v
    }
    fn set_contains(&self, set: &Self::ElementSet, e: &Self::Element) -> bool {
        self.source.set_contains(set, e)
    }
    fn action_names(&self, e: &Self::Element) -> Read<Vec<String>> {
        self.source.action_names(e)
    }
    fn value_settable(&self, e: &Self::Element) -> Settable {
        self.source.value_settable(e)
    }
    fn selected_settable(&self, e: &Self::Element) -> Settable {
        self.source.selected_settable(e)
    }
    fn frame(&self, e: &Self::Element) -> Read<[f64; 4]> {
        self.source.frame(e)
    }
    fn retain_for_snapshot(&self, e: &Self::Element) -> usize {
        self.source.retain_for_snapshot(e)
    }
}
