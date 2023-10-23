use std::path::Path;

use adw::subclass::prelude::*;
use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use futures::AsyncReadExt;
use gettextrs::gettext;
use gio::SocketClient;
use gio::UnixSocketAddress;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use ntfy_daemon::models;
use ntfy_daemon::ntfy_capnp::system_notifier;
use tracing::{debug, info};

use crate::config::{APP_ID, PKGDATADIR, PROFILE, VERSION};
use crate::widgets::*;

mod imp {
    use std::cell::RefCell;

    use glib::WeakRef;
    use once_cell::sync::OnceCell;

    use super::*;

    #[derive(Default)]
    pub struct NotifyApplication {
        pub window: RefCell<WeakRef<NotifyWindow>>,
        pub hold_guard: OnceCell<gio::ApplicationHoldGuard>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NotifyApplication {
        const NAME: &'static str = "NotifyApplication";
        type Type = super::NotifyApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for NotifyApplication {}

    impl ApplicationImpl for NotifyApplication {
        fn activate(&self) {
            debug!("AdwApplication<NotifyApplication>::activate");
            self.parent_activate();
        }

        fn startup(&self) {
            debug!("AdwApplication<NotifyApplication>::startup");
            self.parent_startup();
            let app = self.obj();

            // Set icons for shell
            gtk::Window::set_default_icon_name(APP_ID);

            app.setup_css();
            app.setup_gactions();
            app.setup_accels();
        }
        fn command_line(&self, command_line: &gio::ApplicationCommandLine) -> glib::ExitCode {
            let socket_path = glib::user_data_dir().join("com.ranfdev.Notify.socket");

            debug!("AdwApplication<NotifyApplication>::command_line");
            let arguments = command_line.arguments();
            let is_daemon = arguments.get(1).map(|x| x.to_str()) == Some(Some("--daemon"));
            let app = self.obj();

            if self.hold_guard.get().is_none() {
                self.obj().ensure_rpc_running(&socket_path);
            }

            glib::MainContext::default().spawn_local(async move {
                super::NotifyApplication::run_in_background().await.unwrap();
            });

            if is_daemon {
                return glib::ExitCode::SUCCESS;
            }

            {
                let w = self.window.borrow();
                if let Some(window) = w.upgrade() {
                    if window.is_visible() {
                        window.present();
                        return glib::ExitCode::SUCCESS;
                    }
                }
            }

            app.build_window(&socket_path);
            app.main_window().present();

            glib::ExitCode::SUCCESS
        }
    }

    impl GtkApplicationImpl for NotifyApplication {}
    impl AdwApplicationImpl for NotifyApplication {}
}

glib::wrapper! {
    pub struct NotifyApplication(ObjectSubclass<imp::NotifyApplication>)
        @extends gio::Application, gtk::Application,
        @implements gio::ActionMap, gio::ActionGroup;
}

impl NotifyApplication {
    fn main_window(&self) -> NotifyWindow {
        self.imp().window.borrow().upgrade().unwrap()
    }

    fn setup_gactions(&self) {
        // Quit
        let action_quit = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| {
                // This is needed to trigger the delete event and saving the window state
                app.main_window().close();
                app.quit();
            })
            .build();

        // About
        let action_about = gio::ActionEntry::builder("about")
            .activate(|app: &Self, _, _| {
                app.show_about_dialog();
            })
            .build();
        self.add_action_entries([action_quit, action_about]);
    }

    // Sets up keyboard shortcuts
    fn setup_accels(&self) {
        self.set_accels_for_action("app.quit", &["<Control>q"]);
        self.set_accels_for_action("window.close", &["<Control>w"]);
    }

    fn setup_css(&self) {
        let provider = gtk::CssProvider::new();
        provider.load_from_resource("/com/ranfdev/Notify/style.css");
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    }

    fn show_about_dialog(&self) {
        let dialog = adw::AboutWindow::builder()
            .application_icon(APP_ID)
            .application_name("Notify")
            .license_type(gtk::License::Gpl30)
            .version(VERSION)
            .transient_for(&self.main_window())
            .translator_credits(gettext("translator-credits"))
            .modal(true)
            .developers(vec!["ranfdev"])
            .artists(vec!["ranfdev"])
            .build();

        dialog.present();
    }

    pub fn run(&self) -> glib::ExitCode {
        info!(app_id = %APP_ID, version = %VERSION, profile = %PROFILE, datadir = %PKGDATADIR, "running");

        ApplicationExtManual::run(self)
    }
    async fn run_in_background() -> ashpd::Result<()> {
        let response = ashpd::desktop::background::Background::request()
            .reason("Listen for coming notifications")
            .auto_start(true)
            .command(&["notify", "--daemon"])
            .dbus_activatable(false)
            .send()
            .await?
            .response()?;

        info!(auto_start = %response.auto_start(), run_in_background = %response.run_in_background());

        Ok(())
    }

    fn ensure_rpc_running(&self, socket_path: &Path) {
        let dbpath = glib::user_data_dir().join("com.ranfdev.Notify.sqlite");
        info!(database_path = %dbpath.display());

        let (tx, rx) = glib::MainContext::channel(Default::default());
        let app = self.clone();
        rx.attach(None, move |n: models::Notification| {
            let gio_notif = gio::Notification::new(&n.title);
            gio_notif.set_body(Some(&n.body));
            app.send_notification(None, &gio_notif);
            glib::ControlFlow::Continue
        });

        struct Proxy(glib::Sender<models::Notification>);
        impl models::NotificationProxy for Proxy {
            fn send(&self, n: models::Notification) -> anyhow::Result<()> {
                self.0.send(n)?;
                Ok(())
            }
        }
        ntfy_daemon::system_client::start(
            socket_path.to_owned(),
            dbpath.to_str().unwrap(),
            std::sync::Arc::new(Proxy(tx)),
        )
        .unwrap();
        self.imp().hold_guard.set(self.hold()).unwrap();
    }

    fn build_window(&self, socket_path: &Path) {
        let address = UnixSocketAddress::new(socket_path);
        let client = SocketClient::new();
        let connection =
            SocketClientExt::connect(&client, &address, gio::Cancellable::NONE).unwrap();

        let rw = connection.into_async_read_write().unwrap();
        let (reader, writer) = rw.split();

        let rpc_network = Box::new(twoparty::VatNetwork::new(
            reader,
            writer,
            rpc_twoparty_capnp::Side::Client,
            Default::default(),
        ));
        let mut rpc_system = RpcSystem::new(rpc_network, None);
        let client: system_notifier::Client =
            rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);

        glib::MainContext::default().spawn_local(async move {
            debug!("rpc_system started");
            rpc_system.await.unwrap();
            debug!("rpc_system stopped");
        });

        let window = NotifyWindow::new(self, client);
        *self.imp().window.borrow_mut() = window.downgrade();
    }
}

impl Default for NotifyApplication {
    fn default() -> Self {
        glib::Object::builder()
            .property("application-id", APP_ID)
            .property("flags", gio::ApplicationFlags::HANDLES_COMMAND_LINE)
            .property("resource-base-path", "/com/ranfdev/Notify/")
            .build()
    }
}
