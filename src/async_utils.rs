use std::cell::Cell;
use std::rc::Rc;

use glib::Receiver;
use glib::SourceId;
use gtk::glib;

pub fn debounce_channel<T: 'static>(
    duration: std::time::Duration,
    source: Receiver<T>,
) -> Receiver<T> {
    let (tx, rx) = glib::MainContext::channel(Default::default());
    let scheduled = Rc::new(Cell::new(None::<SourceId>));
    source.attach(None, move |data| {
        if let Some(scheduled) = scheduled.take() {
            scheduled.remove();
        }
        let tx = tx.clone();
        let scheduled_clone = scheduled.clone();
        let source_id = glib::source::timeout_add_local_once(duration, move || {
            tx.send(data).unwrap();
            scheduled_clone.take();
        });
        scheduled.set(Some(source_id));
        glib::ControlFlow::Continue
    });
    rx
}
