//! `deezsrc plugin`: source plugin (protocol v1, see `docs/plugin.md`).
//! JSON-RPC on stdin/stdout.

mod catalog;
mod details;
mod items;
mod lyrics;
mod playlists;
mod relay;
mod report;
mod resolve;
mod rpc;
mod settings;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;

use crate::account::{self, Credentials};
use crate::deezer::{Deezer, Endpoints, SignedOut, User};
use resolve::Output;
use rpc::{Incoming, Out, RpcError, RpcResult, AUTH_REQUIRED};

pub const PROTOCOL: u64 = 1;
/// Favourites may change in other apps: read their ids again after this.
const FAVORITES_MAX_AGE: Duration = Duration::from_secs(10 * 60);

struct State {
    data_dir: Option<PathBuf>,
    output: Output,
    /// `auth.changed {expired}` already sent for the current `arl`.
    expiry_notified: bool,
    /// Language of the host's `locale` (`fr`), for labels and biographies.
    lang: String,
    settings: settings::Settings,
    /// When the favourite ids were last requested.
    favorites_at: Option<Instant>,
}

struct Plugin {
    out: Out,
    state: Mutex<State>,
    /// Signed in when it holds an `arl`.
    deezer: Mutex<Deezer>,
    library: catalog::Library,
    reports: report::Reporter,
    /// Serves the decrypted streams.
    relay: relay::Relay,
}

pub async fn run(endpoints: Endpoints) -> Result<()> {
    let (out, writer) = Out::stdout();
    let plugin = Arc::new(Plugin {
        out: out.clone(),
        state: Mutex::new(State {
            data_dir: None,
            output: Output::default(),
            expiry_notified: false,
            lang: "en".into(),
            settings: settings::Settings::default(),
            favorites_at: None,
        }),
        deezer: Mutex::new(Deezer::new(endpoints)),
        library: catalog::Library::default(),
        reports: report::Reporter::default(),
        relay: relay::Relay::default(),
    });
    tracing::info!("plugin mode, protocol v{PROTOCOL}");

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match rpc::parse(&line) {
            Incoming::Request { id, method, params } => match method.as_str() {
                // Handled in order: nothing else makes sense before it ends.
                "initialize" => out.respond(id, plugin.clone().initialize(params).await),
                "shutdown" => {
                    out.respond(id, Ok(Value::Null));
                    break;
                }
                _ => {
                    let plugin = plugin.clone();
                    tokio::spawn(async move {
                        let result = plugin.clone().handle(&method, params).await;
                        plugin.note_auth_failure(&method, &result);
                        plugin.out.respond(id, result);
                    });
                }
            },
            Incoming::Notification { method, params } => plugin.notification(&method, params),
            Incoming::Response => {}
            Incoming::Invalid { id, error } => out.respond(id, Err(error)),
        }
    }
    tracing::info!("plugin stopping");
    drop(out);
    drop(plugin);
    // In-flight tasks may still hold a sender; give the writer a moment.
    let _ = tokio::time::timeout(Duration::from_millis(500), writer).await;
    // The blocking stdin reader would keep the runtime alive.
    std::process::exit(0)
}

/// Language of a locale: `fr-FR`, `fr_FR.UTF-8`, `fr` → `fr`; `en` if none.
fn lang_of(locale: Option<&str>) -> String {
    let lang = locale.unwrap_or("").split(['-', '_', '.']).next().unwrap_or("").to_ascii_lowercase();
    if lang.is_empty() || !lang.chars().all(|c| c.is_ascii_alphabetic()) { "en".into() } else { lang }
}

fn params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

#[derive(Deserialize)]
struct InitializeParams {
    data_dir: PathBuf,
    #[serde(default)]
    output: Option<Output>,
    #[serde(default)]
    protocol: Option<u64>,
    #[serde(default)]
    locale: Option<String>,
    /// Values the host stored for our settings.
    #[serde(default)]
    settings: Option<Value>,
}

#[derive(Deserialize)]
struct RefParams {
    #[serde(rename = "ref")]
    reference: String,
}

#[derive(Deserialize)]
struct FavoriteParams {
    #[serde(rename = "ref")]
    reference: String,
    on: bool,
}

#[derive(Deserialize)]
struct CompleteParams {
    input: String,
}

