use {
    serenity::{
        all::{
            Attachment, Context, CreateMessage, EventHandler, GatewayIntents, Interaction, Message,
            MessageId, ReactionType, Ready,
        },
        async_trait,
        gateway::ActivityData,
        model::user::OnlineStatus as SerenityOnlineStatus,
    },
    tracing::{debug, info, warn},
};

use crate::config::{
    ActivityType as CfgActivityType, DiscordAccountConfig, OnlineStatus as CfgOnlineStatus,
};

use crate::access;

use {
    moltis_channels::{
        ChannelEvent, ChannelType, Error as ChannelError, InboundMediaDownloader,
        InboundMediaSource, Result as ChannelsResult,
        gating::DmPolicy,
        message_log::MessageLogEntry,
        otp::{
            OtpInitResult, OtpVerifyResult, approve_sender_via_otp, emit_otp_challenge,
            emit_otp_resolution,
        },
        plugin::{
            ChannelAttachment, ChannelEventSink, ChannelMessageKind, ChannelMessageMeta,
            ChannelReplyTarget,
        },
    },
    moltis_common::{http_client::build_default_http_client, ssrf::ssrf_check},
};

use crate::state::AccountStateMap;

/// Required gateway intents for the Discord bot.
pub fn required_intents() -> GatewayIntents {
    GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGE_REACTIONS
        | GatewayIntents::DIRECT_MESSAGE_REACTIONS
}

/// Serenity event handler for a Discord bot account.
pub struct Handler {
    pub account_id: String,
    pub accounts: AccountStateMap,
    downloader: DiscordInboundMediaDownloader,
}

impl Handler {
    #[must_use]
    pub fn new(account_id: String, accounts: AccountStateMap) -> Self {
        Self {
            account_id,
            accounts,
            downloader: DiscordInboundMediaDownloader::new(),
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Discord snowflake epoch (2015-01-01T00:00:00.000Z) in Unix milliseconds.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

fn discord_message_created_ms(message_id: MessageId) -> u64 {
    (message_id.get() >> 22).saturating_add(DISCORD_EPOCH_MS)
}

fn is_valid_lat_lon(latitude: f64, longitude: f64) -> bool {
    (-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude)
}

fn parse_coordinate_component(input: &str) -> Option<f64> {
    let trimmed = input
        .trim()
        .trim_matches(|c| matches!(c, '(' | ')' | '[' | ']' | '{' | '}'));
    if trimmed.is_empty() {
        return None;
    }

    let mut end = 0usize;
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_ascii_digit() || matches!(ch, '+' | '-' | '.') {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let token = &trimmed[..end];
    if !token.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    token.parse::<f64>().ok()
}

fn parse_coordinate_pair(input: &str) -> Option<(f64, f64)> {
    let mut parts = input.split(',');
    let latitude = parse_coordinate_component(parts.next()?)?;
    let longitude = parse_coordinate_component(parts.next()?)?;
    if is_valid_lat_lon(latitude, longitude) {
        Some((latitude, longitude))
    } else {
        None
    }
}

fn parse_coordinates_from_url(url_str: &str) -> Option<(f64, f64)> {
    let parsed = reqwest::Url::parse(url_str).ok()?;

    for key in ["ll", "q", "query"] {
        if let Some((_, value)) = parsed
            .query_pairs()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            && let Some(coords) = parse_coordinate_pair(value.as_ref())
        {
            return Some(coords);
        }
    }

    for segment in [
        parsed.path(),
        parsed.fragment().unwrap_or_default(),
        url_str,
    ] {
        if let Some(at_pos) = segment.find('@')
            && let Some(coords) = parse_coordinate_pair(&segment[at_pos + 1..])
        {
            return Some(coords);
        }
    }

    None
}

fn parse_map_link_coordinates(text: &str) -> Option<(f64, f64)> {
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | '.' | '!' | '?'
            )
        });
        if !(token.starts_with("http://") || token.starts_with("https://")) {
            continue;
        }
        if let Some(coords) = parse_coordinates_from_url(token) {
            return Some(coords);
        }
    }
    None
}

fn parse_plain_text_coordinates(text: &str) -> Option<(f64, f64)> {
    let trimmed = text.trim();
    if trimmed.is_empty() || !trimmed.contains(',') {
        return None;
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | ',' | ' ' | '\t' | '(' | ')'))
    {
        return None;
    }
    parse_coordinate_pair(trimmed)
}

fn extract_location_coordinates(text: &str) -> Option<(f64, f64)> {
    parse_map_link_coordinates(text).or_else(|| parse_plain_text_coordinates(text))
}

/// Maximum byte size for inbound Discord attachments we download. Matches the
/// 25 MiB free-tier upload limit; anything larger is either a Discord Nitro
/// upload we don't want to spend bandwidth on or a misbehaving client.
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

/// Resolved inbound media from a Discord message: the body text the LLM sees,
/// any multimodal attachments, and — if voice — the raw audio bytes so the
/// gateway can persist them to the session media directory.
#[derive(Debug)]
struct InboundMedia {
    body: String,
    attachments: Vec<ChannelAttachment>,
    voice_audio: Option<(Vec<u8>, String)>,
    kind: ChannelMessageKind,
}

/// Outcome of attempting to handle inbound Discord attachments.
#[derive(Debug)]
enum MediaResolveOutcome {
    /// No audio or image attachment present; caller should dispatch text only.
    NoMedia,
    /// Media was handled (successfully or with a recoverable error-fallback body).
    Media(InboundMedia),
    /// A voice attachment was present but STT is not configured. Caller should
    /// send the user-facing hint and stop.
    VoiceSttUnavailable,
}

#[derive(Debug, Clone)]
struct DiscordInboundMediaDownloader {
    client: reqwest::Client,
}

impl DiscordInboundMediaDownloader {
    fn new() -> Self {
        Self {
            client: build_default_http_client(),
        }
    }
}

