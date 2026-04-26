use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use futures_util::stream::BoxStream;
use serde::Deserialize;
use serde::Serialize;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::watch;

pub mod process;
pub mod providers;

use providers::edge::EdgeTts;
use providers::edge::EdgeTtsOptions;
use providers::google::GoogleTts;
use providers::google::GoogleTtsOptions;
use providers::piper::Piper;
use providers::piper::PiperOptions;

pub type Result<T> = std::result::Result<T, TtsError>;

#[derive(Debug, Error)]
pub enum TtsError {
    #[error("operation cancelled")]
    Cancelled,

    #[error("{provider} provider unavailable: {reason}")]
    ProviderUnavailable {
        provider: &'static str,
        reason: String,
    },

    #[error("{provider} provider failed: {reason}")]
    ProviderFailed {
        provider: &'static str,
        reason: String,
    },

    #[error("{provider} does not support {operation}")]
    Unsupported {
        provider: &'static str,
        operation: &'static str,
    },

    #[error("all TTS providers failed: {0}")]
    AllProvidersFailed(String),

    #[error("cache directory unavailable")]
    CacheDirUnavailable,

    #[error("command `{command}` failed {status}: {stderr}")]
    CommandFailed {
        command: String,
        status: std::process::ExitStatus,
        stderr: String,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("websocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("invalid HTTP header: {0}")]
    Header(#[from] tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue),
}

impl TtsError {
    pub(crate) fn provider_unavailable(provider: &'static str, reason: impl Into<String>) -> Self {
        Self::ProviderUnavailable {
            provider,
            reason: reason.into(),
        }
    }

    pub(crate) fn provider_failed(provider: &'static str, reason: impl Into<String>) -> Self {
        Self::ProviderFailed {
            provider,
            reason: reason.into(),
        }
    }

    pub(crate) const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Cancellation {
    shutdown: Option<watch::Receiver<bool>>,
}

impl Cancellation {
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn channel() -> (watch::Sender<bool>, Self) {
        let (sender, receiver) = watch::channel(false);
        (sender, Self::from_watch(receiver))
    }

    #[must_use]
    pub const fn from_watch(shutdown: watch::Receiver<bool>) -> Self {
        Self {
            shutdown: Some(shutdown),
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.shutdown
            .as_ref()
            .is_some_and(|shutdown| *shutdown.borrow())
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(TtsError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn cancelled(&self) {
        let Some(shutdown) = &self.shutdown else {
            std::future::pending::<()>().await;
            return;
        };
        let mut shutdown = shutdown.clone();
        if *shutdown.borrow() {
            return;
        }
        while shutdown.changed().await.is_ok() {
            if *shutdown.borrow() {
                return;
            }
        }
    }

    pub(crate) async fn run<T, F>(&self, future: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        self.check()?;
        if self.shutdown.is_none() {
            return future.await;
        }
        tokio::select! {
            result = future => result,
            () = self.cancelled() => Err(TtsError::Cancelled),
        }
    }
}

#[cfg(unix)]
pub(crate) async fn wait_for_shutdown_signal() {
    use tokio::signal;
    use tokio::signal::unix::SignalKind;
    use tokio::signal::unix::signal as unix_signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("install SIGINT handler");
    };

    let terminate = async {
        unix_signal(SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        () = ctrl_c => tracing::info!("received SIGINT; shutting down"),
        () = terminate => tracing::info!("received SIGTERM; shutting down"),
    }
}

#[cfg(not(unix))]
pub(crate) async fn wait_for_shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("install SIGINT handler");
    tracing::info!("received SIGINT; shutting down");
}

#[must_use]
pub fn cancellation_on_shutdown_signal() -> Cancellation {
    let (sender, cancellation) = Cancellation::channel();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        let _ = sender.send(true);
    });
    cancellation
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioFormat {
    Mp3,
    Wav,
    Unknown(String),
}

impl AudioFormat {
    #[must_use]
    pub fn extension(&self) -> Option<&str> {
        match self {
            Self::Mp3 => Some("mp3"),
            Self::Wav => Some("wav"),
            Self::Unknown(ext) => ext.strip_prefix('.').or(Some(ext.as_str())),
        }
    }
}

pub struct AudioStream {
    pub(crate) format: AudioFormat,
    pub(crate) inner: BoxStream<'static, Result<Bytes>>,
}

impl AudioStream {
    pub(crate) fn new<S>(format: AudioFormat, stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes>> + Send + 'static,
    {
        Self {
            format,
            inner: Box::pin(stream),
        }
    }

    #[must_use]
    pub const fn format(&self) -> &AudioFormat {
        &self.format
    }

    #[must_use]
    pub fn into_inner(self) -> BoxStream<'static, Result<Bytes>> {
        self.inner
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Voice {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) lang: Option<String>,
}

impl Voice {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn lang(&self) -> Option<&str> {
        self.lang.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoiceSelection {
    Single(String),
    Priority(Vec<String>),
}

impl From<String> for VoiceSelection {
    fn from(value: String) -> Self {
        Self::Single(value)
    }
}

impl From<&str> for VoiceSelection {
    fn from(value: &str) -> Self {
        Self::Single(value.to_owned())
    }
}

impl From<Vec<String>> for VoiceSelection {
    fn from(value: Vec<String>) -> Self {
        Self::Priority(value)
    }
}

#[derive(Clone, Debug)]
pub struct SynthesizeOptions {
    pub(crate) voice: Option<VoiceSelection>,
    pub(crate) cancellation: Cancellation,
}

impl Default for SynthesizeOptions {
    fn default() -> Self {
        Self {
            voice: None,
            cancellation: Cancellation::none(),
        }
    }
}

impl SynthesizeOptions {
    #[must_use]
    pub fn with_voice(mut self, voice: impl Into<VoiceSelection>) -> Self {
        self.voice = Some(voice.into());
        self
    }

    #[must_use]
    pub fn with_cancellation(mut self, cancellation: Cancellation) -> Self {
        self.cancellation = cancellation;
        self
    }
}

#[async_trait]
pub(crate) trait VoiceProvider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn synthesize(&self, text: &str, options: &SynthesizeOptions) -> Result<AudioStream>;

    async fn list_voices(&self, _cancellation: &Cancellation) -> Result<Vec<Voice>> {
        Err(TtsError::Unsupported {
            provider: self.name(),
            operation: "listing voices",
        })
    }

    fn has_voice(&self, _id: &str) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderKind {
    Auto,
    EdgeTts,
    GoogleTts,
    Piper,
}

impl ProviderKind {
    pub const ALL: &'static [Self] = &[Self::Auto, Self::EdgeTts, Self::GoogleTts, Self::Piper];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::EdgeTts => "edge-tts",
            Self::GoogleTts => "google-tts",
            Self::Piper => "piper",
        }
    }

    fn into_provider(self) -> Provider {
        match self {
            Self::Auto => Provider::Auto,
            Self::EdgeTts => Provider::EdgeTts(EdgeTtsOptions::default()),
            Self::GoogleTts => Provider::GoogleTts(GoogleTtsOptions::default()),
            Self::Piper => Provider::Piper(PiperOptions::default()),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Provider {
    Auto,
    EdgeTts(EdgeTtsOptions),
    GoogleTts(GoogleTtsOptions),
    Piper(PiperOptions),
}

impl Provider {
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::EdgeTts(_) => "edge-tts",
            Self::GoogleTts(_) => "google-tts",
            Self::Piper(_) => "piper",
        }
    }

    fn build(&self, cancellation: &Cancellation) -> Result<Box<dyn VoiceProvider>> {
        cancellation.check()?;
        match self {
            Self::Auto => Err(TtsError::provider_unavailable(
                self.name(),
                "auto is not a concrete provider",
            )),
            Self::EdgeTts(options) => Ok(Box::new(EdgeTts::new(options.clone()))),
            Self::GoogleTts(options) => Ok(Box::new(GoogleTts::new(options.clone()))),
            Self::Piper(options) => Ok(Box::new(Piper::new(options.clone()))),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TtsOptions {
    pub(crate) providers: Vec<Provider>,
}

pub struct Tts {
    providers: Vec<Provider>,
    provider: Option<Box<dyn VoiceProvider>>,
    provider_index: usize,
}

impl Default for Tts {
    fn default() -> Self {
        Self::new()
    }
}

impl Tts {
    #[must_use]
    pub fn new() -> Self {
        Self::with_options(TtsOptions::default())
    }

    pub(crate) fn with_options(options: TtsOptions) -> Self {
        let providers = normalize_providers(options.providers);
        Self {
            providers,
            provider: None,
            provider_index: 0,
        }
    }

    pub(crate) fn with_providers(providers: Vec<Provider>) -> Self {
        Self::with_options(TtsOptions { providers })
    }

    #[must_use]
    pub fn with_provider_kind(kind: ProviderKind) -> Self {
        Self::with_providers(vec![kind.into_provider()])
    }

    /// Synthesizes `text` using the first available configured provider.
    ///
    /// # Errors
    ///
    /// Returns an error when cancellation is requested or every configured
    /// provider is unavailable or fails to synthesize the text.
    pub async fn synthesize(
        &mut self,
        text: &str,
        options: &SynthesizeOptions,
    ) -> Result<AudioStream> {
        options.cancellation.check()?;
        let mut failures = Vec::new();

        loop {
            self.resolve_provider(&options.cancellation, &mut failures)?;
            let Some(provider) = self.provider.as_ref() else {
                return Err(TtsError::AllProvidersFailed(failures.join("; ")));
            };
            let provider_name = provider.name();

            match provider.synthesize(text, options).await {
                Ok(stream) => return Ok(stream),
                Err(error) if error.is_cancelled() => return Err(error),
                Err(error) => {
                    failures.push(format!("{provider_name}: {error}"));
                    self.provider = None;
                    self.provider_index += 1;
                    if self.provider_index >= self.providers.len() {
                        return Err(TtsError::AllProvidersFailed(failures.join("; ")));
                    }
                }
            }
        }
    }

    /// Lists voices from the first available configured provider.
    ///
    /// # Errors
    ///
    /// Returns an error when cancellation is requested, no provider is
    /// available, or the provider fails to enumerate voices.
    ///
    /// # Panics
    ///
    /// Panics if internal provider resolution leaves the provider slot empty,
    /// which would indicate a bug in `resolve_provider`.
    pub async fn list_voices(&mut self, cancellation: &Cancellation) -> Result<Vec<Voice>> {
        let mut failures = Vec::new();
        self.resolve_provider(cancellation, &mut failures)?;
        let provider = self.provider.as_ref().expect("provider resolved above");
        provider.list_voices(cancellation).await
    }

    fn resolve_provider(
        &mut self,
        cancellation: &Cancellation,
        failures: &mut Vec<String>,
    ) -> Result<()> {
        if self.provider.is_some() {
            return Ok(());
        }

        let single = self.providers.len() == 1;
        let mut last_error = None;
        while self.provider_index < self.providers.len() {
            let provider = &self.providers[self.provider_index];
            match provider.build(cancellation) {
                Ok(provider) => {
                    self.provider = Some(provider);
                    return Ok(());
                }
                Err(error) if error.is_cancelled() => return Err(error),
                Err(error) => {
                    let provider_name = provider.name();
                    failures.push(format!("{provider_name}: {error}"));
                    last_error = Some(error);
                    self.provider_index += 1;
                }
            }
        }

        if let Some(error) = last_error.filter(|_| single) {
            return Err(error);
        }
        Err(TtsError::AllProvidersFailed(failures.join("; ")))
    }
}

pub(crate) async fn resolve_voice<F, Fut, H>(
    voice: Option<&VoiceSelection>,
    has_voice: H,
    list_voices: F,
    cancellation: &Cancellation,
) -> Result<Option<String>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<Voice>>>,
    H: Fn(&str) -> bool,
{
    let Some(voice) = voice else {
        return Ok(None);
    };

    match voice {
        VoiceSelection::Single(voice) => Ok(Some(voice.clone())),
        VoiceSelection::Priority(voices) => {
            if let Some(voice) = voices.iter().find(|voice| has_voice(voice)) {
                return Ok(Some(voice.clone()));
            }

            let available = list_voices().await?;
            cancellation.check()?;
            if let Some(voice) = voices
                .iter()
                .find(|voice| available.iter().any(|available| available.id == **voice))
            {
                return Ok(Some(voice.clone()));
            }

            Ok(voices.first().cloned())
        }
    }
}

pub(crate) fn cache_dir(component: &str) -> Result<PathBuf> {
    let base = dirs::cache_dir().ok_or(TtsError::CacheDirUnavailable)?;
    Ok(base.join("pafd").join(component))
}

const VOICES_CACHE_TTL: Duration = Duration::from_hours(24 * 7);

pub(crate) async fn cached_voices<F, Fut>(component: &str, fetch: F) -> Result<Vec<Voice>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<Voice>>>,
{
    let path = cache_dir(component)?.join("voices.json");
    if let Ok(metadata) = tokio::fs::metadata(&path).await
        && let Ok(modified) = metadata.modified()
        && modified
            .elapsed()
            .is_ok_and(|elapsed| elapsed < VOICES_CACHE_TTL)
        && let Ok(bytes) = tokio::fs::read(&path).await
        && let Ok(voices) = serde_json::from_slice::<Vec<Voice>>(&bytes)
    {
        return Ok(voices);
    }
    let voices = fetch().await?;
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    if let Ok(bytes) = serde_json::to_vec(&voices) {
        let _ = tokio::fs::write(&path, bytes).await;
    }
    Ok(voices)
}

fn normalize_providers(providers: Vec<Provider>) -> Vec<Provider> {
    let providers: Vec<Provider> = providers
        .into_iter()
        .filter(|provider| !matches!(provider, Provider::Auto))
        .collect();

    if providers.is_empty() {
        default_providers()
    } else {
        providers
    }
}

fn default_providers() -> Vec<Provider> {
    vec![
        Provider::EdgeTts(EdgeTtsOptions::default()),
        Provider::GoogleTts(GoogleTtsOptions::default()),
        Provider::Piper(PiperOptions::default()),
    ]
}
