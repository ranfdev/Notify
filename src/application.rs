use std::cell::Cell;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use futures::stream::Stream;
use gtk::{gdk, gio, glib};
use ntfy_daemon::models;
use ntfy_daemon::NtfyHandle;
use tracing::{debug, error, info, warn};

use crate::config::{APP_ID, PKGDATADIR, PROFILE, VERSION};
use crate::widgets::*;
use anyhow::Context;

mod imp {
    use std::cell::RefCell;

    use glib::WeakRef;
    use once_cell::sync::OnceCell;

    use super::*;

    #[derive(Default)]
    pub struct NotifyApplication {
        pub window: RefCell<WeakRef<NotifyWindow>>,
        pub hold_guard: OnceCell<gio::ApplicationHoldGuard>,
        pub ntfy: OnceCell<NtfyHandle>,
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
            self.obj().ensure_window_present();
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
            // Karere-style background portal request at startup
            std::thread::spawn(|| {
                if let Ok(rt) = tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                    rt.block_on(async {
                        debug!("Requesting background permission at startup...");
                        match ashpd::desktop::background::Background::request()
                            .reason("Notify needs to run in the background to receive notifications.")
                            .auto_start(true)
                            .send()
                            .await
                        {
                            Ok(response) => {
                                 info!("Background permission requested: {:?}", response.response());
                                 
                                 // Use zbus directly to call SetStatus
                                 async fn set_status_msg() -> anyhow::Result<()> {
                                     let connection = zbus::Connection::session().await?;
                                     let proxy = zbus::Proxy::new(
                                         &connection, 
                                         "org.freedesktop.portal.Desktop", 
                                         "/org/freedesktop/portal/desktop", 
                                         "org.freedesktop.portal.Background"
                                     ).await?;

                                     let mut options = std::collections::HashMap::new();
                                     options.insert("message", zbus::zvariant::Value::from("Running in background"));

                                     proxy.call_method("SetStatus", &(options)).await?;
                                     Ok(())
                                 }

                                 if let Err(e) = set_status_msg().await {
                                     warn!("Failed to set background status: {}", e);
                                 } else {
                                     debug!("Background status set.");
                                 }
                            }
                            Err(e) => {
                                 warn!("Failed to request background permission: {}", e);
                            }
                        }
                    });
                }
            });
        }
        fn command_line(&self, command_line: &gio::ApplicationCommandLine) -> glib::ExitCode {
            debug!("AdwApplication<NotifyApplication>::command_line");
            let arguments = command_line.arguments();
            let is_daemon = arguments.get(1).map(|x| x.to_str()) == Some(Some("--daemon"));
            let app = self.obj();

            if self.hold_guard.get().is_none() {
                app.ensure_rpc_running();
            }

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    if let Err(e) = super::NotifyApplication::run_in_background(None).await {
                        warn!(error = %e, "couldn't request running in background from portal");
                    }
                });
            });

            if is_daemon {
                return glib::ExitCode::SUCCESS;
            }

            app.ensure_window_present();

            glib::ExitCode::SUCCESS
        }
    }

    impl GtkApplicationImpl for NotifyApplication {}
    impl AdwApplicationImpl for NotifyApplication {}
}

