use super::ComponentError;
use std::collections::VecDeque;
use std::io::Read;
use wasmtime::component::Component;

/// Bounds retained compiler artifacts separately from each guest's memory cap.
/// These are not a cap on Wasmtime compiler working memory or process RSS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompilationLimits {
    /// Maximum bytes read from one Wasm binary or WAT source (default 16 MiB).
    pub max_input_bytes: usize,
    /// Maximum retained components (default 32). Zero disables caching.
    pub max_cache_entries: usize,
    /// Sum of retained source bytes and compiled image lengths (default 64 MiB).
    /// Wasmtime metadata and artifacts retained by live instances are separate.
    /// Zero disables caching; larger components can run without being cached.
    pub max_cache_bytes: usize,
}

impl Default for CompilationLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_cache_entries: 32,
            max_cache_bytes: 64 * 1024 * 1024,
        }
    }
}

impl CompilationLimits {
    pub(super) fn validate(&self) -> Result<(), ComponentError> {
        if self.max_input_bytes == 0 || self.max_input_bytes >= isize::MAX as usize {
            return Err(ComponentError::InvalidConfig(
                "max_input_bytes must be positive and leave room for an overflow sentinel".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn check_input(&self, len: usize) -> Result<(), ComponentError> {
        if len > self.max_input_bytes {
            Err(ComponentError::ResourceLimit(format!(
                "component input exceeds {} bytes",
                self.max_input_bytes
            )))
        } else {
            Ok(())
        }
    }

    pub(super) fn read(&self, reader: impl Read) -> Result<Vec<u8>, ComponentError> {
        // No metadata-sized reservation. Recheck actual bytes even if a file
        // grows after stat, and stop reading at the first excess byte.
        let mut bytes = Vec::new();
        reader
            .take(self.max_input_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| ComponentError::AbiVersion(format!("read component: {e}")))?;
        self.check_input(bytes.len())?;
        Ok(bytes)
    }
}

struct Entry {
    source: Box<[u8]>,
    component: Component,
    weight: usize,
}

#[derive(Default)]
pub(super) struct Cache {
    entries: VecDeque<Entry>,
    retained_bytes: usize,
}

impl Cache {
    pub(super) fn get(&mut self, source: &[u8]) -> Option<Component> {
        // Exact byte equality is the identity check. A digest collision cannot
        // substitute another guest. The bounded small cache uses an LRU list.
        let index = self
            .entries
            .iter()
            .position(|entry| entry.source.as_ref() == source)?;
        let entry = self.entries.remove(index)?;
        let component = entry.component.clone();
        self.entries.push_back(entry);
        Some(component)
    }

    pub(super) fn insert(
        &mut self,
        source: &[u8],
        component: Component,
        limits: &CompilationLimits,
    ) {
        let image = component.image_range();
        let Some(weight) = source
            .len()
            .checked_add(image.end as usize - image.start as usize)
        else {
            return;
        };
        if limits.max_cache_entries == 0
            || limits.max_cache_bytes == 0
            || weight > limits.max_cache_bytes
        {
            return;
        }
        while self.entries.len() >= limits.max_cache_entries
            || self.retained_bytes > limits.max_cache_bytes - weight
        {
            if let Some(entry) = self.entries.pop_front() {
                self.retained_bytes -= entry.weight;
            } else {
                break;
            }
        }
        self.entries.push_back(Entry {
            source: source.into(),
            component,
            weight,
        });
        self.retained_bytes += weight;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled() -> Component {
        Component::new(&wasmtime::Engine::default(), "(component)").unwrap()
    }

    #[test]
    fn byte_budget_counts_both_source_and_compiled_image() {
        let component = compiled();
        let image = component.image_range();
        let weight = 1 + image.end as usize - image.start as usize;
        let limits = CompilationLimits {
            max_cache_bytes: weight,
            ..Default::default()
        };
        let mut cache = Cache::default();
        cache.insert(b"a", component.clone(), &limits);
        assert_eq!(cache.retained_bytes, weight);
        cache.insert(b"b", component.clone(), &limits);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.get(b"a").is_none());
        assert!(Component::same(&component, &cache.get(b"b").unwrap()));
        // An uncacheable component must not flush useful existing entries.
        cache.insert(b"too big", component, &limits);
        assert_eq!(cache.retained_bytes, weight);
        assert!(cache.get(b"b").is_some());
    }

    #[test]
    fn cache_hit_updates_lru_and_eviction_does_not_invalidate_live_components() {
        let component = compiled();
        let limits = CompilationLimits {
            max_cache_entries: 2,
            ..Default::default()
        };
        let mut cache = Cache::default();
        cache.insert(b"a", component.clone(), &limits);
        cache.insert(b"b", component.clone(), &limits);
        let retained = cache.get(b"a").unwrap();
        cache.insert(b"c", component.clone(), &limits);
        assert!(cache.get(b"b").is_none());
        assert!(cache.get(b"a").is_some());
        cache.insert(b"d", component.clone(), &limits);
        cache.insert(b"e", component.clone(), &limits);
        assert!(cache.get(b"a").is_none());
        let engine = component.engine();
        let mut store = wasmtime::Store::new(engine, ());
        wasmtime::component::Linker::new(engine)
            .instantiate(&mut store, &retained)
            .unwrap();
    }

    #[test]
    fn disabled_cache_retains_nothing() {
        let component = compiled();
        for limits in [
            CompilationLimits {
                max_cache_entries: 0,
                ..Default::default()
            },
            CompilationLimits {
                max_cache_bytes: 0,
                ..Default::default()
            },
        ] {
            let mut cache = Cache::default();
            cache.insert(b"a", component.clone(), &limits);
            assert!(cache.entries.is_empty());
            assert_eq!(cache.retained_bytes, 0);
        }
    }

    #[test]
    fn reader_stops_at_limit_plus_one_without_trusting_metadata() {
        let limits = CompilationLimits {
            max_input_bytes: 10,
            ..Default::default()
        };
        let mut input = std::io::Cursor::new([b'x'; 100]);
        assert!(matches!(
            limits.read(&mut input),
            Err(ComponentError::ResourceLimit(_))
        ));
        assert_eq!(input.position(), 11);
        assert_eq!(limits.read(&b"0123456789"[..]).unwrap(), b"0123456789");
        for max_input_bytes in [0, usize::MAX] {
            assert!(CompilationLimits {
                max_input_bytes,
                ..limits
            }
            .validate()
            .is_err());
        }
    }
}
