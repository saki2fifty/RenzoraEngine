//! Reusable storage for descriptors whose pointers are valid for one call only.

use crate::sys;

pub(super) struct CallBuffers {
    cells: Vec<Vec<*mut u8>>,
    views: Vec<sys::QueryView>,
    resources: Vec<sys::ResourceSlot>,
}

// SAFETY: this private storage never dereferences its pointers. It exposes them
// only through an exclusive frame guard, after clearing previous entries. The
// dispatcher fills them from its current ECS borrows and uses them synchronously.
// Moving storage between calls transfers allocations, not access to the pointees.
unsafe impl Send for CallBuffers {}
// SAFETY: shared access exposes no descriptors or pointees; all table access
// requires &mut through the frame guard. Dropping descriptors never dereferences.
unsafe impl Sync for CallBuffers {}

impl CallBuffers {
    pub(super) fn new(query_count: usize, resource_count: usize) -> Self {
        Self {
            cells: (0..query_count).map(|_| Vec::new()).collect(),
            views: Vec::with_capacity(query_count),
            resources: Vec::with_capacity(resource_count),
        }
    }

    pub(super) fn frame(&mut self) -> CallFrame<'_> {
        // Also clear on entry: even forgetting an earlier guard cannot make a
        // later invocation observe descriptors from a previous borrow.
        self.clear();
        CallFrame(self)
    }

    fn clear(&mut self) {
        self.views.clear();
        self.resources.clear();
        for cells in &mut self.cells {
            cells.clear();
        }
    }
}

pub(super) struct CallFrame<'a>(&'a mut CallBuffers);

impl CallFrame<'_> {
    pub(super) fn tables(
        &mut self,
    ) -> (
        &mut [Vec<*mut u8>],
        &mut Vec<sys::QueryView>,
        &mut Vec<sys::ResourceSlot>,
    ) {
        (&mut self.0.cells, &mut self.0.views, &mut self.0.resources)
    }
}

impl Drop for CallFrame<'_> {
    fn drop(&mut self) {
        // Runs on normal return, refused plugin output and host-side unwinding.
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populate(buffers: &mut CallBuffers) {
        let mut frame = buffers.frame();
        let (cells, views, resources) = frame.tables();
        assert!(cells[0].is_empty() && views.is_empty() && resources.is_empty());
        cells[0].resize(64, std::ptr::null_mut());
        views.push(sys::QueryView {
            cells: cells[0].as_mut_ptr(),
            entities: std::ptr::null(),
            entity_count: 0,
            cell_count: 0,
        });
        resources.push(sys::ResourceSlot {
            id: sys::ComponentId(0),
            ptr: std::ptr::null_mut(),
        });
    }

    #[test]
    fn warmed_descriptor_tables_allocate_nothing_across_1000_calls() {
        let mut buffers = CallBuffers::new(1, 1);
        populate(&mut buffers);
        let capacity = buffers.cells[0].capacity();
        let allocations = crate::host::dispatch_tests::allocations_during(|| {
            for _ in 0..1000 {
                populate(&mut buffers);
            }
        });
        assert_eq!(allocations, 0);
        assert_eq!(buffers.cells[0].capacity(), capacity);
        assert!(buffers.cells[0].is_empty());
        assert!(buffers.views.is_empty());
        assert!(buffers.resources.is_empty());
    }

    #[test]
    fn guard_clears_descriptors_when_host_unwinds() {
        let mut buffers = CallBuffers::new(1, 0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut frame = buffers.frame();
            frame.tables().0[0].push(std::ptr::null_mut());
            panic!("simulated host failure");
        }));
        assert!(result.is_err());
        assert!(buffers.cells[0].is_empty());
        assert!(buffers.cells[0].capacity() > 0);
    }

    #[test]
    fn entry_clears_even_a_forgotten_guard_before_reuse() {
        let mut buffers = CallBuffers::new(1, 0);
        let mut frame = buffers.frame();
        frame.tables().0[0].push(std::ptr::null_mut());
        std::mem::forget(frame);
        assert!(buffers.frame().tables().0[0].is_empty());
    }
}