glib::wrapper! {
    pub struct NotifyApplication(ObjectSubclass<imp::NotifyApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionMap, gio::ActionGroup;
}

impl NotifyApplication {
    fn ensure_window_present(&self) {
        if let Some(window) = { self.imp().window.borrow().upgrade() } {
            if window.is_visible() {
                window.present();
                return;
            }
        }
        self.build_window();
        self.main_window().present();
    }

    fn main_window(&self) -> NotifyWindow {
        self.imp().window.borrow().upgrade().unwrap()
    }

    fn setup_gactions(&self) {
        // Quit
        let action_quit = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| {
                // This is needed to trigger the delete event and saving the window state
                if let Some(win) = app.imp().window.borrow().upgrade() {
                    let _ = win.save_window_size();
                    win.close();
                }
                app.quit();
                std::process::exit(0);
            })
            .build();

        // About
        let action_about = gio::ActionEntry::builder("about")
            .activate(|app: &Self, _, _| {
                app.show_about_dialog();
            })
            .build();

        let action_preferences = gio::ActionEntry::builder("preferences")
            .activate(|app: &Self, _, _| {
                app.show_preferences();
            })
            .build();

        let message_action = gio::ActionEntry::builder("message-action")
            .parameter_type(Some(&glib::VariantTy::STRING))
            .activate(|app: &Self, _, params| {
                let Some(params) = params else {
                    return;
                };
                let Some(s) = params.str() else {
                    warn!("action is not a string");
                    return;
                };
                let Ok(action) = serde_json::from_str(s) else {
                    error!("invalid action json");
                    return;
                };
                app.handle_message_action(action);
            })
            .build();
        self.add_action_entries([
            action_quit,
            action_about,
            action_preferences,
            message_action,
        ]);
    }

    fn handle_message_action(&self, action: models::Action) {
        match action {
            models::Action::View { url, .. } => {
                gtk::UriLauncher::builder().uri(url.clone()).build().launch(
                    gtk::Window::NONE,
                    gio::Cancellable::NONE,
                    |_| {},
                );
            }
            models::Action::Http {
                method,
                url,
                body,
                headers,
                ..
            } => {
                gio::spawn_blocking(move || {
                    let agent = ureq::Agent::new_with_config(
                        Default::default()
                    );
                    
                    macro_rules! set_headers {
                        ($req:expr) => {{
                            let mut r = $req;
                            for (k, v) in headers.iter() {
                                r = r.header(k, v);
                            }
                            r
                        }}
                    }

                   let res = match method.as_str() {
                        "GET" => set_headers!(agent.get(url.as_str())).call(),
                        "POST" => set_headers!(agent.post(url.as_str())).send(body.as_bytes()),
                        "PUT" => set_headers!(agent.put(url.as_str())).send(body.as_bytes()),
                        "DELETE" => set_headers!(agent.delete(url.as_str())).call(),
                        "HEAD" => set_headers!(agent.head(url.as_str())).call(),
                        "PATCH" => set_headers!(agent.patch(url.as_str())).send(body.as_bytes()),
                        "OPTIONS" => set_headers!(agent.options(url.as_str())).call(),
                        "TRACE" => set_headers!(agent.trace(url.as_str())).call(),
                        _ => set_headers!(agent.get(url.as_str())).call(),
                    };
                    match res {
                        Err(e) => {
                            error!(error = ?e, "Error sending request");
                        }
                        Ok(_) => {}
                    }
                });
            }
            _ => {}
        }
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
        let dialog = adw::AboutDialog::from_appdata(
            "/com/ranfdev/Notify/com.ranfdev.Notify.metainfo.xml",
            None,
        );
        if let Some(w) = self.imp().window.borrow().upgrade() {
            dialog.present(Some(&w));
        }
    }

    fn show_preferences(&self) {
        let win = crate::widgets::NotifyPreferences::new(
            self.main_window().imp().notifier.get().unwrap().clone(),
        );
        win.present(Some(&self.main_window()));
    }

    pub fn run(&self) -> glib::ExitCode {
        info!(app_id = %APP_ID, version = %VERSION, profile = %PROFILE, datadir = %PKGDATADIR, "running");

        glib::ExitCode::from(self.run_with_args(&std::env::args().collect::<Vec<_>>()))
    }
    
    fn setup_autostart(&self) {
        let settings = gio::Settings::new(crate::config::APP_ID);
        
        let app = self.clone();
        settings.connect_changed(Some("run-on-startup"), move |_, _| {
            debug!("Run on startup setting changed");
            let app = app.clone();
            
            // We need to get the window from the main thread
            let identifier = if let Some(win) = app.imp().window.borrow().upgrade() {
                // from_native is async and needs a reactor. 
                // We use a temporary runtime on the main thread here.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    match ashpd::WindowIdentifier::from_native(&win).await {
                        Some(id) => Some(id),
                        None => {
                            warn!("Failed to get window identifier");
                            None
                        }
                    }
                })
            } else {
                None
            };

            // Run the portal request in a background thread to avoid blocking and provide a reactor for zbus
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    info!("Calling run_in_background from background thread");
                    if let Err(e) = Self::run_in_background(identifier).await {
                         warn!("Failed to update autostart portal: {}", e);
                    } else {
                        info!("Autostart portal updated");
                    }
                });
            });
        });
    }

    fn update_autostart_file(&self, _enable: bool) -> std::io::Result<()> {
        // Handled in preferences.rs to match Karere
        Ok(())
    }


    async fn run_in_background(identifier: Option<ashpd::WindowIdentifier>) -> ashpd::Result<()> {
        let settings = gio::Settings::new(APP_ID);
        let autostart = settings.boolean("run-on-startup");
        info!(autostart_request = autostart, "Initiating background portal request");

        let request = ashpd::desktop::background::Background::request()
            .reason("Receive notifications in the background")
            .auto_start(autostart)
            .command(&["notify", "--daemon"])
            .dbus_activatable(false);
        
        let response = if let Some(id) = identifier {
            info!("Using window identifier for portal request");
            request.identifier(id).send().await?.response()?
        } else {
            info!("No window identifier available for portal request");
            request.send().await?.response()?
        };

        warn!(
            portal_auto_start = %response.auto_start(), 
            portal_run_in_background = %response.run_in_background(),
            "Portal background request result"
        );

        Ok(())
    }

    fn ensure_rpc_running(&self) {
        let dbpath = glib::user_data_dir().join("com.ranfdev.Notify.sqlite");
        info!(database_path = %dbpath.display());

        // Here I'm sending notifications to the desktop environment and listening for network changes.
        // This should have been inside ntfy-daemon, but using portals from another thread causes the error
        // `Invalid client serial` and it's broken.
        // Until https://github.com/flatpak/xdg-dbus-proxy/issues/46 is solved, I have to handle these things
        // in the main thread. Uff.

        let (s, r) = async_channel::unbounded::<models::Notification>();

        let app = self.clone();
        glib::MainContext::ref_thread_default().spawn_local(async move {
            while let Ok(n) = r.recv().await {
                let gio_notif = gio::Notification::new(&n.title);
                gio_notif.set_body(Some(&n.body));

                let action_name = |a| {
                    let json = serde_json::to_string(a).unwrap();
                    gio::Action::print_detailed_name("app.message-action", Some(&json.into()))
                };
                for a in n.actions.iter() {
                    match a {
                        models::Action::View { label, .. } => {
                            gio_notif.add_button(&label, &action_name(a))
                        }
                        models::Action::Http { label, .. } => {
                            gio_notif.add_button(&label, &action_name(a))
                        }
                        _ => {}
                    }
                }

                app.send_notification(None, &gio_notif);
            }
        });
        struct Proxies {
            notification: async_channel::Sender<models::Notification>,
        }
        impl models::NotificationProxy for Proxies {
            fn send(&self, n: models::Notification) -> anyhow::Result<()> {
                self.notification.send_blocking(n)?;
                Ok(())
            }
        }
        impl models::NetworkMonitorProxy for Proxies {
            fn listen(&self) -> Pin<Box<dyn Stream<Item = ()>>> {
                let (tx, rx) = async_channel::bounded(1);
                let prev_available = Rc::new(Cell::new(false));

                gio::NetworkMonitor::default().connect_network_changed(move |_, available| {
                    if available && !prev_available.get() {
                        if let Err(e) = tx.send_blocking(()) {
                            warn!(error = %e);
                        }
                    }
                    prev_available.replace(available);
                });

                Box::pin(rx)
            }
        }
        let proxies = std::sync::Arc::new(Proxies { notification: s });
        let ntfy = ntfy_daemon::start(dbpath.to_str().unwrap(), proxies.clone(), proxies).unwrap();
        self.imp()
            .ntfy
            .set(ntfy)
            .or(Err(anyhow::anyhow!("failed setting ntfy")))
            .unwrap();
        self.imp().hold_guard.set(self.hold()).unwrap();
    }

    fn build_window(&self) {
        let ntfy = self.imp().ntfy.get().unwrap();

        let window = NotifyWindow::new(self, ntfy.clone());
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
