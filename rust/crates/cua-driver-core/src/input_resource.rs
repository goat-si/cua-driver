use crate::guarded_dispatch::Emitted;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceState {
    Released,
    PossiblyPressed,
    Pressed,
    PossiblyReleased,
}

struct Entry<R> {
    resource: R,
    state: ResourceState,
    before_release: Option<ResourceState>,
}

pub struct ResourceTable<R> {
    entries: Vec<Entry<R>>,
}

impl<R> Default for ResourceTable<R> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<R: Eq + Clone> ResourceTable<R> {
    pub fn state(&self, resource: &R) -> ResourceState {
        self.entries
            .iter()
            .find(|entry| &entry.resource == resource)
            .map_or(ResourceState::Released, |entry| entry.state)
    }

    pub fn press_submit(&mut self, resource: R) {
        assert_eq!(self.state(&resource), ResourceState::Released);
        self.entries.push(Entry {
            resource,
            state: ResourceState::PossiblyPressed,
            before_release: None,
        });
    }

    pub fn press_result(&mut self, resource: &R, result: Result<(), Emitted>) {
        let index = self
            .entries
            .iter()
            .position(|entry| &entry.resource == resource)
            .expect("press not submitted");
        assert_eq!(self.entries[index].state, ResourceState::PossiblyPressed);
        match result {
            Ok(()) => self.entries[index].state = ResourceState::Pressed,
            Err(Emitted::NotEmitted) => {
                self.entries.remove(index);
            }
            Err(Emitted::Unknown) => {}
        }
    }

    pub fn release_submit(&mut self, resource: &R) {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| &entry.resource == resource)
            .expect("resource not acquired");
        assert!(entry.before_release.is_none());
        entry.before_release = Some(entry.state);
        entry.state = ResourceState::PossiblyReleased;
    }

    pub fn release_result(&mut self, resource: &R, result: Result<(), Emitted>) {
        let index = self
            .entries
            .iter()
            .position(|entry| &entry.resource == resource)
            .expect("release not submitted");
        let previous = self.entries[index]
            .before_release
            .take()
            .expect("release not submitted");
        match result {
            Ok(()) => {
                self.entries.remove(index);
            }
            Err(Emitted::NotEmitted) => self.entries[index].state = previous,
            Err(Emitted::Unknown) => {}
        }
    }

    pub fn needing_cleanup(&self) -> Vec<R> {
        self.entries
            .iter()
            .rev()
            .map(|entry| entry.resource.clone())
            .collect()
    }
}
