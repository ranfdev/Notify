use std::cell::Cell;
use std::rc::Rc;

use glib::SourceId;
use gtk::glib;

#[derive(Clone)]
pub struct Debouncer {
    scheduled: Rc<Cell<Option<SourceId>>>,
}
impl Debouncer {
    pub fn new() -> Self {
        Self {
            scheduled: Default::default(),
        }
    }
    pub fn call(&self, duration: std::time::Duration, f: impl Fn() -> () + 'static) {
        if let Some(scheduled) = self.scheduled.take() {
            scheduled.remove();
        }
        let scheduled_clone = self.scheduled.clone();
        let source_id = glib::source::timeout_add_local_once(duration, move || {
            f();
            scheduled_clone.take();
        });
        self.scheduled.set(Some(source_id));
    }
}