/// Account shown by the host: the public profile name and the
/// subscription. Never the e-mail address: hosts keep this in their logs
/// and diagnostic reports.
fn status_json(user: &User) -> Value {
    let name = if user.name.is_empty() { "Deezer account" } else { &user.name };
    let mut account = json!({"display_name": name});
    if let Some(offer) = &user.offer {
        account["detail"] = offer.clone().into();
    }
    json!({"state": "signed_in", "account": account})
}

fn sign_in_instructions(lang: &str) -> &'static str {
    if lang == "fr" {
        "Connectez-vous sur la page Deezer qui s'ouvre. Ouvrez ensuite les outils de développement du navigateur \
         (F12) : onglet Stockage (Firefox) ou Application (Chrome, Edge), puis Cookies → https://www.deezer.com. \
         Copiez la valeur du cookie « arl » (une longue suite de chiffres et de lettres) et collez-la ici. Elle \
         donne accès à votre compte : ne la communiquez à personne."
    } else {
        "Sign in on the Deezer page that opens. Then open the browser's developer tools (F12): the Storage tab \
         (Firefox) or Application tab (Chrome, Edge), then Cookies → https://www.deezer.com. Copy the value of the \
         \"arl\" cookie (a long string of digits and letters) and paste it here. It opens your account: never \
         share it."
    }
}

impl Plugin {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dz(&self) -> Deezer {
        self.deezer.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_dz(&self, dz: Deezer) {
        *self.deezer.lock().unwrap_or_else(|e| e.into_inner()) = dz;
    }

    fn data_dir(&self) -> Result<PathBuf, RpcError> {
        self.state().data_dir.clone().ok_or_else(|| RpcError::new(rpc::INVALID_REQUEST, "initialize first"))
    }

    /// The signed-in client, its session open, or `auth_required`.
    async fn authed(&self) -> Result<Deezer, RpcError> {
        let dz = self.dz();
        if !dz.has_arl() {
            return Err(RpcError::auth_required());
        }
        let user = dz.user().await?;
        items::set_user(Some(user.id));
        Ok(dz)
    }

    async fn initialize(self: Arc<Self>, p: Value) -> RpcResult {
        let p: InitializeParams = params(p)?;
        if p.protocol.is_some_and(|v| v != PROTOCOL) {
            tracing::warn!("host speaks protocol {:?}, this plugin v{PROTOCOL}", p.protocol);
        }
        let creds = Credentials::load(&p.data_dir).map_err(RpcError::from)?;
        let lang = lang_of(p.locale.as_deref());
        {
            let mut st = self.state();
            st.output = p.output.unwrap_or_default();
            st.data_dir = Some(p.data_dir);
            st.expiry_notified = false;
            st.lang = lang.clone();
            st.settings = settings::Settings::from_values(p.settings.as_ref());
            st.favorites_at = None;
            tracing::info!("settings: {:?}", st.settings);
        }
        items::set_lang(&lang);
        items::set_user(None);
        self.library.forget(None);
        let dz = self.dz();
        self.set_dz(match &creds {
            Some(c) => dz.with_arl(&c.arl),
            None => dz.signed_out(),
        });
        tracing::info!("initialized, {}", if creds.is_some() { "signed in" } else { "signed out" });
        if creds.is_some() {
            // Off the handshake: the session, then the favourites and the
            // reports left over from last time.
            let me = self.clone();
            tokio::spawn(async move {
                if me.authed().await.is_ok() {
                    me.refresh_favorites();
                    me.flush_reports();
                }
            });
        }
        Ok(json!({
            "protocol": PROTOCOL,
            "plugin": {"id": "deezer", "name": "Deezer", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "auth": true, "browse": true, "search": true, "resolve": true,
                // Always declared: whether reports go out is a setting.
                "favorites": true, "reporting": true, "remote_control": false, "library": true,
                "lyrics": true, "radio": true, "details": true, "playlist_edit": true
            },
            "settings": settings::declaration(&lang),
        }))
    }

