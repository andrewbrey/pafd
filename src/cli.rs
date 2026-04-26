use crate::client::Client;
use crate::client::ClientOptions;
use crate::client::TtsPlaybackRequest;
use crate::server;
use crate::tts;
use crate::tts::ProviderKind;
use crate::tts::Voice;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use std::io::IsTerminal;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

type CliResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Playback bearer token
    #[arg(short, long, global = true, env = "PAFD_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

impl Cli {
    /// # Errors
    /// Returns an error if the dispatched subcommand fails.
    pub async fn run(self) -> CliResult {
        self.command.run(self.token).await
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Synthesize text to speech audio
    Tts(TtsCommand),
    /// Play bundled audio
    Sound(SoundCommand),
    /// Run the playback server daemon
    Server(ServerCommand),
}

impl Command {
    async fn run(self, token: Option<String>) -> CliResult {
        match self {
            Self::Tts(command) => command.run(token).await,
            Self::Sound(command) => command.run(token).await,
            Self::Server(command) => command.run(token).await,
        }
    }
}

#[derive(Args, Debug)]
struct ClientArgs {
    /// Playback server URL (e.g. `http://172.161.0.1:8421`)
    #[arg(short, long, env = "PAFD_SERVER", value_name = "URL")]
    server: Option<String>,
}

impl ClientArgs {
    fn into_client(self, token: Option<String>) -> Result<Client, &'static str> {
        let server = self
            .server
            .ok_or("--server is required (or set PAFD_SERVER)")?;
        let server_url = if server.contains("://") {
            server
        } else {
            format!("http://{server}")
        };
        Ok(Client::new(ClientOptions { server_url, token }))
    }
}

#[derive(Args, Debug)]
struct TtsCommand {
    #[command(flatten)]
    client: ClientArgs,

    /// Text to synthesize
    #[arg(num_args = 0..)]
    message: Vec<String>,

    /// Read text to synthesize from this file
    #[arg(short, long, value_name = "FILE")]
    input: Option<PathBuf>,

    /// TTS provider
    #[arg(short, long, value_enum, default_value_t = CliProvider::Auto)]
    provider: CliProvider,

    /// Preferred provider voice id
    #[arg(long)]
    voice: Option<String>,

    /// List available providers with their voices and exit
    #[arg(long)]
    info: bool,
}

impl TtsCommand {
    async fn run(self, token: Option<String>) -> CliResult {
        let voices = collect_provider_voices().await;

        if self.info {
            print_provider_voices(&voices);
            return Ok(());
        }

        if let Some(voice) = self.voice.as_deref() {
            validate_voice(voice, self.provider.kind(), &voices)?;
        }

        let text = resolve_text(&self.message, self.input.as_deref())?;
        let cancellation = tts::cancellation_on_shutdown_signal();
        let mut request = TtsPlaybackRequest::new(text)
            .with_provider(self.provider.kind())
            .with_cancellation(cancellation);
        if let Some(voice) = self.voice {
            request = request.with_voice(voice);
        }
        let client = self.client.into_client(token)?;
        client.ping().await?;
        client.play_tts(&request).await?;
        Ok(())
    }
}

fn resolve_text(message: &[String], input: Option<&Path>) -> Result<String, &'static str> {
    match (message.is_empty(), input) {
        (false, None) => Ok(message.join(" ")),
        (true, Some(path)) => std::fs::read_to_string(path)
            .map(|text| text.trim().to_owned())
            .map_err(|_| "failed to read --input file"),
        (false, Some(_)) => Err("provide either MESSAGE or --input, not both"),
        (true, None) => {
            let mut stdin = std::io::stdin();
            if stdin.is_terminal() {
                return Err("provide MESSAGE, --input, or pipe text via stdin");
            }
            let mut text = String::new();
            stdin
                .read_to_string(&mut text)
                .map_err(|_| "failed to read stdin")?;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Err("provide MESSAGE or --input");
            }
            Ok(trimmed.to_owned())
        }
    }
}

struct ProviderVoices {
    kind: ProviderKind,
    result: Result<Vec<Voice>, tts::TtsError>,
}

async fn collect_provider_voices() -> Vec<ProviderVoices> {
    let cancellation = tts::Cancellation::none();
    let mut out = Vec::new();
    for provider in CliProvider::ALL {
        let kind = provider.kind();
        if matches!(kind, ProviderKind::Auto) {
            continue;
        }
        let mut tts = tts::Tts::with_provider_kind(kind);
        let result = tts.list_voices(&cancellation).await;
        out.push(ProviderVoices { kind, result });
    }
    out
}

fn print_provider_voices(providers: &[ProviderVoices]) {
    let mut first = true;
    for entry in providers {
        if !first {
            println!();
        }
        first = false;
        println!("{}", entry.kind.name());
        match &entry.result {
            Ok(voices) if voices.is_empty() => println!("  (no voices reported)"),
            Ok(voices) => {
                let id_width = voices.iter().map(|v| v.id().len()).max().unwrap_or(0);
                let lang_width = voices
                    .iter()
                    .map(|v| v.lang().map_or(0, str::len))
                    .max()
                    .unwrap_or(0);
                for voice in voices {
                    let lang = voice.lang().unwrap_or("");
                    let name = if voice.name().is_empty() || voice.name() == voice.id() {
                        ""
                    } else {
                        voice.name()
                    };
                    println!(
                        "  {:<id_width$}  {:<lang_width$}  {}",
                        voice.id(),
                        lang,
                        name
                    );
                }
            }
            Err(error) => println!("  (unavailable: {error})"),
        }
    }
}

