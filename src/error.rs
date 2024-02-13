use futures::Future;
use glib::subclass::prelude::*;
use gtk::prelude::*;
use gtk::{self, glib};

use crate::widgets::NotifyWindow;

pub type Error = anyhow::Error;

pub trait ErrorBoundaryProvider {
    fn error_boundary(&self) -> ErrorBoundary;
}

impl<W: IsA<gtk::Widget>> ErrorBoundaryProvider for W {
    fn error_boundary(&self) -> ErrorBoundary {
        let direct_ancestor: Option<adw::ToastOverlay> = self
            .ancestor(adw::ToastOverlay::static_type())
            .and_downcast();
        let win: Option<adw::ToastOverlay> = self
            .ancestor(NotifyWindow::static_type())
            .and_downcast()
            .map(|win: NotifyWindow| win.imp().toast_overlay.clone());
        let toast_overlay = direct_ancestor.or(win);
        ErrorBoundary {
            source: self.clone().into(),
            boundary: toast_overlay,
        }
    }
}

pub struct ErrorBoundary {
    source: gtk::Widget,
    boundary: Option<adw::ToastOverlay>,
}

impl ErrorBoundary {
    pub fn spawn<T>(self, f: impl Future<Output = Result<T, Error>> + 'static) {
        glib::MainContext::ref_thread_default().spawn_local_with_priority(
            glib::Priority::DEFAULT_IDLE,
            async move {
                if let Err(e) = f.await {
                    if let Some(boundary) = self.boundary {
                        boundary.add_toast(adw::Toast::builder().title(&e.to_string()).build());
                    }
                    tracing::error!(source=?self.source.type_().name(), error=?e);
                }
            },
        );
    }
}