    async fn handle(self: Arc<Self>, method: &str, p: Value) -> RpcResult {
        if matches!(method, "browse.list" | "search" | "item.get") || method.starts_with("library.") {
            self.refresh_favorites();
        }
        match method {
            "auth.status" => self.auth_status().await,
            "auth.begin" => {
                self.data_dir()?;
                let lang = self.state().lang.clone();
                let url = format!("{}/login", self.dz().endpoints().gw);
                Ok(json!({"url": url, "instructions": sign_in_instructions(&lang), "expects_input": true}))
            }
            "auth.complete" => {
                let p: CompleteParams = params(p)?;
                let arl = account::extract_arl(&p.input).ok_or_else(|| {
                    RpcError::invalid_params("no arl in the input: paste the value of the \"arl\" cookie of deezer.com")
                })?;
                self.sign_in(&arl).await
            }
            "auth.sign_out" => {
                let data_dir = self.data_dir()?;
                Credentials::delete(&data_dir).map_err(RpcError::from)?;
                self.set_dz(self.dz().signed_out());
                items::set_user(None);
                self.library.forget(None);
                self.reports.clear(Some(&data_dir));
                self.state().favorites_at = None;
                tracing::info!("signed out");
                Ok(json!({"state": "signed_out"}))
            }
            "browse.root" => catalog::root(),
            "browse.list" => catalog::list(&self.authed().await?, &self.library, params(p)?).await,
            "search" => {
                let p = params(p)?;
                catalog::search(&self.authed().await?, p).await
            }
            "library.albums" | "library.artists" | "library.tracks" | "library.playlists" => {
                let p = params(p)?;
                catalog::library(&self.authed().await?, &self.library, method, p).await
            }
            "item.get" => {
                let p: RefParams = params(p)?;
                catalog::get(&self.authed().await?, &p.reference).await
            }
            "favorites.set" => {
                let p: FavoriteParams = params(p)?;
                catalog::set_favorite(&self.authed().await?, &self.library, &p.reference, p.on).await
            }
            "playlists.create" | "playlists.rename" | "playlists.delete" | "playlists.add" | "playlists.remove"
            | "playlists.move" => {
                let p = params(p)?;
                playlists::edit(&self.authed().await?, &self.library, method, p).await
            }
            "item.details" => {
                let p: RefParams = params(p)?;
                let lang = self.state().lang.clone();
                details::get(&self.authed().await?, &p.reference, &lang).await
            }
            "radio.next" => {
                let p = params(p)?;
                catalog::radio_next(&self.authed().await?, p).await
            }
            "lyrics.get" => {
                let p: RefParams = params(p)?;
                lyrics::get(&self.authed().await?, &p.reference).await
            }
            "track.resolve" => {
                let p: RefParams = params(p)?;
                let dz = self.authed().await?;
                let (output, quality) = {
                    let st = self.state();
                    (st.output.clone(), st.settings.quality)
                };
                let (result, resolved) = resolve::resolve(&dz, &self.relay, &p.reference, &output, quality).await?;
                tracing::debug!("{}: {} through the relay", p.reference, resolved.format);
                self.reports.resolved(&p.reference, resolved);
                Ok(result)
            }
            _ => Err(RpcError::new(rpc::METHOD_NOT_FOUND, format!("unknown method {method}"))),
        }
    }

    fn notification(self: &Arc<Self>, method: &str, p: Value) {
        match method {
            "settings.changed" => {
                let s = settings::Settings::from_values(p.get("settings"));
                tracing::info!("settings changed: {s:?}");
                let stop_reports = !s.report_playback;
                self.state().settings = s;
                if stop_reports {
                    self.reports.clear(self.state().data_dir.as_deref());
                }
            }
            "locale.changed" => {
                let lang = lang_of(p.get("locale").and_then(Value::as_str));
                tracing::info!("host language: {lang}");
                items::set_lang(&lang);
                self.state().lang = lang.clone();
                // Settings' labels follow.
                self.out.notify("settings.declared", json!({"settings": settings::declaration(&lang)}));
            }
            "playback.started" | "playback.ended" => self.playback(method, &p),
            "playback.progress" => {}
            "output.changed" => {
                // Accept the output object itself or wrapped in `output`.
                let p = p.get("output").cloned().unwrap_or(p);
                match serde_json::from_value::<Output>(p) {
                    Ok(output) => {
                        tracing::info!("output changed: max {:?} Hz / {:?} bits", output.max_rate, output.max_bits);
                        self.state().output = output;
                    }
                    Err(e) => tracing::warn!("output.changed: {e}"),
                }
            }
            _ => tracing::debug!("ignored notification {method}"),
        }
    }