/// Returns true if the attachment looks like an audio/voice file.
fn attachment_is_audio(a: &Attachment) -> bool {
    if a.content_type
        .as_deref()
        .is_some_and(|ct| ct.starts_with("audio/"))
    {
        return true;
    }
    let name = a.filename.to_ascii_lowercase();
    [".ogg", ".opus", ".mp3", ".wav", ".m4a", ".webm"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

/// Returns true if the attachment looks like a still image.
fn attachment_is_image(a: &Attachment) -> bool {
    if a.content_type
        .as_deref()
        .is_some_and(|ct| ct.starts_with("image/"))
    {
        return true;
    }
    let name = a.filename.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

/// Pick the first handleable attachment. Audio wins over image when both are
/// present, matching Telegram's voice-first ordering.
fn select_media_attachment(attachments: &[Attachment]) -> Option<&Attachment> {
    if let Some(a) = attachments.iter().find(|a| attachment_is_audio(a)) {
        return Some(a);
    }
    attachments.iter().find(|a| attachment_is_image(a))
}

/// Normalize an audio attachment to a short format string the STT provider
/// understands (`ogg`, `mp3`, `wav`, `m4a`, `webm`).
fn audio_format_from_attachment(content_type: Option<&str>, filename: &str) -> String {
    if let Some(ct) = content_type
        && let Some(rest) = ct.strip_prefix("audio/")
    {
        let first = rest.split(';').next().unwrap_or(rest).trim();
        let first = first.strip_prefix("x-").unwrap_or(first);
        return match first {
            "mpeg" | "mp3" => "mp3".to_string(),
            "mp4" | "m4a" => "m4a".to_string(),
            "wav" | "wave" => "wav".to_string(),
            "webm" => "webm".to_string(),
            "ogg" | "opus" | "vorbis" => "ogg".to_string(),
            other if !other.is_empty() => other.to_string(),
            _ => "ogg".to_string(),
        };
    }
    let lower = filename.to_ascii_lowercase();
    for (ext, fmt) in [
        (".mp3", "mp3"),
        (".m4a", "m4a"),
        (".wav", "wav"),
        (".webm", "webm"),
        (".opus", "ogg"),
        (".ogg", "ogg"),
    ] {
        if lower.ends_with(ext) {
            return fmt.to_string();
        }
    }
    // Discord voice messages are OGG Opus; safe default.
    "ogg".to_string()
}

/// Normalize an image attachment to a MIME media type. Used as a fallback when
/// `optimize_for_llm` fails and we must send the original bytes anyway.
fn image_media_type_fallback(content_type: Option<&str>, filename: &str) -> String {
    if let Some(ct) = content_type
        && ct.starts_with("image/")
    {
        return ct.split(';').next().unwrap_or(ct).trim().to_string();
    }
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else {
        "image/jpeg".to_string()
    }
}

fn voice_stt_unavailable_log_body(caption: &str) -> String {
    if caption.is_empty() {
        "[Voice message - STT unavailable]".to_string()
    } else {
        format!("{caption}\n\n[Voice message - STT unavailable]")
    }
}

/// Download a Discord CDN attachment, enforcing a size cap.
async fn download_discord_attachment(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
) -> ChannelsResult<Vec<u8>> {
    let parsed_url = reqwest::Url::parse(url).map_err(|error| {
        ChannelError::invalid_input(format!("invalid discord attachment URL: {error}"))
    })?;
    ssrf_check(&parsed_url, &[])
        .await
        .map_err(|error| ChannelError::external("discord attachment ssrf check", error))?;

    let response = client
        .get(parsed_url)
        .send()
        .await
        .map_err(|e| ChannelError::external("discord attachment request", e))?;
    if !response.status().is_success() {
        return Err(ChannelError::unavailable(format!(
            "discord attachment request returned HTTP {}",
            response.status()
        )));
    }
    if let Some(len) = response.content_length()
        && len as usize > max_bytes
    {
        return Err(ChannelError::unavailable(format!(
            "attachment too large: {len} bytes (cap {max_bytes})"
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| ChannelError::external("discord attachment read body", e))?;
    if bytes.len() > max_bytes {
        return Err(ChannelError::unavailable(format!(
            "attachment too large: {} bytes (cap {max_bytes})",
            bytes.len()
        )));
    }
    Ok(bytes.to_vec())
}

#[async_trait]
impl InboundMediaDownloader for DiscordInboundMediaDownloader {
    async fn download_media(
        &self,
        source: &InboundMediaSource,
        max_bytes: usize,
    ) -> ChannelsResult<Vec<u8>> {
        match source {
            InboundMediaSource::RemoteUrl { url } => {
                download_discord_attachment(&self.client, url, max_bytes).await
            },
            _ => Err(ChannelError::invalid_input(
                "discord downloader received unsupported media source",
            )),
        }
    }
}

async fn log_discord_message(
    message_log: Option<&std::sync::Arc<dyn moltis_channels::message_log::MessageLog>>,
    account_id: &str,
    peer_id: &str,
    username: &Option<String>,
    sender_name: &Option<String>,
    chat_id: &str,
    is_guild: bool,
    body: &str,
    access_granted: bool,
) {
    if let Some(log) = message_log {
        let _ = log
            .log(MessageLogEntry {
                id: 0,
                account_id: account_id.to_string(),
                channel_type: "discord".into(),
                peer_id: peer_id.to_string(),
                username: username.clone(),
                sender_name: sender_name.clone(),
                chat_id: chat_id.to_string(),
                chat_type: if is_guild {
                    "group".into()
                } else {
                    "private".into()
                },
                body: body.to_string(),
                access_granted,
                created_at: unix_now(),
            })
            .await;
    }
}

/// Resolve inbound media attachments on a Discord message into a body +
/// multimodal attachment list the gateway can dispatch.
async fn resolve_discord_inbound_media(
    attachments: &[Attachment],
    caption: &str,
    sink: &dyn ChannelEventSink,
    account_id: &str,
    downloader: &dyn InboundMediaDownloader,
) -> MediaResolveOutcome {
    let Some(media) = select_media_attachment(attachments) else {
        return MediaResolveOutcome::NoMedia;
    };

    if attachment_is_audio(media) {
        if !sink.voice_stt_available().await {
            return MediaResolveOutcome::VoiceSttUnavailable;
        }

        let format = audio_format_from_attachment(media.content_type.as_deref(), &media.filename);
        let source = InboundMediaSource::RemoteUrl {
            url: media.url.clone(),
        };
        match downloader
            .download_media(&source, MAX_ATTACHMENT_BYTES)
            .await
        {
            Ok(audio_data) => {
                debug!(
                    account_id,
                    attachment_id = %media.id,
                    format,
                    size = audio_data.len(),
                    "downloaded discord voice attachment, transcribing"
                );
                let saved_audio = Some((audio_data.clone(), format.clone()));
                match sink.transcribe_voice(&audio_data, &format).await {
                    Ok(transcribed) if transcribed.trim().is_empty() => {
                        warn!(
                            account_id,
                            audio_size = audio_data.len(),
                            "discord voice transcription returned empty text"
                        );
                        MediaResolveOutcome::Media(InboundMedia {
                            body: "[Voice message - could not transcribe]".to_string(),
                            attachments: Vec::new(),
                            voice_audio: saved_audio,
                            kind: ChannelMessageKind::Voice,
                        })
                    },
                    Ok(transcribed) => {
                        debug!(
                            account_id,
                            text_len = transcribed.len(),
                            "discord voice transcription successful"
                        );
                        let body = if caption.is_empty() {
                            transcribed
                        } else {
                            format!("{caption}\n\n[Voice message]: {transcribed}")
                        };
                        MediaResolveOutcome::Media(InboundMedia {
                            body,
                            attachments: Vec::new(),
                            voice_audio: saved_audio,
                            kind: ChannelMessageKind::Voice,
                        })
                    },
                    Err(e) => {
                        warn!(account_id, error = %e, "discord voice transcription failed");
                        let body = if caption.is_empty() {
                            "[Voice message - transcription unavailable]".to_string()
                        } else {
                            caption.to_string()
                        };
                        MediaResolveOutcome::Media(InboundMedia {
                            body,
                            attachments: Vec::new(),
                            voice_audio: saved_audio,
                            kind: ChannelMessageKind::Voice,
                        })
                    },
                }
            },
            Err(e) => {
                warn!(account_id, error = %e, "failed to download discord voice attachment");
                let body = if caption.is_empty() {
                    "[Voice message - download failed]".to_string()
                } else {
                    caption.to_string()
                };
                MediaResolveOutcome::Media(InboundMedia {
                    body,
                    attachments: Vec::new(),
                    voice_audio: None,
                    kind: ChannelMessageKind::Voice,
                })
            },
        }
    } else {
        let source = InboundMediaSource::RemoteUrl {
            url: media.url.clone(),
        };
        match downloader
            .download_media(&source, MAX_ATTACHMENT_BYTES)
            .await
        {
            Ok(image_data) => {
                debug!(
                    account_id,
                    attachment_id = %media.id,
                    size = image_data.len(),
                    "downloaded discord image attachment"
                );
                let (final_data, media_type) =
                    match moltis_media::image_ops::optimize_for_llm(&image_data, None) {
                        Ok(optimized) => {
                            if optimized.was_resized {
                                info!(
                                    account_id,
                                    original_size = image_data.len(),
                                    final_size = optimized.data.len(),
                                    original_dims = %format!(
                                        "{}x{}",
                                        optimized.original_width, optimized.original_height
                                    ),
                                    final_dims = %format!(
                                        "{}x{}",
                                        optimized.final_width, optimized.final_height
                                    ),
                                    "resized discord image for LLM"
                                );
                            }
                            (optimized.data, optimized.media_type)
                        },
                        Err(e) => {
                            warn!(
                                account_id,
                                error = %e,
                                "failed to optimize discord image, using original"
                            );
                            (
                                image_data,
                                image_media_type_fallback(
                                    media.content_type.as_deref(),
                                    &media.filename,
                                ),
                            )
                        },
                    };
                let attachment = ChannelAttachment {
                    media_type,
                    data: final_data,
                };
                MediaResolveOutcome::Media(InboundMedia {
                    body: caption.to_string(),
                    attachments: vec![attachment],
                    voice_audio: None,
                    kind: ChannelMessageKind::Photo,
                })
            },
            Err(e) => {
                warn!(account_id, error = %e, "failed to download discord image attachment");
                let body = if caption.is_empty() {
                    "[Photo - download failed]".to_string()
                } else {
                    caption.to_string()
                };
                MediaResolveOutcome::Media(InboundMedia {
                    body,
                    attachments: Vec::new(),
                    voice_audio: None,
                    kind: ChannelMessageKind::Photo,
                })
            },
        }
    }
}

/// Strip the bot mention (e.g. `<@123456789>`) from the beginning of a message.
pub fn strip_bot_mention(text: &str, bot_id: u64) -> String {
    let mention = format!("<@{bot_id}>");
    let mention_nick = format!("<@!{bot_id}>");
    let stripped = text
        .trim()
        .strip_prefix(&mention)
        .or_else(|| text.trim().strip_prefix(&mention_nick))
        .unwrap_or(text);
    stripped.trim().to_string()
}

/// Set the bot's presence (activity + online status) from config.
fn set_bot_presence(ctx: &Context, account_id: &str, config: &DiscordAccountConfig) {
    let activity = config.activity.as_deref().map(|text| {
        let activity_type = config.activity_type.unwrap_or_default();
        match activity_type {
            CfgActivityType::Playing => ActivityData::playing(text),
            CfgActivityType::Listening => ActivityData::listening(text),
            CfgActivityType::Watching => ActivityData::watching(text),
            CfgActivityType::Competing => ActivityData::competing(text),
            CfgActivityType::Custom => ActivityData::custom(text),
        }
    });

    let online_status = match config.status {
        Some(CfgOnlineStatus::Online) | None => SerenityOnlineStatus::Online,
        Some(CfgOnlineStatus::Idle) => SerenityOnlineStatus::Idle,
        Some(CfgOnlineStatus::Dnd) => SerenityOnlineStatus::DoNotDisturb,
        Some(CfgOnlineStatus::Invisible) => SerenityOnlineStatus::Invisible,
    };

    // Only set presence if there's something to configure.
    if activity.is_some() || config.status.is_some() {
        ctx.set_presence(activity, online_status);
        info!(
            account_id,
            activity_text = ?config.activity,
            activity_type = ?config.activity_type,
            status = ?config.status,
            "Discord bot presence set"
        );
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        // Ignore messages from bots (including ourselves).
        if msg.author.bot {
            return;
        }

        let accounts_lock_wait_start = std::time::Instant::now();
        let (config, event_sink, message_log, bot_user_id) = {
            let accounts = self.accounts.read().unwrap_or_else(|e| e.into_inner());
            let Some(state) = accounts.get(&self.account_id) else {
                warn!(account_id = %self.account_id, "Discord handler: unknown account");
                return;
            };
            (
                state.config.clone(),
                state.event_sink.clone(),
                state.message_log.clone(),
                state.bot_user_id,
            )
        };
        let accounts_lock_wait_ms = accounts_lock_wait_start.elapsed().as_millis() as u64;

        let is_guild = msg.guild_id.is_some();
        let message_id = msg.id.get();
        let peer_id = msg.author.id.to_string();
        let username = Some(msg.author.name.clone());
        let sender_name = msg.author.global_name.clone().or_else(|| username.clone());
        let chat_id = msg.channel_id.to_string();

        // Check if the bot is mentioned in a guild message.
        let bot_mentioned =
            bot_user_id.is_some_and(|bot_id| msg.mentions.iter().any(|u| u.id == bot_id));

        // Extract and clean message text.
        let text = if let Some(bot_id) = bot_user_id
            && bot_mentioned
        {
            strip_bot_mention(&msg.content, bot_id.get())
        } else {
            msg.content.clone()
        };

        // Drop truly empty messages (no text and no attachments). Messages with
        // attachments but no caption still need processing (e.g. voice notes).
        if text.is_empty() && msg.attachments.is_empty() {
            return;
        }

        let created_ms = discord_message_created_ms(msg.id);
        let ingress_lag_ms = unix_now_ms().saturating_sub(created_ms);

        info!(
            account_id = %self.account_id,
            message_id,
            chat_id,
            peer_id,
            username = ?username,
            sender_name = ?sender_name,
            is_guild,
            bot_mentioned,
            text_len = text.len(),
            ingress_lag_ms,
            accounts_lock_wait_ms,
            "discord inbound message received"
        );
        if ingress_lag_ms > 2_000 {
            warn!(
                account_id = %self.account_id,
                message_id,
                chat_id,
                peer_id,
                ingress_lag_ms,
                "discord inbound delivery lag exceeds 2s"
            );
        }

        // Check DM / guild / mention policy.
        let chat_type = if is_guild {
            moltis_common::types::ChatType::Group
        } else {
            moltis_common::types::ChatType::Dm
        };
        let guild_id_str = msg.guild_id.map(|g| g.to_string());
        let policy_allowed = access::check_access(
            &config,
            &chat_type,
            &peer_id,
            username.as_deref(),
            guild_id_str.as_deref(),
            bot_mentioned,
        )
        .is_ok();
        let access_granted = policy_allowed;

        // Emit inbound message event.
        if let Some(sink) = event_sink.as_ref() {
            sink.emit(ChannelEvent::InboundMessage {
                channel_type: ChannelType::Discord,
                account_id: self.account_id.clone(),
                peer_id: peer_id.clone(),
                username: username.clone(),
                sender_name: sender_name.clone(),
                message_count: None,
                access_granted,
            })
            .await;
        }

        if !access_granted {
            log_discord_message(
                message_log.as_ref(),
                &self.account_id,
                &peer_id,
                &username,
                &sender_name,
                &chat_id,
                is_guild,
                &text,
                access_granted,
            )
            .await;

            // OTP self-approval for non-allowlisted DM users.
            if !is_guild
                && !policy_allowed
                && config.otp_self_approval
                && config.dm_policy == DmPolicy::Allowlist
            {
                handle_otp_flow(
                    &self.accounts,
                    &self.account_id,
                    &peer_id,
                    username.as_deref(),
                    sender_name.as_deref(),
                    &text,
                    msg.channel_id,
                    event_sink.as_deref(),
                    &ctx,
                )
                .await;
            }
            return;
        }

        // Add ack reaction to indicate the bot is processing.
        if let Some(ref emoji) = config.ack_reaction {
            let reaction = ReactionType::Unicode(emoji.clone());
            if let Err(e) = msg.react(&ctx, reaction).await {
                debug!(
                    account_id = %self.account_id,
                    emoji,
                    "failed to add ack reaction: {e}"
                );
            }
        }

        let reply_to = ChannelReplyTarget {
            channel_type: ChannelType::Discord,
            account_id: self.account_id.clone(),
            chat_id: chat_id.clone(),
            message_id: Some(msg.id.to_string()),
            thread_id: None,
        };

        let Some(sink) = event_sink else {
            warn!(
                account_id = %self.account_id,
                "Discord inbound message ignored: no channel event sink"
            );
            return;
        };

        // Handle slash commands.
        if let Some(command) = text.strip_prefix('/') {
            log_discord_message(
                message_log.as_ref(),
                &self.account_id,
                &peer_id,
                &username,
                &sender_name,
                &chat_id,
                is_guild,
                &text,
                access_granted,
            )
            .await;

            let response_text = match sink
                .dispatch_command(command.trim(), reply_to.clone())
                .await
            {
                Ok(response) => response,
                Err(e) => format!("Command failed: {e}"),
            };
            let http = {
                let accounts = self.accounts.read().unwrap_or_else(|e| e.into_inner());
                accounts.get(&self.account_id).and_then(|s| s.http.clone())
            };
            if let Some(http) = http
                && let Err(e) = send_discord_text(&http, msg.channel_id, &response_text).await
            {
                warn!(
                    account_id = %self.account_id,
                    chat_id,
                    "failed to send Discord command response: {e}"
                );
            }
            return;
        }

        // Resolve inbound media attachments (voice transcription, image
        // optimization) before further processing. This mirrors the Telegram
        // flow in `crates/telegram/src/handlers.rs`.
        let (body, attachments, voice_audio, mut inferred_kind) =
            match resolve_discord_inbound_media(
                &msg.attachments,
                &text,
                sink.as_ref(),
                &self.account_id,
                &self.downloader,
            )
            .await
            {
                MediaResolveOutcome::NoMedia => {
                    (text.clone(), Vec::new(), None, ChannelMessageKind::Text)
                },
                MediaResolveOutcome::Media(media) => {
                    (media.body, media.attachments, media.voice_audio, media.kind)
                },
                MediaResolveOutcome::VoiceSttUnavailable => {
                    let body = voice_stt_unavailable_log_body(&text);
                    log_discord_message(
                        message_log.as_ref(),
                        &self.account_id,
                        &peer_id,
                        &username,
                        &sender_name,
                        &chat_id,
                        is_guild,
                        &body,
                        access_granted,
                    )
                    .await;
                    if let Err(e) = send_discord_text_simple(
                        &ctx,
                        msg.channel_id,
                        "I can't understand voice, you did not configure it, please visit Settings -> Voice",
                    )
                    .await
                    {
                        warn!(
                            account_id = %self.account_id,
                            chat_id,
                            "failed to send STT setup hint: {e}"
                        );
                    }
                    return;
                },
            };

        log_discord_message(
            message_log.as_ref(),
            &self.account_id,
            &peer_id,
            &username,
            &sender_name,
            &chat_id,
            is_guild,
            &body,
            access_granted,
        )
        .await;

        if let Some((latitude, longitude)) = extract_location_coordinates(&body) {
            let resolved = sink
                .resolve_pending_location(&reply_to, latitude, longitude)
                .await;
            if resolved {
                info!(
                    account_id = %self.account_id,
                    chat_id,
                    peer_id,
                    latitude,
                    longitude,
                    "discord location input resolved pending request"
                );
                if let Err(e) =
                    send_discord_text_simple(&ctx, msg.channel_id, "Location updated.").await
                {
                    warn!(
                        account_id = %self.account_id,
                        chat_id,
                        "failed to send location confirmation: {e}"
                    );
                }
                return;
            }
            inferred_kind = ChannelMessageKind::Location;
        }

        // Save voice audio to the session media directory (best-effort).
        let audio_filename = if let Some((ref audio_data, ref format)) = voice_audio {
            let filename = format!("voice-discord-{}.{format}", msg.id.get());
            sink.save_channel_voice(audio_data, &filename, &reply_to)
                .await
        } else {
            None
        };

        // Dispatch to chat.
        info!(
            account_id = %self.account_id,
            chat_id,
            peer_id,
            body_len = body.len(),
            attachment_count = attachments.len(),
            ?inferred_kind,
            "discord dispatching to chat"
        );

        #[cfg(feature = "metrics")]
        moltis_metrics::counter!(
            moltis_metrics::channels::MESSAGES_RECEIVED_TOTAL,
            moltis_metrics::labels::CHANNEL => "discord"
        )
        .increment(1);

        let meta = ChannelMessageMeta {
            channel_type: ChannelType::Discord,
            sender_name,
            username,
            message_kind: Some(inferred_kind),
            model: config.model.clone(),
            agent_id: None,
            audio_filename,
        };

        if attachments.is_empty() {
            sink.dispatch_to_chat(&body, reply_to, meta).await;
        } else {
            sink.dispatch_to_chat_with_attachments(&body, attachments, reply_to, meta)
                .await;
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(
            account_id = %self.account_id,
            bot_user = %ready.user.name,
            "Discord bot connected as {}",
            ready.user.name,
        );

        let config = {
            let mut accounts = self.accounts.write().unwrap_or_else(|e| e.into_inner());
            if let Some(state) = accounts.get_mut(&self.account_id) {
                state.bot_user_id = Some(ready.user.id);
            }
            accounts.get(&self.account_id).map(|s| s.config.clone())
        };

        // Set bot presence/activity if configured.
        if let Some(config) = config {
            set_bot_presence(&ctx, &self.account_id, &config);
        }

        // Register slash commands.
        crate::commands::register_global_commands(&ctx, &self.account_id).await;
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        crate::commands::handle_interaction(&ctx, &interaction, &self.account_id, &self.accounts)
            .await;
    }
}

/// OTP challenge message sent to non-allowlisted DM users.
///
/// SECURITY: This message must NEVER contain the OTP code. The code is only
/// visible to the admin in the web UI under Channels → Senders.
const OTP_CHALLENGE_MSG: &str = "To use this bot, please enter the verification code.\n\nAsk the bot owner for the code \u{2014} it is visible in the web UI under **Channels \u{2192} Senders**.\n\nThe code expires in 5 minutes.";

/// Check if a message body looks like a 6-digit OTP code.
fn looks_like_otp_code(text: &str) -> bool {
    text.len() == 6 && text.chars().all(|c| c.is_ascii_digit())
}

/// Handle OTP challenge/verification flow for a non-allowlisted DM user.
///
/// Called when `dm_policy = Allowlist`, the peer is not on the allowlist, and
/// `otp_self_approval` is enabled. Manages the full lifecycle:
/// - First message: issue a 6-digit OTP challenge
/// - Code reply: verify and auto-approve on match
/// - Non-code messages while pending: silently ignored (flood protection)
#[allow(clippy::too_many_arguments)]
async fn handle_otp_flow(
    accounts: &AccountStateMap,
    account_id: &str,
    peer_id: &str,
    username: Option<&str>,
    sender_name: Option<&str>,
    body: &str,
    channel_id: serenity::all::ChannelId,
    event_sink: Option<&dyn ChannelEventSink>,
    ctx: &Context,
) {
    let has_pending = {
        let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
        accts
            .get(account_id)
            .map(|s| {
                let otp = s.otp.lock().unwrap_or_else(|e| e.into_inner());
                otp.has_pending(peer_id)
            })
            .unwrap_or(false)
    };

    if has_pending {
        // Only process messages that look like OTP codes (6 digits).
        let trimmed = body.trim();
        if !looks_like_otp_code(trimmed) {
            return; // Silently ignore non-code messages while pending.
        }

        // Verify the code.
        let result = {
            let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
            match accts.get(account_id) {
                Some(s) => {
                    let mut otp = s.otp.lock().unwrap_or_else(|e| e.into_inner());
                    otp.verify(peer_id, trimmed)
                },
                None => return,
            }
        };

        match result {
            OtpVerifyResult::Approved => {
                let identifier = peer_id;
                approve_sender_via_otp(
                    event_sink,
                    ChannelType::Discord,
                    account_id,
                    identifier,
                    peer_id,
                    username,
                )
                .await;

                let _ = send_discord_text_simple(
                    ctx,
                    channel_id,
                    "Approved! You can now use this bot.",
                )
                .await;
            },
            OtpVerifyResult::WrongCode { attempts_left } => {
                let msg = format!(
                    "Incorrect code. {attempts_left} attempt{} remaining.",
                    if attempts_left == 1 {
                        ""
                    } else {
                        "s"
                    }
                );
                let _ = send_discord_text_simple(ctx, channel_id, &msg).await;
            },
            OtpVerifyResult::LockedOut => {
                let _ = send_discord_text_simple(
                    ctx,
                    channel_id,
                    "Too many failed attempts. Please try again later.",
                )
                .await;

                emit_otp_resolution(
                    event_sink,
                    ChannelType::Discord,
                    account_id,
                    peer_id,
                    username,
                    "locked_out",
                )
                .await;
            },
            OtpVerifyResult::Expired => {
                let _ = send_discord_text_simple(
                    ctx,
                    channel_id,
                    "Your code has expired. Send any message to get a new one.",
                )
                .await;

                emit_otp_resolution(
                    event_sink,
                    ChannelType::Discord,
                    account_id,
                    peer_id,
                    username,
                    "expired",
                )
                .await;
            },
            OtpVerifyResult::NoPending => {
                // Shouldn't happen since we checked has_pending, but handle gracefully.
            },
        }
    } else {
        // No pending challenge — initiate one.
        let init_result = {
            let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
            match accts.get(account_id) {
                Some(s) => {
                    let mut otp = s.otp.lock().unwrap_or_else(|e| e.into_inner());
                    otp.initiate(
                        peer_id,
                        username.map(String::from),
                        sender_name.map(String::from),
                    )
                },
                None => return,
            }
        };

        match init_result {
            OtpInitResult::Created(code) => {
                let _ = send_discord_text_simple(ctx, channel_id, OTP_CHALLENGE_MSG).await;

                let expires_at = unix_now() + 300; // 5 minutes
                emit_otp_challenge(
                    event_sink,
                    ChannelType::Discord,
                    account_id,
                    peer_id,
                    username,
                    sender_name,
                    code,
                    expires_at,
                )
                .await;
            },
            OtpInitResult::AlreadyPending | OtpInitResult::LockedOut => {
                // Silent ignore.
            },
        }
    }
}

/// Simple send using the Context's http — used for OTP messages where we don't
/// have the full Http handle from state.
async fn send_discord_text_simple(
    ctx: &Context,
    channel_id: serenity::all::ChannelId,
    text: &str,
) -> Result<(), String> {
    let msg = CreateMessage::new().content(text);
    channel_id
        .send_message(&ctx, msg)
        .await
        .map_err(|e| format!("Discord send: {e}"))?;
    Ok(())
}

/// Send a text message to a Discord channel, chunking at the 2000-character limit.
pub async fn send_discord_text(
    http: &serenity::http::Http,
    channel_id: serenity::all::ChannelId,
    text: &str,
) -> Result<(), String> {
    send_discord_message(http, channel_id, text, None).await?;
    Ok(())
}

/// Send a text message and return the last sent `Message` (needed for
/// edit-in-place streaming).
///
/// When `reference` is `Some`, the first chunk is sent as a Discord reply
/// to that message (using `reference_message`).
pub async fn send_discord_message(
    http: &serenity::http::Http,
    channel_id: serenity::all::ChannelId,
    text: &str,
    reference: Option<MessageId>,
) -> Result<Message, String> {
    if text.is_empty() {
        return Err("empty message".into());
    }

    let chunks = chunk_message(text, 2000);
    let mut last_msg = None;
    for (i, chunk) in chunks.iter().enumerate() {
        let mut create = CreateMessage::new().content(*chunk);
        // Only the first chunk gets the reply reference.
        if i == 0
            && let Some(ref_id) = reference
        {
            create = create.reference_message((channel_id, ref_id));
        }
        last_msg = Some(
            channel_id
                .send_message(http, create)
                .await
                .map_err(|e| format!("Discord send: {e}"))?,
        );
    }
    // `last_msg` is always `Some` because `text` is non-empty.
    last_msg.ok_or_else(|| "no chunks produced".into())
}

/// Split a message into chunks of at most `max_len` characters.
///
/// The chunker is markdown-aware: it avoids splitting inside fenced code blocks
/// (triple-backtick regions) so that Discord renders them correctly.
fn chunk_message(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining);
            break;
        }
        let split_at = find_split_point(remaining, max_len);
        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk);
        remaining = rest;
    }
    chunks
}

/// Find the best position to split `text` within `max_len` bytes.
///
/// Avoids splitting inside fenced code blocks. Prefers newlines outside of code
/// fences, falls back to `max_len` if no better boundary is found.
fn find_split_point(text: &str, max_len: usize) -> usize {
    let window = &text[..max_len];

    // Track whether each newline position is inside a fenced code block.
    let mut in_fence = false;
    let mut best_outside_fence = None;
    let mut best_any_newline = None;

    for (i, line) in window.split('\n').scan(0usize, |pos, line| {
        let start = *pos;
        *pos += line.len() + 1; // +1 for the '\n'
        Some((start, line))
    }) {
        let newline_pos = i + line.len(); // position of the '\n' itself
        if newline_pos >= max_len {
            break;
        }

        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }

        // Record the split position (right after the newline).
        let split = newline_pos + 1;
        best_any_newline = Some(split);
        if !in_fence {
            best_outside_fence = Some(split);
        }
    }

    // Prefer splitting outside a code fence; fall back to any newline; finally
    // fall back to the hard limit.
    best_outside_fence.or(best_any_newline).unwrap_or(max_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_mention_at_start() {
        assert_eq!(strip_bot_mention("<@123> hello world", 123), "hello world");
        assert_eq!(strip_bot_mention("<@!123> hello world", 123), "hello world");
    }

    #[test]
    fn strip_mention_no_match() {
        assert_eq!(
            strip_bot_mention("hello <@123> world", 123),
            "hello <@123> world"
        );
        assert_eq!(strip_bot_mention("hello world", 123), "hello world");
    }

    #[test]
    fn strip_mention_different_bot() {
        assert_eq!(strip_bot_mention("<@999> hello", 123), "<@999> hello");
    }

    #[test]
    fn extract_location_coordinates_from_plain_pair() {
        let coords = extract_location_coordinates("48.8566, 2.3522")
            .unwrap_or_else(|| panic!("expected coordinates"));
        assert!((coords.0 - 48.8566).abs() < 1e-6);
        assert!((coords.1 - 2.3522).abs() < 1e-6);
    }

    #[test]
    fn extract_location_coordinates_from_google_query() {
        let coords =
            extract_location_coordinates("https://www.google.com/maps?q=37.7749,-122.4194")
                .unwrap_or_else(|| panic!("expected coordinates"));
        assert!((coords.0 - 37.7749).abs() < 1e-6);
        assert!((coords.1 + 122.4194).abs() < 1e-6);
    }

    #[test]
    fn extract_location_coordinates_from_google_path_marker() {
        let coords = extract_location_coordinates(
            "https://www.google.com/maps/place/test/@48.8566,2.3522,14z/data=!3m1!4b1",
        )
        .unwrap_or_else(|| panic!("expected coordinates"));
        assert!((coords.0 - 48.8566).abs() < 1e-6);
        assert!((coords.1 - 2.3522).abs() < 1e-6);
    }

    #[test]
    fn extract_location_coordinates_from_apple_maps() {
        let coords =
            extract_location_coordinates("https://maps.apple.com/?ll=34.0522,-118.2437&z=12")
                .unwrap_or_else(|| panic!("expected coordinates"));
        assert!((coords.0 - 34.0522).abs() < 1e-6);
        assert!((coords.1 + 118.2437).abs() < 1e-6);
    }

    #[test]
    fn extract_location_coordinates_rejects_non_location_text() {
        assert!(extract_location_coordinates("hey what's up?").is_none());
        assert!(extract_location_coordinates("my score is 1,2 today").is_none());
    }

    #[test]
    fn chunk_short_message() {
        let chunks = chunk_message("hello", 2000);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn chunk_long_message() {
        let text = "a".repeat(4500);
        let chunks = chunk_message(&text, 2000);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 2000);
        assert_eq!(chunks[1].len(), 2000);
        assert_eq!(chunks[2].len(), 500);
    }

    #[test]
    fn chunk_splits_at_newline() {
        let mut text = String::new();
        text.push_str(&"a".repeat(1500));
        text.push('\n');
        text.push_str(&"b".repeat(1000));
        let chunks = chunk_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 1501); // 1500 + newline
        assert_eq!(chunks[1].len(), 1000);
    }

    #[test]
    fn chunk_avoids_splitting_inside_code_fence() {
        // The code fence fits within max_len, so the split should land before
        // or after the fence — not inside it.
        let mut text = String::new();
        text.push_str(&"a".repeat(80));
        text.push('\n');
        text.push_str("```\n");
        text.push_str("code line 1\ncode line 2\n");
        text.push_str("```\n");
        text.push_str(&"b".repeat(80));
        // max_len = 120: the 80+newline prefix is 81 chars, which fits.
        // The code fence block is ~30 chars. A naive newline split at ~120
        // would land inside the fence. The markdown-aware splitter should
        // split at the newline before the fence (position 81).
        let chunks = chunk_message(&text, 120);
        for chunk in &chunks {
            let opens = chunk.matches("```").count();
            assert_eq!(opens % 2, 0, "unbalanced code fence in chunk: {chunk:?}");
        }
    }

    /// Security: the OTP challenge message sent to the Discord user must
    /// NEVER contain the verification code. The code should only be visible
    /// to the admin in the web UI. If this test fails, unauthenticated users
    /// can self-approve without admin involvement.
    #[test]
    fn security_otp_challenge_message_does_not_contain_code() {
        let msg = OTP_CHALLENGE_MSG;

        // Must not contain any 6-digit numeric sequences (OTP codes are 6 digits).
        let has_six_digits = msg
            .as_bytes()
            .windows(6)
            .any(|w| w.iter().all(|b| b.is_ascii_digit()));
        assert!(
            !has_six_digits,
            "SECURITY: OTP_CHALLENGE_MSG must not contain numeric codes"
        );
    }

    #[test]
    fn chunk_code_fence_too_large_falls_back() {
        // When the code fence itself exceeds max_len, we must still split
        // (graceful degradation — can't avoid unbalanced fences here).
        let mut text = String::from("```\n");
        text.push_str(&"x".repeat(300));
        text.push_str("\n```\n");
        let chunks = chunk_message(&text, 100);
        assert!(chunks.len() >= 2, "should split oversized code fence");
        let reassembled: String = chunks.iter().copied().collect();
        assert_eq!(reassembled, text);
    }

    // ── OTP code detection tests ─────────────────────────────────────

    #[test]
    fn looks_like_otp_code_valid() {
        assert!(looks_like_otp_code("123456"));
        assert!(looks_like_otp_code("000000"));
        assert!(looks_like_otp_code("999999"));
    }

    #[test]
    fn looks_like_otp_code_rejects_non_codes() {
        assert!(!looks_like_otp_code("hello"));
        assert!(!looks_like_otp_code("12345")); // too short
        assert!(!looks_like_otp_code("1234567")); // too long
        assert!(!looks_like_otp_code("12345a")); // not all digits
        assert!(!looks_like_otp_code("")); // empty
        assert!(!looks_like_otp_code("abcdef")); // no digits
        assert!(!looks_like_otp_code("12 345")); // space
    }

    #[test]
    fn looks_like_otp_code_rejects_unicode_digits() {
        // Arabic-Indic digits (U+0660..U+0669) should not be accepted.
        assert!(!looks_like_otp_code(
            "\u{0660}\u{0661}\u{0662}\u{0663}\u{0664}\u{0665}"
        ));
    }

    // ── OTP message security tests ───────────────────────────────────

    #[test]
    fn security_otp_message_has_no_format_placeholders() {
        let msg = OTP_CHALLENGE_MSG;
        assert!(
            !msg.contains("{code}") && !msg.contains("{0}") && !msg.contains("%s"),
            "OTP challenge message must not contain format placeholders"
        );
    }

    #[test]
    fn security_otp_message_points_to_web_ui() {
        let msg = OTP_CHALLENGE_MSG;
        assert!(
            msg.contains("Channels") && msg.contains("Senders"),
            "OTP message must tell user where to find the code"
        );
    }

    #[test]
    fn otp_message_uses_discord_markdown_not_html() {
        let msg = OTP_CHALLENGE_MSG;
        // Discord uses ** for bold, not <b>.
        assert!(
            !msg.contains("<b>") && !msg.contains("<i>"),
            "OTP message should use Discord markdown, not HTML tags"
        );
        // Should use ** or nothing, but never HTML.
        assert!(
            !msg.contains("</"),
            "OTP message contains HTML closing tags"
        );
    }

    #[test]
    fn otp_message_mentions_expiry() {
        let msg = OTP_CHALLENGE_MSG;
        assert!(
            msg.contains("5 minutes") || msg.contains("expires"),
            "OTP message should mention the expiry time"
        );
    }

    // ── Presence config mapping tests ────────────────────────────────

    #[test]
    fn required_intents_includes_reactions() {
        let intents = required_intents();
        assert!(
            intents.contains(GatewayIntents::GUILD_MESSAGE_REACTIONS),
            "must include GUILD_MESSAGE_REACTIONS"
        );
        assert!(
            intents.contains(GatewayIntents::DIRECT_MESSAGE_REACTIONS),
            "must include DIRECT_MESSAGE_REACTIONS"
        );
    }

    #[test]
    fn required_intents_includes_message_content() {
        let intents = required_intents();
        assert!(
            intents.contains(GatewayIntents::MESSAGE_CONTENT),
            "must include MESSAGE_CONTENT for reading message text"
        );
        assert!(
            intents.contains(GatewayIntents::GUILD_MESSAGES),
            "must include GUILD_MESSAGES"
        );
        assert!(
            intents.contains(GatewayIntents::DIRECT_MESSAGES),
            "must include DIRECT_MESSAGES"
        );
        assert!(
            intents.contains(GatewayIntents::GUILDS),
            "must include GUILDS"
        );
    }

    #[test]
    fn strip_mention_with_leading_whitespace() {
        assert_eq!(strip_bot_mention("  <@123> hello", 123), "hello");
    }

    #[test]
    fn strip_mention_only_mention() {
        // When the message is just the mention, result should be empty after trim.
        assert_eq!(strip_bot_mention("<@123>", 123), "");
    }

    #[test]
    fn discord_message_created_ms_from_snowflake() {
        // Example snowflake from Discord docs / Serenity tests.
        let id = MessageId::new(175_928_847_299_117_063);
        assert_eq!(discord_message_created_ms(id), 1_462_015_105_796);
    }

    // ── Inbound media attachment tests ───────────────────────────────

    fn make_attachment(content_type: Option<&str>, filename: &str) -> Attachment {
        let content_type_json = match content_type {
            Some(ct) => format!("\"{ct}\""),
            None => "null".to_string(),
        };
        let json = format!(
            r#"{{
                "id": "1",
                "filename": "{filename}",
                "size": 1024,
                "url": "https://cdn.discordapp.com/attachments/1/2/{filename}",
                "proxy_url": "https://media.discordapp.net/attachments/1/2/{filename}",
                "content_type": {content_type_json}
            }}"#
        );
        match serde_json::from_str(&json) {
            Ok(attachment) => attachment,
            Err(error) => panic!("attachment json should deserialize: {error}"),
        }
    }

    #[test]
    fn audio_format_maps_common_mime_types() {
        assert_eq!(
            audio_format_from_attachment(Some("audio/ogg"), "x.ogg"),
            "ogg"
        );
        assert_eq!(
            audio_format_from_attachment(Some("audio/ogg; codecs=opus"), "voice.ogg"),
            "ogg"
        );
        assert_eq!(
            audio_format_from_attachment(Some("audio/mpeg"), "x.mp3"),
            "mp3"
        );
        assert_eq!(
            audio_format_from_attachment(Some("audio/mp3"), "x.mp3"),
            "mp3"
        );
        assert_eq!(
            audio_format_from_attachment(Some("audio/mp4"), "x.m4a"),
            "m4a"
        );
        assert_eq!(
            audio_format_from_attachment(Some("audio/x-m4a"), "x.m4a"),
            "m4a"
        );
        assert_eq!(
            audio_format_from_attachment(Some("audio/wav"), "x.wav"),
            "wav"
        );
        assert_eq!(
            audio_format_from_attachment(Some("audio/webm"), "x.webm"),
            "webm"
        );
    }

    #[test]
    fn audio_format_falls_back_to_extension() {
        assert_eq!(audio_format_from_attachment(None, "note.mp3"), "mp3");
        assert_eq!(audio_format_from_attachment(None, "note.Ogg"), "ogg");
        assert_eq!(audio_format_from_attachment(None, "note.opus"), "ogg");
        assert_eq!(audio_format_from_attachment(None, "note.webm"), "webm");
        // Unknown: default to ogg (Discord voice messages are OGG Opus).
        assert_eq!(audio_format_from_attachment(None, "mystery.bin"), "ogg");
    }

    #[test]
    fn image_media_type_fallback_picks_extension() {
        assert_eq!(
            image_media_type_fallback(Some("image/png; charset=binary"), "x.png"),
            "image/png"
        );
        assert_eq!(
            image_media_type_fallback(None, "screenshot.PNG"),
            "image/png"
        );
        assert_eq!(image_media_type_fallback(None, "pic.gif"), "image/gif");
        assert_eq!(image_media_type_fallback(None, "pic.webp"), "image/webp");
        // Unknown: default to jpeg.
        assert_eq!(image_media_type_fallback(None, "pic.bin"), "image/jpeg");
    }

    #[test]
    fn voice_stt_unavailable_body_uses_marker_and_caption() {
        assert_eq!(
            voice_stt_unavailable_log_body(""),
            "[Voice message - STT unavailable]"
        );
        assert_eq!(
            voice_stt_unavailable_log_body("caption"),
            "caption\n\n[Voice message - STT unavailable]"
        );
    }

    #[test]
    fn attachment_is_audio_detects_mime_and_extension() {
        assert!(attachment_is_audio(&make_attachment(
            Some("audio/ogg"),
            "v.ogg"
        )));
        assert!(attachment_is_audio(&make_attachment(None, "v.opus")));
        assert!(attachment_is_audio(&make_attachment(None, "clip.MP3")));
        assert!(!attachment_is_audio(&make_attachment(
            Some("image/png"),
            "x.png"
        )));
        assert!(!attachment_is_audio(&make_attachment(None, "notes.txt")));
    }

    #[test]
    fn attachment_is_image_detects_mime_and_extension() {
        assert!(attachment_is_image(&make_attachment(
            Some("image/jpeg"),
            "x.jpg"
        )));
        assert!(attachment_is_image(&make_attachment(None, "x.PNG")));
        assert!(attachment_is_image(&make_attachment(None, "x.webp")));
        assert!(!attachment_is_image(&make_attachment(
            Some("audio/ogg"),
            "v.ogg"
        )));
        assert!(!attachment_is_image(&make_attachment(None, "doc.pdf")));
    }

    #[test]
    fn select_media_prefers_audio_over_image() {
        let image = make_attachment(Some("image/png"), "a.png");
        let audio = make_attachment(Some("audio/ogg"), "b.ogg");
        let attachments = vec![image, audio];
        let picked = match select_media_attachment(&attachments) {
            Some(attachment) => attachment,
            None => panic!("expected audio attachment to be selected"),
        };
        assert_eq!(picked.filename, "b.ogg");
    }

    #[test]
    fn select_media_returns_none_for_unsupported() {
        let doc = make_attachment(Some("application/pdf"), "spec.pdf");
        assert!(select_media_attachment(&[doc]).is_none());
        assert!(select_media_attachment(&[]).is_none());
    }

    #[test]
    fn select_media_picks_image_when_no_audio() {
        let doc = make_attachment(Some("application/pdf"), "spec.pdf");
        let image = make_attachment(Some("image/jpeg"), "pic.jpg");
        let attachments = [doc, image];
        let picked = match select_media_attachment(&attachments) {
            Some(attachment) => attachment,
            None => panic!("expected image attachment to be selected"),
        };
        assert_eq!(picked.filename, "pic.jpg");
    }

    // ── resolve_discord_inbound_media via mock sink ──────────────────
    //
    // These tests exercise the branching logic without hitting the network.
    // We use a minimal sink mock that only implements the required trait
    // methods; everything else falls through to trait defaults.

    use {
        image::{DynamicImage, ImageBuffer, ImageFormat, Rgb},
        std::io::Cursor,
    };

    use moltis_channels::Result as ChannelsResult;

    enum MockTranscription {
        Success(String),
        Failure(String),
    }

    enum MockDownload {
        Success(Vec<u8>),
        Failure(String),
    }

    struct MockDownloader {
        outcome: MockDownload,
    }

    #[async_trait]
    impl InboundMediaDownloader for MockDownloader {
        async fn download_media(
            &self,
            _source: &InboundMediaSource,
            _max_bytes: usize,
        ) -> ChannelsResult<Vec<u8>> {
            match &self.outcome {
                MockDownload::Success(bytes) => Ok(bytes.clone()),
                MockDownload::Failure(message) => Err(ChannelError::unavailable(message)),
            }
        }
    }

    struct MockSink {
        stt_available: bool,
        transcription: MockTranscription,
    }

    #[async_trait]
    impl ChannelEventSink for MockSink {
        async fn emit(&self, _event: ChannelEvent) {}

        async fn dispatch_to_chat(
            &self,
            _text: &str,
            _reply_to: ChannelReplyTarget,
            _meta: ChannelMessageMeta,
        ) {
        }

        async fn dispatch_command(
            &self,
            _command: &str,
            _reply_to: ChannelReplyTarget,
        ) -> ChannelsResult<String> {
            Ok(String::new())
        }

        async fn transcribe_voice(
            &self,
            _audio_data: &[u8],
            _format: &str,
        ) -> ChannelsResult<String> {
            match &self.transcription {
                MockTranscription::Success(text) => Ok(text.clone()),
                MockTranscription::Failure(message) => Err(ChannelError::unavailable(message)),
            }
        }

        async fn request_disable_account(
            &self,
            _channel_type: &str,
            _account_id: &str,
            _reason: &str,
        ) {
        }

        async fn voice_stt_available(&self) -> bool {
            self.stt_available
        }
    }

    #[tokio::test]
    async fn resolve_returns_no_media_when_attachments_empty() {
        let sink = MockSink {
            stt_available: true,
            transcription: MockTranscription::Success(String::new()),
        };
        let downloader = MockDownloader {
            outcome: MockDownload::Success(Vec::new()),
        };
        let outcome = resolve_discord_inbound_media(&[], "hello", &sink, "acct", &downloader).await;
        assert!(matches!(outcome, MediaResolveOutcome::NoMedia));
    }

    #[tokio::test]
    async fn resolve_returns_no_media_when_only_unsupported() {
        let sink = MockSink {
            stt_available: true,
            transcription: MockTranscription::Success(String::new()),
        };
        let downloader = MockDownloader {
            outcome: MockDownload::Success(Vec::new()),
        };
        let doc = make_attachment(Some("application/pdf"), "spec.pdf");
        let outcome = resolve_discord_inbound_media(&[doc], "", &sink, "acct", &downloader).await;
        assert!(matches!(outcome, MediaResolveOutcome::NoMedia));
    }

    #[tokio::test]
    async fn resolve_voice_without_stt_returns_unavailable() {
        let sink = MockSink {
            stt_available: false,
            transcription: MockTranscription::Success(String::new()),
        };
        let downloader = MockDownloader {
            outcome: MockDownload::Success(Vec::new()),
        };
        let voice = make_attachment(Some("audio/ogg"), "v.ogg");
        let outcome = resolve_discord_inbound_media(&[voice], "", &sink, "acct", &downloader).await;
        assert!(matches!(outcome, MediaResolveOutcome::VoiceSttUnavailable));
    }

    fn tiny_jpeg() -> Vec<u8> {
        let image = ImageBuffer::from_pixel(1, 1, Rgb([255, 0, 0]));
        let mut cursor = Cursor::new(Vec::new());
        let write_result = DynamicImage::ImageRgb8(image).write_to(&mut cursor, ImageFormat::Jpeg);
        match write_result {
            Ok(()) => cursor.into_inner(),
            Err(error) => panic!("tiny jpeg generation should succeed: {error}"),
        }
    }

    #[tokio::test]
    async fn resolve_voice_success_combines_caption_and_transcript() {
        let sink = MockSink {
            stt_available: true,
            transcription: MockTranscription::Success("transcribed discord voice".to_string()),
        };
        let downloader = MockDownloader {
            outcome: MockDownload::Success(b"voice-bytes".to_vec()),
        };
        let voice = make_attachment(Some("audio/ogg"), "v.ogg");

        let outcome =
            resolve_discord_inbound_media(&[voice], "caption", &sink, "acct", &downloader).await;

        let media = match outcome {
            MediaResolveOutcome::Media(media) => media,
            _ => panic!("expected voice media outcome"),
        };
        assert_eq!(
            media.body,
            "caption\n\n[Voice message]: transcribed discord voice"
        );
        assert!(media.attachments.is_empty());
        assert!(matches!(media.kind, ChannelMessageKind::Voice));
        let (voice_audio, format) = match media.voice_audio {
            Some(audio) => audio,
            None => panic!("expected saved voice audio"),
        };
        assert_eq!(voice_audio, b"voice-bytes".to_vec());
        assert_eq!(format, "ogg");
    }

    #[tokio::test]
    async fn resolve_voice_download_failure_falls_back_to_marker() {
        let sink = MockSink {
            stt_available: true,
            transcription: MockTranscription::Success(String::new()),
        };
        let downloader = MockDownloader {
            outcome: MockDownload::Failure("boom".to_string()),
        };
        let voice = make_attachment(Some("audio/ogg"), "v.ogg");

        let outcome = resolve_discord_inbound_media(&[voice], "", &sink, "acct", &downloader).await;

        let media = match outcome {
            MediaResolveOutcome::Media(media) => media,
            _ => panic!("expected voice media fallback outcome"),
        };
        assert_eq!(media.body, "[Voice message - download failed]");
        assert!(media.attachments.is_empty());
        assert!(media.voice_audio.is_none());
        assert!(matches!(media.kind, ChannelMessageKind::Voice));
    }

    #[tokio::test]
    async fn resolve_voice_transcription_failure_falls_back_to_caption() {
        let sink = MockSink {
            stt_available: true,
            transcription: MockTranscription::Failure("stt offline".to_string()),
        };
        let downloader = MockDownloader {
            outcome: MockDownload::Success(b"voice-bytes".to_vec()),
        };
        let voice = make_attachment(Some("audio/ogg"), "v.ogg");

        let outcome =
            resolve_discord_inbound_media(&[voice], "caption", &sink, "acct", &downloader).await;

        let media = match outcome {
            MediaResolveOutcome::Media(media) => media,
            _ => panic!("expected voice media fallback outcome"),
        };
        assert_eq!(media.body, "caption");
        assert!(media.attachments.is_empty());
        assert!(matches!(media.kind, ChannelMessageKind::Voice));
        assert!(media.voice_audio.is_some());
    }

    #[tokio::test]
    async fn resolve_image_success_builds_multimodal_attachment() {
        let sink = MockSink {
            stt_available: true,
            transcription: MockTranscription::Success(String::new()),
        };
        let downloader = MockDownloader {
            outcome: MockDownload::Success(tiny_jpeg()),
        };
        let image = make_attachment(Some("image/jpeg"), "pic.jpg");

        let outcome =
            resolve_discord_inbound_media(&[image], "diagram", &sink, "acct", &downloader).await;

        let media = match outcome {
            MediaResolveOutcome::Media(media) => media,
            _ => panic!("expected image media outcome"),
        };
        assert_eq!(media.body, "diagram");
        assert_eq!(media.attachments.len(), 1);
        assert!(matches!(media.kind, ChannelMessageKind::Photo));
        assert!(media.voice_audio.is_none());
        assert_eq!(media.attachments[0].media_type, "image/jpeg");
        assert!(!media.attachments[0].data.is_empty());
    }
}
