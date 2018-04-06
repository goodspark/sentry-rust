use std::env;
use std::sync::Arc;
use std::time::Duration;
use std::ffi::{OsStr, OsString};

use uuid::Uuid;
use regex::Regex;

use api::Dsn;
use scope::Scope;
use protocol::Event;
use transport::Transport;
use backtrace_support::{WELL_KNOWN_BORDER_FRAMES, WELL_KNOWN_SYS_MODULES};

/// The Sentry client object.
#[derive(Debug, Clone)]
pub struct Client {
    dsn: Dsn,
    options: ClientOptions,
    transport: Arc<Transport>,
}

/// Configuration settings for the client.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// module prefixes that are always considered in_app
    pub in_app_include: Vec<&'static str>,
    /// module prefixes that are never in_app
    pub in_app_exclude: Vec<&'static str>,
    /// border frames which indicate a border from a backtrace to
    /// useless internals.  Some are automatically included.
    pub extra_border_frames: Vec<&'static str>,
    /// Maximum number of breadcrumbs (0 to disable feature).
    pub max_breadcrumbs: usize,
    /// Automatically trim backtraces of junk before sending.
    pub trim_backtraces: bool,
}

impl Default for ClientOptions {
    fn default() -> ClientOptions {
        ClientOptions {
            in_app_include: vec![],
            in_app_exclude: vec![],
            extra_border_frames: vec![],
            max_breadcrumbs: 100,
            trim_backtraces: true,
        }
    }
}

lazy_static! {
    static ref CRATE_RE: Regex = Regex::new(r"^([^:]+?)::").unwrap();
}

/// Helper trait to convert an object into a client config
/// for create.
pub trait IntoClientConfig {
    /// Converts the object into a client config tuple of
    /// DSN and options.
    ///
    /// This can panic in cases where the conversion cannot be
    /// performed due to an error.
    fn into_client_config(self) -> (Option<Dsn>, Option<ClientOptions>);
}

impl IntoClientConfig for () {
    fn into_client_config(self) -> (Option<Dsn>, Option<ClientOptions>) {
        (None, None)
    }
}

impl<C: IntoClientConfig> IntoClientConfig for Option<C> {
    fn into_client_config(self) -> (Option<Dsn>, Option<ClientOptions>) {
        self.map(|x| x.into_client_config()).unwrap_or((None, None))
    }
}

impl<'a> IntoClientConfig for &'a str {
    fn into_client_config(self) -> (Option<Dsn>, Option<ClientOptions>) {
        if self.is_empty() {
            (None, None)
        } else {
            (Some(self.parse().unwrap()), None)
        }
    }
}

impl<'a> IntoClientConfig for &'a OsStr {
    fn into_client_config(self) -> (Option<Dsn>, Option<ClientOptions>) {
        if self.is_empty() {
            (None, None)
        } else {
            (Some(self.to_string_lossy().parse().unwrap()), None)
        }
    }
}

impl IntoClientConfig for OsString {
    fn into_client_config(self) -> (Option<Dsn>, Option<ClientOptions>) {
        if self.is_empty() {
            (None, None)
        } else {
            (Some(self.to_string_lossy().parse().unwrap()), None)
        }
    }
}

impl IntoClientConfig for String {
    fn into_client_config(self) -> (Option<Dsn>, Option<ClientOptions>) {
        if self.is_empty() {
            (None, None)
        } else {
            (Some(self.parse().unwrap()), None)
        }
    }
}

impl<'a> IntoClientConfig for &'a Dsn {
    fn into_client_config(self) -> (Option<Dsn>, Option<ClientOptions>) {
        (Some(self.clone()), None)
    }
}

impl IntoClientConfig for Dsn {
    fn into_client_config(self) -> (Option<Dsn>, Option<ClientOptions>) {
        (Some(self), None)
    }
}

impl<C: IntoClientConfig> IntoClientConfig for (C, ClientOptions) {
    fn into_client_config(self) -> (Option<Dsn>, Option<ClientOptions>) {
        let (dsn, _) = self.0.into_client_config();
        (dsn, Some(self.1))
    }
}

impl Client {
    /// Creates a new Sentry client from a config helper.
    ///
    /// As the config helper can also disable the client this method might return
    /// `None` instead.  This is what `sentry::init` uses internally before binding
    /// the client.
    ///
    /// The client config can be of one of many formats as implemented by the
    /// `IntoClientConfig` trait.  The most common form is to just supply a
    /// string with the DSN.
    ///
    /// # Supported Configs
    ///
    /// The following common values are supported for the client config:
    ///
    /// * `()`: pick up the default config from the environment only
    /// * `&str` / `String` / `&OsStr` / `String`: configure the client with the given DSN
    /// * `Dsn` / `&Dsn`: configure the client with a given DSN
    /// * `(C, options)`: configure the client from the given DSN and optional options.
    ///
    /// The tuple form lets you do things like `(Dsn, ClientOptions)` for instance.
    ///
    /// # Panics
    ///
    /// The `IntoClientConfig` can panic for the forms where a DSN needs to be parsed.
    /// If you want to handle invalid DSNs you need to parse them manually by calling
    /// parse on it and handle the error.
    pub fn from_config<C: IntoClientConfig>(cfg: C) -> Option<Client> {
        let (dsn, options) = cfg.into_client_config();
        let dsn = dsn.or_else(|| {
            env::var("SENTRY_DSN")
                .ok()
                .and_then(|dsn| dsn.parse::<Dsn>().ok())
        });
        if let Some(dsn) = dsn {
            Some(if let Some(options) = options {
                Client::with_dsn_and_options(dsn, options)
            } else {
                Client::with_dsn(dsn)
            })
        } else {
            None
        }
    }

