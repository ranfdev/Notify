use std::cell::Cell;
use std::rc::Rc;

use async_channel::Receiver;
use glib::SourceId;
use gtk::glib;

pub fn debounce_channel<T: 'static>(
    duration: std::time::Duration,
    source: Receiver<T>,
) -> Receiver<T> {
    let (tx, rx) = async_channel::unbounded();
    let scheduled = Rc::new(Cell::new(None::<SourceId>));
    let rx_clone = rx.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(data) = rx_clone.recv().await {
            if let Some(scheduled) = scheduled.take() {
                scheduled.remove();
            }
            let tx = tx.clone();
            let scheduled_clone = scheduled.clone();
            let source_id = glib::source::timeout_add_local_once(duration, move || {
                tx.send_blocking(data).unwrap();
                scheduled_clone.take();
            });
            scheduled.set(Some(source_id));
        }
    });
    rx
}
