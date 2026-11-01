//! Scripted batch replies shared by walker tests.

use crate::ax::bindings::AXError;
use crate::ax::content_attrs::AttrStatus;
use crate::ax::tree_source::{Read, ReadFailure, TreeBatch};

#[derive(Clone, Copy)]
pub(super) enum SlotFailure {
    Ax(AXError),
    WrongType,
}

impl SlotFailure {
    fn read<T>(self) -> Read<T> {
        Read::Unknown(match self {
            Self::Ax(error) => ReadFailure::Ax(error),
            Self::WrongType => ReadFailure::WrongType,
        })
    }
}

#[derive(Clone)]
pub(super) enum Slot {
    Text(AttrStatus),
    MixedText(String),
    Bool(Option<bool>),
    Number(Option<f64>),
    Point(Option<(f64, f64)>),
    Size(Option<(f64, f64)>),
    Failure(SlotFailure),
}

pub(super) struct Batch(pub(super) Vec<Slot>);

impl TreeBatch for Batch {
    fn text(&self, i: usize) -> AttrStatus {
        match &self.0[i] {
            Slot::Text(v) => v.clone(),
            Slot::MixedText(v) => AttrStatus::Known(v.clone()),
            _ => panic!("wrong text slot"),
        }
    }
    fn boolean(&self, i: usize) -> Read<bool> {
        match self.0[i] {
            Slot::Bool(v) => read(v),
            Slot::MixedText(_) => Read::Unknown(ReadFailure::WrongType),
            Slot::Failure(e) => e.read(),
            _ => panic!("wrong bool slot"),
        }
    }
    fn number(&self, i: usize) -> Read<f64> {
        match self.0[i] {
            Slot::Number(v) => read(v),
            Slot::Bool(None) => Read::Absent,
            Slot::Bool(Some(_)) | Slot::MixedText(_) => Read::Unknown(ReadFailure::WrongType),
            Slot::Failure(e) => e.read(),
            _ => panic!("wrong number slot"),
        }
    }
    fn position(&self, i: usize) -> Read<(f64, f64)> {
        match self.0[i] {
            Slot::Point(v) => read(v),
            Slot::Failure(e) => e.read(),
            _ => panic!("wrong position slot"),
        }
    }
    fn size(&self, i: usize) -> Read<(f64, f64)> {
        match self.0[i] {
            Slot::Size(v) => read(v),
            Slot::Failure(e) => e.read(),
            _ => panic!("wrong size slot"),
        }
    }
}

fn read<T>(value: Option<T>) -> Read<T> {
    value.map_or(Read::Absent, Read::Known)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::bindings::kAXErrorCannotComplete;

    #[test]
    fn scalar_slots_distinguish_known_absent_and_failures() {
        let batch = Batch(vec![
            Slot::Bool(Some(true)),
            Slot::Number(Some(2.0)),
            Slot::Point(Some((3.0, 4.0))),
            Slot::Size(Some((5.0, 6.0))),
            Slot::Bool(None),
            Slot::Number(None),
            Slot::Point(None),
            Slot::Size(None),
            Slot::Failure(SlotFailure::Ax(kAXErrorCannotComplete)),
            Slot::Failure(SlotFailure::WrongType),
        ]);
        assert!(matches!(batch.boolean(0), Read::Known(true)));
        assert!(matches!(batch.number(1), Read::Known(2.0)));
        assert!(matches!(batch.position(2), Read::Known((3.0, 4.0))));
        assert!(matches!(batch.size(3), Read::Known((5.0, 6.0))));
        assert!(matches!(batch.boolean(4), Read::Absent));
        assert!(matches!(batch.number(5), Read::Absent));
        assert!(matches!(batch.position(6), Read::Absent));
        assert!(matches!(batch.size(7), Read::Absent));

        macro_rules! check_failure {
            ($method:ident) => {
                assert!(matches!(batch.$method(8), Read::Unknown(ReadFailure::Ax(code)) if code == kAXErrorCannotComplete));
                assert!(matches!(batch.$method(9), Read::Unknown(ReadFailure::WrongType)));
            };
        }
        check_failure!(boolean);
        check_failure!(number);
        check_failure!(position);
        check_failure!(size);
    }
}
