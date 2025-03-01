mod application;
#[rustfmt::skip]
mod config;
mod async_utils;
pub mod error;
mod subscription;
pub mod widgets;

use adw::prelude::*;
use gettextrs::{gettext, LocaleCategory};
use gtk::{gio, glib};
use tracing::debug;

use self::application::NotifyApplication;
use self::config::{GETTEXT_PACKAGE, LOCALEDIR, RESOURCES_FILE};

fn main() -> glib::ExitCode {
    // Initialize logger
    tracing_subscriber::fmt::init();

    // Prepare i18n
    gettextrs::setlocale(LocaleCategory::LcAll, "");
    gettextrs::bindtextdomain(GETTEXT_PACKAGE, LOCALEDIR).expect("Unable to bind the text domain");
    gettextrs::textdomain(GETTEXT_PACKAGE).expect("Unable to switch to the text domain");

    glib::set_application_name(&gettext("Notify"));

    let res = gio::Resource::load(RESOURCES_FILE).expect("Could not load gresource file");
    gio::resources_register(&res);

    let app = NotifyApplication::new();
    app.register(gio::Cancellable::NONE)
        .expect("Failed to register application");
    if !app.is_remote() {
        debug!("primary instance");
    };
    app.run()
}