    /// `playback.started` and `playback.ended` from the host: play reports,
    /// when the user left them on.
    fn playback(self: &Arc<Self>, method: &str, p: &Value) {
        let (enabled, data_dir) = {
            let st = self.state();
            (st.settings.report_playback, st.data_dir.clone())
        };
        if !enabled || !self.dz().has_arl() {
            return;
        }
        let Some(reference) = p.get("ref").and_then(Value::as_str) else { return };
        if method == "playback.started" {
            self.reports.started(reference);
        } else {
            let listened_ms = p.get("listened_ms").and_then(Value::as_u64).unwrap_or(0);
            if self.reports.ended(reference, listened_ms, data_dir.as_deref()) {
                self.flush_reports();
            }
        }
    }

    /// Read the favourite lists again in the background, when they are
    /// older than `FAVORITES_MAX_AGE` (or were never read). Items say
    /// whether they are favourites once the ids are in.
    fn refresh_favorites(self: &Arc<Self>) {
        if !self.dz().has_arl() {
            return;
        }
        {
            let mut st = self.state();
            if st.favorites_at.is_some_and(|t| t.elapsed() < FAVORITES_MAX_AGE) {
                return;
            }
            st.favorites_at = Some(Instant::now());
        }
        let me = self.clone();
        tokio::spawn(async move {
            let Ok(dz) = me.authed().await else {
                me.state().favorites_at = None;
                return;
            };
            for tab in catalog::Tab::ALL {
                if let Err(e) = me.library.list(&dz, tab).await {
                    tracing::info!("favourite {}s not read: {e:#}", tab.kind());
                    me.state().favorites_at = None;
                }
            }
        });
    }

    /// Send the queued play reports in the background.
    fn flush_reports(self: &Arc<Self>) {
        if !self.state().settings.report_playback {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let Ok(dz) = me.authed().await else { return };
            let data_dir = me.state().data_dir.clone();
            me.reports.flush(&dz, data_dir.as_deref()).await;
        });
    }

    /// An `arl` refused mid-session: tell the host once, without waiting
    /// for its next `auth.status`.
    fn note_auth_failure(&self, method: &str, result: &RpcResult) {
        if !matches!(result, Err(e) if e.code == AUTH_REQUIRED) || method.starts_with("auth.") || !self.dz().has_arl() {
            return;
        }
        let mut st = self.state();
        if !st.expiry_notified {
            st.expiry_notified = true;
            tracing::info!("the session cookie was refused: signed out");
            self.out.notify("auth.changed", json!({"state": "expired"}));
        }
    }

    async fn auth_status(&self) -> RpcResult {
        let dz = self.dz();
        if !dz.has_arl() {
            return Ok(json!({"state": "signed_out"}));
        }
        match dz.user().await {
            Ok(user) => {
                items::set_user(Some(user.id));
                Ok(status_json(&user))
            }
            Err(e) if e.chain().any(|c| c.downcast_ref::<SignedOut>().is_some()) => {
                tracing::info!("stored session cookie refused");
                Ok(json!({"state": "expired"}))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn sign_in(self: &Arc<Self>, arl: &str) -> RpcResult {
        let data_dir = self.data_dir()?;
        let dz = self.dz().with_arl(arl);
        let user = match dz.refresh().await {
            Ok(user) => user,
            Err(e) if e.chain().any(|c| c.downcast_ref::<SignedOut>().is_some()) => {
                return Err(RpcError::invalid_params(
                    "Deezer does not accept this arl: it may have expired, or been copied incompletely",
                ));
            }
            Err(e) => return Err(e.into()),
        };
        Credentials { arl: arl.to_string() }.save(&data_dir).map_err(RpcError::from)?;
        self.set_dz(dz);
        items::set_user(Some(user.id));
        self.library.forget(None);
        {
            let mut st = self.state();
            st.expiry_notified = false;
            st.favorites_at = None;
        }
        self.refresh_favorites();
        tracing::info!("signed in");
        Ok(status_json(&user))
    }
}
