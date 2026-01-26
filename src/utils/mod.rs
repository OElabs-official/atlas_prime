use std::sync::mpsc;
use std::thread;
use std::time::Duration;



pub fn apply_affinity(index: Option<usize>) {
    if let Some(i) = index {
        if let Some(ids) = core_affinity::get_core_ids() {
            if i < ids.len() {
                core_affinity::set_for_current(ids[i]);
            }
        }
    }
}