    /// Creates a new sentry client for the given DSN.
    pub fn with_dsn(dsn: Dsn) -> Client {
        Client::with_dsn_and_options(dsn, Default::default())
    }

    /// Creates a new sentry client for the given DSN.
    pub fn with_dsn_and_options(dsn: Dsn, options: ClientOptions) -> Client {
        let transport = Transport::new(&dsn);
        Client {
            dsn: dsn,
            options: options,
            transport: Arc::new(transport),
        }
    }

    fn prepare_event(&self, event: &mut Event, scope: Option<&Scope>) {
        if let Some(scope) = scope {
            if !scope.breadcrumbs.is_empty() {
                event
                    .breadcrumbs
                    .extend(scope.breadcrumbs.iter().map(|x| x.clone()));
            }

            if event.user.is_none() {
                if let Some(ref user) = scope.user {
                    event.user = Some(user.clone());
                }
            }

            if let Some(ref extra) = scope.extra {
                event
                    .extra
                    .extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
            }

            if let Some(ref tags) = scope.tags {
                event
                    .tags
                    .extend(tags.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }

        if &event.platform == "other" {
            event.platform = "native".into();
        }

        for exc in event.exceptions.iter_mut() {
            if let Some(ref mut stacktrace) = exc.stacktrace {
                // automatically trim backtraces
                if self.options.trim_backtraces {
                    if let Some(cutoff) = stacktrace.frames.iter().rev().position(|frame| {
                        if let Some(ref func) = frame.function {
                            WELL_KNOWN_BORDER_FRAMES.contains(&func.as_str())
                                || self.options.extra_border_frames.contains(&func.as_str())
                        } else {
                            false
                        }
                    }) {
                        let trunc = stacktrace.frames.len() - cutoff - 1;
                        stacktrace.frames.truncate(trunc);
                    }
                }

                // automatically prime in_app and set package
                let mut any_in_app = false;
                for frame in stacktrace.frames.iter_mut() {
                    let func_name = match frame.function {
                        Some(ref func) => func,
                        None => continue,
                    };

                    // set package if missing to crate prefix
                    if frame.package.is_none() {
                        frame.package = CRATE_RE
                            .captures(func_name)
                            .and_then(|caps| caps.get(1))
                            .map(|cr| cr.as_str().into());
                    }

                    match frame.in_app {
                        Some(true) => {
                            any_in_app = true;
                            continue;
                        }
                        Some(false) => {
                            continue;
                        }
                        None => {}
                    }

                    for m in &self.options.in_app_exclude {
                        if func_name.starts_with(m) {
                            frame.in_app = Some(false);
                            break;
                        }
                    }

                    if frame.in_app.is_some() {
                        continue;
                    }

                    for m in &self.options.in_app_include {
                        if func_name.starts_with(m) {
                            frame.in_app = Some(true);
                            any_in_app = true;
                            break;
                        }
                    }

                    if frame.in_app.is_some() {
                        continue;
                    }

                    for m in WELL_KNOWN_SYS_MODULES.iter() {
                        if func_name.starts_with(m) {
                            frame.in_app = Some(false);
                            break;
                        }
                    }
                }

                if !any_in_app {
                    for frame in stacktrace.frames.iter_mut() {
                        if frame.in_app.is_none() {
                            frame.in_app = Some(true);
                        }
                    }
                }
            }
        }
    }

    /// Returns the options of this client.
    pub fn options(&self) -> &ClientOptions {
        &self.options
    }

    /// Returns the DSN that constructed this client.
    pub fn dsn(&self) -> &Dsn {
        &self.dsn
    }

    /// Captures an event and sends it to sentry.
    pub fn capture_event(&self, mut event: Event, scope: Option<&Scope>) -> Uuid {
        self.prepare_event(&mut event, scope);
        self.transport.send_event(event)
    }

    /// Drains all pending events up to the current time.
    ///
    /// This returns `true` if the queue was successfully drained in the
    /// given time or `false` if not (for instance because of a timeout).
    /// If no timeout is provided the client will wait forever.
    pub fn drain_events(&self, timeout: Option<Duration>) -> bool {
        self.transport.drain(timeout)
    }
}