fn validate_voice(
    voice: &str,
    selected: ProviderKind,
    providers: &[ProviderVoices],
) -> Result<(), Box<dyn std::error::Error>> {
    let relevant: Vec<&ProviderVoices> = providers
        .iter()
        .filter(|entry| matches!(selected, ProviderKind::Auto) || entry.kind == selected)
        .collect();

    let mut have_data = false;
    for entry in &relevant {
        if let Ok(voices) = &entry.result {
            have_data = true;
            if voices.iter().any(|v| v.id() == voice) {
                return Ok(());
            }
        }
    }

    if !have_data {
        return Ok(());
    }

    Err(format!(
        "voice '{voice}' is not available for provider '{}'",
        selected.name()
    )
    .into())
}

#[derive(Args, Debug)]
struct SoundCommand {
    #[command(flatten)]
    client: ClientArgs,

    /// Bundled audio to play
    #[arg(value_enum, required_unless_present = "list")]
    sound: Option<Sound>,

    /// List bundled sounds and exit
    #[arg(long)]
    list: bool,
}

impl SoundCommand {
    async fn run(self, token: Option<String>) -> CliResult {
        if self.list {
            for sound in Sound::ALL {
                println!("{}", sound.slug());
            }
            return Ok(());
        }
        let sound = self.sound.expect("clap enforces sound when --list absent");
        let client = self.client.into_client(token)?;
        let cancellation = tts::cancellation_on_shutdown_signal();
        client.ping().await?;
        client.play_ogg_bytes(sound.bytes(), &cancellation).await?;
        Ok(())
    }
}

#[derive(Args, Debug)]
struct ServerCommand {
    /// Address to bind the HTTP server to
    #[arg(short, long, default_value = "0.0.0.0:8421")]
    bind: String,
}

impl ServerCommand {
    async fn run(self, token: Option<String>) -> CliResult {
        server::serve(&self.bind, token).await;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum CliProvider {
    Auto,
    EdgeTts,
    GoogleTts,
    Piper,
}

impl CliProvider {
    const ALL: &'static [Self] = &[Self::Auto, Self::EdgeTts, Self::GoogleTts, Self::Piper];

    const fn kind(self) -> ProviderKind {
        match self {
            Self::Auto => ProviderKind::Auto,
            Self::EdgeTts => ProviderKind::EdgeTts,
            Self::GoogleTts => ProviderKind::GoogleTts,
            Self::Piper => ProviderKind::Piper,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "snake_case")]
enum Sound {
    AnimalStick,
    B2,
    BeenTree,
    CompleteQuestRequirement,
    ConfirmDelivery,
    Flitterbug,
    HereYouGoLighter,
    HiFlowersHit,
    ItemPickup,
    SaveAndCheckout,
}

impl Sound {
    const ALL: &'static [Self] = &[
        Self::AnimalStick,
        Self::B2,
        Self::BeenTree,
        Self::CompleteQuestRequirement,
        Self::ConfirmDelivery,
        Self::Flitterbug,
        Self::HereYouGoLighter,
        Self::HiFlowersHit,
        Self::ItemPickup,
        Self::SaveAndCheckout,
    ];

    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::AnimalStick => include_bytes!("../audio/animal_stick.ogg"),
            Self::B2 => include_bytes!("../audio/b2.ogg"),
            Self::BeenTree => include_bytes!("../audio/been_tree.ogg"),
            Self::CompleteQuestRequirement => {
                include_bytes!("../audio/complete_quest_requirement.ogg")
            }
            Self::ConfirmDelivery => include_bytes!("../audio/confirm_delivery.ogg"),
            Self::Flitterbug => include_bytes!("../audio/flitterbug.ogg"),
            Self::HereYouGoLighter => include_bytes!("../audio/here_you_go_lighter.ogg"),
            Self::HiFlowersHit => include_bytes!("../audio/hi_flowers_hit.ogg"),
            Self::ItemPickup => include_bytes!("../audio/item_pickup.ogg"),
            Self::SaveAndCheckout => include_bytes!("../audio/save_and_checkout.ogg"),
        }
    }

    const fn slug(self) -> &'static str {
        match self {
            Self::AnimalStick => "animal_stick",
            Self::B2 => "b2",
            Self::BeenTree => "been_tree",
            Self::CompleteQuestRequirement => "complete_quest_requirement",
            Self::ConfirmDelivery => "confirm_delivery",
            Self::Flitterbug => "flitterbug",
            Self::HereYouGoLighter => "here_you_go_lighter",
            Self::HiFlowersHit => "hi_flowers_hit",
            Self::ItemPickup => "item_pickup",
            Self::SaveAndCheckout => "save_and_checkout",
        }
    }
}
